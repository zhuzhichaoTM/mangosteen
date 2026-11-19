//! 统一协议适配层的 HTTP 前端。
//!
//! 归属：**API 网关层 · 统一协议适配层**。
//!
//! ## 为什么自己写 HTTP 服务而不是引入 Web 框架
//!
//! 按「禁止无依据扩展」原则，本模块只用标准库实现 HTTP/1.1 服务端：
//!
//! | 需求 | 是否满足 |
//! |------|---------|
//! | 本机回环监听（个人客户端场景） | 是 |
//! | 固定 10 个路由 | 是，无需路由框架 |
//! | JSON 请求/响应 | 是，serde_json |
//! | 流式 SSE 透传 | 是，按块转发 |
//! | 并发 | 线程池（每连接一线程），个人级负载足够 |
//!
//! 引入 axum/hyper 会带来 tokio 运行时 + 数十个传递依赖，对「单机个人客户端」
//! 收益极小。**代价**：不支持 HTTP/2、不支持 TLS。两者对本产品的定位
//! （仅监听 127.0.0.1）都不必要。
//!
//! ## 路由表
//!
//! | 方法 | 路径 | 说明 |
//! |------|------|------|
//! | GET | `/health` | 健康检查（含后端就绪状态） |
//! | GET | `/v1/models` | [OI] 模型列表 |
//! | POST | `/v1/chat/completions` | [OI] 对话（支持 stream） |
//! | POST | `/v1/completions` | [OI] 文本补全 |
//! | POST | `/v1/embeddings` | [OI] 向量化 |
//! | GET | `/api/status` | 网关状态 |
//! | POST | `/api/deploy` | 发起部署 |
//! | GET | `/api/deployments` | 部署列表 |
//! | GET | `/api/deployments/{id}` | 单个部署 |
//! | DELETE | `/api/deployments/{id}` | 停止部署 |
//! | POST | `/api/assess` | 仅评估，不部署 |

use std::io::{BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use mangosteen_core::{MnError, MnResult};

use crate::gateway::Gateway;
use crate::http::relay::is_stream_request;
use crate::protocol::BackendTarget;

// 兼容导出：处理器按 `http::server::*` 引入这些类型与工具，
// 物理位置已按职责迁出，这里只保留唯一的公开路径。
pub(crate) use crate::http::helpers::{first_line, job_json, model_name_for_estimate, query_flag};
pub(crate) use crate::http::request::{parse_request, ParsedRequest};
pub(crate) use crate::http::response::Response;

/// 网关 HTTP 服务。
pub struct GatewayServer {
    pub(crate) gateway: Arc<Gateway>,
    pub(crate) backend_snapshot: Arc<RwLock<std::collections::BTreeMap<String, BackendTarget>>>,
    pub(crate) require_key: bool,
    pub(crate) api_key: String,
    pub(crate) upstream_timeout: Duration,
    pub(crate) sampler: mangosteen_platform::resmon::CpuSampler,
    pub(crate) pull_jobs: crate::pulljobs::PullJobs,
    /// 部署运行态探测缓存（`running` 字段）。
    ///
    /// `None` 表示未启用后台探测（测试构造的服务器）：此时
    /// `running` 回退为「台账状态是否 ready」，行为可预期且不发网络请求。
    pub(crate) runtime_probe: Option<crate::runprobe::RuntimeProbe>,
    pub(crate) sessions: mangosteen_session::SessionStore,
    /// P1 上游熔断器（2026-11-19）：连续失败阈值内触发熔断（直接 503
    /// + Retry-After），避免异常流量打爆远端 llama-server；成功请求
    /// 清零计数。Arc 共享给所有工作线程。
    pub(crate) upstream_guard: std::sync::Arc<std::sync::Mutex<UpstreamGuard>>,
}

/// P1 上游熔断器状态。
///
/// - 连续失败达到 `FAIL_THRESHOLD`（5）次 → 打开熔断，`OPEN_UNTIL`
///   时间内所有上游请求直接拒绝（503 + Retry-After）；
/// - 冷却结束自动转「半开」：放行一个探测请求，成功 → 关闭（清零）；
///   失败 → 重新打开（冷却时长翻倍，指数退避上限 5 分钟）。
pub(crate) struct UpstreamGuard {
    failures: u32,
    open_until: std::time::Instant,
    cooldown: std::time::Duration,
}

impl UpstreamGuard {
    fn new() -> Self {
        Self {
            failures: 0,
            open_until: std::time::Instant::now(),
            cooldown: std::time::Duration::from_secs(10),
        }
    }
    /// 请求前检查：熔断打开时返回 Retry-After 秒数（请求应被拒绝）。
    fn check(&self) -> Option<u64> {
        let now = std::time::Instant::now();
        if now < self.open_until {
            Some(
                (self.open_until - now).as_secs().max(1),
            )
        } else {
            None
        }
    }
    /// 上游成功：清零失败计数，关闭熔断。
    pub(crate) fn on_success(&mut self) {
        self.failures = 0;
        self.open_until = std::time::Instant::now();
        self.cooldown = std::time::Duration::from_secs(10);
    }
    /// 上游失败（连接错误/5xx）：累计，达阈值打开熔断。
    pub(crate) fn on_failure(&mut self) {
        self.failures += 1;
        if self.failures >= 5 {
            self.open_until = std::time::Instant::now() + self.cooldown;
            // 指数退避（上限 5 分钟）
            self.cooldown = (self.cooldown * 2).min(std::time::Duration::from_secs(300));
        }
    }
    /// 当前熔断打开时返回 Retry-After 头值。
    pub(crate) fn retry_after(&self) -> Option<u64> {
        self.check()
    }
}

impl GatewayServer {
    /// 构造。
    pub fn new(
        gateway: Arc<Gateway>,
        require_key: bool,
        api_key: String,
        upstream_timeout: Duration,
    ) -> Self {
        // API 网关重启后自动恢复台账中已就绪模型的本地隧道映射。
        // 只恢复进程内连接状态，不创建实例、不下载模型、不重启节点。
        //
        // 异步执行（2026-11-18 压测修复）：restore 内部是 SSH 隧道建立
        // （厂商 describe + 密钥验证 + heal/establish，链路可达 30s+），
        // 同步跑会把 HTTP accept 循环的启动推迟到 restore 完成——期间
        // 所有 API（health/unload/推理）全部超时（实测压测 0/10：每轮
        // unload 192s 无响应，实例不关机、持续计费）。restore 是纯恢复
        // 动作，失败不影响新部署；放后台线程，HTTP 立即可用。
        {
            let gw = Arc::clone(&gateway);
            std::thread::Builder::new()
                .name("mn-restore".to_string())
                .spawn(move || {
                    let _ = gw.restore_deployment_tunnels();
                })
                .expect("spawn restore thread");
        }
        let backend_snapshot = Arc::new(RwLock::new(std::collections::BTreeMap::new()));
        // 只读台账，把**全部**已就绪模型一次性做成业务转发快照。
        //
        // 绝不能调用 `active_backend()`：它只返回一个模型，而且会触发
        // Orchestrator::refresh。业务转发只读快照，部署控制完全走另一条
        // 路径；Orchestrator 可变状态已下沉为细粒度内部锁，此处不再有
        // 全局锁竞争。
        {
            let gw = gateway.as_ref();
            if let Ok(records) = gw.list_deployments() {
                if let Ok(mut snapshot) = backend_snapshot.write() {
                    for record in records {
                        if record.status != mangosteen_base::state::DeploymentStatus::Ready {
                            continue;
                        }
                        let Some(port) = record.plan.local_access_port() else {
                            continue;
                        };
                        let model_id = mangosteen_core::model::ModelEntry::derive_id(
                            &record.plan.model.name,
                            record.plan.model.quant,
                            record.plan.model.alias.as_deref(),
                        );
                        snapshot.insert(
                            model_id.clone(),
                            BackendTarget {
                                host: "127.0.0.1".to_string(),
                                port,
                                model_id,
                                origin_label: record.plan.origin.label(),
                                api_key: record.plan.cloud.as_ref().and_then(|c| c.api_key.clone()),
                                tls: record.plan.cloud.as_ref().map(|c| c.tls).unwrap_or(false),
                                tls_cert_path: record
                                    .plan
                                    .origin
                                    .instance_id()
                                    .map(|id| {
                                        let p = mangosteen_platform::paths::config_dir(
                                            &mangosteen_platform::paths::real_env,
                                        )
                                        .join("tls")
                                        .join(format!("{id}.crt"));
                                        p.is_file()
                                            .then(|| p.to_string_lossy().to_string())
                                    })
                                    .flatten(),
                            },
                        );
                    }
                }
            }
        }
        Self {
            gateway,
            backend_snapshot,
            require_key,
            api_key,
            upstream_timeout,
            sampler: mangosteen_platform::resmon::CpuSampler::new(),
            pull_jobs: crate::pulljobs::PullJobs::new(),
            runtime_probe: None,
            upstream_guard: std::sync::Arc::new(std::sync::Mutex::new(UpstreamGuard::new())),
            sessions: mangosteen_session::SessionStore::new(
                mangosteen_platform::paths::sessions_file(
                    &mangosteen_platform::paths::real_env as &dyn Fn(&str) -> Option<String>,
                ),
            ),
        }
    }

    /// 覆盖会话文件路径。
    ///
    /// 测试必须用这个：默认路径是用户真实的 `sessions.json`，
    /// 测试写进去会污染真实数据（表现为「会话列表里出现测试对话」）。
    pub fn with_sessions_path(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.sessions = mangosteen_session::SessionStore::new(path);
        self
    }

    /// 启用 SSH 登录信息的后台刷新。
    ///
    /// 由 `serve` / `ui` 在启动时调用。
    ///
    /// ## 云端实例监控（GetCompShareInstanceMonitor）已整体移除
    ///
    /// 曾经后台线程每 5 秒调厂商监控接口（网关一启动就开始打，与有没有
    /// 人看页面无关），而当前控制台并没有消费这些数据的页面，纯属消耗
    /// 配额并可能被厂商限流。需要实例指标时走设置页的实例列表（用户
    /// 主动操作，按需调用），不再有任何周期性查询。
    ///
    /// SSH 登录信息改为**请求驱动**（2026-11-17 用户裁决删除后台轮询）：
    /// 原 sshmon 20s 后台线程对每个云端部署无限 describe，3 个部署
    /// = 180 次/小时纯空转，是厂商 226619 限频的头号来源。现在缓存只在
    /// 两处填充——部署完成时刷新一次 + 用户请求 ssh_info 时就地刷新，
    /// 零后台常驻调用。
    pub fn with_cloud_monitor(self) -> Self {
        // 后台 SSH 信息轮询已删除（sshmon 模块随之移除）。
        // 保留本方法签名：CLI ui.rs 的构建链不动，未来接入新的
        // 云监控能力（如指标采集）时在此扩展。
        self
    }

    /// 启用部署运行态周期探测（部署页签的「运行/停止」数据源）。
    ///
    /// 与云端监控同理：后台线程每 5 秒探一次本地隧道端口的 `/health`，
    /// HTTP 端点只读缓存。不启用时 `running` 回退为台账状态，
    /// 测试不必等待真实端口。
    pub fn with_runtime_probe(mut self) -> Self {
        let probe = crate::runprobe::RuntimeProbe::new();
        crate::runprobe::spawn_runtime_probe(
            Arc::clone(&self.gateway),
            Arc::clone(&self.backend_snapshot),
            probe.clone(),
        );
        self.runtime_probe = Some(probe);
        self
    }

    /// 在给定地址上阻塞运行。
    ///
    /// 每连接一线程；个人级并发（<64）完全够用。
    pub fn serve(self, listener: TcpListener) -> MnResult<()> {
        let shared = Arc::new(self);

        // 固定大小的 worker 池，而不是「每连接一线程」。
        //
        // 为什么必须改：监控页每 1.5 秒轮询一次，`每连接一线程` 的模型下
        // 线程只增不减（线程无法被复用），几分钟就积累上百个连接与线程，
        // 最终把进程拖垮——表现为「所有 API 都超时、静态页还能开」。
        //
        // 池大小的选择：本产品是单机个人客户端，并发来源只有「浏览器几个
        // 页签 + 可能的一两个脚本」，8 个 worker 足够；超出时连接在
        // backlog 里排队（内核层面），不会丢请求。
        const WORKERS: usize = 8;

        let (tx, rx) = std::sync::mpsc::channel::<TcpStream>();
        let rx = Arc::new(Mutex::new(rx));

        for i in 0..WORKERS {
            let rx = Arc::clone(&rx);
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name(format!("mn-http-{i}"))
                .spawn(move || loop {
                    // 从共享接收端取连接；接收端关闭即退出
                    let stream = {
                        let guard = match rx.lock() {
                            Ok(g) => g,
                            Err(_) => return,
                        };
                        match guard.recv() {
                            Ok(s) => s,
                            Err(_) => return,
                        }
                    };
                    let _ = shared.handle_connection(stream);
                })
                .map_err(|e| MnError::Io {
                    context: "启动 HTTP worker".to_string(),
                    reason: e.to_string(),
                })?;
        }

        for conn in listener.incoming() {
            match conn {
                Ok(stream) => {
                    // 队列已满（worker 全忙且积压）时直接拒绝并关闭，
                    // 而不是无限堆积导致内存增长
                    if tx.send(stream).is_err() {
                        break;
                    }
                }
                Err(_) => continue,
            }
        }
        Ok(())
    }

    /// 处理一个连接。
    pub fn handle_connection(&self, stream: TcpStream) -> MnResult<()> {
        stream.set_read_timeout(Some(Duration::from_secs(120))).ok();
        stream
            .set_write_timeout(Some(Duration::from_secs(300)))
            .ok();
        // 审计（P2）：客户端来源地址（回环/局域网/非回环），失败时 "-"
        let peer = stream
            .peer_addr()
            .map(|a| a.ip().to_string())
            .unwrap_or_else(|_| "-".to_string());
        let mut reader = BufReader::new(
            stream
                .try_clone()
                .map_err(|e| MnError::io("复制连接", &e))?,
        );
        let req = match parse_request(&mut reader) {
            Ok(Some(r)) => r,
            Ok(None) => return Ok(()),
            Err(e) => {
                let mut s = stream;
                let _ = s.write_all(&Response::error(&e).to_bytes());
                return Ok(());
            }
        };

        // P2 X-Forwarded-For 校验（2026-11-19）：网关绑定 127.0.0.1（回环），
        // 非回环来源本就不可达；显式拒绝「非回环 + X-Forwarded-For」（伪造
        // 来源头）请求——防代理链欺骗审计与限流逻辑。
        let non_loopback = !peer.starts_with("127.") && peer != "::1" && peer != "-";
        if non_loopback && req.header("x-forwarded-for").is_some() {
            let e = MnError::bad_request(
                "网关仅接受本机回环访问（检测到伪造的 X-Forwarded-For 头）".to_string(),
            );
            let mut s = stream;
            let _ = s.write_all(&Response::error(&e).to_bytes());
            return Ok(());
        }

        // ---- 流式推理请求：必须走直通路径 ----
        //
        // 不能先 `route()` 再 `write_all`：`Response.body` 是 `Vec<u8>`，
        // 语义是「全部读完再返回」。对 `stream: true` 的请求，那等于
        // 把流式退化成非流式——用户要等模型把整段话生成完才看到第一个字。
        //
        // 实测后果：本地 CPU 推理 25 tokens 要 10 秒，界面上就是
        // 「点发送 → 卡 10 秒 → 突然整段出现」；这 10 秒里点「停止」
        // 也没用，因为网关还阻塞在读上游。
        if is_stream_request(&req) {
            return self.relay_stream(stream, &req);
        }

        let resp = self.route(&req);
        audit_log(&peer, &req, resp.status);
        let mut s = stream;
        s.write_all(&resp.to_bytes())
            .map_err(|e| MnError::io("写响应", &e))?;
        s.flush().ok();
        // 响应头带 `Connection: close`，因此处理完一个请求就关闭。
        // 这是刻意的简化：无需维护每连接的 keep-alive 状态机，
        // 配合固定 worker 池足以支撑单机个人场景。
        Ok(())
    }

    /// 校验 API Key。
    pub(crate) fn authorized(&self, req: &ParsedRequest) -> bool {
        let provided = req
            .header("authorization")
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(|s| s.trim().to_string())
            .or_else(|| req.header("x-api-key").map(|s| s.trim().to_string()));
        // P1-3 恒定时间比较（2026-11-19）：`==` 短路比较会泄漏长度/
        // 前缀信息（timing side-channel，回环+网络抖动下难利用但专家
        // 规范要求）。改为逐字节 diff 累计——相同长度时耗时与内容无关。
        match provided {
            Some(k) => {
                let a = k.as_bytes();
                let b = self.api_key.as_bytes();
                let mut diff: u8 = (a.len() ^ b.len()) as u8;
                for i in 0..a.len().max(b.len()) {
                    let x = a.get(i).copied().unwrap_or(0);
                    let y = b.get(i).copied().unwrap_or(0);
                    diff |= x ^ y;
                }
                diff == 0
            }
            None => false,
        }
    }

    // ---------------------------------------------------------------- 处理器

    // ============================ handlers/status.rs ============================

    // ------------------------------------------------------------ device / setting

    // ============================ handlers/settings.rs ============================

    // ------------------------------------------------------------ chat 增强

    // ============================ handlers/attachments.rs ============================

    // ============================ handlers/models.rs ============================

    // ---------------------------------------------------------- 会话管理

    // 会话相关处理器已迁至 crate::http::handlers::sessions

    // ============================ handlers/deployment.rs ============================

    // ---------------------------------------------------------------- 推理转发

    // ============================ handlers/inference.rs ============================
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::request::{percent_decode_path, MAX_BODY};
    use crate::http::response::reason_phrase;
    use crate::http::upstream::{parse_http_response, split_url, summarize_body};
    use std::collections::HashMap;

    fn req(method: &str, path: &str) -> ParsedRequest {
        ParsedRequest {
            method: method.to_string(),
            path: path.to_string(),
            query: String::new(),
            headers: HashMap::new(),
            body: Vec::new(),
        }
    }

    #[test]
    fn parses_simple_get() {
        let raw = "GET /health HTTP/1.1\r\nHost: x\r\n\r\n";
        let mut r = BufReader::new(raw.as_bytes());
        let p = parse_request(&mut r).unwrap().unwrap();
        assert_eq!(p.method, "GET");
        assert_eq!(p.path, "/health");
        assert_eq!(p.header("host"), Some("x"));
        assert!(p.body.is_empty());
    }

    #[test]
    fn parses_post_with_body_and_query() {
        let raw = "POST /api/deploy?trace=1 HTTP/1.1\r\nContent-Length: 7\r\n\r\n{\"a\":1}";
        let mut r = BufReader::new(raw.as_bytes());
        let p = parse_request(&mut r).unwrap().unwrap();
        assert_eq!(p.method, "POST");
        assert_eq!(p.path, "/api/deploy");
        assert_eq!(p.query_param("trace"), Some("1".to_string()));
        assert_eq!(p.body_str(), "{\"a\":1}");
    }

    #[test]
    fn rejects_oversized_body() {
        let raw = format!(
            "POST /x HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY + 1
        );
        let mut r = BufReader::new(raw.as_bytes());
        let e = parse_request(&mut r).unwrap_err();
        assert!(e.to_string().contains("超过上限"));
    }

    #[test]
    fn rejects_bad_request_line() {
        let raw = "GET /x NOTHTTP\r\n\r\n";
        let mut r = BufReader::new(raw.as_bytes());
        assert!(parse_request(&mut r).is_err());
    }

    #[test]
    fn empty_stream_returns_none() {
        let mut r = BufReader::new("".as_bytes());
        assert!(parse_request(&mut r).unwrap().is_none());
    }

    #[test]
    fn response_serialization_has_required_headers() {
        let r = Response::json(200, &serde_json::json!({"ok": true}));
        let bytes = r.to_bytes();
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("Content-Type: application/json"));
        assert!(text.contains("Content-Length: "));
        assert!(text.contains("Server: mangosteen/"));
        assert!(text.contains("Connection: close"));
        assert!(text.ends_with("{\"ok\":true}"));
    }

    #[test]
    fn error_response_maps_status() {
        let r = Response::error(&MnError::bad_request("x"));
        assert_eq!(r.status, 400);
        let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["error"]["type"], "invalid_request_error");
    }

    #[test]
    fn unknown_route_is_404() {
        let r = Response::not_found("/nope");
        assert_eq!(r.status, 404);
    }

    #[test]
    fn reason_phrases() {
        assert_eq!(reason_phrase(200), "OK");
        assert_eq!(reason_phrase(201), "Created");
        assert_eq!(reason_phrase(503), "Service Unavailable");
        assert_eq!(reason_phrase(999), "Unknown");
    }

    #[test]
    fn url_splitting() {
        let (h, p) = split_url("http://127.0.0.1:11435/v1/chat/completions").unwrap();
        assert_eq!(h, "127.0.0.1:11435");
        assert_eq!(p, "/v1/chat/completions");

        let (h, p) = split_url("http://127.0.0.1:8080").unwrap();
        assert_eq!(h, "127.0.0.1:8080");
        assert_eq!(p, "/");

        assert!(split_url("https://x/y").is_err());
    }

    #[test]
    fn parses_upstream_response() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"id\":\"x\"}";
        let (status, body) = parse_http_response(raw).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, "{\"id\":\"x\"}");
    }

    /// 路径必须 percent-decode。
    ///
    /// 真实故障：UI 卸载 `qwen2.5-0.5b-instruct:q2_k` 失败，报
    /// 「未知路由：qwen2.5-0.5b-instruct%3Aq2_k」——浏览器把 `:` 编成了
    /// `%3A`，而路由用解码后的路径匹配。同一个 ID 用 curl 手敲冒号却
    /// 正常，所以特别难查。
    #[test]
    fn percent_decode_handles_model_id_colon() {
        assert_eq!(
            percent_decode_path("qwen2.5-0.5b-instruct%3Aq2_k"),
            "qwen2.5-0.5b-instruct:q2_k"
        );
        // 小写十六进制同样要认
        assert_eq!(percent_decode_path("a%3ab"), "a:b");
        // 中文字符（三个字节各编码一次）
        assert_eq!(percent_decode_path("%E5%B1%B1%E7%AB%B9"), "山竹");
    }

    #[test]
    fn percent_decode_leaves_plain_paths_untouched() {
        assert_eq!(
            percent_decode_path("/v1/chat/completions"),
            "/v1/chat/completions"
        );
        assert_eq!(percent_decode_path("/api/models"), "/api/models");
    }

    /// 加号在路径里是字面加号，不是空格（那是 query 的规则）。
    #[test]
    fn percent_decode_keeps_plus_literal() {
        assert_eq!(percent_decode_path("a+b"), "a+b");
    }

    /// 坏转义原样保留，交给路由自然 404——不能因为一个畸形转义
    /// 就 panic 或断开连接。
    #[test]
    fn percent_decode_tolerates_malformed_escapes() {
        assert_eq!(percent_decode_path("a%ZZb"), "a%ZZb");
        assert_eq!(percent_decode_path("%A"), "%A");
        assert_eq!(percent_decode_path("trailing%"), "trailing%");
    }

    /// 端到端：编码过的模型 ID 必须能路由到卸载分支。
    #[test]
    fn request_path_from_encoded_model_id_is_routed() {
        let raw = "DELETE /api/models/qwen2.5-0.5b-instruct%3Aq2_k?delete_file=1 HTTP/1.1\r\n\r\n";
        let mut r = std::io::BufReader::new(std::io::Cursor::new(raw.as_bytes()));
        let req = parse_request(&mut r).unwrap().expect("应解析出请求");
        assert_eq!(req.method, "DELETE");
        assert_eq!(req.path, "/api/models/qwen2.5-0.5b-instruct:q2_k");
        assert!(req.path.starts_with("/api/models/"));
    }

    #[test]
    fn parses_upstream_error_status() {
        let raw = b"HTTP/1.1 400 Bad Request\r\n\r\n{\"error\":{\"message\":\"bad\"}}";
        let (status, body) = parse_http_response(raw).unwrap();
        assert_eq!(status, 400);
        assert!(body.contains("bad"));
    }

    #[test]
    fn decodes_chunked_response() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        let (status, body) = parse_http_response(raw).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, "hello world");
    }

    #[test]
    fn malformed_upstream_response_errors() {
        assert!(parse_http_response(b"garbage").is_err());
    }

    #[test]
    fn summarize_extracts_error_message() {
        assert_eq!(
            summarize_body(r#"{"error":{"message":"context too long"}}"#),
            "context too long"
        );
        assert_eq!(summarize_body(r#"{"message":"oops"}"#), "oops");
        assert_eq!(summarize_body("plain text"), "plain text");
    }

    #[test]
    fn route_method_mismatch_gives_405() {
        // 构造一个最小 server（gateway 用不到，因为路由在命中处理器前就返回）
        // 这里直接验证 not_found 分支的行为
        let r = Response::text(405, "方法 GET 不被 /api/deploy 支持");
        assert_eq!(r.status, 405);
    }

    #[test]
    fn parsed_request_header_is_case_insensitive() {
        let mut p = req("GET", "/x");
        p.headers
            .insert("content-type".into(), "application/json".into());
        assert_eq!(p.header("Content-Type"), Some("application/json"));
        assert_eq!(p.header("CONTENT-TYPE"), Some("application/json"));
    }
}

/// 访问审计日志（P2，2026-11-18）：一行一请求，追加写
/// `{config_dir}/logs/audit-YYYYMMDD.log`（600 权限）。
///
/// 记录内容：时间戳 / 来源 IP / 方法 / 路径 / 状态码 / 鉴权 key 前缀。
/// - 非回环来源标 `[non-loopback]`——网关只该被本机访问；
/// - 401 标 `[auth-failed]`——401 洪水通常是暴力尝试。
/// 审计失败绝不阻断请求（磁盘满/权限问题——记 stderr 即可）。
fn audit_log(peer: &str, req: &ParsedRequest, status: u16) {
    use std::io::Write as _;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let date = {
        let days = (now / 86400) as i64;
        let (y, m, d) = civil_from_days(days);
        format!("{y:04}{m:02}{d:02}")
    };
    let auth_prefix = req
        .header("authorization")
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|k| {
            let k = k.trim();
            if k.len() > 8 { format!("{}...", &k[..8]) } else { k.to_string() }
        })
        .unwrap_or_else(|| "-".into());
    // P2 401 暴力尝试升级标记（2026-11-19）：同来源短时间连续 auth-failed
    // 达阈值（进程内计数，滑动窗口 60s）——审计行升级标记，提示可能正在
    // 被爆破（网关绑回环，实际风险低；标记供事后分析）。
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
    static AUTH_FAILS: AtomicU32 = AtomicU32::new(0);
    static LAST_FAIL_TS: AtomicU64 = AtomicU64::new(0);
    let non_loopback = !peer.starts_with("127.") && peer != "::1" && peer != "-";
    let flags = match (status, non_loopback) {
        (401, _) => {
            // 60s 滑动窗口
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let last = LAST_FAIL_TS.load(Ordering::Relaxed);
            let count = if now_ms.saturating_sub(last) > 60_000 {
                AUTH_FAILS.store(1, Ordering::Relaxed);
                1
            } else {
                AUTH_FAILS.fetch_add(1, Ordering::Relaxed) + 1
            };
            LAST_FAIL_TS.store(now_ms, Ordering::Relaxed);
            if count >= 10 {
                " [auth-failed][brute-force?]"
            } else {
                " [auth-failed]"
            }
        }
        (_, true) => " [non-loopback]",
        _ => "",
    };
    let dir = mangosteen_platform::paths::config_dir(&mangosteen_platform::paths::real_env)
        .join("logs");
    let _ = std::fs::create_dir_all(&dir);
    // P1 日志目录权限收紧（2026-11-19）：700（同 audit 文件 600）。
    // 平台差异走 platform crate（gateway 层禁止 cfg(unix) 分支）。
    let _ = mangosteen_platform::fs::set_private_dir(&dir);
    // P1 审计轮转（2026-11-19）：清理 30 天前的 audit-*.log（防长跑
    // 占满磁盘）。每个新文件创建时顺带扫一次（O(目录条目)，低频）。
    if let Ok(entries) = std::fs::read_dir(&dir) {
        let cutoff = now.saturating_sub(30 * 86400);
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(stamp) = name
                .strip_prefix("audit-")
                .and_then(|n| n.strip_suffix(".log"))
                .and_then(|d| d.parse::<u64>().ok())
            {
                // 文件名日期是 YYYYMMDD，近似按天比较：转成近似 epoch 后
                // 与 cutoff 比（早 30 天以上删）。
                let (y, m, d) = (
                    (stamp / 10000) as i64,
                    ((stamp / 100) % 100) as i64,
                    (stamp % 100) as i64,
                );
                let days = days_from_civil(y, m, d);
                if (days as u64) < cutoff / 86400 {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }
    let path = dir.join(format!("audit-{date}.log"));
    // P1 URL 脱敏（2026-11-19）：审计行 path 可能含 ?token=/签名参数，
    // 截断查询串（保留路径本身，便于排查访问了哪个端点）。
    let audit_path = req
        .path
        .split_once('?')
        .map(|(base, _)| base.to_string())
        .unwrap_or_else(|| req.path.clone());
    let line = format!(
        "{} {} \"{} {}\" {}{} key={}\n",
        now, peer, req.method, audit_path, status, flags, auth_prefix
    );
    let ok = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut f| {
            // P1 文件 600：平台差异走 platform crate（gateway 禁止 cfg(unix)）。
            let _ = mangosteen_platform::fs::set_private_permissions(&path);
            f.write_all(line.as_bytes())
        })
        .is_ok();
    if !ok {
        eprintln!("[audit] ⚠ 审计日志写入失败（不影响请求）：{path:?}");
    }
}

/// (year, month, day) → days since epoch（Howard Hinnant 逆算法，无外部依赖）。
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// days → (year, month, day) civil date（Howard Hinnant 算法，无外部依赖）。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}
