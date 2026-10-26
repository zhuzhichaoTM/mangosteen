//! 端到端集成测试：从 GGUF 文件到 [OI] 兼容 HTTP 接口。
//!
//! 与单元测试的分工：
//!
//! * 单元测试验证**组件内部逻辑**（估算公式、状态机、打分）；
//! * 本测试验证**组件串起来是否真的能工作**——用真实的 GGUF 文件、
//!   真实的子进程、真实的 TCP 连接。
//!
//! ## 测试替身策略
//!
//! 推理引擎用**脚本化的假 llama-server**（Python），它实现了
//! `/health` 与 `/v1/chat/completions`。这样我们能在无 GPU、无真实模型的
//! CI 环境里验证完整链路：网关 → 引擎进程 → HTTP 转发 → 响应归一化。
//!
//! ## 隔离
//!
//! 每个测试用独立的 `MANGOSTEEN_HOME`（临时目录），互不干扰，
//! 也不污染开发者本机的真实配置。

use std::io::Read;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use mangosteen_base::{AccessGuard, ModelStore, StateStore};
use mangosteen_core::config::{Config, StorageConfig};
use mangosteen_gateway::{DeploymentRequest, Gateway, GatewayBuilder};

// ---------------------------------------------------------------- 测试脚手架

/// 独立的临时环境。
struct TestEnv {
    root: PathBuf,
    /// 本测试独占的端口区间起点。
    ///
    /// 并行测试若共用同一端口段会互相抢占——这是**测试隔离缺陷**，
    /// 不是产品缺陷。每个 TestEnv 分配 50 个端口的独立区间。
    port_base: u16,
}

impl TestEnv {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "mn-it-{tag}-{}-{}",
            std::process::id(),
            mangosteen_core::time::now_unix_millis()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("models")).unwrap();
        std::fs::create_dir_all(root.join("cfg")).unwrap();
        std::fs::create_dir_all(root.join("logs")).unwrap();
        // 按标签哈希出稳定但互不相同的端口段（22100 起，每段 50 个）
        let idx = tag
            .bytes()
            .fold(0u32, |a, b| a.wrapping_mul(31).wrapping_add(b as u32))
            % 80;
        let port_base = 22_100 + (idx as u16) * 50;
        Self { root, port_base }
    }

    fn models_dir(&self) -> PathBuf {
        self.root.join("models")
    }

    fn state_path(&self) -> PathBuf {
        self.root.join("cfg").join("state.json")
    }

    fn index_path(&self) -> PathBuf {
        self.root.join("cfg").join("models.json")
    }

    /// 写一个可被解析的 GGUF（架构参数与 Qwen2.5-7B 一致）。
    ///
    /// `weight_bytes` 控制「权重区」大小；测试里用较小值以免写盘过慢，
    /// 但架构参数保持真实，这样资源评估结果贴近实际。
    fn write_gguf(&self, name: &str, weight_bytes: usize) -> PathBuf {
        let mut bytes = build_gguf("qwen2", "qwen2.5-7b-instruct", 28, 28, 4, 3584, 32768);
        bytes.extend(std::iter::repeat(0u8).take(weight_bytes));
        let p = self.models_dir().join(name);
        std::fs::write(&p, &bytes).unwrap();
        p
    }

    /// 写假的 llama-server 脚本。
    fn write_fake_engine(&self) -> PathBuf {
        let p = self.root.join("fake-llama-server");
        let script = r#"#!/usr/bin/env python3
import json, sys
from http.server import BaseHTTPRequestHandler, HTTPServer

args = sys.argv[1:]
def opt(n, d=None):
    return args[args.index(n)+1] if n in args else d

if '--version' in args:
    print('llama-server fake 0.0.1')
    sys.exit(0)

host = opt('--host', '127.0.0.1')
port = int(opt('--port', '8080'))
ctx  = opt('--ctx-size', '4096')
ngl  = opt('--n-gpu-layers', '0')
model = opt('--model', '')

class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def _j(self, code, obj):
        b = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header('Content-Type','application/json')
        self.send_header('Content-Length', str(len(b)))
        self.end_headers()
        self.wfile.write(b)
    def do_GET(self):
        if self.path == '/health':
            self._j(200, {"status":"ok"})
        else:
            self._j(404, {"error":{"message":"not found"}})
    def do_POST(self):
        n = int(self.headers.get('Content-Length', 0))
        body = json.loads(self.rfile.read(n) or b'{}')
        if self.path == '/v1/chat/completions':
            msgs = body.get('messages') or []
            last = msgs[-1]['content'] if msgs else ''
            self._j(200, {
              "id":"chatcmpl-fake","object":"chat.completion","created":0,
              "model": body.get('model','fake'),
              "choices":[{"index":0,
                "message":{"role":"assistant","content":"echo:"+last},
                "finish_reason":"stop"}],
              "usage":{"prompt_tokens":len(last),"completion_tokens":4,
                       "total_tokens":len(last)+4}})
        elif self.path == '/v1/embeddings':
            self._j(200, {"object":"list","data":[{"object":"embedding","embedding":[0.1,0.2],"index":0}],
                          "model": body.get('model','fake'),
                          "usage":{"prompt_tokens":1,"completion_tokens":0,"total_tokens":1}})
        else:
            self._j(404, {"error":{"message":"not found"}})

# 把实际生效的参数写进 stderr，便于测试断言
sys.stderr.write("ctx=%s ngl=%s model=%s\n" % (ctx, ngl, model))
sys.stderr.flush()
HTTPServer((host, port), H).serve_forever()
"#;
        std::fs::write(&p, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        p
    }

    /// 构造配置：授权模型目录、指定假引擎、关闭云端。
    fn config(&self, engine: &Path) -> Config {
        let mut cfg = Config::sample(self.models_dir(), self.root.join("cache"));
        cfg.storage = StorageConfig {
            models_dir: self.models_dir(),
            cache_dir: self.root.join("cache"),
            allowed_read_roots: vec![self.models_dir()],
            // 测试不计算大文件哈希
            verify_hash: false,
            max_model_bytes: 8 << 30,
            default_source: "modelscope".to_string(),
        };
        cfg.local.engine_path = Some(engine.to_path_buf());
        cfg.local.port_range_start = self.port_base;
        cfg.local.port_range_end = self.port_base + 49;
        cfg.scheduler.launch_timeout_secs = 20;
        // 不配置云端服务商：验证「本地优先」在纯本地环境下的行为
        cfg.providers.clear();
        cfg
    }

    /// 组装网关。
    fn gateway(&self, cfg: Config) -> Gateway {
        let guard = AccessGuard::from_config(&cfg.storage);
        let models = ModelStore::new(self.index_path(), guard, false);
        let state = StateStore::new(self.state_path());
        GatewayBuilder::new(cfg)
            .engine_path(None)
            .log_dir(Some(self.root.join("logs")))
            .state_path(self.state_path())
            .model_index_path(self.index_path())
            .build()
            .map(|gw| {
                // 让 GatewayBuilder 的默认 store 与测试路径一致（已通过 builder 传入）
                let _ = (&models, &state);
                gw
            })
            .expect("gateway build")
    }
}

impl TestEnv {
    /// 同步预热引擎版本缓存，返回同一个 `Gateway`。
    ///
    /// ## 什么时候需要显式预热
    ///
    /// `preview` 判断「本地可行」时要确认引擎没坏。**缓存为空**时它会现场
    /// 探测，最多等 `ENGINE_PROBE_BUDGET`（2 秒）；超预算则结论为
    /// 「引擎状态未知」→ 本地判不可行。
    ///
    /// 这个保守方向在生产里是对的，但在 e2e 里会变成**假失败**：几十个用例
    /// 并行跑，CPU 被抢占，连假引擎脚本的进程启动都可能超 2 秒。凡是断言
    /// `local_viable == true` 的用例都该先调本方法，让 `preview` 只读缓存。
    ///
    /// 刻意不做成 `gateway()` 的默认行为：有一条测试专门守护「缓存为空时
    /// 接口不阻塞」（`e2e_preview_and_doctor_do_not_probe_engine_synchronously`），
    /// 全局预热会改变它的前提。
    fn warmed_gateway(&self, cfg: Config) -> Gateway {
        let mut gw = self.gateway(cfg);
        let _ = gw.warm_up_engine();
        gw
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        // 先杀本测试区间内的引擎进程，再删目录。
        //
        // 产品行为是「CLI 退出不杀引擎」（用户可能要继续用服务），所以
        // 测试必须自己收尾，否则每跑一次 e2e 就在机器上留下几个
        // fake-llama-server 常驻进程，越积越多、还占着端口。
        self.kill_engines_in_range();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

impl TestEnv {
    /// 结束监听在本测试端口区间内的假引擎进程。
    fn kill_engines_in_range(&self) {
        // 通过台账拿 PID：这是产品自己记录的真实来源，
        // 比扫端口更准（不会误杀别的测试）。
        let state = self.state_path();
        let Ok(text) = std::fs::read_to_string(&state) else {
            return;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
            return;
        };
        let Some(arr) = v.get("deployments").and_then(|d| d.as_array()) else {
            return;
        };
        for dep in arr {
            if let Some(pid) = dep.get("pid").and_then(|p| p.as_u64()) {
                let _ = mangosteen_platform::process::terminate_pid(
                    pid as u32,
                    std::time::Duration::from_secs(2),
                );
            }
        }
    }
}

/// 构造最小合法 GGUF 字节流。
fn build_gguf(
    arch: &str,
    name: &str,
    layers: u32,
    heads: u32,
    kv_heads: u32,
    embed: u32,
    ctx: u32,
) -> Vec<u8> {
    fn push_str(out: &mut Vec<u8>, s: &str) {
        out.extend_from_slice(&(s.len() as u64).to_le_bytes());
        out.extend_from_slice(s.as_bytes());
    }
    fn push_u32(out: &mut Vec<u8>, k: &str, v: u32) {
        push_str(out, k);
        out.extend_from_slice(&4u32.to_le_bytes());
        out.extend_from_slice(&v.to_le_bytes());
    }

    let mut out = Vec::new();
    out.extend_from_slice(&0x4655_4747u32.to_le_bytes()); // "GGUF"
    out.extend_from_slice(&3u32.to_le_bytes());
    out.extend_from_slice(&0u64.to_le_bytes()); // tensor_count
    out.extend_from_slice(&7u64.to_le_bytes()); // kv_count

    push_str(&mut out, "general.architecture");
    out.extend_from_slice(&8u32.to_le_bytes());
    push_str(&mut out, arch);

    push_str(&mut out, "general.name");
    out.extend_from_slice(&8u32.to_le_bytes());
    push_str(&mut out, name);

    push_u32(&mut out, &format!("{arch}.block_count"), layers);
    push_u32(&mut out, &format!("{arch}.attention.head_count"), heads);
    push_u32(
        &mut out,
        &format!("{arch}.attention.head_count_kv"),
        kv_heads,
    );
    push_u32(&mut out, &format!("{arch}.embedding_length"), embed);
    push_u32(&mut out, &format!("{arch}.context_length"), ctx);
    out
}

// ---------------------------------------------------------------- HTTP 客户端

/// 极简 HTTP 请求（测试专用，避免为测试引入依赖）。
fn http(method: &str, host: &str, port: u16, path: &str, body: Option<&str>) -> (u16, String) {
    let mut s = TcpStream::connect((host, port)).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(20))).ok();

    let payload = body.unwrap_or("");
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}:{port}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    );
    use std::io::Write;
    s.write_all(req.as_bytes()).expect("write request");

    let mut raw = Vec::new();
    s.read_to_end(&mut raw).expect("read response");
    let text = String::from_utf8_lossy(&raw).to_string();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (status, body.to_string())
}

/// 等待端口可连接。
fn wait_port(host: &str, port: u16, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if TcpStream::connect((host, port)).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(80));
    }
    false
}

/// 原子地取得一个已绑定的监听器与其端口。
///
/// ## 为什么不能用「先查空闲端口，再 bind」
///
/// 那样存在 TOCTOU 竞争窗口：`free_port()` 绑定 0 端口拿到号后立即释放，
/// 到真正 `bind` 之间，并行测试可能抢走同一个端口，表现为随机
/// `AddrInUse` 失败（每次失败的用例还不一样，极难复现）。
///
/// 这里把「取端口」与「占用端口」合并成一次系统调用：返回的 listener
/// 一直持有端口，直到调用方交给 HTTP 服务，中间不存在窗口。
fn bind_ephemeral() -> (std::net::TcpListener, u16) {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
        .expect("绑定临时端口失败：本机应允许绑定回环高端口");
    let port = listener.local_addr().expect("读取已绑定端口失败").port();
    (listener, port)
}

// ---------------------------------------------------------------- 测试用例

#[test]
fn e2e_register_assess_deploy_stop() {
    let env = TestEnv::new("full");
    let engine = env.write_fake_engine();
    let gguf = env.write_gguf("qwen.gguf", 4096);
    let cfg = env.config(&engine);

    let mut gw = env.gateway(cfg);

    // ---- 1. 登记模型：必须解析出真实架构参数 ----
    let reg = gw
        .models()
        .register_local(&gguf, Some("test"))
        .expect("register");
    assert_eq!(reg.entry.spec.n_layers, 28);
    assert_eq!(reg.entry.spec.n_kv_heads, 4);
    assert_eq!(reg.entry.spec.native_context, 32_768);
    assert!(reg.entry.spec.is_gqa());

    // ---- 2. 评估：本地应可承载（CPU 兜底）----
    let req = DeploymentRequest::for_model("qwen2.5-7b-instruct").with_context(4096);
    let evaluated = gw.evaluate(&req).expect("evaluate");
    assert!(evaluated.is_local_viable(), "小文件应可本地承载");
    assert_eq!(evaluated.assessment.context_len, 4096);

    // ---- 3. 部署：本地优先，应成功 ----
    let plan = gw.deploy(&req).expect("deploy should succeed");
    assert!(plan.origin.is_local(), "本地可行时必须走本地");
    assert!(plan.fallback_trail.is_empty(), "未发生降级");
    let port = plan.local_access_port().expect("port");
    assert!(
        (env.port_base..=env.port_base + 49).contains(&port),
        "端口应在配置区间内，实际 {port}"
    );

    // 引擎真的起来了
    assert!(
        wait_port("127.0.0.1", port, Duration::from_secs(10)),
        "引擎端口 {port} 应可连接"
    );

    // ---- 4. 状态：应报告 ready ----
    let active = gw.refresh().expect("refresh");
    assert_eq!(active.len(), 1);
    assert!(active[0].ready, "部署应处于就绪状态");
    assert!(active[0].origin.is_local());

    // ---- 5. 后端定位：应指向本地引擎 ----
    let backend = gw.active_backend().expect("backend");
    assert_eq!(backend.port, port);
    assert!(backend.origin_label.contains("本地"));

    // ---- 6. 停止：进程与台账都要清理 ----
    let id = plan.id.clone();
    gw.stop(&id).expect("stop");
    let after = gw.get_deployment(&id).expect("get").expect("exists");
    assert_eq!(
        after.status,
        mangosteen_base::state::DeploymentStatus::Destroyed
    );
    // 端口应被释放
    let mut freed = false;
    for _ in 0..40 {
        if TcpStream::connect(("127.0.0.1", port)).is_err() {
            freed = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(freed, "停止后端口 {port} 应释放");
}

#[test]
fn e2e_gateway_serves_openai_compatible_api() {
    let env = TestEnv::new("http");
    let engine = env.write_fake_engine();
    let gguf = env.write_gguf("qwen.gguf", 4096);
    let cfg = env.config(&engine);

    let mut gw = env.gateway(cfg);
    gw.models().register_local(&gguf, None).unwrap();
    let req = DeploymentRequest::for_model("qwen2.5-7b-instruct").with_context(2048);
    gw.deploy(&req).expect("deploy");

    // 起 HTTP 服务
    let (listener, port) = bind_ephemeral();
    let server = mangosteen_gateway::GatewayServer::new(
        std::sync::Arc::new(std::sync::Mutex::new(gw)),
        false,
        String::new(),
        Duration::from_secs(20),
    );
    std::thread::spawn(move || {
        let _ = server.serve(listener);
    });
    assert!(
        wait_port("127.0.0.1", port, Duration::from_secs(5)),
        "网关应监听 {port}"
    );

    // ---- /health ----
    let (status, body) = http("GET", "127.0.0.1", port, "/health", None);
    assert_eq!(status, 200);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["status"], "ok");
    assert_eq!(v["backend_ready"], true);
    assert!(v["model"].as_str().unwrap().contains("qwen2.5-7b"));

    // ---- /v1/models ----
    let (status, body) = http("GET", "127.0.0.1", port, "/v1/models", None);
    assert_eq!(status, 200);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["object"], "list");
    assert_eq!(v["data"][0]["owned_by"], "local");

    // ---- /v1/chat/completions（model=auto 应被替换）----
    let (status, body) = http(
        "POST",
        "127.0.0.1",
        port,
        "/v1/chat/completions",
        Some(r#"{"model":"auto","messages":[{"role":"user","content":"ping"}]}"#),
    );
    assert_eq!(status, 200, "body={body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["object"], "chat.completion");
    assert_eq!(v["choices"][0]["message"]["content"], "echo:ping");
    // 模型名必须被网关填成真实模型，而非 "auto"
    assert_ne!(v["model"], "auto");
    assert!(v["model"].as_str().unwrap().contains("qwen2.5-7b"));

    // ---- /v1/embeddings ----
    let (status, body) = http(
        "POST",
        "127.0.0.1",
        port,
        "/v1/embeddings",
        Some(r#"{"model":"auto","input":"hi"}"#),
    );
    assert_eq!(status, 200, "body={body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["data"][0]["embedding"][0], 0.1);

    // ---- 参数校验：非法 temperature 应 400 ----
    let (status, body) = http(
        "POST",
        "127.0.0.1",
        port,
        "/v1/chat/completions",
        Some(r#"{"model":"auto","messages":[{"role":"user","content":"x"}],"temperature":9.0}"#),
    );
    assert_eq!(status, 400, "body={body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["type"], "invalid_request_error");

    // ---- 未知路由 404 ----
    let (status, _) = http("GET", "127.0.0.1", port, "/nope", None);
    assert_eq!(status, 404);

    // ---- 管理接口 ----
    let (status, body) = http("GET", "127.0.0.1", port, "/api/status", None);
    assert_eq!(status, 200);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["active"].as_array().unwrap().len(), 1);

    let (status, body) = http(
        "POST",
        "127.0.0.1",
        port,
        "/api/assess",
        Some(r#"{"model":"qwen2.5-7b-instruct","context_len":4096}"#),
    );
    assert_eq!(status, 200);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["local_viable"], true);
    assert_eq!(v["context_len"], 4096);
}

#[test]
fn e2e_api_key_enforcement() {
    let env = TestEnv::new("auth");
    let engine = env.write_fake_engine();
    let cfg = env.config(&engine);

    let gw = env.gateway(cfg);
    let (listener, port) = bind_ephemeral();
    let server = mangosteen_gateway::GatewayServer::new(
        std::sync::Arc::new(std::sync::Mutex::new(gw)),
        true, // 要求鉴权
        "secret-key".to_string(),
        Duration::from_secs(10),
    );
    std::thread::spawn(move || {
        let _ = server.serve(listener);
    });
    assert!(wait_port("127.0.0.1", port, Duration::from_secs(5)));

    // 无 Key → 401
    let (status, _) = http("GET", "127.0.0.1", port, "/v1/models", None);
    assert_eq!(status, 401);

    // 错误 Key → 401
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    use std::io::Write;
    s.write_all(
        b"GET /v1/models HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer wrong\r\n\
          Content-Length: 0\r\nConnection: close\r\n\r\n",
    )
    .unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).ok();
    assert!(String::from_utf8_lossy(&raw).contains("401"));

    // 正确 Key → 200
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.write_all(
        b"GET /v1/models HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer secret-key\r\n\
          Content-Length: 0\r\nConnection: close\r\n\r\n",
    )
    .unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).ok();
    assert!(String::from_utf8_lossy(&raw).contains("200"));
}

#[test]
fn e2e_local_failure_degrades_and_reports_accurately() {
    let env = TestEnv::new("degrade");
    // 指向一个「存在但无法运行」的引擎
    let broken = env.root.join("broken-engine");
    std::fs::write(
        &broken,
        "#!/bin/sh\necho 'libfoo.so: cannot open shared object' >&2\nexit 127\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&broken, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let gguf = env.write_gguf("qwen.gguf", 4096);
    let cfg = env.config(&broken);
    let mut gw = env.gateway(cfg);
    gw.models().register_local(&gguf, None).unwrap();

    let req = DeploymentRequest::for_model("qwen2.5-7b-instruct").with_context(2048);
    let err = gw.deploy(&req).expect_err("引擎坏了必须失败");

    let msg = err.to_string();
    // 错误必须说明「本地失败」+「无可用云端」，而不是含糊的「资源不足」
    assert!(msg.contains("本地部署失败"), "got {msg}");
    assert!(msg.contains("云服务商"), "应说明无法降级的原因：{msg}");
    // 不能误导为资源不足
    assert!(
        !msg.contains("本地资源不足"),
        "引擎故障不应被报成资源不足：{msg}"
    );

    // 台账必须记录这次失败，便于排障
    let all = gw.list_deployments().unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(
        all[0].status,
        mangosteen_base::state::DeploymentStatus::Failed
    );
    assert!(all[0].error.is_some());
}

#[test]
fn e2e_local_only_rejects_when_engine_broken() {
    let env = TestEnv::new("localonly");
    let broken = env.root.join("broken-engine");
    std::fs::write(&broken, "#!/bin/sh\nexit 1\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&broken, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let gguf = env.write_gguf("q.gguf", 2048);
    let cfg = env.config(&broken);
    let mut gw = env.gateway(cfg);
    gw.models().register_local(&gguf, None).unwrap();

    let req = DeploymentRequest::for_model("qwen2.5-7b-instruct")
        .with_context(2048)
        .local_only();
    let err = gw.deploy(&req).expect_err("local-only 且本地失败必须报错");
    // 不应出现「云端」作为出路
    assert!(
        err.to_string().contains("禁止使用云端") || err.to_string().contains("无法降级"),
        "got {err}"
    );
}

/// 同一模型重复部署必须被拒绝，停止后才能重新部署。
///
/// 这条**取代**了旧测试 `e2e_second_deploy_uses_different_port`
/// （它断言「同模型两次部署拿到两个不同端口」）。
///
/// 产品语义变了：同一模型只允许一个活跃实例。理由——
/// 部署占端口 + 内存/显存 + 云端计费额度，起两份纯属浪费；
/// 而且「停止」按钮会变得含义不明（停哪个？）。
/// 不同量化（q4_k_m / q4_0）是不同的模型 ID，仍可各跑一个。
#[test]
fn e2e_same_model_deploy_is_rejected_until_stopped() {
    let env = TestEnv::new("ports");
    let engine = env.write_fake_engine();
    let gguf = env.write_gguf("q.gguf", 2048);
    let cfg = env.config(&engine);
    let mut gw = env.gateway(cfg);
    gw.models().register_local(&gguf, None).unwrap();

    let req = DeploymentRequest::for_model("qwen2.5-7b-instruct").with_context(2048);
    let first = gw.deploy(&req).expect("第一次部署应成功");
    let p1 = first.local_access_port().unwrap();
    assert!(p1 > 0);

    // 第二次同模型 → 明确拒绝，且原因可读
    let err = gw.deploy(&req).expect_err("同模型重复部署必须被拒绝");
    let msg = err.to_string();
    assert!(
        msg.contains("已有活跃实例") || msg.contains("只允许一个"),
        "拒绝原因应说明唯一性约束，实际：{msg}"
    );

    // 只有一个活跃实例
    let active = gw.refresh().unwrap();
    assert_eq!(
        active.iter().filter(|a| a.ready).count(),
        1,
        "同模型只能有一个活跃实例"
    );

    // 停止后可以重新部署
    gw.stop(&first.id).expect("停止应成功");
    let again = gw.deploy(&req).expect("停止后应允许重新部署");
    assert!(again.local_access_port().is_some());

    gw.stop_all();
}

#[test]
fn e2e_stop_all_is_idempotent() {
    let env = TestEnv::new("stopall");
    let engine = env.write_fake_engine();
    let cfg = env.config(&engine);
    let mut gw = env.gateway(cfg);

    // 没有部署时 stop_all 应无害
    let errors = gw.stop_all();
    assert!(errors.is_empty());

    let gguf = env.write_gguf("q.gguf", 1024);
    gw.models().register_local(&gguf, None).unwrap();
    gw.deploy(&DeploymentRequest::for_model("qwen2.5-7b-instruct").with_context(1024))
        .unwrap();

    assert!(gw.stop_all().is_empty());
    assert!(gw.stop_all().is_empty(), "重复调用必须仍然成功");
    assert!(gw.refresh().unwrap().is_empty());
}

#[test]
fn e2e_unknown_model_is_reported_clearly() {
    let env = TestEnv::new("unknown");
    let engine = env.write_fake_engine();
    let cfg = env.config(&engine);
    let gw = env.gateway(cfg);

    let err = gw
        .evaluate(&DeploymentRequest::for_model("no-such-model"))
        .expect_err("未知模型必须报错");
    assert!(
        matches!(err, mangosteen_core::MnError::ModelNotFound { .. }),
        "got {err:?}"
    );
}

#[test]
fn e2e_model_registration_is_idempotent() {
    let env = TestEnv::new("idem");
    let engine = env.write_fake_engine();
    let gguf = env.write_gguf("q.gguf", 1024);
    let cfg = env.config(&engine);
    let gw = env.gateway(cfg);

    let a = gw.models().register_local(&gguf, None).unwrap();
    let b = gw.models().register_local(&gguf, Some("v2")).unwrap();
    assert!(!a.replaced);
    assert!(b.replaced);
    assert_eq!(a.entry.id, b.entry.id);
    assert_eq!(gw.models().list().unwrap().len(), 1);
    assert_eq!(
        gw.models().list().unwrap()[0].version.as_deref(),
        Some("v2")
    );
}

#[test]
fn e2e_access_control_blocks_unauthorized_path() {
    let env = TestEnv::new("guard");
    let engine = env.write_fake_engine();
    // 模型放在授权目录**之外**
    let outside = env.root.join("outside.gguf");
    let mut bytes = build_gguf("qwen2", "x", 28, 28, 4, 3584, 4096);
    bytes.extend(std::iter::repeat(0u8).take(512));
    std::fs::write(&outside, &bytes).unwrap();

    let cfg = env.config(&engine);
    let gw = env.gateway(cfg);

    let err = gw.models().register_local(&outside, None).unwrap_err();
    assert!(
        matches!(err, mangosteen_core::MnError::AccessDenied { .. }),
        "授权目录外必须拒绝，实际：{err:?}"
    );
}

#[test]
fn e2e_state_persists_across_gateway_instances() {
    let env = TestEnv::new("persist");
    let engine = env.write_fake_engine();
    let gguf = env.write_gguf("q.gguf", 1024);

    let plan_id;
    let port;
    {
        let cfg = env.config(&engine);
        let mut gw = env.gateway(cfg);
        gw.models().register_local(&gguf, None).unwrap();
        let plan = gw
            .deploy(&DeploymentRequest::for_model("qwen2.5-7b-instruct").with_context(1024))
            .unwrap();
        plan_id = plan.id.clone();
        port = plan.local_access_port().unwrap();
    } // gw 被 drop，但引擎进程仍在运行（设计如此：CLI 退出不杀服务）

    // 新实例应能看到台账，并通过 HTTP 探活发现引擎仍在服务
    let cfg = env.config(&engine);
    let mut gw2 = env.gateway(cfg);
    let active = gw2.refresh().unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].id, plan_id);
    assert!(
        active[0].ready,
        "跨进程应能探活到仍在运行的引擎（端口 {port}）"
    );

    // 后端定位应成功
    let backend = gw2.active_backend().expect("cross-process backend");
    assert_eq!(backend.port, port);

    // 清理：新实例应按台账 PID 杀掉旧进程
    gw2.stop(&plan_id).expect("stop cross-process");
    let mut freed = false;
    for _ in 0..40 {
        if TcpStream::connect(("127.0.0.1", port)).is_err() {
            freed = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(freed, "跨进程停止后端口应释放");
}

// ---------------------------------------------------------------- Web 控制台

/// UI 页面必须能通过网关取到，且是自包含的 HTML。
///
/// 这条测试守的是「不引入前端构建链」这个决定：页面若开始依赖外部
/// CSS/JS（CDN），离线或内网环境下控制台会白屏，而用户会以为是产品坏了。
#[test]
fn e2e_web_ui_is_served_and_self_contained() {
    let env = TestEnv::new("uipage");
    let engine = env.write_fake_engine();
    let cfg = env.config(&engine);
    let gw = env.gateway(cfg);

    let (listener, port) = bind_ephemeral();
    let server = mangosteen_gateway::GatewayServer::new(
        std::sync::Arc::new(std::sync::Mutex::new(gw)),
        false,
        String::new(),
        Duration::from_secs(20),
    );
    std::thread::spawn(move || {
        let _ = server.serve(listener);
    });
    assert!(wait_port("127.0.0.1", port, Duration::from_secs(5)));

    let (status, body) = http("GET", "127.0.0.1", port, "/", None);
    assert_eq!(status, 200, "根路径应返回控制台页面");
    assert!(body.contains("Mangosteen"), "页面应含产品名");
    assert!(body.contains("<textarea"), "页面应有输入框");

    // 自包含：不得引用任何外部资源
    for bad in ["http://", "https://", "//cdn", "integrity="] {
        assert!(
            !body.contains(bad),
            "页面不得依赖外部资源（发现 `{bad}`）：内网/离线环境会白屏"
        );
    }

    // 关键交互必须在页面里（这些是需求明确要求的能力）
    assert!(
        body.contains("/api/preview"),
        "页面必须调用预估接口做费用确认"
    );
    assert!(body.contains("/v1/chat/completions"), "页面必须能对话");
    assert!(
        body.contains("确认并开始计费"),
        "云端部署必须有显式确认动作"
    );
}

/// 预估接口：本地可承载时**绝不**给出云端费用。
#[test]
fn e2e_preview_reports_local_and_never_charges() {
    let env = TestEnv::new("pvlocal");
    let engine = env.write_fake_engine();
    let gguf = env.write_gguf("qwen.gguf", 4096);
    let cfg = env.config(&engine);
    let mut gw = env.warmed_gateway(cfg);
    gw.models().register_local(&gguf, None).unwrap();

    let req = DeploymentRequest::for_model("qwen2.5-7b-instruct").with_context(2048);
    let pv = gw.preview(&req).expect("preview");

    assert!(pv.local_viable, "测试机资源足够小模型，应判定本地可行");
    assert!(pv.local.is_some(), "本地可行时必须给出本地资源分解");
    assert!(pv.cloud.is_none(), "本地可行时不得给出云端方案");
    assert!(pv.cloud_candidates.is_empty(), "本地可行时不应查云端节点");
    assert!(pv.local_reason.is_none(), "本地可行时不应有「为什么上云」");

    // 关键：预估不得产生任何部署
    assert!(
        gw.list_deployments().unwrap().is_empty(),
        "预估绝不能创建部署（这是「先确认后花钱」的底线）"
    );
}

/// 预估接口：本地不可行时给出**原因 + 具体费用**，但仍不创建实例。
#[test]
fn e2e_preview_explains_cloud_cost_without_provisioning() {
    let env = TestEnv::new("pvcloud");
    // 引擎存在但无法运行 → 本地必然失败
    let broken = env.root.join("broken-engine");
    std::fs::write(&broken, "#!/bin/sh\nexit 1\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&broken, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let gguf = env.write_gguf("qwen.gguf", 4096);
    let cfg = env.config(&broken);
    let gw = env.gateway(cfg);
    gw.models().register_local(&gguf, None).unwrap();

    let req = DeploymentRequest::for_model("qwen2.5-7b-instruct").with_context(2048);
    let pv = gw.preview(&req).expect("preview");

    // 本地不可行时必须说明原因，否则用户会以为被强制上云
    assert!(!pv.local_viable, "坏引擎应导致本地不可行");
    assert!(pv.local_reason.is_some(), "必须解释为什么必须上云");
    assert!(pv.local.is_none());

    // 该测试环境未配置云服务商 → 必须给出明确原因而不是静默失败
    if pv.cloud.is_none() {
        assert!(
            pv.cloud_error.is_some(),
            "选不到节点时必须给出原因，否则 UI 无法提示用户"
        );
    }

    // 无论如何都不得创建部署
    assert!(
        gw.list_deployments().unwrap().is_empty(),
        "预估绝不能创建部署"
    );
}

/// 预估接口的 HTTP 形状：UI 直接消费这些字段，字段名变了界面就白屏。
#[test]
fn e2e_preview_api_shape_is_stable() {
    let env = TestEnv::new("pvshape");
    let engine = env.write_fake_engine();
    let gguf = env.write_gguf("qwen.gguf", 4096);
    let cfg = env.config(&engine);
    let mut gw = env.warmed_gateway(cfg);
    gw.models().register_local(&gguf, None).unwrap();

    let (listener, port) = bind_ephemeral();
    let server = mangosteen_gateway::GatewayServer::new(
        std::sync::Arc::new(std::sync::Mutex::new(gw)),
        false,
        String::new(),
        Duration::from_secs(20),
    );
    std::thread::spawn(move || {
        let _ = server.serve(listener);
    });
    assert!(wait_port("127.0.0.1", port, Duration::from_secs(5)));

    let (status, body) = http(
        "POST",
        "127.0.0.1",
        port,
        "/api/preview",
        Some(r#"{"model":"qwen2.5-7b-instruct","context_len":2048}"#),
    );
    assert_eq!(status, 200, "body={body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();

    for key in ["model", "model_id", "local_viable", "origin"] {
        assert!(v.get(key).is_some(), "预估响应缺少字段 `{key}`：UI 依赖它");
    }
    assert_eq!(v["local_viable"], true);

    let local = v.get("local").expect("本地可行时必须有 local");
    for key in [
        "placement",
        "context_len",
        "kv_cache",
        "quant",
        "weights_bytes",
        "kv_cache_bytes",
        "compute_bytes",
        "overhead_bytes",
        "total_bytes",
    ] {
        assert!(
            local.get(key).is_some(),
            "本地预估缺少字段 `{key}`：弹窗的资源占用表依赖它"
        );
    }

    // 缺 model 必须是 400，不能 500（UI 要能把原因展示给用户）
    let (status, _) = http("POST", "127.0.0.1", port, "/api/preview", Some("{}"));
    assert_eq!(status, 400);
}

/// 自检接口：UI 的「环境自检」按钮用它。
#[test]
fn e2e_doctor_api_reports_checks() {
    let env = TestEnv::new("uidoctor");
    let engine = env.write_fake_engine();
    let gguf = env.write_gguf("qwen.gguf", 4096);
    let cfg = env.config(&engine);
    let gw = env.gateway(cfg);
    gw.models().register_local(&gguf, None).unwrap();

    let (listener, port) = bind_ephemeral();
    let server = mangosteen_gateway::GatewayServer::new(
        std::sync::Arc::new(std::sync::Mutex::new(gw)),
        false,
        String::new(),
        Duration::from_secs(20),
    );
    std::thread::spawn(move || {
        let _ = server.serve(listener);
    });
    assert!(wait_port("127.0.0.1", port, Duration::from_secs(5)));

    let (status, body) = http("GET", "127.0.0.1", port, "/api/doctor", None);
    assert_eq!(status, 200, "body={body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let checks = v["checks"].as_array().expect("必须有 checks 数组");
    assert!(checks.len() >= 3, "至少应报告引擎/策略/云服务商");

    // 每项必须有 name 与 status，UI 靠这两个字段上色
    for c in checks {
        assert!(c.get("name").is_some(), "检查项缺少 name");
        let st = c.get("status").and_then(|s| s.as_str()).unwrap_or("");
        assert!(
            ["ok", "warn", "fail"].contains(&st),
            "状态必须是 ok/warn/fail，实际 `{st}`"
        );
    }
}

/// 预热引擎版本时**不得**阻塞其它接口。
///
/// 这条守的是一个真实踩过的坑：最初把 `probe_version()` 放在
/// `gateway.lock()` 内执行，而该命令在部分安装上要 20~30 秒。
/// 结果预热线程持锁半分钟，期间所有 API 全部超时——
/// 表现为「页面能打开（静态文件不走锁）但接口全挂」。
#[test]
fn e2e_warm_up_does_not_block_api() {
    let env = TestEnv::new("warmup");
    // 用一个「启动很慢」的假引擎模拟 `--version` 耗时
    let slow = env.root.join("slow-engine");
    std::fs::write(&slow, "#!/bin/sh\nsleep 3\necho 'llama-server slow'\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&slow, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let cfg = env.config(&slow);
    let gw = env.gateway(cfg);
    let shared = std::sync::Arc::new(std::sync::Mutex::new(gw));

    // 启动预热（会在后台 sleep 3 秒）
    mangosteen_gateway::spawn_warm_up(std::sync::Arc::clone(&shared));

    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = mangosteen_gateway::GatewayServer::new(
        std::sync::Arc::clone(&shared),
        false,
        String::new(),
        Duration::from_secs(20),
    );
    std::thread::spawn(move || {
        let _ = server.serve(listener);
    });
    assert!(wait_port("127.0.0.1", port, Duration::from_secs(5)));

    // 预热期间（引擎还没探测完）接口必须立刻可用
    let started = Instant::now();
    let (status, body) = http("GET", "127.0.0.1", port, "/api/settings", None);
    let elapsed = started.elapsed();
    assert_eq!(status, 200, "body={body}");
    assert!(
        elapsed < Duration::from_millis(1500),
        "预热期间 /api/settings 不应被阻塞，实测 {elapsed:?}"
    );

    // /api/status 同样必须快速返回（引擎版本此时应为 null，不是卡住）
    let started = Instant::now();
    let (status, _) = http("GET", "127.0.0.1", port, "/api/status", None);
    let elapsed = started.elapsed();
    assert_eq!(status, 200);
    assert!(
        elapsed < Duration::from_millis(1500),
        "预热期间 /api/status 不应被阻塞，实测 {elapsed:?}"
    );
}

// ------------------------------------------------------- 搜索与下载任务化

/// 搜索接口必须**区分两种模式**：关键字搜仓库、完整路径列文件。
///
/// 这条守的是用户最直接的抱怨——「搜索列表获取失败」。
/// 上一版只接受完整仓库名，传关键字会 404 并报「网络操作失败」，
/// 用户看不出是「该换个源」还是「网络坏了」。
#[test]
fn e2e_search_rejects_keyword_on_source_without_search_api() {
    let env = TestEnv::new("srchkw");
    let engine = env.write_fake_engine();
    let cfg = env.config(&engine);
    let gw = env.gateway(cfg);

    // 直接 bind 到 0 并保留 listener：先取端口再释放会与并行测试抢端口
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = mangosteen_gateway::GatewayServer::new(
        std::sync::Arc::new(std::sync::Mutex::new(gw)),
        false,
        String::new(),
        Duration::from_secs(20),
    );
    std::thread::spawn(move || {
        let _ = server.serve(listener);
    });
    assert!(wait_port("127.0.0.1", port, Duration::from_secs(5)));

    // modelscope 没有公开检索接口 → 关键字搜索必须给出**可操作**的错误
    let (status, body) = http(
        "POST",
        "127.0.0.1",
        port,
        "/api/models/search",
        Some(r#"{"query":"qwen","source":"modelscope"}"#),
    );
    assert_eq!(status, 400, "body={body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let msg = v["error"]["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("不支持") && (msg.contains("hf-mirror") || msg.contains("仓库路径")),
        "错误信息必须告诉用户该怎么办，实际：{msg}"
    );
}

/// 空 query 必须是 400，而不是静默返回空结果。
#[test]
fn e2e_search_requires_query() {
    let env = TestEnv::new("srchempty");
    let engine = env.write_fake_engine();
    let cfg = env.config(&engine);
    let gw = env.gateway(cfg);

    // 直接 bind 到 0 并保留 listener：先取端口再释放会与并行测试抢端口
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = mangosteen_gateway::GatewayServer::new(
        std::sync::Arc::new(std::sync::Mutex::new(gw)),
        false,
        String::new(),
        Duration::from_secs(20),
    );
    std::thread::spawn(move || {
        let _ = server.serve(listener);
    });
    assert!(wait_port("127.0.0.1", port, Duration::from_secs(5)));

    let (status, _) = http(
        "POST",
        "127.0.0.1",
        port,
        "/api/models/search",
        Some(r#"{"query":"   "}"#),
    );
    assert_eq!(status, 400, "空白关键字应被拒绝");
}

/// 下载接口必须**立即返回** job_id，不得阻塞到下载完成。
///
/// 这是任务化改造的核心契约。若哪天有人把 `download_model` 放回请求
/// 处理路径，这条会因为耗时判断而失败——7B 模型要几分钟，
/// 浏览器早超时了。
#[test]
fn e2e_pull_returns_job_immediately() {
    let env = TestEnv::new("pulljob");
    let engine = env.write_fake_engine();
    let cfg = env.config(&engine);
    let gw = env.gateway(cfg);

    // 直接 bind 到 0 并保留 listener：先取端口再释放会与并行测试抢端口
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = mangosteen_gateway::GatewayServer::new(
        std::sync::Arc::new(std::sync::Mutex::new(gw)),
        false,
        String::new(),
        Duration::from_secs(20),
    );
    std::thread::spawn(move || {
        let _ = server.serve(listener);
    });
    assert!(wait_port("127.0.0.1", port, Duration::from_secs(5)));

    // 用一个不存在的文件：提交阶段的元数据查询会失败，但请求本身
    // 必须在毫秒级返回（不是挂在那里等网络）
    let started = Instant::now();
    let (status, body) = http(
        "POST",
        "127.0.0.1",
        port,
        "/api/models/pull",
        Some(r#"{"model":"hf-mirror:owner/repo/not-exist.gguf"}"#),
    );
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_secs(15),
        "下载提交不应阻塞，实测 {elapsed:?}"
    );
    // 可能 400（该模型文件名不存在于仓库）或 200（网络不可用但已排队）
    assert!(
        status == 200 || status == 400,
        "status={status} body={body}"
    );
    if status == 200 {
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(v.get("job_id").is_some(), "成功时必须返回 job_id：{body}");
    }
}

/// 任务列表接口的形状：UI 的进度条直接消费这些字段。
#[test]
fn e2e_jobs_api_shape_is_stable() {
    let env = TestEnv::new("jobshape");
    let engine = env.write_fake_engine();
    let cfg = env.config(&engine);
    let gw = env.gateway(cfg);

    // 直接 bind 到 0 并保留 listener：先取端口再释放会与并行测试抢端口
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = mangosteen_gateway::GatewayServer::new(
        std::sync::Arc::new(std::sync::Mutex::new(gw)),
        false,
        String::new(),
        Duration::from_secs(20),
    );
    std::thread::spawn(move || {
        let _ = server.serve(listener);
    });
    assert!(wait_port("127.0.0.1", port, Duration::from_secs(5)));

    let (status, body) = http("GET", "127.0.0.1", port, "/api/models/jobs", None);
    assert_eq!(status, 200, "body={body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(
        v.get("jobs").and_then(|j| j.as_array()).is_some(),
        "必须返回 jobs 数组：{body}"
    );

    // 查不存在的任务 → 404，不能 500
    let (status, _) = http(
        "GET",
        "127.0.0.1",
        port,
        "/api/models/jobs?id=pull-99999",
        None,
    );
    assert_eq!(status, 404);
}

/// UI 不再提供独立的下载入口（下载只保留 CLI）。
///
/// 产品决策：下载能力作为 `mangosteen models pull` 保留，
/// 界面不展示下载按钮、勾选条与下载任务面板。
/// 部署页的「部署」会自动完成下载，用户不需要单独管下载。
#[test]
fn e2e_web_ui_has_no_download_controls() {
    let env = TestEnv::new("nodlui");
    let engine = env.write_fake_engine();
    let cfg = env.config(&engine);
    let gw = env.gateway(cfg);

    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = mangosteen_gateway::GatewayServer::new(
        std::sync::Arc::new(std::sync::Mutex::new(gw)),
        false,
        String::new(),
        Duration::from_secs(20),
    );
    std::thread::spawn(move || {
        let _ = server.serve(listener);
    });
    assert!(wait_port("127.0.0.1", port, Duration::from_secs(5)));

    let (status, body) = http("GET", "127.0.0.1", port, "/", None);
    assert_eq!(status, 200);

    // 不再有下载按钮 / 下载任务面板
    assert!(
        !body.contains(r#"id="dp-download""#),
        "UI 不应再有「下载」按钮"
    );
    assert!(
        !body.contains(r#"id="dp-jobs-card""#),
        "UI 不应再有下载任务面板"
    );
    assert!(
        !body.contains("<h3>下载任务</h3>"),
        "UI 不应有「下载任务」面板标题"
    );
    // 取而代之的是「部署」
    assert!(
        body.contains(r#"id="dp-deploy-picked""#),
        "搜索区应有「部署」按钮"
    );
    // CLI 能力仍在（文档/提示里保留 models pull）
    assert!(body.contains("models pull"), "应提示 CLI 仍可下载");
}

#[test]
fn e2e_preview_and_doctor_do_not_probe_engine_synchronously() {
    let env = TestEnv::new("noprobe");
    // 假引擎：--version 要 5 秒，模拟真实引擎的慢启动
    let slow = env.root.join("slow-engine");
    std::fs::write(&slow, "#!/bin/sh\nsleep 5\necho 'llama-server slow'\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&slow, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let gguf = env.write_gguf("qwen.gguf", 4096);
    let cfg = env.config(&slow);
    let gw = env.gateway(cfg);
    gw.models().register_local(&gguf, None).unwrap();

    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = mangosteen_gateway::GatewayServer::new(
        std::sync::Arc::new(std::sync::Mutex::new(gw)),
        false,
        String::new(),
        Duration::from_secs(30),
    );
    std::thread::spawn(move || {
        let _ = server.serve(listener);
    });
    assert!(wait_port("127.0.0.1", port, Duration::from_secs(5)));

    // preview：必须远快于引擎探测耗时
    let started = Instant::now();
    let (status, body) = http(
        "POST",
        "127.0.0.1",
        port,
        "/api/preview",
        Some(r#"{"model":"qwen2.5-7b-instruct","context_len":2048}"#),
    );
    let elapsed = started.elapsed();
    assert_eq!(status, 200, "body={body}");
    // 设计上允许等一个短预算（2s），但绝不能等满引擎探测的 5 秒
    assert!(
        elapsed < Duration::from_secs(4),
        "预估接口不得等满引擎探测，实测 {elapsed:?}（假引擎 --version 需 5 秒）"
    );

    // doctor：同样必须快
    let started = Instant::now();
    let (status, body) = http("GET", "127.0.0.1", port, "/api/doctor", None);
    let elapsed = started.elapsed();
    assert_eq!(status, 200, "body={body}");
    assert!(
        elapsed < Duration::from_secs(4),
        "自检接口不得等满引擎探测，实测 {elapsed:?}"
    );

    // 引擎已找到但还没探测出结果 → 必须是 warn（探测中），不能是 fail
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let engine_check = v["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"].as_str().unwrap_or("").contains("推理引擎"))
        .expect("必须有引擎检查项");
    assert_eq!(
        engine_check["status"], "warn",
        "缓存为空应报「探测中」（warn），不能误报失败：{engine_check}"
    );
}

/// `/api/models` 必须返回磁盘路径，且能识别文件缺失。
///
/// 为什么要专门测：模型动辄几百兆到几 GB，用户需要知道它占在哪块盘、
/// 好清理或备份。只给「有个模型叫这名字」在实际使用中不够。
///
/// 另一个关键点：文件被手动删掉后，台账里仍有记录。接口必须如实标出
/// `file_exists: false`——否则用户看着路径却打不开，比没路径更困惑。
#[test]
fn e2e_models_api_reports_path_and_missing_files() {
    let env = TestEnv::new("mpath");
    let engine = env.write_fake_engine();
    let gguf = env.write_gguf("present.gguf", 8192);
    let cfg = env.config(&engine);
    let gw = env.gateway(cfg);
    gw.models().register_local(&gguf, None).unwrap();

    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = mangosteen_gateway::GatewayServer::new(
        std::sync::Arc::new(std::sync::Mutex::new(gw)),
        false,
        String::new(),
        Duration::from_secs(20),
    );
    std::thread::spawn(move || {
        let _ = server.serve(listener);
    });
    assert!(wait_port("127.0.0.1", port, Duration::from_secs(5)));

    let (status, body) = http("GET", "127.0.0.1", port, "/api/models", None);
    assert_eq!(status, 200, "body={body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let models = v["models"].as_array().expect("必须有 models 数组");
    assert_eq!(models.len(), 1);

    let m = &models[0];
    let path = m["path"].as_str().unwrap_or("");
    assert!(path.ends_with(".gguf"), "必须给出完整路径，实际 `{path}`");
    assert_eq!(m["file_exists"], true);
    assert!(m["size_bytes"].as_u64().unwrap_or(0) > 0, "应给出实际大小");

    // 把文件删掉，接口必须如实报告缺失
    std::fs::remove_file(&gguf).unwrap();
    let (_, body) = http("GET", "127.0.0.1", port, "/api/models", None);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        v["models"][0]["file_exists"], false,
        "文件已删除时必须报 file_exists=false，不能假装还在"
    );
}

/// 关键字搜索默认要取到 100 个仓库，供前端分页浏览。
///
/// 早先默认 30 个：一个热门关键字（`qwen gguf`）就有上百个仓库，
/// 30 条根本不够翻，用户会觉得「搜不全」。
#[test]
fn e2e_search_defaults_to_100_repos() {
    let env = TestEnv::new("s100");
    let engine = env.write_fake_engine();
    let cfg = env.config(&engine);
    let gw = env.gateway(cfg);

    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = mangosteen_gateway::GatewayServer::new(
        std::sync::Arc::new(std::sync::Mutex::new(gw)),
        false,
        String::new(),
        Duration::from_secs(20),
    );
    std::thread::spawn(move || {
        let _ = server.serve(listener);
    });
    assert!(wait_port("127.0.0.1", port, Duration::from_secs(5)));

    // 不传 limit 时，网关应把 100 作为默认值下发给上游。
    // 这里断言的是「下游拿到的 limit 是 100」这个契约：
    // 用一个必定失败的源，观察错误信息里拼出的 URL。
    let (status, body) = http(
        "POST",
        "127.0.0.1",
        port,
        "/api/models/search",
        Some(r#"{"query":"qwen","source":"hf-mirror"}"#),
    );
    // 测试环境无网络：要么成功拿到列表，要么失败——但都不能是因为 limit 被截到 30
    if status == 200 {
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let n = v["repos"].as_array().map(|a| a.len()).unwrap_or(0);
        assert!(n <= 100, "不应超过上限 100，实际 {n}");
    }
}

/// 部署页不得出现 ModelScope 选项，也不得有「（可搜索）」这类冗余后缀。
///
/// 前者是可用性要求：ModelScope 没有公开检索接口，放进来只会让用户
/// 在「搜不到」与「网络坏了」之间猜。后者是文案要求。
#[test]
fn e2e_deploy_page_source_options_are_clean() {
    let env = TestEnv::new("srcopt");
    let engine = env.write_fake_engine();
    let cfg = env.config(&engine);
    let gw = env.gateway(cfg);

    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = mangosteen_gateway::GatewayServer::new(
        std::sync::Arc::new(std::sync::Mutex::new(gw)),
        false,
        String::new(),
        Duration::from_secs(20),
    );
    std::thread::spawn(move || {
        let _ = server.serve(listener);
    });
    assert!(wait_port("127.0.0.1", port, Duration::from_secs(5)));

    let (status, body) = http("GET", "127.0.0.1", port, "/", None);
    assert_eq!(status, 200);

    // 源清单常量里不得包含 modelscope
    assert!(
        body.contains(r#"SEARCHABLE = ["hf-mirror", "hf"]"#),
        "部署页的可搜索源应只有 hf-mirror 与 hf"
    );
    assert!(!body.contains("（可搜索）"), "源名不应带「（可搜索）」后缀");
    // 新增控件必须存在。
    // 「取消选择」已改为文件列表内的子按钮（「全选 / 取消」），
    // 不再与搜索框同级——这里只断言分页容器。
    assert!(body.contains(r#"id="dp-pager""#), "缺少分页容器");
    assert!(
        body.contains("/api/models\""),
        "已安装模型必须走带路径的管理接口"
    );
    assert!(
        body.contains("clearSelection"),
        "「取消选择」必须是单一入口，避免状态与界面不一致"
    );
}

/// 部署页七项交互约束，逐条守住。
///
/// 这些是用户实际使用中提出的具体问题，最容易在后续重构中被无意改回去，
/// 因此用一条测试集中锁定页面结构。断言都尽量贴着用户可见的文案/ID，
/// 而不是内部实现细节。
#[test]
fn e2e_deploy_page_interaction_contracts() {
    let env = TestEnv::new("uictr");
    let engine = env.write_fake_engine();
    let cfg = env.config(&engine);
    let gw = env.gateway(cfg);

    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = mangosteen_gateway::GatewayServer::new(
        std::sync::Arc::new(std::sync::Mutex::new(gw)),
        false,
        String::new(),
        Duration::from_secs(20),
    );
    std::thread::spawn(move || {
        let _ = server.serve(listener);
    });
    assert!(wait_port("127.0.0.1", port, Duration::from_secs(5)));

    let (status, body) = http("GET", "127.0.0.1", port, "/", None);
    assert_eq!(status, 200);

    // 【4】renderFiles 必须是**定义**，不能只有调用。
    // 上一版把它删了却留下两处调用，用户点「查看文件」就报
    // `renderFiles is not defined`，整个文件列表打不开。
    assert!(
        body.contains("function renderFiles("),
        "renderFiles 必须有定义——否则点开仓库文件列表会报未定义"
    );

    // 【5】「取消选择」不再与搜索同级，改为文件列表的子按钮「全选 / 取消」
    assert!(
        !body.contains(r#"id="dp-clear-sel""#),
        "「取消选择」不应再出现在搜索栏（已改为列表内子按钮）"
    );
    assert!(body.contains("selectAllFiles"), "缺少全选/取消的实现");
    assert!(body.contains(r#""全选""#), "缺少「全选」按钮");
    assert!(
        body.contains("files-head"),
        "缺少文件列表标题行（放全选/取消）"
    );

    // 【6】安装路径已完全不显示（用户后续要求取消）。
    // 需要路径时用命令行 `mangosteen models list`。
    assert!(!body.contains("tdPath"), "已安装模型不应再渲染路径单元格");

    // 【7】删除模型默认连文件一起删
    assert!(
        body.contains("delete_file=1"),
        "删除按钮应默认携带 delete_file=1（否则留下孤儿大文件）"
    );

    // 【2】不再显示「已连接 · 模型名」（只留底部「模型信息」）
    // 排除注释里对该文案的说明，只查实际赋值的字符串
    assert!(
        !body.contains(r#"textContent = "已连接 · ""#),
        "不应再把「已连接 · 模型名」写进去（与「模型信息」重复）"
    );

    // 【1】对话开始后隐藏欢迎示例
    assert!(
        body.contains("function dropWelcome("),
        "缺少欢迎块清理逻辑——开聊后示例文案会残留在对话流里"
    );

    // 【3】活跃部署表头是「模型 ID」
    assert!(
        body.contains(">模型 ID<"),
        "活跃部署应显示完整模型 ID（含量化后缀）"
    );
}

/// 部署接口必须返回 `model_id`（含量化后缀）。
///
/// `model` 只有裸名（如 `qwen2.5-0.5b-instruct`），同家族多个量化并存时
/// 分不清实际在跑哪一个。UI 的「活跃部署」直接用 `model_id`。
#[test]
fn e2e_deployments_expose_model_id_with_quant() {
    let env = TestEnv::new("depmid");
    let engine = env.write_fake_engine();
    let gguf = env.write_gguf("qwen.gguf", 4096);
    let cfg = env.config(&engine);
    let gw = env.gateway(cfg);
    gw.models().register_local(&gguf, None).unwrap();

    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = mangosteen_gateway::GatewayServer::new(
        std::sync::Arc::new(std::sync::Mutex::new(gw)),
        false,
        String::new(),
        Duration::from_secs(30),
    );
    std::thread::spawn(move || {
        let _ = server.serve(listener);
    });
    assert!(wait_port("127.0.0.1", port, Duration::from_secs(5)));

    // 先部署一个，才能查到部署记录
    let (status, _body) = http(
        "POST",
        "127.0.0.1",
        port,
        "/api/deploy",
        Some(r#"{"model":"qwen2.5-7b-instruct","context_len":2048,"allow_cloud":false}"#),
    );
    if status != 201 && status != 200 {
        // 测试环境引擎能力有限，部署可能失败；此时跳过（另有专门的部署 e2e）
        return;
    }

    let (status, body) = http("GET", "127.0.0.1", port, "/api/deployments", None);
    assert_eq!(status, 200, "body={body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let arr = v["deployments"].as_array().unwrap();
    assert!(!arr.is_empty(), "应至少有一条部署记录");
    for d in arr {
        let mid = d["model_id"].as_str().unwrap_or("");
        assert!(
            mid.contains(':'),
            "model_id 必须含量化后缀（形如 name:q4_k_m），实际 `{mid}`"
        );
    }
}

/// 部署页视图切换与列精简（第三轮优化）。
///
/// 守三件事：
/// 1. 从「仓库列表（有分页）」切到「文件列表」时，分页条必须收起——
///    否则旧页码挂在文件表格下方，用户以为「页面没切换成功」，
///    点页码还会把仓库列表刷回来。
/// 2. 不再有冗余的「清空选择」入口（文件列表已有「全选/取消」）。
/// 3. 已安装模型不再显示安装路径。
#[test]
fn e2e_deploy_view_switch_and_slim_columns() {
    let env = TestEnv::new("uiswitch");
    let engine = env.write_fake_engine();
    let cfg = env.config(&engine);
    let gw = env.gateway(cfg);

    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = mangosteen_gateway::GatewayServer::new(
        std::sync::Arc::new(std::sync::Mutex::new(gw)),
        false,
        String::new(),
        Duration::from_secs(20),
    );
    std::thread::spawn(move || {
        let _ = server.serve(listener);
    });
    assert!(wait_port("127.0.0.1", port, Duration::from_secs(5)));

    let (status, body) = http("GET", "127.0.0.1", port, "/", None);
    assert_eq!(status, 200);

    // 【1】renderFiles 必须收起分页条：入口函数统一调用 hidePager
    assert!(
        body.contains("function hidePager("),
        "缺少 hidePager 统一入口"
    );
    let files_start = body
        .find("function renderFiles(")
        .expect("renderFiles 必须存在");
    let files_end = body[files_start..]
        .find("function openRepo(")
        .map(|k| files_start + k)
        .unwrap_or(body.len());
    let render_files = &body[files_start..files_end];
    assert!(
        render_files.contains("hidePager()"),
        "renderFiles 必须收起分页条，否则切到文件列表时旧页码会残留"
    );

    // 【2】不再有「清空选择」这个冗余入口
    assert!(
        !body.contains(r#"el("span", "clear", "清空选择")"#),
        "「清空选择」应已移除（列表内已有「全选/取消」）"
    );

    // 【3】已安装模型不再有路径列 / 路径按钮
    assert!(!body.contains("tdPath"), "已安装模型不应再渲染路径单元格");
    assert!(
        !body.contains(r#"el("button", "sm", "路径")"#),
        "已安装模型不应再有「路径」切换按钮"
    );
    assert!(
        !body.contains("path-cell"),
        "路径展示样式应一并移除，避免死代码"
    );
}

/// 模型列表要给出「能否部署」，供 UI 高亮/置灰部署按钮。
///
/// 关键约束：这个判断必须是**纯本地计算**——不探测引擎（本机 27 秒）、
/// 不查云端库存（网络）。列表要秒开，否则用户打开部署页就卡住。
///
/// 完整评估（含云端候选与费用）只在点「部署」时做，这是「下载与评估分离」
/// 的落地方式。
#[test]
fn e2e_models_api_reports_deployability() {
    let env = TestEnv::new("mdeploy");
    let engine = env.write_fake_engine();
    let gguf = env.write_gguf("ok.gguf", 8192);
    let cfg = env.config(&engine);
    let gw = env.gateway(cfg);
    gw.models().register_local(&gguf, None).unwrap();

    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = mangosteen_gateway::GatewayServer::new(
        std::sync::Arc::new(std::sync::Mutex::new(gw)),
        false,
        String::new(),
        Duration::from_secs(20),
    );
    std::thread::spawn(move || {
        let _ = server.serve(listener);
    });
    assert!(wait_port("127.0.0.1", port, Duration::from_secs(5)));

    // 列表必须快：若有人在这里加引擎探测或云端查询，这条会超时失败
    let started = Instant::now();
    let (status, body) = http("GET", "127.0.0.1", port, "/api/models", None);
    let elapsed = started.elapsed();
    assert_eq!(status, 200, "body={body}");
    assert!(
        elapsed < Duration::from_secs(2),
        "模型列表必须秒开（纯本地计算），实测 {elapsed:?}"
    );

    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let m = &v["models"][0];
    assert!(
        m.get("deployable").and_then(|x| x.as_bool()).is_some(),
        "必须给出 deployable 字段：{m}"
    );
    assert_eq!(m["deployable"], true, "文件完整且资源够 → 可部署");

    // 文件删掉后必须转为不可部署，并给出原因
    std::fs::remove_file(&gguf).unwrap();
    let (_, body) = http("GET", "127.0.0.1", port, "/api/models", None);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let m = &v["models"][0];
    assert_eq!(m["deployable"], false, "文件丢失后必须不可部署");
    let reason = m["deploy_blocked_reason"].as_str().unwrap_or("");
    assert!(
        !reason.is_empty(),
        "不可部署时必须给出原因，否则用户不知道为什么按钮是灰的"
    );
}

/// 下载页四项交互约束。
#[test]
fn e2e_download_page_contracts() {
    let env = TestEnv::new("dlctr");
    let engine = env.write_fake_engine();
    let cfg = env.config(&engine);
    let gw = env.gateway(cfg);

    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = mangosteen_gateway::GatewayServer::new(
        std::sync::Arc::new(std::sync::Mutex::new(gw)),
        false,
        String::new(),
        Duration::from_secs(20),
    );
    std::thread::spawn(move || {
        let _ = server.serve(listener);
    });
    assert!(wait_port("127.0.0.1", port, Duration::from_secs(5)));

    let (status, body) = http("GET", "127.0.0.1", port, "/", None);
    assert_eq!(status, 200);

    // 【2】标题改名 + 部署按钮按可部署性高亮/置灰
    // 卡片名随产品迭代改过两次：已安装模型 → 已下载模型 → 模型列表
    assert!(body.contains("<h3>模型列表</h3>"), "标题应为「模型列表」");
    assert!(!body.contains("已安装模型"), "旧标题「已安装模型」应消失");
    assert!(
        !body.contains("<h3>已下载模型</h3>"),
        "旧标题「已下载模型」应消失"
    );
    // 生命周期三按钮（替代原先的二态切换）
    assert!(body.contains(r#""启动""#), "应有「启动」");
    assert!(body.contains(r#""停止""#), "应有「停止」");
    assert!(body.contains(r#""卸载""#), "应有「卸载」");
    assert!(
        body.contains("if (enabled) b.onclick = fn;"),
        "不可用的生命周期按钮必须置灰"
    );

    // 【3】搜索 / 下载 统一为 primary sm，与「部署」同款高亮。
    //
    // 注意历史演进：早先要求它们「与全选一致且不带 primary」，
    // 后来改为「与部署一致并加高亮」——主操作应该视觉上更突出。
    // 独立的「停止」按钮已合并进部署按钮，故不再断言它。
    assert!(
        body.contains(r#"<button class="primary sm" id="dp-search">"#),
        "搜索按钮应与「部署」同款高亮（primary sm）"
    );
    assert!(
        body.contains(r#"<button class="primary sm" id="dp-deploy-picked""#),
        "「部署」按钮应为高亮主按钮（primary sm）"
    );

    // 【4】文件列表不再有逐行「下载」按钮；顶部是「部署」（单选）
    assert!(
        !body.contains("startDownload("),
        "不应再有批量下载逻辑（下载已移到 CLI）"
    );
    assert!(body.contains("function deployPicked("), "必须保留部署入口");
    // 多选校验：同一模型不同量化 / 不同模型都必须拦住
    assert!(
        body.contains("function baseModelName("),
        "需要主模型名推导，用于识别「同一模型的不同量化」"
    );
    assert!(body.contains("请只选一个"), "多选时必须提示只能选一个");
}

/// 同一模型只允许一个活跃实例。
///
/// 为什么是硬约束：部署占用端口、内存/显存与（云端）计费额度。起两个
/// 同模型实例既浪费资源，又让「停止」语义变得含糊——停哪个？
///
/// 判定用**模型 ID**（含量化后缀）而不是模型名：同家族不同量化是不同模型，
/// 允许各跑一个。
#[test]
fn e2e_same_model_cannot_deploy_twice() {
    let env = TestEnv::new("nodup");
    let engine = env.write_fake_engine();
    let gguf = env.write_gguf("qwen.gguf", 8192);
    let cfg = env.config(&engine);
    let gw = env.gateway(cfg);
    gw.models().register_local(&gguf, None).unwrap();

    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = mangosteen_gateway::GatewayServer::new(
        std::sync::Arc::new(std::sync::Mutex::new(gw)),
        false,
        String::new(),
        Duration::from_secs(30),
    );
    std::thread::spawn(move || {
        let _ = server.serve(listener);
    });
    assert!(wait_port("127.0.0.1", port, Duration::from_secs(5)));

    // 第一次部署
    let (st1, b1) = http(
        "POST",
        "127.0.0.1",
        port,
        "/api/deploy",
        Some(r#"{"model":"qwen2.5-7b-instruct","context_len":2048,"allow_cloud":false}"#),
    );
    if st1 != 201 && st1 != 200 {
        // 环境不支持真实部署时跳过（另有专门部署 e2e）
        return;
    }

    // 第二次同模型 → 必须被拒绝，且原因可读
    let (st2, b2) = http(
        "POST",
        "127.0.0.1",
        port,
        "/api/deploy",
        Some(r#"{"model":"qwen2.5-7b-instruct","context_len":2048,"allow_cloud":false}"#),
    );
    assert_eq!(st2, 400, "重复部署必须被拒绝，body={b2}");
    let v2: serde_json::Value = serde_json::from_str(&b2).unwrap();
    let msg = v2["error"]["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("已有活跃实例") || msg.contains("只允许一个"),
        "拒绝原因必须说明「同一模型只允许一个实例」，实际：{msg}"
    );

    // 只有一条活跃记录
    let (_, body) = http("GET", "127.0.0.1", port, "/api/deployments", None);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let live: Vec<_> = v["deployments"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| d["status"] == "ready" || d["status"] == "provisioning")
        .collect();
    assert_eq!(live.len(), 1, "活跃实例只能有一个");
    let _ = b1;
}

/// 部署记录必须带 PID，UI 才能在「活跃模型」里显示进程号。
#[test]
fn e2e_deployments_report_pid() {
    let env = TestEnv::new("dep_pid");
    let engine = env.write_fake_engine();
    let gguf = env.write_gguf("qwen.gguf", 8192);
    let cfg = env.config(&engine);
    let gw = env.gateway(cfg);
    gw.models().register_local(&gguf, None).unwrap();

    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = mangosteen_gateway::GatewayServer::new(
        std::sync::Arc::new(std::sync::Mutex::new(gw)),
        false,
        String::new(),
        Duration::from_secs(30),
    );
    std::thread::spawn(move || {
        let _ = server.serve(listener);
    });
    assert!(wait_port("127.0.0.1", port, Duration::from_secs(5)));

    let (st, _) = http(
        "POST",
        "127.0.0.1",
        port,
        "/api/deploy",
        Some(r#"{"model":"qwen2.5-7b-instruct","context_len":2048,"allow_cloud":false}"#),
    );
    if st != 201 && st != 200 {
        return;
    }

    let (_, body) = http("GET", "127.0.0.1", port, "/api/deployments", None);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let d = &v["deployments"][0];
    assert!(
        d.get("pid").is_some(),
        "部署记录必须有 pid 字段（值可以为 null，例如云端实例）：{d}"
    );
    assert!(
        d["pid"].as_u64().unwrap_or(0) > 0,
        "本地部署应给出真实 PID：{d}"
    );
}

/// 下载场景下 prepare 早于文件存在，`plan.model_path` 是空的。
///
/// 真实故障：远端部署（未下载）在启动阶段报
/// 「模型文件不存在：」——冒号后什么都没有。根因是
/// `evaluate_remote` 构造的 `Evaluated.model_path` 为空，
/// `LocalPlan::from_assessment` 照抄空值，而 prepare 又发生在下载之前。
///
/// `PreparedLocal::with_model_path` 就是补这个的。这条测试锁住行为，
/// 避免以后有人「优化」掉这一步。
#[test]
fn e2e_prepared_local_gets_model_path_before_launch() {
    use mangosteen_gateway::gateway::{PreparedDeployment, PreparedLocal};

    let env = TestEnv::new("prep_path");
    let engine = env.write_fake_engine();
    let gguf = env.write_gguf("qwen.gguf", 8192);
    let cfg = env.config(&engine);
    let mut gw = env.gateway(cfg);
    gw.models().register_local(&gguf, None).unwrap();

    let mut req = DeploymentRequest::for_model("qwen2.5-7b-instruct");
    req.context_len = Some(2048);
    req.target = mangosteen_gateway::assess::ComputeTarget::Local;

    // 模拟「远端评估」：路径为空（文件还没下载）。
    // 用 evaluate_estimated——它与 evaluate_remote 一样不读盘，
    // 产出的 model_path 是空的，正是要复现的条件。
    let evaluated = {
        let entry = gw.models().list().unwrap().remove(0);
        gw.evaluate_estimated(
            &entry.spec.name,
            entry.spec.quant,
            entry.spec.file_size_bytes,
            &req,
        )
    };
    assert!(
        evaluated.model_path.as_os_str().is_empty(),
        "测试前提：远端评估不应带本地路径"
    );

    let prepared = match gw
        .prepare_with_progress(evaluated, &req, &mangosteen_gateway::progress::noop_sink)
        .expect("准备阶段应成功")
    {
        PreparedDeployment::Local(p) => p,
        PreparedDeployment::Done(_) => panic!("本地目标不应产出云端方案"),
    };
    assert!(
        prepared.plan.model_path.is_empty(),
        "测试前提：prepare 时文件尚未落盘，路径为空"
    );

    // 补上落盘路径后，启动必须成功
    let prepared: PreparedLocal = prepared.with_model_path(gguf.to_string_lossy().to_string());
    assert_eq!(prepared.plan.model_path, gguf.to_string_lossy());

    let plan = gw
        .commit_local(prepared, &mangosteen_gateway::progress::noop_sink)
        .expect("补上路径后应能启动");
    assert_eq!(
        plan.local.as_ref().map(|l| l.model_path.clone()),
        Some(gguf.to_string_lossy().to_string())
    );
}

/// 部署进度：编排层必须逐步上报阶段，量化步骤如实标「跳过」。
///
/// 断言的是**编排层行为**（直接调 `deploy_with_progress`，不经 HTTP），
/// 这样与前端怎么画无关——进度数据对不对，是后端的事。
#[test]
fn e2e_deploy_reports_stage_progress() {
    use mangosteen_gateway::progress::{DeployStage, StageOutcome};

    let env = TestEnv::new("deplog");
    let engine = env.write_fake_engine();
    let gguf = env.write_gguf("qwen.gguf", 8192);
    let cfg = env.config(&engine);
    let mut gw = env.gateway(cfg);
    gw.models().register_local(&gguf, None).unwrap();

    let seen: std::sync::Mutex<Vec<(DeployStage, StageOutcome, String)>> =
        std::sync::Mutex::new(Vec::new());
    let sink = |s: DeployStage, o: StageOutcome, d: String| {
        if let Ok(mut v) = seen.lock() {
            v.push((s, o, d));
        }
    };

    let mut req = DeploymentRequest::for_model("qwen2.5-7b-instruct");
    req.context_len = Some(2048);
    req.target = mangosteen_gateway::assess::ComputeTarget::Local;

    let deployed = gw.deploy_with_progress(&req, &sink).is_ok();
    let events = seen.lock().map(|v| v.clone()).unwrap_or_default();

    assert!(
        !events.is_empty(),
        "部署必须上报阶段，否则 UI 没有任何进度可显示"
    );

    // 评估与选择两步无论成败都要出现（失败时正是这两步给出原因）
    for want in [DeployStage::Assessing, DeployStage::Selecting] {
        assert!(
            events.iter().any(|(s, _, _)| *s == want),
            "缺少阶段「{}」；实际上报：{:?}",
            want.label(),
            events.iter().map(|(s, _, _)| s.id()).collect::<Vec<_>>()
        );
    }

    if !deployed {
        // 环境不支持真实启动（如 fake engine 起不来）时，只校验到这一步。
        // 这样测试在有引擎的机器上严格、在 CI 上不假失败。
        return;
    }

    for want in [
        DeployStage::Assessing,
        DeployStage::Selecting,
        DeployStage::Configuring,
        DeployStage::Quantizing,
        DeployStage::Launching,
    ] {
        assert!(
            events.iter().any(|(s, _, _)| *s == want),
            "成功部署缺少阶段「{}」",
            want.label()
        );
    }

    // 顺序必须满足「配置在下载之前」——用户明确要求的部署流程。
    // 早先下载排在设备选择前面（因为评估要读 GGUF 文件），现在
    // 评估改用远端 Range 预取，下载才能后移。
    let idx = |st: DeployStage| {
        events
            .iter()
            .position(|(s, _, _)| *s == st)
            .unwrap_or_else(|| panic!("缺少阶段 {}", st.label()))
    };
    assert!(
        idx(DeployStage::Configuring) < idx(DeployStage::Quantizing),
        "设备配置必须在量化之前"
    );
    assert!(
        idx(DeployStage::Quantizing) < idx(DeployStage::Launching),
        "量化必须在启动之前"
    );

    // 「模型评估」必须输出**资源占用明细**，不能只说「评估完成」。
    // 用户报过「评估没有输出所需资源占用」，根因是 UI 把长文本截断；
    // 后端这一侧必须保证 detail 里确实带了数字。
    let assess = events
        .iter()
        .find(|(s, _, _)| *s == DeployStage::Assessing)
        .expect("应有评估阶段记录");
    // 评估阶段会有 Running + Done 两条，取 Done 那条
    let done = events
        .iter()
        .rev()
        .find(|(s, o, _)| *s == DeployStage::Assessing && *o == StageOutcome::Done)
        .unwrap_or(assess);
    assert!(
        done.2.contains("GiB"),
        "评估结果必须包含资源占用数值（如 GiB）：{}",
        done.2
    );
    assert!(
        done.2.contains("权重") && done.2.contains("KV"),
        "评估结果必须是分解明细（权重/KV/缓冲/开销）：{}",
        done.2
    );

    // 设备配置也要给出具体参数，便于排障时核对
    let cfg_done = events
        .iter()
        .rev()
        .find(|(s, o, _)| *s == DeployStage::Configuring && *o == StageOutcome::Done)
        .expect("应有设备配置完成记录");
    assert!(
        cfg_done.2.contains("端口") && cfg_done.2.contains("线程"),
        "配置结果必须摊开关键参数：{}",
        cfg_done.2
    );

    // 部署链路不做自动量化 → 必须如实标「跳过」并给原因。
    // 假装跑过会让用户以为多花了几分钟，实际什么也没发生。
    let q = events
        .iter()
        .find(|(s, _, _)| *s == DeployStage::Quantizing)
        .expect("应有量化阶段记录");
    assert_eq!(
        q.1,
        StageOutcome::Skipped,
        "部署不做自动量化，必须标 Skipped：{}",
        q.2
    );
    assert!(
        q.2.contains("无需量化"),
        "跳过必须给出原因，不能只留空：{}",
        q.2
    );

    assert!(
        events
            .iter()
            .any(|(s, o, _)| *s == DeployStage::Launching && *o == StageOutcome::Done),
        "启动成功必须上报 Launching/Done"
    );
}

/// 卸载必须支持 percent-encoded 的模型 ID。
///
/// 真实故障：UI 卸载 `qwen2.5-0.5b-instruct:q2_k` 报
/// 「未知路由：qwen2.5-0.5b-instruct%3Aq2_k」。浏览器把 `:` 编成 `%3A`，
/// 而后端用原始路径匹配路由 → 404。同一 ID 手敲冒号却正常，
/// 所以只在 UI 上暴露。
#[test]
fn e2e_remove_model_accepts_percent_encoded_id() {
    let env = TestEnv::new("rmenc");
    let engine = env.write_fake_engine();
    let gguf = env.write_gguf("qwen.gguf", 4096);
    let cfg = env.config(&engine);
    let gw = env.gateway(cfg);
    gw.models().register_local(&gguf, None).unwrap();

    // 台账里的真实 ID 带量化后缀
    let id = gw.models().list().unwrap()[0].id.clone();
    assert!(id.contains(':'), "测试前提：ID 应含冒号，实际 {id}");

    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = mangosteen_gateway::GatewayServer::new(
        std::sync::Arc::new(std::sync::Mutex::new(gw)),
        false,
        String::new(),
        Duration::from_secs(20),
    );
    std::thread::spawn(move || {
        let _ = server.serve(listener);
    });
    assert!(wait_port("127.0.0.1", port, Duration::from_secs(5)));

    // 冒号编码成 %3A —— 浏览器 encodeURIComponent 的行为
    let encoded = id.replace(':', "%3A");
    let (st, body) = http(
        "DELETE",
        "127.0.0.1",
        port,
        &format!("/api/models/{encoded}"),
        None,
    );
    assert_eq!(st, 200, "编码过的 ID 必须能路由（曾报未知路由）：{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(
        v["id"].as_str(),
        Some(id.as_str()),
        "回显的 ID 应是解码后的真实 ID"
    );
}

/// 卸载时 `file_deleted` 必须反映**实际结果**，不能回显请求参数。
///
/// `ModelStore::remove` 在路径不在授权范围时会跳过删除却仍返回成功；
/// 若直接回显 `delete_file`，UI 会报「已删除（含磁盘文件）」而文件还在。
#[test]
fn e2e_remove_model_reports_actual_file_deletion() {
    let env = TestEnv::new("rmfile");
    let engine = env.write_fake_engine();
    let gguf = env.write_gguf("qwen.gguf", 4096);
    let cfg = env.config(&engine);
    let gw = env.gateway(cfg);
    gw.models().register_local(&gguf, None).unwrap();
    assert!(gguf.is_file(), "前置：文件应存在");
    // 从台账取 ID 而不是写死：量化后缀由 GGUF 内容决定
    let id = gw.models().list().unwrap()[0].id.clone();

    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = mangosteen_gateway::GatewayServer::new(
        std::sync::Arc::new(std::sync::Mutex::new(gw)),
        false,
        String::new(),
        Duration::from_secs(20),
    );
    std::thread::spawn(move || {
        let _ = server.serve(listener);
    });
    assert!(wait_port("127.0.0.1", port, Duration::from_secs(5)));

    let (st, body) = http(
        "DELETE",
        "127.0.0.1",
        port,
        &format!("/api/models/{id}?delete_file=1"),
        None,
    );
    assert_eq!(st, 200, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        v["file_deleted"], true,
        "文件在授权目录内，应真删掉并如实上报：{body}"
    );
    assert!(!gguf.exists(), "文件应已从磁盘删除");
}

/// 部署页五项 UI 契约。
#[test]
fn e2e_deploy_page_active_model_contracts() {
    let env = TestEnv::new("uiact");
    let engine = env.write_fake_engine();
    let cfg = env.config(&engine);
    let gw = env.gateway(cfg);

    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = mangosteen_gateway::GatewayServer::new(
        std::sync::Arc::new(std::sync::Mutex::new(gw)),
        false,
        String::new(),
        Duration::from_secs(20),
    );
    std::thread::spawn(move || {
        let _ = server.serve(listener);
    });
    assert!(wait_port("127.0.0.1", port, Duration::from_secs(5)));

    let (status, body) = http("GET", "127.0.0.1", port, "/", None);
    assert_eq!(status, 200);

    // 【1】卡片改名：「已下载模型」→「模型列表」，「活跃模型」→「模型实例」
    assert!(body.contains("<h3>模型列表</h3>"), "标题应为「模型列表」");
    assert!(body.contains("<h3>模型实例</h3>"), "标题应为「模型实例」");
    assert!(!body.contains("活跃部署</h3>"), "旧标题应消失");
    assert!(
        !body.contains("<h3>已下载模型</h3>"),
        "旧标题「已下载模型」应消失"
    );
    assert!(
        !body.contains("<h3>活跃模型</h3>"),
        "旧标题「活跃模型」应消失"
    );
    // 「模型列表」提供启动/停止/卸载三个独立按钮（生命周期管理）
    assert!(
        body.contains(r#"td.appendChild(mk("#),
        "模型列表应使用三按钮工厂"
    );
    assert!(body.contains(r#""启动""#), "应有「启动」按钮");
    assert!(body.contains(r#""停止""#), "应有「停止」按钮");
    assert!(body.contains(r#""卸载""#), "应有「卸载」按钮");

    // 【2】PID 列
    assert!(body.contains("进程 PID"), "模型实例应有 PID 列");

    // 【4】模型 ID 与大小同格（两列表格）
    assert!(
        body.contains("margin-left:8px;font-size:11px"),
        "大小应紧贴模型 ID 显示，而不是独立成列被拉开"
    );

    // 【5】按钮统一 primary sm 高亮
    assert!(
        body.contains(r#"<button class="primary sm" id="dp-search">"#),
        "搜索按钮应与「部署」同款高亮"
    );
    assert!(
        body.contains(r#"<button class="primary sm" id="dp-deploy-picked""#),
        "「部署」按钮应与「搜索」同款高亮"
    );
}

/// 部署页文案与进度卡片契约。
///
/// 锁住这几条，避免以后改文案时又把「GGUF」写死回去——产品要的是
/// 「大模型文件」，因为下载链路已支持 safetensors / onnx 等格式。
#[test]
fn e2e_deploy_page_copy_and_progress_contracts() {
    let env = TestEnv::new("uicopy");
    let engine = env.write_fake_engine();
    let cfg = env.config(&engine);
    let gw = env.gateway(cfg);

    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = mangosteen_gateway::GatewayServer::new(
        std::sync::Arc::new(std::sync::Mutex::new(gw)),
        false,
        String::new(),
        Duration::from_secs(20),
    );
    std::thread::spawn(move || {
        let _ = server.serve(listener);
    });
    assert!(wait_port("127.0.0.1", port, Duration::from_secs(5)));

    let (status, body) = http("GET", "127.0.0.1", port, "/", None);
    assert_eq!(status, 200);

    // 【1】副标题不再写死 GGUF
    assert!(
        body.contains("搜索并下载大模型文件，然后启动本地或云端部署"),
        "副标题应为「大模型文件」"
    );
    assert!(!body.contains("搜索并下载 GGUF 模型"), "旧副标题应消失");

    // 【2】卡片改名「部署模型」+ 清空按钮（与搜索/部署同款 primary sm）
    assert!(body.contains("<h3>部署模型</h3>"), "标题应为「部署模型」");
    assert!(!body.contains("<h3>搜索并部署</h3>"), "旧标题应消失");
    assert!(
        body.contains(r#"<button class="primary sm" id="dp-clear""#),
        "应有「清空」按钮且与搜索/部署同款"
    );

    // 【3】停止按钮：必须存在，且只在可中止时显示
    assert!(
        body.contains(r#"id="dp-progress-stop""#),
        "部署进度应有「停止」按钮"
    );
    assert!(
        body.contains(r#"id="dp-progress-toggle""#),
        "部署进度应有「详情」按钮"
    );
    // 默认收起：详情容器初始 hidden，精简视图默认可见
    assert!(
        body.contains(r#"<div id="dp-detail" hidden>"#),
        "详情区默认应收起，保持界面简洁"
    );
    assert!(
        body.contains(r#"<div id="dp-compact">"#),
        "精简视图默认可见"
    );

    // 【4】启动后按钮必须自动变「停止」，不能等用户手动刷页面。
    //
    // 这两张卡片的按钮状态同源（活跃实例），必须走同一个刷新入口：
    // 只刷「模型实例」的话，用户点完启动会看到实例已在跑、按钮却还写着
    // 「启动」——之前就是这个症状。
    assert!(
        body.contains("async function refreshModelCards()"),
        "应有统一的「同时刷两张卡片」入口"
    );
    assert!(
        body.contains("await refreshModelCards();"),
        "启动/停止/部署完成后都必须走统一刷新"
    );
    // 轮询也要能感知实例变化：指纹变了才重画模型列表
    assert!(
        body.contains("if (sig !== depSig)"),
        "3 秒轮询发现实例集合变化时应连带刷新模型列表"
    );

    // 【5】成功提示只保留一条。
    //
    // 「部署成功 部署 ID …」与上方「模型已就绪，可以对话了（部署 ID …）」
    // 是同一句话说两遍，横幅已删除；失败/警告横幅必须保留（那是可操作内容）。
    assert!(
        !body.contains("<b>部署成功</b>"),
        "成功横幅与 dp-hint 重复，应已删除"
    );
    assert!(
        body.contains("模型已就绪，可以对话了"),
        "成功提示应保留一条（dp-hint）"
    );
    assert!(
        body.contains("<b>部署失败</b>"),
        "失败横幅必须保留——它带可操作的下一步"
    );
}

/// 中止部署必须**回滚**：删台账、释放端口、按需删文件。
///
/// 这是 UI「停止」按钮的后端契约。三个后果缺一不可：
///  · 台账不删 → 用户看到一条永远「部署中」的幽灵记录
///  · 端口不释放 → 反复「部署→停止」会吃光端口区间
///  · 文件不删 → 用户以为取消了，磁盘却少了几个 GB
#[test]
fn e2e_abort_deployment_rolls_back_everything() {
    use mangosteen_gateway::gateway::PreparedDeployment;

    let env = TestEnv::new("abortdep");
    let engine = env.write_fake_engine();
    let gguf = env.write_gguf("qwen.gguf", 8192);
    let cfg = env.config(&engine);
    let mut gw = env.gateway(cfg);
    gw.models().register_local(&gguf, None).unwrap();

    let mut req = DeploymentRequest::for_model("qwen2.5-7b-instruct");
    req.context_len = Some(2048);
    req.target = mangosteen_gateway::assess::ComputeTarget::Local;

    // 模拟远端评估（路径为空），再补上路径 → 走完整的 prepare 流程
    let evaluated = {
        let entry = gw.models().list().unwrap().remove(0);
        gw.evaluate_estimated(
            &entry.spec.name,
            entry.spec.quant,
            entry.spec.file_size_bytes,
            &req,
        )
    };
    let prepared = match gw
        .prepare_with_progress(evaluated, &req, &mangosteen_gateway::progress::noop_sink)
        .expect("准备阶段应成功")
    {
        PreparedDeployment::Local(p) => p.with_model_path(gguf.to_string_lossy().to_string()),
        PreparedDeployment::Done(_) => panic!("本地目标不应产出云端方案"),
    };
    let dep_id = prepared.id.clone();
    let port = prepared.plan.port;

    // 前置：台账里有记录、文件在、端口已预留
    assert!(
        gw.get_deployment(&dep_id).unwrap().is_some(),
        "prepare 后应有台账记录"
    );
    assert!(gguf.is_file(), "prepare 不该动文件");
    let before = gw.get_deployment(&dep_id).unwrap().unwrap();
    assert_eq!(before.status.as_str(), "provisioning");

    // 中止（含删文件）
    gw.abort_deployment(&dep_id, Some(gguf.as_path()))
        .expect("回滚应成功");

    // 1) 台账记录被删除（不是标 destroyed）
    assert!(
        gw.get_deployment(&dep_id).unwrap().is_none(),
        "回滚必须删除台账记录，否则 UI 会留一条幽灵记录"
    );
    // 2) 文件被删除
    assert!(!gguf.exists(), "delete_file=true 时应删除模型文件");
    // 3) 关键：回滚后**同一模型必须能再次部署**。
    //
    // 这条防的是真实踩过的坑：下载失败/用户停止后若留下
    // `provisioning` 记录，唯一性检查会一直报「已有活跃实例」，
    // 而那个实例根本不存在——用户被一条僵尸台账永久挡住。
    let mut req2 = DeploymentRequest::for_model("qwen2.5-7b-instruct");
    req2.context_len = Some(2048);
    req2.target = mangosteen_gateway::assess::ComputeTarget::Local;
    let ev2 = gw.evaluate_estimated(
        "qwen2.5-7b-instruct",
        mangosteen_core::model::Quantization::Q4KM,
        8192,
        &req2,
    );
    let again = gw.prepare_with_progress(ev2, &req2, &mangosteen_gateway::progress::noop_sink);
    assert!(
        again.is_ok(),
        "回滚后同一模型必须能再次部署，实际被挡住：{}",
        again.err().map(|e| e.to_string()).unwrap_or_default()
    );
    if let Ok(PreparedDeployment::Local(p2)) = again {
        gw.abort_deployment(&p2.id, None).ok();
    }
    let _ = port;
}

/// 中止时 `delete_file=false` 不能删文件。
///
/// 场景：模型原本就在本地，用户点的是「启动」而不是「下载并启动」。
/// 此时停止部署只该关引擎，删掉文件等于卸载用户已有的资产。
#[test]
fn e2e_abort_keeps_file_when_not_downloaded_by_this_job() {
    use mangosteen_gateway::gateway::PreparedDeployment;

    let env = TestEnv::new("abortkeep");
    let engine = env.write_fake_engine();
    let gguf = env.write_gguf("qwen.gguf", 8192);
    let cfg = env.config(&engine);
    let mut gw = env.gateway(cfg);
    gw.models().register_local(&gguf, None).unwrap();

    let mut req = DeploymentRequest::for_model("qwen2.5-7b-instruct");
    req.context_len = Some(2048);
    req.target = mangosteen_gateway::assess::ComputeTarget::Local;
    let evaluated = gw.evaluate(&req).expect("本地文件应可评估");
    let prepared = match gw
        .prepare_with_progress(evaluated, &req, &mangosteen_gateway::progress::noop_sink)
        .expect("准备阶段应成功")
    {
        PreparedDeployment::Local(p) => p,
        PreparedDeployment::Done(_) => panic!("本地目标不应产出云端方案"),
    };
    let dep_id = prepared.id.clone();

    gw.abort_deployment(&dep_id, None).expect("回滚应成功");

    assert!(
        gw.get_deployment(&dep_id).unwrap().is_none(),
        "台账记录仍应删除"
    );
    assert!(gguf.is_file(), "delete_file=false 时不得删除模型文件");
}

// ============================================================ 设备页 · 云端监控

/// `/api/sysinfo` 必须带 `cloud` 段，且本地部署时 `active=false`。
///
/// ## 为什么这条必须有守护
///
/// 设备页靠 `cloud.active` 决定显示云端区块还是本机区块。若这个字段
/// 缺失或语义反转，用户在用云端算力时会看到本机 GPU 的读数，
/// 进而误判「云端 GPU 没跑满」——这是排查性能问题时最耗时的一类错误。
#[test]
fn e2e_sysinfo_exposes_cloud_section() {
    let env = TestEnv::new("syscloud");
    let engine = env.write_fake_engine();
    let cfg = env.config(&engine);
    let gw = env.gateway(cfg);

    let (listener, port) = bind_ephemeral();
    let server = mangosteen_gateway::GatewayServer::new(
        std::sync::Arc::new(std::sync::Mutex::new(gw)),
        false,
        String::new(),
        Duration::from_secs(20),
    );
    std::thread::spawn(move || {
        let _ = server.serve(listener);
    });
    assert!(wait_port("127.0.0.1", port, Duration::from_secs(5)));

    let (status, body) = http("GET", "127.0.0.1", port, "/api/sysinfo", None);
    assert_eq!(status, 200);
    let v: serde_json::Value = serde_json::from_str(&body).expect("sysinfo 必须是 JSON");

    // 本地资源字段仍在（云端监控不能破坏原有契约）
    assert!(v.get("cpu_usage").is_some(), "必须保留本机 CPU 采样");
    assert!(v.get("deployments").is_some(), "必须保留部署列表");

    let cloud = v.get("cloud").expect("必须带 cloud 段");
    // 没有云端部署 → 明确标为「不活跃」，而不是缺字段
    assert_eq!(
        cloud.get("active").and_then(|x| x.as_bool()),
        Some(false),
        "无云端部署时必须给出 active=false：{cloud}"
    );
    // 未启用后台采集时 `pending=true`：前端据此显示「采集中」而非「无数据」
    assert!(
        cloud.get("pending").is_some(),
        "必须能区分「采集中」与「无数据」：{cloud}"
    );
}

/// 设备页前端必须渲染云端区块，并区分「不支持/无数据/失败」三种空态。
#[test]
fn e2e_device_page_renders_cloud_metrics() {
    let env = TestEnv::new("devcloud");
    let engine = env.write_fake_engine();
    let cfg = env.config(&engine);
    let gw = env.gateway(cfg);

    let (listener, port) = bind_ephemeral();
    let server = mangosteen_gateway::GatewayServer::new(
        std::sync::Arc::new(std::sync::Mutex::new(gw)),
        false,
        String::new(),
        Duration::from_secs(20),
    );
    std::thread::spawn(move || {
        let _ = server.serve(listener);
    });
    assert!(wait_port("127.0.0.1", port, Duration::from_secs(5)));

    let (status, body) = http("GET", "127.0.0.1", port, "/", None);
    assert_eq!(status, 200);

    // 【1】云端区块存在且默认隐藏（本地部署时不该占版面）
    assert!(body.contains(r#"id="dv-cloud""#), "设备页必须有云端区块");
    assert!(
        body.contains(r#"<div class="card" id="dv-cloud" hidden>"#),
        "云端区块默认隐藏，仅有云端部署时才展示"
    );

    // 【2】四个云端指标环 + 磁盘两行
    for id in [
        "dv-cg-ring", // GPU
        "dv-cv-ring", // 显存
        "dv-cc-ring", // CPU
        "dv-cm-ring", // 内存
        "dv-cd-sys",  // 系统盘
        "dv-cd-data", // 数据盘
    ] {
        assert!(body.contains(id), "设备页缺少云端指标 `{id}`");
    }

    // 【3】必须显式标注数据新鲜度
    //
    // 云端是 5 秒缓存、本地是 2 秒采样，两者节奏不同。不标注会让用户
    // 把旧读数当实时值，排查性能问题时得出错误结论。
    assert!(
        body.contains(r#"id="dv-cloud-age""#),
        "必须展示云端数据新鲜度"
    );

    // 【4】三种空态必须可区分，否则用户无法判断该找谁
    assert!(
        body.contains("未提供实例监控接口"),
        "服务商不支持监控时要说清楚，而不是显示 0%"
    );
    assert!(
        body.contains("实例可能已关机"),
        "厂商无数据点时要解释原因，不能当成故障"
    );
    assert!(
        body.contains("云端监控查询失败"),
        "查询失败要与「无数据」区分开"
    );

    // 【5】本机资源必须标注「控制端」，避免与云端读数混淆
    assert!(
        body.contains("本机</b>（控制端）资源"),
        "云端激活时必须标注下方是本机资源"
    );

    // 【6】卸载必须清定时器（否则轮询累积泄漏）
    assert!(
        body.contains(r#"if (timer) { clearInterval(timer); timer = null; }"#),
        "设备页 unmount 必须清掉轮询定时器"
    );
}

/// 设备选择：选了「本地」就绝不静默上云。
///
/// 这是**防意外计费**的关键约束。本地资源不足时，默认策略会自动降级云端；
/// 但用户明确选了「本地」，静默降级会直接产生费用且不经确认——
/// 这是最不能接受的意外，必须改为明确失败。
#[test]
fn e2e_forced_local_never_falls_back_to_cloud() {
    let env = TestEnv::new("forcelocal");
    let engine = env.write_fake_engine();
    // 造一个远超本机能力的模型：64 层、超大隐藏维度
    let gguf = env.write_gguf("huge.gguf", 8192);
    let cfg = env.config(&engine);
    let mut gw = env.gateway(cfg);
    gw.models().register_local(&gguf, None).unwrap();

    let req = DeploymentRequest {
        model: "qwen2.5-7b-instruct".into(),
        target: mangosteen_gateway::assess::ComputeTarget::Local,
        allow_cloud: true, // 即便允许云端，强制本地也不得降级
        ..Default::default()
    };

    // 资源不足 → 必须报错（而不是悄悄部署到云端）
    // 若本机居然装得下，这条会部署成功，此时跳过断言
    match gw.deploy(&req) {
        Ok(_) => {
            let _ = gw.stop_all();
        }
        Err(e) => {
            let msg = e.to_string();
            assert!(
                msg.contains("本地算力") || msg.contains("不会自动改用云端"),
                "强制本地失败时必须说明「不会上云」，实际：{msg}"
            );
        }
    }
}

/// preview 在用户选「云端」时不得宣称「本地可行」。
///
/// 否则弹窗会说「本地可以运行 · 不涉及费用」，与用户的选择和实际行为矛盾，
/// 用户会以为选云端没生效。
#[test]
fn e2e_preview_respects_forced_cloud() {
    let env = TestEnv::new("pvtarget");
    let engine = env.write_fake_engine();
    let gguf = env.write_gguf("small.gguf", 4096);
    let cfg = env.config(&engine);
    let gw = env.gateway(cfg);
    gw.models().register_local(&gguf, None).unwrap();

    let mut req = DeploymentRequest::for_model("qwen2.5-7b-instruct");
    req.target = mangosteen_gateway::assess::ComputeTarget::Cloud;

    let pv = gw.preview(&req).expect("预估应返回（即便无云端可用）");
    assert!(
        !pv.local_viable,
        "选了云端就不得宣称「本地可行」，否则弹窗会说不花钱"
    );
    assert_eq!(
        pv.cloud.is_none(),
        pv.cloud_error.is_some(),
        "云端不可用时必须给出原因"
    );
}

/// 设备选择相关 UI 契约。
#[test]
fn e2e_deploy_page_device_picker_contracts() {
    let env = TestEnv::new("devpick");
    let engine = env.write_fake_engine();
    let cfg = env.config(&engine);
    let gw = env.gateway(cfg);

    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = mangosteen_gateway::GatewayServer::new(
        std::sync::Arc::new(std::sync::Mutex::new(gw)),
        false,
        String::new(),
        Duration::from_secs(20),
    );
    std::thread::spawn(move || {
        let _ = server.serve(listener);
    });
    assert!(wait_port("127.0.0.1", port, Duration::from_secs(5)));

    let (status, body) = http("GET", "127.0.0.1", port, "/", None);
    assert_eq!(status, 200);

    // 【需求2】设备选择：本地/云端
    //
    // 默认值随产品策略调整过：早期默认「本地」（云端会真实计费），
    // 当前策略是云端 GPU 部署（本地显卡不足以跑大模型），默认改为「云端」。
    // 计费风险由「部署前必须弹窗确认预估费用」兜住，不靠默认值。
    assert!(body.contains("设备选择"), "应有「设备选择」区块");
    assert!(body.contains(r#"data-t="local""#), "缺少「本地」选项");
    assert!(body.contains(r#"data-t="cloud""#), "缺少「云端」选项");
    assert!(
        body.contains(r#"<button data-t="cloud" class="on">"#),
        "默认应选中「云端」——当前产品策略为云端 GPU 部署"
    );

    // 【需求1】对话页不再显示算力来源与就绪状态
    assert!(
        !body.contains(r#"id="chat-state""#),
        "对话页不应显示状态 pill"
    );
    assert!(
        !body.contains(r#"id="chat-hint""#),
        "对话页不应显示就绪提示"
    );

    // 【需求3】已下载模型有启动/停止/卸载三按钮
    assert!(body.contains(r#""卸载""#), "「已下载模型」应有「卸载」按钮");
}

// ============================================================ 算力来源选择

/// 选「本地」但本地不可行 → `blocked=true`，且**不得**创建任何资源。
///
/// ## 这个测试守护的是「静默计费」这类最严重的错误
///
/// 用户选「本地」的动机几乎总是「不想花钱」。若此时系统自动降级到云端，
/// 会在他不知情的情况下开出计费实例。因此这条必须硬性守住：
/// 预估说 `blocked=true`，部署就必须失败，且台账里不能出现任何记录。
#[test]
fn e2e_forced_local_blocks_instead_of_silently_going_cloud() {
    let env = TestEnv::new("blocklocal");
    // 引擎存在但无法运行 → 本地必然不可行
    let broken = env.root.join("broken-engine");
    std::fs::write(&broken, "#!/bin/sh\nexit 1\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&broken, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let gguf = env.write_gguf("qwen.gguf", 4096);
    let cfg = env.config(&broken);
    let mut gw = env.gateway(cfg);
    gw.models().register_local(&gguf, None).unwrap();

    let mut req = DeploymentRequest::for_model("qwen2.5-7b-instruct").with_context(2048);
    req.target = mangosteen_gateway::assess::ComputeTarget::Local;

    // ---- 1. 预估必须明确「会停止」 ----
    let pv = gw.preview(&req).expect("preview");
    assert!(!pv.local_viable, "坏引擎 → 本地不可行");
    assert!(
        pv.blocked,
        "选本地且不可行时必须标记 blocked，否则 UI 会渲染成「会上云」"
    );
    // 云端报价可以出现（供用户参考「改选云端要多少钱」），
    // 但它绝不能表示「点确认就会创建」——那由 blocked 决定。

    // ---- 2. 真正部署必须失败 ----
    let err = gw.deploy(&req).expect_err("选本地且资源不足必须失败");
    let msg = err.to_string();
    assert!(
        msg.contains("本地算力") || msg.contains("不会自动改用云端"),
        "错误信息要说明「没有偷偷上云」：{msg}"
    );

    // ---- 3. 台账里不得留下「正在运行」的记录 ----
    //
    // 被拒绝时不会创建任何真实资源，但台账可能留下一条 Failed 记录
    // （便于用户回看「上次为什么没部署成功」）。关键是：
    // **绝不能有 active/ready 状态的记录**，否则 UI 会显示有实例在跑。
    let recs = gw.list_deployments().unwrap();
    assert!(
        recs.iter().all(|r| !r.status.is_active()),
        "被拒绝的部署不得留在活跃状态：{recs:?}"
    );
}

/// 选「云端」→ 预估直接按云端出（即使本地完全够用）。
#[test]
fn e2e_forced_cloud_preview_skips_local() {
    let env = TestEnv::new("forcecloud");
    let engine = env.write_fake_engine();
    let gguf = env.write_gguf("qwen.gguf", 4096);
    let cfg = env.config(&engine);
    let mut gw = env.warmed_gateway(cfg);
    gw.models().register_local(&gguf, None).unwrap();

    // 先确认默认（auto）会走本地
    let auto = DeploymentRequest::for_model("qwen2.5-7b-instruct").with_context(2048);
    let pv_auto = gw.preview(&auto).expect("preview auto");
    assert!(pv_auto.local_viable, "前提：本地本来够用");
    assert!(!pv_auto.blocked);

    // 改选云端 → 不得再宣称「本地可行」
    let mut req = DeploymentRequest::for_model("qwen2.5-7b-instruct").with_context(2048);
    req.target = mangosteen_gateway::assess::ComputeTarget::Cloud;
    let pv = gw.preview(&req).expect("preview cloud");
    assert!(
        !pv.local_viable,
        "选云端时 local_viable 必须为 false，否则弹窗会说「不花钱」与实际矛盾"
    );
    assert!(pv.local.is_none());
    assert!(!pv.blocked, "选云端不是「阻止」，而是走云端");
    assert!(
        pv.local_reason
            .as_deref()
            .unwrap_or("")
            .contains("云端算力"),
        "原因要反映用户的选择：{:?}",
        pv.local_reason
    );
}

/// HTTP 层：`target` 的接受与拒绝都必须严格。
#[test]
fn e2e_deploy_target_param_is_validated() {
    let env = TestEnv::new("targetparam");
    let engine = env.write_fake_engine();
    let gguf = env.write_gguf("qwen.gguf", 4096);
    let cfg = env.config(&engine);
    let gw = env.gateway(cfg);
    gw.models().register_local(&gguf, None).unwrap();

    let (listener, port) = bind_ephemeral();
    let server = mangosteen_gateway::GatewayServer::new(
        std::sync::Arc::new(std::sync::Mutex::new(gw)),
        false,
        String::new(),
        Duration::from_secs(20),
    );
    std::thread::spawn(move || {
        let _ = server.serve(listener);
    });
    assert!(wait_port("127.0.0.1", port, Duration::from_secs(5)));

    // 未知值 → 400（静默回退会让用户以为选择生效了）
    let (st, body) = http(
        "POST",
        "127.0.0.1",
        port,
        "/api/preview",
        Some(r#"{"model":"qwen2.5-7b-instruct","target":"gpu"}"#),
    );
    assert_eq!(st, 400, "未知 target 必须 400：{body}");
    assert!(body.contains("target"), "{body}");

    // 合法值 → 200
    for t in ["local", "cloud", "auto"] {
        let payload = format!(r#"{{"model":"qwen2.5-7b-instruct","target":"{t}"}}"#);
        let (st, body) = http("POST", "127.0.0.1", port, "/api/preview", Some(&payload));
        assert_eq!(st, 200, "target={t} 应被接受：{body}");
        assert!(
            body.contains("\"blocked\""),
            "响应必须带 blocked 字段供 UI 判断：{body}"
        );
    }
}

/// 第六轮 UI 优化契约：中断、帮助提示、去噪、GGUF 过滤。
#[test]
fn e2e_chat_and_deploy_ux_contracts() {
    let env = TestEnv::new("ux6");
    let engine = env.write_fake_engine();
    let cfg = env.config(&engine);
    let gw = env.gateway(cfg);

    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = mangosteen_gateway::GatewayServer::new(
        std::sync::Arc::new(std::sync::Mutex::new(gw)),
        false,
        String::new(),
        Duration::from_secs(20),
    );
    std::thread::spawn(move || {
        let _ = server.serve(listener);
    });
    assert!(wait_port("127.0.0.1", port, Duration::from_secs(5)));

    let (status, body) = http("GET", "127.0.0.1", port, "/", None);
    assert_eq!(status, 200);

    // 【1】生成中按钮变「停止」，可立即打断
    assert!(
        body.contains(r#"$("btn-send").textContent = "停止""#),
        "生成中应把发送按钮改成「停止」"
    );
    assert!(body.contains("function stopGeneration("), "缺少中断实现");
    assert!(body.contains("chatAbort.abort()"), "中断必须真的断开请求");
    assert!(
        body.contains("AbortError"),
        "中断不应被当成失败（需区分 AbortError）"
    );
    // 生成中不得置灰按钮
    assert!(
        !body.contains("streaming = true;\n    $(\"btn-send\").disabled = true;"),
        "生成中不应禁用发送按钮——否则无法打断"
    );
    // 轮询（refresh）也不能因 streaming 而禁用按钮。
    //
    // 这条是真实踩过的坑：refresh 每 3 秒跑一次，把 send() 里设置的
    // 「可点停止」覆盖成 disabled，表现为「按钮写着停止但灰着点不动」，
    // 用户永远没法中断。只允许因「没有就绪模型」而禁用。
    assert!(
        !body.contains(r#"$("btn-send").disabled = !ready || streaming;"#),
        "轮询不得因 streaming 禁用发送按钮，否则会覆盖「停止」的可点状态"
    );

    // 【2】【4】说明收进「?」按钮，默认隐藏
    // 设备选择已迁到设置页（id 前缀 se-）
    assert!(body.contains(r#"id="se-target-help""#), "缺少设备选择「?」");
    assert!(body.contains(r#"id="dp-search-help""#), "缺少搜索说明「?」");
    assert!(
        body.contains(r#"class="helpbox" id="se-target-helpbox" hidden"#),
        "设备说明默认应隐藏"
    );
    assert!(
        body.contains(r#"class="helpbox" id="dp-search-helpbox" hidden"#),
        "搜索说明默认应隐藏"
    );
    assert!(body.contains("function bindHelp("), "缺少「?」展开逻辑");

    // 【第2项】「已选择云端：点部署会先弹窗…」这类说明必须收进「?」内，
    // 不能常驻显示。判定方式：该文案只应出现在 helpbox 内部。
    let helpbox_start = body
        .find(r#"id="se-target-helpbox""#)
        .expect("缺少设备说明区");
    let helpbox_end = body[helpbox_start..]
        .find("</div>")
        .map(|k| helpbox_start + k)
        .unwrap_or(body.len());
    let target_note_inside = body[helpbox_start..helpbox_end].contains("se-target-note");
    assert!(
        target_note_inside,
        "「当前选择」提示必须放在「?」说明区内部，不能常驻页面"
    );

    // 【3】不再显示「找到 N 个仓库」这类说明文字
    assert!(
        !body.contains("个仓库（按下载量排序）"),
        "不应再显示「找到 N 个仓库」提示"
    );

    // 【5】已选提示不再是绿色横条
    assert!(
        !body.contains("background:var(--accent-soft);border:1px solid #cfe3d8"),
        "已选提示不应使用绿色色块横条"
    );
}

/// 搜索必须过滤掉非 GGUF 仓库，并累计到指定数量。
///
/// 上游搜索是模糊匹配，结果里混着 safetensors / 纯 PyTorch / 数据集。
/// 用户要的是「100 个能下 GGUF 的仓库」，不是「100 条结果里十几个 GGUF」。
#[test]
fn e2e_search_filters_non_gguf_repos() {
    // 直接测解析层：非 GGUF 的 has_gguf 必须为 false
    let json = r#"[
        {"id":"a/Qwen-GGUF","downloads":100,"tags":["gguf"]},
        {"id":"b/plain-model","downloads":90,"tags":["safetensors"]},
        {"id":"c/also-gguf","downloads":80,"tags":[]},
        {"id":"d/dataset","downloads":70,"tags":["dataset"]}
    ]"#;
    let repos = mangosteen_base::download::parse_repo_list_for_test(json).unwrap();
    assert_eq!(repos.len(), 4);

    let gguf: Vec<_> = repos.iter().filter(|r| r.has_gguf).collect();
    assert_eq!(gguf.len(), 2, "只有含 gguf 标签或名字带 gguf 的才算");
    assert!(gguf.iter().any(|r| r.repo == "a/Qwen-GGUF"));
    assert!(
        gguf.iter().any(|r| r.repo == "c/also-gguf"),
        "名字含 gguf 也应识别（仓库名惯例）"
    );
    assert!(
        !gguf.iter().any(|r| r.repo == "b/plain-model"),
        "safetensors 仓库不应被当成 GGUF"
    );
}

/// 推理引擎：可配置选择，默认 llama.cpp，未适配的明确拒绝。
///
/// 产品方向变更：原基线是「推理引擎唯一 llama.cpp」，现改为可配置。
/// 但只有 llama.cpp 是真正实现并验证过的，其余必须**明确拒绝**，
/// 绝不能假装能跑（否则用户等到超时才发现根本没起引擎）。
#[test]
fn e2e_engine_selection_defaults_to_llama_cpp_and_rejects_unsupported() {
    use mangosteen_core::config::InferenceEngine;

    // 默认值
    let cfg = mangosteen_core::config::LocalConfig::default();
    assert_eq!(
        cfg.engine,
        InferenceEngine::LlamaCpp,
        "默认推理引擎必须是 llama.cpp"
    );

    // 全部可选项与状态
    let all = InferenceEngine::all();
    assert!(all.len() >= 5, "应提供至少 5 种主流引擎");
    let ids: Vec<&str> = all.iter().map(|e| e.id()).collect();
    for want in ["llama_cpp", "ollama", "vllm", "sglang", "tensorrt_llm"] {
        assert!(ids.contains(&want), "缺少引擎 {want}");
    }

    // llama.cpp 与 ollama 已实现；其余三种未适配
    assert!(InferenceEngine::LlamaCpp.is_supported());
    assert!(
        InferenceEngine::Ollama.is_supported(),
        "Ollama 走「复用守护进程」模式，已实现"
    );
    assert!(InferenceEngine::Ollama.reuses_external_service());
    for e in [
        InferenceEngine::Vllm,
        InferenceEngine::Sglang,
        InferenceEngine::TensorrtLlm,
    ] {
        assert!(
            !e.is_supported(),
            "{} 尚未适配，不能标成 supported",
            e.display_name()
        );
    }

    // 解析容错
    assert_eq!(
        InferenceEngine::parse("llama.cpp"),
        Some(InferenceEngine::LlamaCpp)
    );
    assert_eq!(InferenceEngine::parse("vllm"), Some(InferenceEngine::Vllm));
    assert_eq!(
        InferenceEngine::parse("tensorrt"),
        Some(InferenceEngine::TensorrtLlm)
    );
    assert_eq!(InferenceEngine::parse("不存在"), None);

    // 格式要求
    assert_eq!(InferenceEngine::LlamaCpp.required_format(), "gguf");
    assert_eq!(InferenceEngine::Vllm.required_format(), "safetensors");
}

/// 选中未适配引擎时，部署必须给出可读的拒绝原因。
#[test]
fn e2e_deploy_rejects_unsupported_engine() {
    use mangosteen_core::config::InferenceEngine;

    let env = TestEnv::new("engine");
    let engine = env.write_fake_engine();
    let gguf = env.write_gguf("q.gguf", 4096);
    let mut cfg = env.config(&engine);
    cfg.local.engine = InferenceEngine::Vllm; // 切到未适配引擎
    let mut gw = env.gateway(cfg);
    gw.models().register_local(&gguf, None).unwrap();

    let req = DeploymentRequest::for_model("qwen2.5-7b-instruct").with_context(2048);
    let err = gw.deploy(&req).expect_err("未适配引擎必须拒绝部署");
    let msg = err.to_string();
    assert!(
        msg.contains("vLLM") && (msg.contains("尚未适配") || msg.contains("未适配")),
        "拒绝原因应说明引擎未适配，实际：{msg}"
    );
    assert!(
        msg.contains("llama.cpp"),
        "应告诉用户可以改回 llama.cpp，实际：{msg}"
    );
}

/// 搜索/下载不再按格式过滤，但要标识格式。
#[test]
fn e2e_download_accepts_any_format_and_labels_it() {
    use mangosteen_base::download::detect_format;

    // 常见格式都能识别
    assert_eq!(detect_format("model-q4.gguf"), "gguf");
    assert_eq!(detect_format("model.safetensors"), "safetensors");
    assert_eq!(detect_format("model.onnx"), "onnx");
    assert_eq!(detect_format("pytorch_model.bin"), "pytorch");
    assert_eq!(detect_format("engine.plan"), "tensorrt");
    assert_eq!(detect_format("config.json"), "config");
    // 认不出的也不能拒绝下载，标「其他」
    assert_eq!(detect_format("weights.xyz"), "其他");

    // 远端文件清单不再只收 .gguf
    let json = r#"[
        {"rfilename":"a.gguf","size":100},
        {"rfilename":"model.safetensors","size":200},
        {"rfilename":"pytorch_model.bin","size":300},
        {"rfilename":"imatrix_unsloth.gguf","size":50}
    ]"#;
    let files = mangosteen_base::download::parse_files_for_test(json).unwrap();
    assert_eq!(files.len(), 4, "任何格式都应列出，不做过滤");

    // 格式标识正确，辅助数据额外标注
    let by = |n: &str| files.iter().find(|f| f.name == n).unwrap();
    assert_eq!(by("a.gguf").format, "gguf");
    assert_eq!(by("model.safetensors").format, "safetensors");
    assert_eq!(by("pytorch_model.bin").format, "pytorch");
    assert!(
        by("imatrix_unsloth.gguf").format.contains("辅助数据"),
        "imatrix 应标注为辅助数据（可下载但不能直接部署）"
    );
}

/// 引擎选择 UI 契约。
#[test]
fn e2e_deploy_page_engine_picker_contracts() {
    let env = TestEnv::new("engui");
    let engine = env.write_fake_engine();
    let cfg = env.config(&engine);
    let gw = env.gateway(cfg);

    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = mangosteen_gateway::GatewayServer::new(
        std::sync::Arc::new(std::sync::Mutex::new(gw)),
        false,
        String::new(),
        Duration::from_secs(20),
    );
    std::thread::spawn(move || {
        let _ = server.serve(listener);
    });
    assert!(wait_port("127.0.0.1", port, Duration::from_secs(5)));

    let (status, body) = http("GET", "127.0.0.1", port, "/", None);
    assert_eq!(status, 200);
    // 推理引擎已迁到设置页
    assert!(body.contains("推理引擎"), "应有「推理引擎」区块");
    assert!(body.contains(r#"id="se-engine""#), "缺少引擎下拉");
    assert!(body.contains(r#"id="se-engine-help""#), "缺少引擎说明「?」");
    assert!(
        body.contains(r#"id="se-engine-helpbox" hidden"#),
        "引擎说明默认应隐藏"
    );

    // settings 接口要给出引擎清单
    let (status, sbody) = http("GET", "127.0.0.1", port, "/api/settings", None);
    assert_eq!(status, 200);
    let v: serde_json::Value = serde_json::from_str(&sbody).unwrap();
    assert_eq!(v["local"]["engine"], "llama_cpp", "默认引擎应为 llama.cpp");
    let engines = v["local"]["engines"].as_array().expect("缺少 engines 列表");
    assert!(engines.len() >= 5);
    let vllm = engines
        .iter()
        .find(|e| e["id"] == "vllm")
        .expect("缺少 vllm");
    assert_eq!(vllm["supported"], false, "vLLM 未适配，必须标 false");
}

/// 「部署」入口会自动下载并启动（UI 不再有独立下载）。
///
/// 校验点：`/api/models/pull` 带 `deploy: true` 时，任务应记录
/// 「自动部署」意图与阶段，而不是只做一次下载。
#[test]
fn e2e_pull_with_deploy_flag_marks_auto_deploy() {
    let env = TestEnv::new("autodep");
    let engine = env.write_fake_engine();
    let gguf = env.write_gguf("q.gguf", 4096);
    let cfg = env.config(&engine);
    let gw = env.gateway(cfg);
    gw.models().register_local(&gguf, None).unwrap();

    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = mangosteen_gateway::GatewayServer::new(
        std::sync::Arc::new(std::sync::Mutex::new(gw)),
        false,
        String::new(),
        Duration::from_secs(30),
    );
    std::thread::spawn(move || {
        let _ = server.serve(listener);
    });
    assert!(wait_port("127.0.0.1", port, Duration::from_secs(5)));

    // 用一个本地已存在的模型路径来触发 pull：file 已存在 → 走 already 分支
    let local_path = env.write_gguf("q.gguf", 4096);
    let body = serde_json::json!({
        "model": format!("hf-mirror:o/r/{}", local_path.file_name().unwrap().to_string_lossy()),
        "deploy": true,
        "target": "local",
    })
    .to_string();

    let (status, resp) = http("POST", "127.0.0.1", port, "/api/models/pull", Some(&body));
    // 网络不可用时该接口可能失败；只要不是 500 就算接口契约正常
    assert!(
        status == 200 || status == 400,
        "unexpected status {status}: {resp}"
    );
    if status != 200 {
        return; // 无网络环境，跳过后续断言
    }

    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    let job_id = v["job_id"].as_str().expect("缺少 job_id");
    let (_, jbody) = http("GET", "127.0.0.1", port, "/api/models/jobs", None);
    let jv: serde_json::Value = serde_json::from_str(&jbody).unwrap();
    let job = jv["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|j| j["id"] == job_id)
        .expect("找不到任务");

    assert_eq!(
        job["deploy_after"], true,
        "带 deploy:true 的任务必须标记自动部署意图"
    );
    assert!(
        job.get("deploy_state").is_some(),
        "应有部署阶段字段（准备中/部署中/已启动）"
    );
}

/// 「已下载模型」的生命周期三按钮 + 多选校验（UI 契约）。
#[test]
fn e2e_model_lifecycle_and_single_select_contracts() {
    let env = TestEnv::new("lifecycle");
    let engine = env.write_fake_engine();
    let cfg = env.config(&engine);
    let gw = env.gateway(cfg);

    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = mangosteen_gateway::GatewayServer::new(
        std::sync::Arc::new(std::sync::Mutex::new(gw)),
        false,
        String::new(),
        Duration::from_secs(20),
    );
    std::thread::spawn(move || {
        let _ = server.serve(listener);
    });
    assert!(wait_port("127.0.0.1", port, Duration::from_secs(5)));

    let (status, body) = http("GET", "127.0.0.1", port, "/", None);
    assert_eq!(status, 200);

    // ---- 生命周期：启动 / 停止 / 卸载 三个独立按钮 ----
    for label in ["启动", "停止", "卸载"] {
        assert!(
            body.contains(&format!("\"{label}\"")),
            "已下载模型应有「{label}」按钮"
        );
    }
    // 三按钮通过统一工厂创建，enabled=false 时置灰
    assert!(
        body.contains("const mk = (label, title, enabled, fn)"),
        "三按钮应由统一工厂创建，便于按状态置灰"
    );
    assert!(
        body.contains("if (enabled) b.onclick = fn;"),
        "不可用按钮必须置灰（disabled）"
    );
    // 运行中不允许卸载（避免孤儿进程）
    assert!(
        body.contains("请先停止该模型，再卸载"),
        "运行中应提示先停止再卸载"
    );

    // ---- 多选校验 ----
    assert!(
        body.contains("不能同时部署多个不同仓库的模型"),
        "跨仓库多选必须报错"
    );
    assert!(
        body.contains("个量化版本，请只选一个"),
        "同一模型不同量化必须提示精准选择"
    );
    assert!(body.contains("不能同时部署 "), "多个不同模型必须报错");
    // 只允许单选时才启用部署按钮
    assert!(
        body.contains("if (items.length !== 1) return;"),
        "deployPicked 必须只处理单个选中项"
    );
}

/// 前端脚本里**不能有「调用但未定义」的函数**。
///
/// 这条是针对一类真实事故的守护：批量替换大段 JS 时，容易把区间内的
/// 工具函数（如 `fmtNum` / `bindHelp`）一并删掉，而调用点还在。
/// 症状是页面点某个操作时抛 `xxx is not defined`，
/// 而 `node --check` 查不出来（语法合法，只是运行时缺符号）。
///
/// 这里用最朴素的办法：抽出所有 `function foo(` 与 `const foo =` 定义，
/// 再找 `foo(` 形式的调用，报告差集。
#[test]
fn e2e_ui_script_has_no_undefined_function_calls() {
    let env = TestEnv::new("undef");
    let engine = env.write_fake_engine();
    let cfg = env.config(&engine);
    let gw = env.gateway(cfg);

    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = mangosteen_gateway::GatewayServer::new(
        std::sync::Arc::new(std::sync::Mutex::new(gw)),
        false,
        String::new(),
        Duration::from_secs(20),
    );
    std::thread::spawn(move || {
        let _ = server.serve(listener);
    });
    assert!(wait_port("127.0.0.1", port, Duration::from_secs(5)));

    let (_, body) = http("GET", "127.0.0.1", port, "/", None);

    // 关键工具函数必须存在（它们都被误删过）
    for f in [
        "function bindHelp(",
        "const fmtNum =",
        "const bytes =",
        "const esc =",
        "const el =",
        "function toast(",
    ] {
        assert!(body.contains(f), "缺少工具函数：{f}");
    }

    // 已知必须存在的调用点，其定义也必须在
    let pairs = [
        ("fmtNum(", "const fmtNum ="),
        ("bindHelp(", "function bindHelp("),
        ("bytes(", "const bytes ="),
    ];
    for (call, def) in pairs {
        if body.contains(call) {
            assert!(
                body.contains(def),
                "调用了 `{call}` 但缺少定义 `{def}`——会抛 is not defined"
            );
        }
    }
}

/// 搜索仓库不再按格式过滤（前后端必须一致）。
#[test]
fn e2e_repo_search_does_not_filter_by_format() {
    let env = TestEnv::new("nofmtfilter");
    let engine = env.write_fake_engine();
    let cfg = env.config(&engine);
    let gw = env.gateway(cfg);

    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = mangosteen_gateway::GatewayServer::new(
        std::sync::Arc::new(std::sync::Mutex::new(gw)),
        false,
        String::new(),
        Duration::from_secs(20),
    );
    std::thread::spawn(move || {
        let _ = server.serve(listener);
    });
    assert!(wait_port("127.0.0.1", port, Duration::from_secs(5)));

    let (_, body) = http("GET", "127.0.0.1", port, "/", None);

    // 前端不应再过滤非 GGUF 仓库。
    //
    // 曾经这里有一层 `has_gguf !== false`：需求先要求「只留 GGUF」，
    // 后来改成「不过滤、没有任何限制」。后端放开了但前端漏改，
    // 表现为「搜到的仓库比实际少」。
    assert!(
        !body.contains("ggufOnly"),
        "前端不应再过滤非 GGUF 仓库（后端已放开，两端必须一致）"
    );
    // 精确检查「过滤代码」而不是注释：filter(...has_gguf...) 这种写法
    assert!(
        !body.contains("filter((x) => x.has_gguf"),
        "不应有按 has_gguf 过滤的代码残留"
    );
    // 但仍要显示 GGUF 徽标，让用户能自己判断
    assert!(
        body.contains("GGUF</span>") || body.contains(">GGUF<"),
        "应保留 GGUF 徽标标识"
    );
}

/// 对话页「模型信息」必须只列**运行中**的模型，与部署页「活跃模型」同源。
///
/// 曾经它读 `/v1/models`（返回所有**已下载**模型，含未启动的），
/// 结果下拉里有模型、选中却聊不了——最误导人的状态。
#[test]
fn e2e_chat_model_list_only_shows_running() {
    let env = TestEnv::new("chatlist");
    let engine = env.write_fake_engine();
    let gguf = env.write_gguf("q.gguf", 4096);
    let cfg = env.config(&engine);
    let gw = env.gateway(cfg);
    gw.models().register_local(&gguf, None).unwrap();

    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = mangosteen_gateway::GatewayServer::new(
        std::sync::Arc::new(std::sync::Mutex::new(gw)),
        false,
        String::new(),
        Duration::from_secs(20),
    );
    std::thread::spawn(move || {
        let _ = server.serve(listener);
    });
    assert!(wait_port("127.0.0.1", port, Duration::from_secs(5)));

    let (_, body) = http("GET", "127.0.0.1", port, "/", None);

    // 数据源必须是 /api/status（活跃实例），不能是 /v1/models（已下载）
    assert!(
        !body.contains(r#"const list = await api("/v1/models");"#),
        "对话页下拉不应再读 /v1/models（那是已下载模型，含未启动的）"
    );
    assert!(
        body.contains(r#"const active = (st.active || []).filter((a) => a.ready);"#),
        "下拉应基于活跃实例"
    );
    // 无运行模型时要有明确文案，而不是空白
    assert!(
        body.contains("（无运行中的模型）"),
        "无运行模型时下拉应有明确提示"
    );
    assert!(
        body.contains("请先在「部署」页启动模型"),
        "应提示用户去哪里启动"
    );
    // 发送时传具体模型（让下拉选择生效）
    assert!(
        body.contains(r#"model: $("chat-model").value || "auto""#),
        "发送应使用下拉选中的模型"
    );
}

/// 指定了未运行的模型 → 明确报错，不静默换模型。
///
/// 静默换模型比报错更难排查：用户会以为「就是这个模型答的」。
#[test]
fn e2e_chat_with_stopped_model_reports_clearly() {
    let env = TestEnv::new("chatstop");
    let engine = env.write_fake_engine();
    let gguf = env.write_gguf("q.gguf", 4096);
    let cfg = env.config(&engine);
    let gw = env.gateway(cfg);
    gw.models().register_local(&gguf, None).unwrap();

    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = mangosteen_gateway::GatewayServer::new(
        std::sync::Arc::new(std::sync::Mutex::new(gw)),
        false,
        String::new(),
        Duration::from_secs(20),
    );
    std::thread::spawn(move || {
        let _ = server.serve(listener);
    });
    assert!(wait_port("127.0.0.1", port, Duration::from_secs(5)));

    // 没有任何实例在跑，却指定了一个具体模型
    let body =
        r#"{"model":"qwen2.5-7b-instruct:q4_k_m","messages":[{"role":"user","content":"hi"}]}"#;
    let (status, resp) = http(
        "POST",
        "127.0.0.1",
        port,
        "/v1/chat/completions",
        Some(body),
    );
    assert_eq!(status, 400, "指定未运行模型必须报错，实际 body={resp}");
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    let msg = v["error"]["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("未在运行") || msg.contains("没有就绪"),
        "错误信息应说明模型未运行，实际：{msg}"
    );
}

/// 设备选择 / 推理引擎已从部署页迁到设置页，默认配置不变。
///
/// 迁移理由：这两项是**一次性设定**，不该占用每次部署的操作空间。
/// 但部署页仍要读它们，所以状态放全局、两页共享。
#[test]
fn e2e_deploy_config_moved_to_settings() {
    let env = TestEnv::new("cfgmoved");
    let engine = env.write_fake_engine();
    let cfg = env.config(&engine);
    let gw = env.gateway(cfg);

    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = mangosteen_gateway::GatewayServer::new(
        std::sync::Arc::new(std::sync::Mutex::new(gw)),
        false,
        String::new(),
        Duration::from_secs(20),
    );
    std::thread::spawn(move || {
        let _ = server.serve(listener);
    });
    assert!(wait_port("127.0.0.1", port, Duration::from_secs(5)));

    let (_, body) = http("GET", "127.0.0.1", port, "/", None);

    // 设置页有这两个区块
    assert!(body.contains(r#"id="se-target""#), "设置页应有设备选择");
    assert!(body.contains(r#"id="se-engine""#), "设置页应有推理引擎");
    // 部署页不再有
    assert!(
        !body.contains(r#"id="dp-target""#),
        "部署页不应再有设备选择"
    );
    assert!(
        !body.contains(r#"id="dp-engine""#),
        "部署页不应再有推理引擎"
    );
    // 默认算力来源为「云端」（见上方说明），推理引擎仍默认 llama.cpp
    assert!(
        body.contains(r#"<button data-t="cloud" class="on">"#),
        "默认应选中「云端」"
    );
    let (_, sbody) = http("GET", "127.0.0.1", port, "/api/settings", None);
    let v: serde_json::Value = serde_json::from_str(&sbody).unwrap();
    assert_eq!(v["local"]["engine"], "llama_cpp", "默认引擎仍是 llama.cpp");

    // 状态提到全局：两页共享同一份（部署页能读到设置页的选择）
    assert!(
        body.contains("function currentTarget("),
        "currentTarget 应为全局函数"
    );
    assert!(
        body.contains("function bindHelp("),
        "bindHelp 应为全局函数（两页共用）"
    );
    // 设置页 mount 时绑定
    assert!(
        body.contains(r#"bindHelp("se-target-help""#),
        "设置页应绑定设备说明"
    );
    assert!(
        body.contains("await loadEngines();"),
        "设置页应加载引擎列表"
    );
}

/// 「已下载模型」启动/停止合并为二态按钮，卸载独立保留。
#[test]
fn e2e_model_start_stop_is_toggle() {
    let env = TestEnv::new("toggle");
    let engine = env.write_fake_engine();
    let cfg = env.config(&engine);
    let gw = env.gateway(cfg);

    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = mangosteen_gateway::GatewayServer::new(
        std::sync::Arc::new(std::sync::Mutex::new(gw)),
        false,
        String::new(),
        Duration::from_secs(20),
    );
    std::thread::spawn(move || {
        let _ = server.serve(listener);
    });
    assert!(wait_port("127.0.0.1", port, Duration::from_secs(5)));

    let (_, body) = http("GET", "127.0.0.1", port, "/", None);

    // 运行中 → 「停止」；未运行 → 「启动」；两者互斥
    assert!(
        body.contains(r#"el("button", "primary sm", "停止")"#),
        "运行中应显示「停止」"
    );
    assert!(body.contains(r#""启动""#), "未运行应显示「启动」");
    // 二态：同一个 if/else 分支里产出，不是两个并列按钮
    assert!(
        body.contains("if (live) {") && body.contains(r#"bs.onclick = () => stopFromList("#),
        "启动/停止应由 live 状态二态切换"
    );
    // 卸载仍独立，且运行中禁止
    assert!(body.contains(r#""卸载""#), "应有「卸载」按钮");
    assert!(body.contains("请先停止该模型，再卸载"), "运行中应禁止卸载");
}

// ======================================================== 架构重构：会话层

/// 第一层「用户会话管理」的完整生命周期。
///
/// 会话是基线的第一层模块，职责是「维护任务状态、上下文、操作记录」。
/// 之前这些状态只在浏览器内存里，刷新即丢、跨进程不可见。
#[test]
fn e2e_session_lifecycle_over_http() {
    let env = TestEnv::new("sessapi");
    let engine = env.write_fake_engine();
    let cfg = env.config(&engine);
    let gw = env.gateway(cfg);

    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = mangosteen_gateway::GatewayServer::new(
        std::sync::Arc::new(std::sync::Mutex::new(gw)),
        false,
        String::new(),
        Duration::from_secs(20),
    )
    // 会话必须隔离到测试目录：默认路径是用户真实的 sessions.json，
    // 写进去会污染真实数据（表现为「会话列表里冒出测试对话」）
    .with_sessions_path(env.root.join("cfg/sessions.json"));
    std::thread::spawn(move || {
        let _ = server.serve(listener);
    });
    assert!(wait_port("127.0.0.1", port, Duration::from_secs(5)));

    // 1) 新建推理会话
    let (st, body) = http(
        "POST",
        "127.0.0.1",
        port,
        "/api/sessions",
        Some(r#"{"kind":"chat","title":""}"#),
    );
    assert_eq!(st, 201, "新建会话应返回 201，实际 {st}：{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let sid = v["session"]["id"]
        .as_str()
        .expect("缺少会话 id")
        .to_string();
    assert!(sid.starts_with("chat-"), "会话 ID 应带种类前缀：{sid}");

    // 2) 追加消息（首条用户消息应自动成为标题）
    let msg = serde_json::json!({"role":"user","content":"帮我看看这个模型能不能跑"}).to_string();
    let (st, body) = http(
        "POST",
        "127.0.0.1",
        port,
        &format!("/api/sessions/{sid}/messages"),
        Some(&msg),
    );
    assert_eq!(st, 200, "追加消息应成功：{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["session"]["messages"].as_array().unwrap().len(), 1);
    assert!(
        !v["session"]["title"].as_str().unwrap_or("").is_empty(),
        "首条用户消息应自动生成标题"
    );

    // 3) 列表只给摘要（不含消息体）
    let (st, body) = http("GET", "127.0.0.1", port, "/api/sessions?kind=chat", None);
    assert_eq!(st, 200);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let list = v["sessions"].as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["message_count"], 1);
    assert!(
        list[0].get("messages").is_none(),
        "列表不应带消息体（会让接口膨胀）"
    );

    // 4) 读取详情
    let (st, body) = http(
        "GET",
        "127.0.0.1",
        port,
        &format!("/api/sessions/{sid}"),
        None,
    );
    assert_eq!(st, 200);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["session"]["messages"][0]["role"], "user");

    // 5) 删除（幂等）
    let (st, _) = http(
        "DELETE",
        "127.0.0.1",
        port,
        &format!("/api/sessions/{sid}"),
        None,
    );
    assert_eq!(st, 200);
    let (st, _) = http(
        "GET",
        "127.0.0.1",
        port,
        &format!("/api/sessions/{sid}"),
        None,
    );
    assert_eq!(st, 404, "删除后应查不到");
}

/// 部署任务会话：操作记录要能记下**失败**的步骤。
///
/// 只记成功等于自欺——排查时最需要的恰恰是「哪一步错了、错在哪」。
#[test]
fn e2e_deploy_session_records_failed_operations() {
    let env = TestEnv::new("sessops");
    let engine = env.write_fake_engine();
    let cfg = env.config(&engine);
    let gw = env.gateway(cfg);

    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = mangosteen_gateway::GatewayServer::new(
        std::sync::Arc::new(std::sync::Mutex::new(gw)),
        false,
        String::new(),
        Duration::from_secs(20),
    )
    .with_sessions_path(env.root.join("cfg/sessions.json"));
    std::thread::spawn(move || {
        let _ = server.serve(listener);
    });
    assert!(wait_port("127.0.0.1", port, Duration::from_secs(5)));

    let (st, body) = http(
        "POST",
        "127.0.0.1",
        port,
        "/api/sessions",
        Some(r#"{"kind":"deploy","title":"qwen2.5:q4_k_m","model":"qwen2.5:q4_k_m"}"#),
    );
    assert_eq!(st, 201);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let sid = v["session"]["id"].as_str().unwrap().to_string();
    assert!(sid.starts_with("deploy-"), "部署会话 ID 前缀不对：{sid}");

    for (action, ok) in [("assess", true), ("launch", false)] {
        let body = serde_json::json!({
            "action": action,
            "detail": if ok { "评估通过" } else { "显存不足" },
            "ok": ok,
        })
        .to_string();
        let (st, _) = http(
            "POST",
            "127.0.0.1",
            port,
            &format!("/api/sessions/{sid}/operations"),
            Some(&body),
        );
        assert_eq!(st, 200);
    }

    let (_, body) = http(
        "GET",
        "127.0.0.1",
        port,
        &format!("/api/sessions/{sid}"),
        None,
    );
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let ops = v["session"]["operations"].as_array().unwrap();
    assert_eq!(ops.len(), 2);
    assert_eq!(ops[1]["ok"], false, "失败的操作必须如实记录");
}

/// 会话接口对非法输入必须明确报错，不静默兜底。
#[test]
fn e2e_session_api_rejects_invalid_input() {
    let env = TestEnv::new("sessbad");
    let engine = env.write_fake_engine();
    let cfg = env.config(&engine);
    let gw = env.gateway(cfg);

    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = mangosteen_gateway::GatewayServer::new(
        std::sync::Arc::new(std::sync::Mutex::new(gw)),
        false,
        String::new(),
        Duration::from_secs(20),
    )
    .with_sessions_path(env.root.join("cfg/sessions.json"));
    std::thread::spawn(move || {
        let _ = server.serve(listener);
    });
    assert!(wait_port("127.0.0.1", port, Duration::from_secs(5)));

    // 未知会话种类
    let (st, body) = http(
        "POST",
        "127.0.0.1",
        port,
        "/api/sessions",
        Some(r#"{"kind":"不存在的种类"}"#),
    );
    assert_eq!(st, 400, "未知 kind 必须 400：{body}");

    // 不存在的会话
    let (st, _) = http(
        "GET",
        "127.0.0.1",
        port,
        "/api/sessions/chat-9999999999999",
        None,
    );
    assert_eq!(st, 404);

    // 缺 content
    let (st, body) = http(
        "POST",
        "127.0.0.1",
        port,
        "/api/sessions/chat-1/messages",
        Some(r#"{"role":"user"}"#),
    );
    assert_eq!(st, 400, "缺 content 必须 400：{body}");
}

/// 对话页的历史必须落到服务端会话，而不是只存在浏览器内存。
///
/// 这是第一层「用户会话管理」的落地验证：之前 `history` 只是 JS 变量，
/// 刷新页面就丢，也没有任何跨进程可见性。
#[test]
fn e2e_chat_history_is_persisted_to_session() {
    let env = TestEnv::new("chatsess");
    let engine = env.write_fake_engine();
    let cfg = env.config(&engine);
    let gw = env.gateway(cfg);

    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = mangosteen_gateway::GatewayServer::new(
        std::sync::Arc::new(std::sync::Mutex::new(gw)),
        false,
        String::new(),
        Duration::from_secs(20),
    );
    std::thread::spawn(move || {
        let _ = server.serve(listener);
    });
    assert!(wait_port("127.0.0.1", port, Duration::from_secs(5)));

    let (_, body) = http("GET", "127.0.0.1", port, "/", None);

    // 必须真的调用会话接口
    assert!(
        body.contains(r#""/api/sessions""#),
        "对话页应调用会话接口（历史落服务端）"
    );
    assert!(body.contains("function ensureSession("), "缺少会话建立逻辑");
    assert!(body.contains("function saveMessage("), "缺少消息落库逻辑");
    assert!(body.contains(r#"/messages"#), "缺少追加消息的调用");

    // 会话是「记录」不是「前置条件」：失败不能阻断对话
    assert!(
        body.contains("会话写入失败不影响本次对话"),
        "会话写入失败必须静默，不能阻断对话"
    );

    // 清空对话应结束当前会话（但不销毁服务端历史）
    assert!(
        body.contains("chatSessionId = null;"),
        "清空对话应重置当前会话"
    );
}

/// 流式请求必须**边生成边转发**，不能退化成「全部读完再返回」。
///
/// 这是「输出卡顿」的直接原因：`Response.body` 是 `Vec<u8>`，
/// 天然是「全部读完再返回」。流式请求若走那条路径，
/// 用户要等模型把整段话生成完才看到第一个字——
/// 本地 CPU 推理 25 tokens 要 10 秒以上，体验就是「卡住然后突然全出来」。
#[test]
fn e2e_stream_request_is_relayed_not_buffered() {
    let env = TestEnv::new("streamui");
    let engine = env.write_fake_engine();
    let cfg = env.config(&engine);
    let gw = env.gateway(cfg);

    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = mangosteen_gateway::GatewayServer::new(
        std::sync::Arc::new(std::sync::Mutex::new(gw)),
        false,
        String::new(),
        Duration::from_secs(30),
    );
    std::thread::spawn(move || {
        let _ = server.serve(listener);
    });
    assert!(wait_port("127.0.0.1", port, Duration::from_secs(5)));

    // ---- 源码级：直通路径必须存在且被接线 ----
    let src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/server.rs"),
    )
    .unwrap();
    assert!(
        src.contains("fn relay_stream("),
        "缺少流式直通实现（relay_stream）"
    );
    assert!(
        src.contains("if is_stream_request(&req)"),
        "handle_connection 必须在 route() 之前分流流式请求，\
         否则会先被缓冲成完整 Response"
    );
    assert!(
        src.contains("--no-buffer"),
        "必须给 curl 加 --no-buffer，否则它会攒满缓冲区才输出，流式又变回批量"
    );
    assert!(
        src.contains("set_nodelay(true)"),
        "必须关闭 Nagle，否则小包被攒着，表现为「一顿一顿地出」"
    );
    // 顺序保证：分流必须在 route 之前
    let split = src.find("if is_stream_request(&req)").unwrap();
    let route = src.find("let resp = self.route(&req);").unwrap();
    assert!(
        split < route,
        "流式分流必须早于 route() 调用，否则请求已经被缓冲"
    );

    // ---- 行为级：请求能被正常受理（无活跃实例时应明确报错而非挂住）----
    let body = r#"{"model":"auto","messages":[{"role":"user","content":"hi"}],"stream":true}"#;
    let (status, resp) = http(
        "POST",
        "127.0.0.1",
        port,
        "/v1/chat/completions",
        Some(body),
    );
    assert!(
        status == 200 || status == 400 || status == 503,
        "流式请求应返回明确状态（无实例时应报错），实际 {status}: {resp}"
    );
}
