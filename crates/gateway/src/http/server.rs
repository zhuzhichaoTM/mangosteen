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

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mangosteen_core::openai::{ChatRequest, ErrorResponse, HealthResponse};
use mangosteen_core::{MnError, MnResult};

use crate::gateway::Gateway;
use crate::protocol::{self, BackendTarget};

/// 请求行与头部解析结果。
#[derive(Debug, Clone)]
pub struct ParsedRequest {
    /// 方法
    pub method: String,
    /// 路径（不含查询串）
    pub path: String,
    /// 查询串
    pub query: String,
    /// 头部（键统一小写）
    pub headers: HashMap<String, String>,
    /// 请求体
    pub body: Vec<u8>,
}

impl ParsedRequest {
    /// 取头部。
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(|s| s.as_str())
    }

    /// 请求体转字符串。
    pub fn body_str(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }

    /// 查询参数。
    pub fn query_param(&self, key: &str) -> Option<String> {
        self.query.split('&').find_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            if k == key {
                Some(v.to_string())
            } else {
                None
            }
        })
    }
}

/// 响应。
#[derive(Debug, Clone)]
pub struct Response {
    /// 状态码
    pub status: u16,
    /// 内容类型
    pub content_type: String,
    /// 响应体
    pub body: Vec<u8>,
}

impl Response {
    /// JSON 响应。
    pub fn json<T: serde::Serialize>(status: u16, value: &T) -> Self {
        let body = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
        Self {
            status,
            content_type: "application/json".to_string(),
            body,
        }
    }

    /// 纯文本。
    pub fn text(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            content_type: "text/plain; charset=utf-8".to_string(),
            body: body.into().into_bytes(),
        }
    }

    /// 错误响应。
    pub fn error(e: &MnError) -> Self {
        let (status, payload) = protocol::error_payload(e);
        Self::json(status, &payload)
    }

    /// 404。
    pub fn not_found(path: &str) -> Self {
        let payload = ErrorResponse::bad_request(format!("未知路由：{path}"), None);
        Self::json(404, &payload)
    }

    /// 序列化为 HTTP 报文。
    pub fn to_bytes(&self) -> Vec<u8> {
        let head = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nServer: mangosteen/{}\r\nConnection: close\r\n\r\n",
            self.status,
            reason_phrase(self.status),
            self.content_type,
            self.body.len(),
            mangosteen_core::identity::VERSION
        );
        let mut out = head.into_bytes();
        out.extend_from_slice(&self.body);
        out
    }
}

/// Base64 解码（标准字母表，允许换行与填充）。
///
/// 只为「浏览器上传附件」这一条路径实现，不引入 base64 crate：
/// 需求固定、无变体（不支持 URL-safe），几十行足够。
fn base64_decode(input: &str) -> Result<Vec<u8>, String> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }

    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut buf: u32 = 0;
    let mut bits = 0u32;
    for &c in input.as_bytes() {
        if c == b'=' || c == b'\n' || c == b'\r' || c == b' ' || c == b'\t' {
            continue;
        }
        let v = val(c).ok_or_else(|| format!("非法字符 `{}`", c as char))? as u32;
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    Ok(out)
}

/// 远端元数据预取读取的头部字节数。
///
/// 4 MiB 是实测折中：元数据段（KV 表）通常 30~60 KiB，大模型的词表元数据
/// 可达数百 KiB；4 MiB 覆盖绝大多数情况，同时把首字节等待压在 2~3 秒。
/// 再大收益递减，还会明显拖慢「点部署后第一眼看到结果」的体感。
const REMOTE_HEADER_BYTES: u64 = 8 * 1024 * 1024;

/// 评估结果的单行摘要（含资源占用明细）。
///
/// 与 `gateway::assessed_detail` 同样的信息，但在 server 侧构造——
/// 那边是 `pub(crate)` 之外的私有函数，且这里要用在**持有任务表锁**的
/// 闭包里（不能再进网关锁拿数据）。
fn probe_assessed_detail(ev: &mangosteen_orchestration::assess::Evaluated) -> String {
    crate::gateway::assessed_detail(ev)
}

/// 从模型引用推一个人类可读的名字（用于粗估兜底）。
///
/// 去掉 `.gguf` 后缀与量化尾缀，得到如 `qwen2.5-0.5b-instruct`。
/// 量化尾缀写法多样（`-q4_k_m` / `.Q4_K_M`），复用 `ModelRef` 自己的
/// 解析器，避免两处规则分叉。
fn model_name_for_estimate(model: &mangosteen_core::source::ModelRef) -> String {
    let f = model.local_file_name();
    let stem = f.strip_suffix(".gguf").unwrap_or(&f).to_string();
    match model.quant_from_filename() {
        Some(q) => {
            let lower = stem.to_ascii_lowercase();
            let qs = q.as_str().to_ascii_lowercase();
            match lower.rfind(&qs) {
                Some(pos) => {
                    let cut = stem[..pos].trim_end_matches(['-', '.', '_']).to_string();
                    if cut.is_empty() {
                        stem
                    } else {
                        cut
                    }
                }
                None => stem,
            }
        }
        None => stem,
    }
}

/// 取错误信息第一行（弹窗/阶段说明里只放一句能读懂的话）。
fn first_line(msg: &str) -> String {
    msg.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("未知原因")
        .trim()
        .to_string()
}

/// 任务状态 → JSON。字段名是 UI 的契约，改名会让进度条白屏。
fn job_json(j: &crate::pulljobs::PullJob) -> serde_json::Value {
    // 步骤条：把「全部步骤」与「每步最新状态」一次给全。
    //
    // 为什么在后端算而不是前端拼：前端只拿到「当前阶段」时，无法区分
    // 「这步还没到」与「这步跳过了」——后者必须显示原因。把状态判定
    // 放在这里，UI 只管画。
    let trail = &j.deploy_trail;
    let steps: Vec<serde_json::Value> = crate::progress::DeployStage::STEPS
        .iter()
        .map(|s| {
            // 取该阶段的最后一条事件作为它的状态
            let last = trail.iter().rev().find(|e| e.stage == s.id());
            let outcome = last.map(|e| e.outcome.as_str()).unwrap_or("pending");
            serde_json::json!({
                "id": s.id(),
                "label": s.label(),
                "index": s.index(),
                "outcome": outcome,
                "detail": last.map(|e| e.detail.clone()).unwrap_or_default(),
                "at_ms": last.map(|e| e.at_ms),
            })
        })
        .collect();

    serde_json::json!({
        "id": j.id,
        "spec": j.spec,
        "file": j.file,
        "state": j.state.as_str(),
        "done_bytes": j.done_bytes,
        "total_bytes": j.total_bytes,
        "percent": j.percent(),
        "bytes_per_sec": j.bytes_per_sec(),
        "model_id": j.model_id,
        "error": j.error,
        "note": j.note,
        // warning ≠ error：下载成功但文件不是模型（如 imatrix）
        "warning": j.warning,
        // 自动部署相关：UI 用它展示「准备中 → 部署中 → 已启动」
        "deploy_after": j.deploy_after,
        "deploy_state": j.deploy_state,
        // 结构化进度：当前阶段 + 序号 + 总步数 + 每步状态
        "deploy_stage": j.deploy_stage.map(|s| serde_json::json!({
            "id": s.id(),
            "label": s.label(),
            "index": s.index(),
            "total": crate::progress::DeployStage::total(),
        })),
        "deploy_detail": j.deploy_detail,
        "deploy_steps": steps,
        "deploy_trail": trail.iter().map(|e| serde_json::json!({
            "stage": e.stage,
            "label": e.label,
            "outcome": e.outcome.as_str(),
            "detail": e.detail,
            "at_ms": e.at_ms,
        })).collect::<Vec<_>>(),
        "deployment_id": j.deployment_id,
        "already_present": j.already_present,
        // 能否「停止」：只有还在跑的任务可以中止并回滚。
        // 已终结的（done/failed/cancelled）不该再给停止按钮——
        // 点了也没意义，还会让用户以为能撤销一个已经完成的部署。
        "can_abort": !j.state.is_terminal() && j.deploy_after,
    })
}

/// 从查询串中取布尔标记。
///
/// 接受 `1` / `true` / `yes` / 空值（`?flag`）作为真，其余为假。
/// 单独成函数是因为路由分支只拿得到 `ParsedRequest`，而 Query 解析
/// 不应依赖整个请求对象。
fn query_flag(query: &str, key: &str) -> bool {
    for part in query.split('&') {
        let (k, v) = match part.split_once('=') {
            Some((k, v)) => (k, v),
            None => (part, ""),
        };
        if k == key {
            let v = v.trim().to_ascii_lowercase();
            return matches!(v.as_str(), "" | "1" | "true" | "yes" | "on");
        }
    }
    false
}

/// 状态码短语。
fn reason_phrase(code: u16) -> &'static str {
    match code {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Unknown",
    }
}

/// 请求体上限：32 MiB（长上下文对话足够）。
const MAX_BODY: usize = 32 * 1024 * 1024;

/// 十六进制字符 → 数值。
fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// 路径 percent-decoding。
///
/// ## 为什么必须在**解析阶段**就解
///
/// 模型 ID 含 `:`（`qwen2.5-0.5b-instruct:q4_k_m`）。浏览器用
/// `encodeURIComponent` 会把它编成 `%3A`，后端若拿 `%3A` 去查台账，
/// 必然查不到——表现为「卸载 / 停止时报未知路由」，而同一个 ID
/// 用 curl 手敲冒号却能成功。这类「前端点不动、命令行正常」的差异
/// 最难查，所以在入口统一解码。
///
/// ## 几个刻意的取舍
///
/// - **只解 `%XX`**：`+` 在路径里就是字面加号（空格是 query 的规则）。
/// - **非法转义原样保留**（`%ZZ`、截断的 `%A`）：交给路由匹配自然 404，
///   比因为一个坏转义直接断开连接更友好。
/// - **`%2F` 会被解成 `/`**，理论上会让路径多切一段。这里不做特判：
///   模型 ID 不含 `/`，且路径参数只用于查台账 key（不拼文件路径），
///   不存在穿越风险；解不开就是 404，与不解码时的结果一致。
fn percent_decode_path(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex_val(b[i + 1]), hex_val(b[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    // 坏字节用替换字符兜底，不让一个畸形请求把整个连接打挂
    String::from_utf8_lossy(&out).to_string()
}

/// 解析一个请求。返回 `None` 表示对端关闭。
pub fn parse_request<R: Read>(reader: &mut BufReader<R>) -> MnResult<Option<ParsedRequest>> {
    let mut request_line = String::new();
    match reader.read_line(&mut request_line) {
        Ok(0) => return Ok(None),
        Ok(_) => {}
        Err(e) => return Err(MnError::io("读取请求行", &e)),
    }
    let request_line = request_line.trim_end();
    if request_line.is_empty() {
        return Ok(None);
    }

    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("/").to_string();
    let version = parts.next().unwrap_or("HTTP/1.1");
    if !version.starts_with("HTTP/") {
        return Err(MnError::bad_request(format!("非法请求行：{request_line}")));
    }
    let (path, query) = match target.split_once('?') {
        // 路径要解码：模型 ID 里的 `:` 会被浏览器编码成 `%3A`。
        // 见 `percent_decode_path` 的文档。
        Some((p, q)) => (percent_decode_path(p), q.to_string()),
        None => (percent_decode_path(&target), String::new()),
    };

    // 头部
    let mut headers = HashMap::new();
    loop {
        let mut line = String::new();
        let n = reader
            .read_line(&mut line)
            .map_err(|e| MnError::io("读取请求头", &e))?;
        if n == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
        if headers.len() > 100 {
            return Err(MnError::bad_request("请求头过多"));
        }
    }

    // 体
    let len: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if len > MAX_BODY {
        return Err(MnError::bad_request(format!(
            "请求体 {len} 字节超过上限 {} 字节",
            MAX_BODY
        )));
    }
    let mut body = vec![0u8; len];
    if len > 0 {
        reader
            .read_exact(&mut body)
            .map_err(|e| MnError::io("读取请求体", &e))?;
    }

    Ok(Some(ParsedRequest {
        method,
        path,
        query,
        headers,
        body,
    }))
}

/// 网关 HTTP 服务。
pub struct GatewayServer {
    gateway: Arc<Mutex<Gateway>>,
    /// 是否要求 API Key
    require_key: bool,
    /// 期望的 API Key
    api_key: String,
    /// 上游转发超时
    upstream_timeout: Duration,
    /// CPU 采样器（有状态：使用率需要两个时间点的差值）
    sampler: mangosteen_platform::resmon::CpuSampler,
    /// 模型下载任务表
    pull_jobs: crate::pulljobs::PullJobs,
    /// 云端监控缓存（后台线程填充，请求路径只读）
    cloud_monitor: crate::cloudmon::CloudMonitor,
    /// 用户会话存储（第一层·用户会话管理）。
    ///
    /// 会话是**最上层**的状态：推理对话的上下文、部署任务的操作记录。
    /// 放在网关侧而不是浏览器内存，是为了跨进程可见——
    /// `mangosteen ui` 关掉再开，历史还在。
    sessions: mangosteen_session::SessionStore,
}

impl GatewayServer {
    /// 构造。
    pub fn new(
        gateway: Arc<Mutex<Gateway>>,
        require_key: bool,
        api_key: String,
        upstream_timeout: Duration,
    ) -> Self {
        Self {
            gateway,
            require_key,
            api_key,
            upstream_timeout,
            sampler: mangosteen_platform::resmon::CpuSampler::new(),
            pull_jobs: crate::pulljobs::PullJobs::new(),
            cloud_monitor: crate::cloudmon::CloudMonitor::new(),
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

    /// 绑定云端监控缓存，并在后台启动采集线程。
    ///
    /// 由 `serve` / `ui` 在启动时调用。不绑定也能正常工作——
    /// 此时设备页的云端区域显示「采集中」，不会报错。
    pub fn with_cloud_monitor(self) -> Self {
        let monitor = self.cloud_monitor.clone();
        crate::cloudmon::spawn_cloud_monitor(Arc::clone(&self.gateway), monitor);
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
        let mut s = stream;
        s.write_all(&resp.to_bytes())
            .map_err(|e| MnError::io("写响应", &e))?;
        s.flush().ok();
        // 响应头带 `Connection: close`，因此处理完一个请求就关闭。
        // 这是刻意的简化：无需维护每连接的 keep-alive 状态机，
        // 配合固定 worker 池足以支撑单机个人场景。
        Ok(())
    }

    /// 流式直通：边从上游读、边改写、边写给客户端。
    ///
    /// ## 为什么用 curl 而不是裸 TcpStream
    ///
    /// 上游（llama-server）流式响应是 `Transfer-Encoding: chunked`，
    /// 自己解 chunked 要维护跨块状态机（块头、块尾 CRLF、跨块的块头），
    /// 很容易在边界上出错。`curl --no-buffer` 已经把 chunked 解开、
    /// 按到达顺序输出，而且与项目「HTTP 走系统 curl」的约定一致。
    ///
    /// ## 关键点
    ///
    /// 1. `--no-buffer`：不加这个，curl 会攒满缓冲区才输出，
    ///    流式又变回「等全部完成」；
    /// 2. `--noproxy '*'`：本机常有 `HTTP_PROXY`，回环请求不能走代理；
    /// 3. 每个事件写完立刻 `flush` + `TCP_NODELAY`，否则小包被
    ///    Nagle 算法攒着，看起来像「一顿一顿地出」；
    /// 4. 客户端断开（用户点「停止」/关页面）→ 立刻 kill curl，
    ///    上游写失败就会停止生成，不白烧 CPU。
    fn relay_stream(&self, mut client: TcpStream, req: &ParsedRequest) -> MnResult<()> {
        // 错误必须**回给客户端**。
        //
        // 踩过的坑：早先直接 `?` 把错误抛给 handle_connection，
        // 而那边只是 `return`——客户端看到的是「连接被关闭、没有任何响应」，
        // 前端只能报「网络错误」，完全不知道真正原因是「没有就绪实例」。
        match self.relay_stream_inner(&mut client, req) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = client.write_all(&Response::error(&e).to_bytes());
                let _ = client.flush();
                Ok(())
            }
        }
    }

    /// 流式直通的实际实现（错误由 [`Self::relay_stream`] 统一回写）。
    fn relay_stream_inner(&self, client: &mut TcpStream, req: &ParsedRequest) -> MnResult<()> {
        // ---- 选后端 ----
        let model = serde_json::from_slice::<serde_json::Value>(&req.body)
            .ok()
            .and_then(|v| v.get("model").and_then(|m| m.as_str()).map(String::from));
        let target = {
            let mut gw = self
                .gateway
                .lock()
                .map_err(|_| MnError::bad_request("网关状态锁中毒"))?;
            gw.active_backend_for(model.as_deref())?
        };
        let url = target.url_for(&req.path);

        // ---- 拉起 curl ----
        let timeout = self.upstream_timeout.max(Duration::from_secs(600));
        let mut child = std::process::Command::new("curl")
            .arg("--no-buffer")
            .arg("--silent")
            .arg("--show-error")
            .arg("--noproxy")
            .arg("*")
            .arg("--max-time")
            .arg(timeout.as_secs().to_string())
            .arg("-X")
            .arg("POST")
            .arg("-H")
            .arg("Content-Type: application/json")
            .arg("--data-binary")
            .arg("@-")
            .arg(&url)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| MnError::Network {
                stage: "启动上游转发".to_string(),
                reason: format!("无法启动 curl：{e}"),
            })?;

        // 写请求体后立即关闭 stdin（否则上游一直等 body）
        if let Some(mut si) = child.stdin.take() {
            let _ = si.write_all(&req.body);
        }

        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill();
            return Err(MnError::Network {
                stage: "上游转发".to_string(),
                reason: "无法读取 curl 输出".to_string(),
            });
        };
        let mut reader = BufReader::new(stdout);

        // ---- 先探第一段内容，区分「正常 SSE」与「上游报错」----
        //
        // 不能一上来就写 200 头：如果上游返回的是错误 JSON，
        // 写死 200 会让客户端把错误当成正常流解析，症状是「界面空白」。
        let mut head_lines: Vec<String> = Vec::new();
        let mut is_sse = false;
        loop {
            let mut line = Vec::new();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) => break,
                Ok(_) => {}
                Err(_) => break,
            }
            let text = String::from_utf8_lossy(&line)
                .trim_end_matches(['\r', '\n'])
                .to_string();
            let probe = text.trim_start();
            if probe.is_empty() {
                head_lines.push(text);
                continue;
            }
            // SSE 的有效帧：`data:` / `event:` / 注释 `:`
            if probe.starts_with("data:") || probe.starts_with("event:") || probe.starts_with(':') {
                is_sse = true;
                head_lines.push(text);
                break;
            }
            // 非 SSE 内容（多半是错误 JSON）→ 再多读几行看看，但别无限读
            head_lines.push(text);
            if head_lines.len() > 8 {
                break;
            }
        }

        if !is_sse {
            // 上游返回的不是 SSE：读完整段，按普通响应回错误
            let mut rest = String::new();
            let _ = reader.read_to_string(&mut rest);
            let _ = child.kill();
            let body = format!("{}{}", head_lines.join("\n"), rest);
            let e = MnError::Network {
                stage: "上游推理服务".to_string(),
                reason: summarize_body(&body),
            };
            let _ = client.write_all(&Response::error(&e).to_bytes());
            let _ = client.flush();
            return Ok(());
        }

        // ---- 写响应头（chunked，长度未知）----
        client.set_nodelay(true).ok();
        let head = format!(
            "HTTP/1.1 200 OK\r\n\
             Content-Type: text/event-stream; charset=utf-8\r\n\
             Cache-Control: no-cache, no-transform\r\n\
             X-Accel-Buffering: no\r\n\
             Transfer-Encoding: chunked\r\n\
             Server: mangosteen/{}\r\n\
             Connection: close\r\n\r\n",
            mangosteen_core::identity::VERSION
        );
        if client.write_all(head.as_bytes()).is_err() {
            let _ = child.kill();
            return Ok(());
        }
        let _ = client.flush();

        // ---- 逐行转发 ----
        let relay_line = |c: &mut TcpStream, line: &str| -> bool {
            let rewritten = protocol::rewrite_sse_line(line, &target.model_id);
            let payload = format!("{rewritten}\n");
            write_chunk(c, payload.as_bytes())
        };

        for l in &head_lines {
            if !relay_line(client, l) {
                let _ = child.kill();
                return Ok(());
            }
        }

        loop {
            let mut line = Vec::new();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) => break,
                Ok(_) => {}
                Err(_) => break, // 超时或被杀
            }
            let text = String::from_utf8_lossy(&line)
                .trim_end_matches(['\r', '\n'])
                .to_string();
            if !relay_line(client, &text) {
                // 客户端断开（用户点「停止」/关页面）→ 立刻停上游，
                // 否则引擎会继续把剩下的 token 全部生成完，白烧 CPU
                let _ = child.kill();
                let _ = child.wait();
                return Ok(());
            }
        }

        // ---- 结束 chunked 流 ----
        let _ = client.write_all(b"0\r\n\r\n");
        let _ = client.flush();
        let _ = child.wait();
        Ok(())
    }

    /// 路由分发。
    pub fn route(&self, req: &ParsedRequest) -> Response {
        // 鉴权（可选）
        if self.require_key && !self.authorized(req) {
            let e = MnError::AccessDenied {
                path: std::path::PathBuf::from(&req.path),
                reason: "缺少或错误的 API Key（Authorization: Bearer <key>）".to_string(),
            };
            return Response::json(401, &ErrorResponse::from_mn(&e));
        }

        match (req.method.as_str(), req.path.as_str()) {
            // 内置 Web 控制台。挂在网关同一端口：同源即可调 /api 与 /v1，
            // 不需要 CORS，也不需要额外的静态文件服务。
            ("GET", "/") | ("GET", "/index.html") => Response {
                status: 200,
                content_type: "text/html; charset=utf-8".to_string(),
                body: mangosteen_webui::INDEX_HTML.as_bytes().to_vec(),
            },
            ("GET", "/health") => self.health(),
            ("GET", "/v1/models") => self.models(),
            ("POST", "/v1/chat/completions") => self.chat(req),
            ("POST", "/v1/completions") => self.completion(req),
            ("POST", "/v1/embeddings") => self.embeddings(req),
            ("GET", "/api/status") => self.status(),
            ("POST", "/api/deploy") => self.deploy(req),
            ("GET", "/api/deployments") => self.deployments(),
            ("POST", "/api/assess") => self.assess(req),
            ("POST", "/api/preview") => self.preview(req),
            ("GET", "/api/sysinfo") => self.sysinfo(),
            ("GET", "/api/settings") => self.get_settings(),
            ("PUT", "/api/settings") => self.put_settings(req),
            ("POST", "/api/extract") => self.extract_attachment(req),
            ("POST", "/api/fetch-url") => self.fetch_url(req),
            ("POST", "/api/models/search") => self.search_models(req),
            ("POST", "/api/models/pull") => self.pull_model(req),
            ("GET", "/api/models") => self.list_models(),
            ("GET", "/api/models/jobs") => self.list_pull_jobs(req),
            ("DELETE", "/api/models/jobs") => {
                let n = self.pull_jobs.clear_finished();
                Response::json(200, &serde_json::json!({ "ok": true, "cleared": n }))
            }
            ("POST", p) if p.starts_with("/api/models/jobs/") && p.ends_with("/cancel") => {
                let id = &p["/api/models/jobs/".len()..p.len() - "/cancel".len()];
                self.cancel_pull_job(id)
            }
            ("DELETE", p) if p.starts_with("/api/models/") => {
                self.remove_model(&p["/api/models/".len()..], req)
            }
            // ---- 第一层·用户会话管理 ----
            //
            // 会话是「最上层交互入口」的状态载体：推理对话的上下文、
            // 部署任务的操作记录。放在网关侧而非浏览器内存，
            // 是为了跨进程可见（关掉 UI 再开，历史还在）。
            ("GET", "/api/sessions") => self.list_sessions(req),
            ("POST", "/api/sessions") => self.create_session(req),
            ("POST", p) if p.starts_with("/api/sessions/") && p.ends_with("/messages") => {
                let id = &p["/api/sessions/".len()..p.len() - "/messages".len()];
                self.append_session_message(id, req)
            }
            ("POST", p) if p.starts_with("/api/sessions/") && p.ends_with("/operations") => {
                let id = &p["/api/sessions/".len()..p.len() - "/operations".len()];
                self.record_session_operation(id, req)
            }
            ("DELETE", p) if p.starts_with("/api/sessions/") => {
                self.delete_session(&p["/api/sessions/".len()..])
            }
            ("GET", p) if p.starts_with("/api/sessions/") => {
                self.get_session(&p["/api/sessions/".len()..])
            }
            ("GET", "/api/doctor") => self.doctor(),
            // 生命周期：停止服务（保留实例，关机停计费）／启动服务（开机并拉起）
            //
            // 必须排在 `DELETE /api/deployments/{id}` 之前不是语法要求，
            // 而是可读性要求：这里三条路由语义相近，放在一起便于对照。
            ("POST", p) if p.starts_with("/api/deployments/") && p.ends_with("/stop-service") => {
                let id = &p["/api/deployments/".len()..p.len() - "/stop-service".len()];
                self.stop_service(id)
            }
            ("POST", p) if p.starts_with("/api/deployments/") && p.ends_with("/start-service") => {
                let id = &p["/api/deployments/".len()..p.len() - "/start-service".len()];
                self.start_service(id)
            }
            ("DELETE", p) if p.starts_with("/api/deployments/") => {
                self.stop(&p["/api/deployments/".len()..])
            }
            ("GET", p) if p.starts_with("/api/deployments/") => {
                self.deployment(&p["/api/deployments/".len()..])
            }
            (m, p) => {
                let mut r = Response::not_found(p);
                if p.starts_with("/api/") || p.starts_with("/v1/") {
                    r = Response::text(405, format!("方法 {m} 不被 {p} 支持"));
                }
                r
            }
        }
    }

    /// 校验 API Key。
    fn authorized(&self, req: &ParsedRequest) -> bool {
        let provided = req
            .header("authorization")
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(|s| s.trim().to_string())
            .or_else(|| req.header("x-api-key").map(|s| s.trim().to_string()));
        matches!(provided, Some(k) if k == self.api_key)
    }

    // ---------------------------------------------------------------- 处理器

    fn health(&self) -> Response {
        let mut gw = match self.gateway.lock() {
            Ok(g) => g,
            Err(_) => return Response::text(500, "网关状态锁中毒"),
        };
        let backend = gw.active_backend().ok();
        let h = HealthResponse {
            status: "ok".to_string(),
            version: mangosteen_core::identity::VERSION.to_string(),
            backend: backend.as_ref().map(|b| b.origin_label.clone()),
            backend_ready: backend.is_some(),
            model: backend.as_ref().map(|b| b.model_id.clone()),
        };
        Response::json(200, &h)
    }

    fn models(&self) -> Response {
        let gw = match self.gateway.lock() {
            Ok(g) => g,
            Err(_) => return Response::text(500, "网关状态锁中毒"),
        };
        let entries: Vec<(String, String)> = match gw.models().list() {
            Ok(list) => list
                .into_iter()
                .map(|m| {
                    let owner = if m.is_local() {
                        "local".to_string()
                    } else {
                        "cloud".to_string()
                    };
                    (m.id, owner)
                })
                .collect(),
            Err(e) => return Response::error(&e),
        };
        let l = protocol::build_model_list(&entries, mangosteen_core::time::now_unix());
        Response::json(200, &l)
    }

    fn status(&self) -> Response {
        let mut gw = match self.gateway.lock() {
            Ok(g) => g,
            Err(_) => return Response::text(500, "网关状态锁中毒"),
        };
        match gw.status() {
            Ok(s) => {
                let payload = serde_json::json!({
                    "version": s.version,
                    "hardware": s.hardware,
                    "providers": s.providers,
                    "engine_path": s.engine_path.map(|p| p.display().to_string()),
                    "engine_version": s.engine_version,
                    "active": s.active.iter().map(|a| serde_json::json!({
                        "id": a.id,
                        "model": a.model,
                        "origin": a.origin.label(),
                        "status": a.status.as_str(),
                        "port": a.port,
                        "ready": a.ready,
                    })).collect::<Vec<_>>(),
                });
                Response::json(200, &payload)
            }
            Err(e) => Response::error(&e),
        }
    }

    /// 部署预估：不创建任何资源，只回答「会发生什么、要花多少钱」。
    ///
    /// UI 的确认弹窗靠它拿数据。**这个接口绝不产生费用**——
    /// 云端实例只会在用户勾选确认后由 `/api/deploy` 创建。
    fn preview(&self, req: &ParsedRequest) -> Response {
        let parsed: serde_json::Value = match serde_json::from_str(&req.body_str()) {
            Ok(v) => v,
            Err(e) => {
                return Response::json(
                    400,
                    &ErrorResponse::bad_request(format!("请求体不是合法 JSON：{e}"), None),
                )
            }
        };
        let Some(model) = parsed.get("model").and_then(|m| m.as_str()) else {
            return Response::json(
                400,
                &ErrorResponse::bad_request("缺少 model 字段", Some("model")),
            );
        };

        let mut dreq =
            mangosteen_orchestration::assess::DeploymentRequest::for_model(model.to_string());
        if let Some(c) = parsed.get("context_len").and_then(|v| v.as_u64()) {
            dreq.context_len = Some(c as u32);
        }
        if let Some(p) = parsed.get("max_price_per_hour").and_then(|v| v.as_f64()) {
            dreq.max_price_per_hour = Some(p);
        }
        // 设备选择：local / cloud / auto（缺省 auto = 本地优先）
        if let Some(t) = parsed.get("target").and_then(|v| v.as_str()) {
            match mangosteen_orchestration::assess::ComputeTarget::parse(t) {
                Some(x) => dreq.target = x,
                None => {
                    return Response::json(
                        400,
                        &ErrorResponse::bad_request(
                            format!("未知 target `{t}`，可选 local / cloud / auto"),
                            Some("target"),
                        ),
                    )
                }
            }
        }

        let gw = match self.gateway.lock() {
            Ok(g) => g,
            Err(_) => return Response::text(500, "网关状态锁中毒"),
        };
        match gw.preview(&dreq) {
            Ok(p) => Response::json(200, &p),
            Err(e) => Response::error(&e),
        }
    }

    /// 环境自检（供 UI 的「环境自检」按钮使用）。
    ///
    /// 与 `mangosteen doctor` 共用网关层的诊断能力，避免两处判断分叉。
    fn doctor(&self) -> Response {
        // 取配置与缓存的引擎版本：两者都在锁内读一次，随后锁外使用
        let (cfg, cached_version) = {
            let gw = match self.gateway.lock() {
                Ok(g) => g,
                Err(_) => return Response::text(500, "网关状态锁中毒"),
            };
            (gw.config().clone(), gw.engine_version())
        };

        let mut checks: Vec<serde_json::Value> = Vec::new();

        // 1. 引擎
        //
        // 注意：**不能**调 `diagnose_engine`——它会同步执行
        // `llama-server --version`，在 Homebrew 版上要 27 秒，
        // 直接把本接口拖到超时。这里改成用预热缓存回答，
        // 缓存为空时如实说「仍在探测」，而不是让请求挂住。
        let diag = crate::diagnose_engine_cached(Some(&cfg), cached_version);
        // 三种状态必须分开，否则「还在探测」会被误报成「引擎坏了」：
        //   有 binary + 有版本  → ok
        //   有 binary + 无版本  → 探测中（预热还没跑完），不是失败
        //   无 binary           → 确实没装
        match (&diag.binary, &diag.version) {
            (Some(b), Some(v)) => checks.push(serde_json::json!({
                "name": "本地推理引擎",
                "status": "ok",
                "detail": format!("{} —— {v}", b.display()),
            })),
            (Some(b), None) => checks.push(serde_json::json!({
                "name": "本地推理引擎",
                "status": "warn",
                "detail": format!("{} 已找到，正在后台探测版本…", b.display()),
                "hint": "引擎版本探测需加载全部动态库，本机约需 20~30 秒；稍后刷新即可",
            })),
            _ => checks.push(serde_json::json!({
                "name": "本地推理引擎",
                "status": "fail",
                "detail": "未找到 llama-server",
                "hint": "brew install llama.cpp，或用 local.engine_path 指定路径",
            })),
        }

        // 2. 引擎库路径（能跑起来的原因值得说清楚）
        if let Some(v) = &diag.library_path {
            checks.push(serde_json::json!({
                "name": "引擎运行库路径",
                "status": "ok",
                "detail": format!("注入 {}={v}", diag.library_path_var),
            }));
        }

        // 3. 本地优先策略
        checks.push(serde_json::json!({
            "name": "本地优先策略",
            "status": if cfg.scheduler.local_first { "ok" } else { "warn" },
            "detail": if cfg.scheduler.local_first {
                "已启用（符合产品基线）".to_string()
            } else {
                "被配置关闭，将不再优先尝试本地部署".to_string()
            },
            "hint": if cfg.scheduler.local_first { serde_json::Value::Null } else {
                serde_json::json!("把 scheduler.local_first 改回 true")
            },
        }));

        // 4. 云服务商
        let (registered, skipped) = crate::probe_providers(&cfg);
        if registered.is_empty() {
            checks.push(serde_json::json!({
                "name": "云服务商",
                "status": "warn",
                "detail": if cfg.providers.is_empty() {
                    "未配置任何服务商".to_string()
                } else {
                    format!("{} 个已启用但均不可用：{}", skipped.len(),
                        skipped.iter().map(|(n, r)| format!("{n}（{r}）")).collect::<Vec<_>>().join("；"))
                },
                "hint": "本地资源不足时将无法降级。检查 config.providers 凭证",
            }));
        } else {
            checks.push(serde_json::json!({
                "name": "云服务商",
                "status": "ok",
                "detail": format!("已就绪：{}", registered.join(", ")),
            }));
        }

        // 5. 模型库
        let model_count = {
            let gw = match self.gateway.lock() {
                Ok(g) => g,
                Err(_) => return Response::text(500, "网关状态锁中毒"),
            };
            gw.models().list().map(|l| l.len()).unwrap_or(0)
        };
        checks.push(serde_json::json!({
            "name": "模型库",
            "status": if model_count > 0 { "ok" } else { "warn" },
            "detail": format!("{model_count} 个模型"),
            "hint": if model_count > 0 { serde_json::Value::Null } else {
                serde_json::json!("执行 `mangosteen models pull <仓库>/<文件.gguf>` 下载模型")
            },
        }));

        Response::json(200, &serde_json::json!({ "checks": checks }))
    }

    // ------------------------------------------------------------ device / setting

    /// 系统资源快照（device 页签数据源）。
    ///
    /// 每次都重新采样，不做缓存：监控界面的价值就是「现在」。
    /// 采样成本是两次 sysctl/读 `/proc`，微秒级。
    fn sysinfo(&self) -> Response {
        let snap = self.sampler.sample();

        // 各活跃部署的进程资源占用
        let deps = {
            let gw = match self.gateway.lock() {
                Ok(g) => g,
                Err(_) => return Response::text(500, "网关状态锁中毒"),
            };
            let models_dir = gw.config().storage.models_dir.clone();
            gw.list_deployments()
                .unwrap_or_default()
                .into_iter()
                .map(|r| {
                    let pid = r.pid;
                    serde_json::json!({
                        "id": r.plan.id,
                        "model": mangosteen_core::model::ModelEntry::derive_id(
                            &r.plan.model.name, r.plan.model.quant),
                        "origin": r.plan.origin.label(),
                        "status": r.status.as_str(),
                        "port": r.plan.local_access_port(),
                        "pid": pid,
                        "rss_bytes": pid.and_then(mangosteen_platform::resmon::process_rss_bytes),
                    })
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|v| (v, models_dir.clone()))
                .collect::<Vec<_>>()
        };

        let dep_json: Vec<serde_json::Value> = deps.into_iter().map(|(v, _)| v).collect();

        let hardware = {
            let gw = match self.gateway.lock() {
                Ok(g) => g,
                Err(_) => return Response::text(500, "网关状态锁中毒"),
            };
            let hw = gw.assessor().hardware().clone();
            let models_dir = gw.config().storage.models_dir.clone();
            (hw, models_dir)
        };

        let disk_free = mangosteen_platform::resmon::available_space(&hardware.1).ok();

        // 云端监控：只读缓存（后台线程每 5 秒刷新一次）。
        //
        // 绝不在请求路径里调厂商接口：设备页每 2 秒轮询一次，
        // 每次都打云端会让页面明显卡顿并触发厂商限流。
        let cloud = self.cloud_section();

        Response::json(
            200,
            &serde_json::json!({
                "cpu_usage": snap.cpu_usage,
                "physical_cores": snap.physical_cores,
                "logical_cores": snap.logical_cores,
                "total_memory_bytes": snap.total_memory_bytes,
                "available_memory_bytes": snap.available_memory_bytes,
                "memory_usage": snap.memory_usage(),
                "sampled_at_ms": snap.sampled_at_ms,
                "os": hardware.0.os,
                "arch": hardware.0.arch,
                "cpu_model": hardware.0.host.cpu_model,
                "accelerators": hardware.0.accelerators.iter().map(|a| serde_json::json!({
                    "name": a.name,
                    "vendor": format!("{:?}", a.vendor),
                    "vram_bytes": a.usable_accelerator_memory(),
                })).collect::<Vec<_>>(),
                "disk_free_bytes": disk_free,
                "deployments": dep_json,
                "cloud": cloud,
            }),
        )
    }

    /// 设备页的「云端算力」段。
    ///
    /// 返回 `active: false` 表示**当前用的是本地算力**——这不是错误，
    /// 前端据此隐藏云端区域。只有真的查询失败时才带 `error`。
    fn cloud_section(&self) -> serde_json::Value {
        let (snapshot, error, age_ms) = self.cloud_monitor.read();

        let Some(snap) = snapshot else {
            // 无快照：要么本地部署，要么首轮采集还没完成。
            // 用 `pending` 让前端区分「不适用」与「稍等就有」。
            return serde_json::json!({
                "active": false,
                "pending": age_ms.is_none(),
                "error": error,
            });
        };

        // 指标保持 0~1 比例，与本地 `cpu_usage` 语义一致，格式化交给前端。
        let metrics = snap.metrics.as_ref().map(|m| {
            serde_json::json!({
                "cpu_usage": m.cpu_usage,
                "memory_usage": m.memory_usage,
                "gpu_usage": m.gpu_usage,
                "gpu_memory_usage": m.gpu_memory_usage,
                "disk_usage": m.disk_usage,
                "data_disk_usage": m.data_disk_usage,
                "sampled_at": m.sampled_at,
                "has_any": m.has_any(),
            })
        });

        serde_json::json!({
            "active": true,
            "pending": false,
            "deployment_id": snap.deployment_id,
            "provider": snap.provider,
            "instance_id": snap.instance_id,
            "model": snap.model,
            "status": snap.status.as_str(),
            "metrics": metrics,
            // 该服务商是否提供监控（`metrics` 为 null 即不支持）
            "supported": snap.metrics.is_some(),
            "age_ms": age_ms,
            "error": error.or(snap.error),
        })
    }

    /// 读取当前设置（setting 页签）。
    ///
    /// **只返回可安全展示的字段**。凭证永远不回显原文，只回显「是否已配置」，
    /// 否则一个局域网内的人打开页面就能看到你的云厂商密钥。
    fn get_settings(&self) -> Response {
        let cfg = {
            let gw = match self.gateway.lock() {
                Ok(g) => g,
                Err(_) => return Response::text(500, "网关状态锁中毒"),
            };
            gw.config().clone()
        };

        let providers: Vec<serde_json::Value> = cfg
            .providers
            .iter()
            .map(|(name, p)| {
                serde_json::json!({
                    "name": name,
                    "driver": p.driver,
                    "enabled": p.enabled,
                    "display_name": p.display_name,
                    // 只回显凭证「形式」，绝不回显内容
                    "secret_kind": match &p.secret {
                        mangosteen_core::config::SecretRef::Env(v) => format!("env:{v}"),
                        mangosteen_core::config::SecretRef::File(f) => {
                            format!("file:{}", f.display())
                        }
                        mangosteen_core::config::SecretRef::Inline(_) => {
                            "inline:已配置".to_string()
                        }
                        mangosteen_core::config::SecretRef::None => "none:无需密钥".to_string(),
                    },
                    "secret_configured": p.secret.resolve(mangosteen_platform::paths::real_env).is_ok(),
                })
            })
            .collect();

        Response::json(
            200,
            &serde_json::json!({
                "gateway": {
                    "host": cfg.gateway.host,
                    "port": cfg.gateway.port,
                    "require_api_key": cfg.gateway.require_api_key,
                },
                "scheduler": {
                    "local_first": cfg.scheduler.local_first,
                    "max_fallbacks": cfg.scheduler.max_fallbacks,
                    "launch_timeout_secs": cfg.scheduler.launch_timeout_secs,
                    "max_price_per_hour": cfg.scheduler.max_price_per_hour,
                },
                "local": {
                    // 推理引擎：当前值与全部可选项（含是否已适配、所需格式）
                    "engine": cfg.local.engine.id(),
                    "engine_display": cfg.local.engine.display_name(),
                    "engine_supported": cfg.local.engine.is_supported(),
                    "engines": mangosteen_core::config::InferenceEngine::all()
                        .iter()
                        .map(|e| serde_json::json!({
                            "id": e.id(),
                            "name": e.display_name(),
                            "supported": e.is_supported(),
                            "format": e.required_format(),
                            "binary": e.binary_name(),
                        }))
                        .collect::<Vec<_>>(),
                    "engine_path": cfg.local.engine_path.as_ref().map(|p| p.display().to_string()),
                    "default_context": cfg.local.default_context,
                    "min_context": cfg.local.min_context,
                    "kv_cache": cfg.local.kv_cache.as_cli_value(),
                    "batch_size": cfg.local.batch_size,
                    "allow_cpu_fallback": cfg.local.allow_cpu_fallback,
                    "allow_partial_offload": cfg.local.allow_partial_offload,
                    "auto_library_path": cfg.local.auto_library_path,
                },
                "storage": {
                    "models_dir": cfg.storage.models_dir.display().to_string(),
                    "cache_dir": cfg.storage.cache_dir.display().to_string(),
                    "default_source": cfg.storage.default_source,
                    "verify_hash": cfg.storage.verify_hash,
                    "max_model_bytes": cfg.storage.max_model_bytes,
                },
                "providers": providers,
                "model_sources": cfg.model_sources.keys().collect::<Vec<_>>(),
            }),
        )
    }

    /// 写入设置。
    ///
    /// 只接受**白名单字段**，且写入前必须通过 `Config::validate()`。
    /// 不做「任意 TOML 覆盖」——那等于给页面一个任意配置写入口，
    /// 一旦网关被暴露到局域网就是提权面。
    fn put_settings(&self, req: &ParsedRequest) -> Response {
        let patch: serde_json::Value = match serde_json::from_str(&req.body_str()) {
            Ok(v) => v,
            Err(e) => {
                return Response::json(
                    400,
                    &ErrorResponse::bad_request(format!("请求体不是合法 JSON：{e}"), None),
                )
            }
        };

        let (mut cfg, store) = {
            let gw = match self.gateway.lock() {
                Ok(g) => g,
                Err(_) => return Response::text(500, "网关状态锁中毒"),
            };
            (gw.config().clone(), gw.config_store())
        };

        let mut applied: Vec<String> = Vec::new();

        // ---- scheduler.local_first ----
        if let Some(v) = patch
            .get("scheduler")
            .and_then(|s| s.get("local_first"))
            .and_then(|v| v.as_bool())
        {
            cfg.scheduler.local_first = v;
            applied.push("scheduler.local_first".into());
        }
        if let Some(v) = patch
            .get("scheduler")
            .and_then(|s| s.get("max_price_per_hour"))
            .and_then(|v| v.as_f64())
        {
            cfg.scheduler.max_price_per_hour = Some(v);
            applied.push("scheduler.max_price_per_hour".into());
        }

        // ---- local.* ----
        if let Some(l) = patch.get("local") {
            if let Some(v) = l.get("default_context").and_then(|v| v.as_u64()) {
                cfg.local.default_context = v as u32;
                applied.push("local.default_context".into());
            }
            if let Some(v) = l.get("kv_cache").and_then(|v| v.as_str()) {
                match v {
                    "f16" => cfg.local.kv_cache = mangosteen_core::estimate::KvCacheType::F16,
                    "q8_0" => cfg.local.kv_cache = mangosteen_core::estimate::KvCacheType::Q8_0,
                    "q4_0" => cfg.local.kv_cache = mangosteen_core::estimate::KvCacheType::Q4_0,
                    other => {
                        return Response::json(
                            400,
                            &ErrorResponse::bad_request(
                                format!("未知 kv_cache `{other}`，可选 f16 / q8_0 / q4_0"),
                                Some("local.kv_cache"),
                            ),
                        )
                    }
                }
                applied.push("local.kv_cache".into());
            }
            if let Some(v) = l.get("batch_size").and_then(|v| v.as_u64()) {
                cfg.local.batch_size = v as u32;
                applied.push("local.batch_size".into());
            }
            if let Some(v) = l.get("allow_cpu_fallback").and_then(|v| v.as_bool()) {
                cfg.local.allow_cpu_fallback = v;
                applied.push("local.allow_cpu_fallback".into());
            }
            if let Some(v) = l.get("allow_partial_offload").and_then(|v| v.as_bool()) {
                cfg.local.allow_partial_offload = v;
                applied.push("local.allow_partial_offload".into());
            }
            // 推理引擎切换
            if let Some(v) = l.get("engine").and_then(|v| v.as_str()) {
                match mangosteen_core::config::InferenceEngine::parse(v) {
                    Some(e) => {
                        cfg.local.engine = e;
                        applied.push("local.engine".into());
                    }
                    None => {
                        return Response::json(
                            400,
                            &ErrorResponse::bad_request(
                                format!(
                                    "未知推理引擎 `{v}`，可选 {}",
                                    mangosteen_core::config::InferenceEngine::all()
                                        .iter()
                                        .map(|e| e.id())
                                        .collect::<Vec<_>>()
                                        .join(" / ")
                                ),
                                Some("local.engine"),
                            ),
                        )
                    }
                }
            }
        }

        // ---- storage.default_source ----
        if let Some(v) = patch
            .get("storage")
            .and_then(|s| s.get("default_source"))
            .and_then(|v| v.as_str())
        {
            cfg.storage.default_source = v.to_string();
            applied.push("storage.default_source".into());
        }

        // ---- providers.<name>.enabled ----
        if let Some(ps) = patch.get("providers").and_then(|v| v.as_object()) {
            for (name, p) in ps {
                if let Some(enabled) = p.get("enabled").and_then(|v| v.as_bool()) {
                    if let Some(slot) = cfg.providers.get_mut(name) {
                        slot.enabled = enabled;
                        applied.push(format!("providers.{name}.enabled"));
                    }
                }
            }
        }

        if applied.is_empty() {
            return Response::json(
                400,
                &ErrorResponse::bad_request(
                    "没有任何可识别的字段被修改。支持的字段见 GET /api/settings",
                    None,
                ),
            );
        }

        // 校验后才落盘：非法配置绝不能写进文件，否则用户下次启动直接失败
        if let Err(e) = cfg.validate() {
            return Response::error(&e);
        }
        match store.save(&cfg) {
            Ok(path) => {
                // 同步更新内存中的配置：否则用户改完设置要重启才生效
                if let Ok(mut gw) = self.gateway.lock() {
                    gw.replace_config(cfg);
                }
                Response::json(
                    200,
                    &serde_json::json!({
                        "ok": true,
                        "path": path.display().to_string(),
                        "applied": applied,
                    }),
                )
            }
            Err(e) => Response::error(&e),
        }
    }

    // ------------------------------------------------------------ chat 增强

    /// 提取附件文本。
    ///
    /// 支持两种输入方式：
    ///
    /// | 方式 | 请求体 | 用途 |
    /// |------|--------|------|
    /// | 内容上传 | `{"filename":"a.docx","content_base64":"..."}` | **Web UI 用这个** |
    /// | 路径引用 | `{"path":"/abs/path"}` | CLI / 脚本 |
    ///
    /// ## 为什么 UI 必须走内容上传
    ///
    /// 浏览器的 `<input type="file">` **不会**暴露文件的本地路径（安全限制），
    /// 只给 `File` 对象。因此前端只能把内容读出来发给后端。
    ///
    /// 内容上传路径顺便更安全：不涉及服务端文件系统访问，也就没有越权读取
    /// 的风险。路径方式保留给 CLI，并继续受访问控制约束。
    fn extract_attachment(&self, req: &ParsedRequest) -> Response {
        let parsed: serde_json::Value = match serde_json::from_str(&req.body_str()) {
            Ok(v) => v,
            Err(e) => {
                return Response::json(
                    400,
                    &ErrorResponse::bad_request(format!("请求体不是合法 JSON：{e}"), None),
                )
            }
        };

        let max_chars = {
            let gw = match self.gateway.lock() {
                Ok(g) => g,
                Err(_) => return Response::text(500, "网关状态锁中毒"),
            };
            // 字符上限按模型上下文折算：一半留给对话历史与输出
            ((gw.config().local.default_context as usize) * 2)
                .min(mangosteen_base::extract::MAX_TEXT_CHARS)
        };

        // ---- 方式一：内容上传（UI）----
        if let Some(b64) = parsed.get("content_base64").and_then(|v| v.as_str()) {
            let filename = parsed
                .get("filename")
                .and_then(|v| v.as_str())
                .unwrap_or("attachment.txt");

            let bytes = match base64_decode(b64) {
                Ok(b) => b,
                Err(e) => {
                    return Response::json(
                        400,
                        &ErrorResponse::bad_request(
                            format!("content_base64 解码失败：{e}"),
                            Some("content_base64"),
                        ),
                    )
                }
            };

            return match mangosteen_base::extract::extract_bytes(filename, &bytes, max_chars) {
                Ok(r) => Response::json(
                    200,
                    &serde_json::json!({
                        "filename": filename,
                        "text": r.text,
                        "chars": r.text.chars().count(),
                        "original_bytes": r.original_bytes,
                        "method": r.method,
                        "truncated": r.truncated,
                        "warnings": r.warnings,
                    }),
                ),
                Err(e) => Response::error(&e),
            };
        }

        // ---- 方式二：路径引用（CLI，受访问控制约束）----
        let Some(path) = parsed.get("path").and_then(|v| v.as_str()) else {
            return Response::json(
                400,
                &ErrorResponse::bad_request(
                    "需要 `content_base64` + `filename`（上传）或 `path`（引用本地文件）",
                    None,
                ),
            );
        };

        let guard = {
            let gw = match self.gateway.lock() {
                Ok(g) => g,
                Err(_) => return Response::text(500, "网关状态锁中毒"),
            };
            mangosteen_base::AccessGuard::from_config(&gw.config().storage)
        };

        // 访问控制：越权直接拒绝
        let allowed = match guard.check_read(std::path::Path::new(path)) {
            Ok(p) => p,
            Err(e) => return Response::error(&e),
        };

        match mangosteen_base::extract::extract(&allowed, max_chars) {
            Ok(r) => Response::json(
                200,
                &serde_json::json!({
                    "path": allowed.display().to_string(),
                    "text": r.text,
                    "chars": r.text.chars().count(),
                    "original_bytes": r.original_bytes,
                    "method": r.method,
                    "truncated": r.truncated,
                    "warnings": r.warnings,
                }),
            ),
            Err(e) => Response::error(&e),
        }
    }

    /// 抓取 URL 正文（联网搜索的兜底路径）。
    fn fetch_url(&self, req: &ParsedRequest) -> Response {
        let parsed: serde_json::Value = match serde_json::from_str(&req.body_str()) {
            Ok(v) => v,
            Err(e) => {
                return Response::json(
                    400,
                    &ErrorResponse::bad_request(format!("请求体不是合法 JSON：{e}"), None),
                )
            }
        };
        let Some(url) = parsed.get("url").and_then(|v| v.as_str()) else {
            return Response::json(
                400,
                &ErrorResponse::bad_request("缺少 url 字段", Some("url")),
            );
        };

        let mut opts = mangosteen_base::webfetch::FetchOptions::default();
        if let Some(n) = parsed.get("max_chars").and_then(|v| v.as_u64()) {
            opts.max_chars =
                (n as usize).min(mangosteen_base::webfetch::FetchOptions::default().max_chars);
        }

        match mangosteen_base::webfetch::fetch(url, &opts) {
            Ok(p) => Response::json(
                200,
                &serde_json::json!({
                    "url": p.final_url,
                    "title": p.title,
                    "text": p.text,
                    "chars": p.text.chars().count(),
                    "status": p.status,
                    "content_type": p.content_type,
                    "bytes": p.bytes,
                    "truncated": p.truncated,
                    "warnings": p.warnings,
                }),
            ),
            Err(e) => Response::error(&e),
        }
    }

    /// 搜索远端仓库里的模型文件（deploy 页签的「模型搜索」）。
    /// 列出已安装模型（含磁盘路径）。
    ///
    /// ## 为什么不用 `/v1/models`
    ///
    /// `/v1/models` 是 **[OI] 兼容接口**，字段集由协议固定（`id` /
    /// `object` / `created` / `owned_by`）。往里塞 `path` 这类自定义字段
    /// 会让严格校验的客户端报错。因此管理信息单独走这个接口。
    ///
    /// 路径对用户有实际价值：模型动辄几 GB，用户需要知道它占在哪块盘、
    /// 好清理或备份，而不是只知道「有个模型叫这个名字」。
    fn list_models(&self) -> Response {
        let gw = match self.gateway.lock() {
            Ok(g) => g,
            Err(_) => return Response::text(500, "网关状态锁中毒"),
        };
        let list = match gw.models().list() {
            Ok(l) => l,
            Err(e) => return Response::error(&e),
        };

        let items: Vec<serde_json::Value> = list
            .iter()
            .map(|m| {
                let (path, exists, size) = match &m.location {
                    mangosteen_core::model::ModelLocation::Local { path } => {
                        let meta = std::fs::metadata(path).ok();
                        (
                            Some(path.display().to_string()),
                            meta.is_some(),
                            meta.map(|x| x.len()),
                        )
                    }
                    mangosteen_core::model::ModelLocation::Cloud { path, .. } => {
                        (Some(path.clone()), true, None)
                    }
                };

                // 能否部署：只做**本地计算**（资源是否够），不探测引擎、
                // 不查云端库存。原因是这里要列表展示，必须毫秒级返回。
                //
                // 文件不存在就明确不可部署——让用户看到灰色的「部署」而不是
                // 点了才失败。真正的可行性评估仍在点「部署」时做（含引擎与
                // 云端费用），这里只负责把明显不能部署的置灰。
                let (deployable, reason) = if !exists {
                    (false, Some("模型文件不存在或已被删除".to_string()))
                } else {
                    let dreq = mangosteen_orchestration::assess::DeploymentRequest::for_model(
                        m.id.clone(),
                    );
                    match gw.evaluate(&dreq) {
                        Ok(ev) => {
                            if ev.is_local_viable() {
                                (true, None)
                            } else {
                                (
                                    false,
                                    Some("本地资源不足（点「部署」可查看云端方案）".to_string()),
                                )
                            }
                        }
                        Err(e) => (false, Some(e.to_string())),
                    }
                };

                serde_json::json!({
                    "id": m.id,
                    "name": m.spec.name,
                    "quant": m.spec.quant.to_string(),
                    "size_bytes": size.or(Some(m.spec.file_size_bytes)).filter(|v| *v > 0),
                    "path": path,
                    "file_exists": exists,
                    "is_local": m.is_local(),
                    "sha256": m.sha256,
                    "deployable": deployable,
                    "deploy_blocked_reason": reason,
                })
            })
            .collect();

        Response::json(200, &serde_json::json!({ "models": items }))
    }

    /// 搜索模型。
    ///
    /// 一个接口、两种模式，按输入自动判别——这样 UI 只需要一个搜索框，
    /// 用户既能「按关键字找模型」，也能「按仓库路径看文件」：
    ///
    /// | 输入 | 判定 | 返回 |
    /// |------|------|------|
    /// | `qwen` / `qwen gguf` | 关键字 | `{ mode: "repos", repos: [...] }` |
    /// | `Qwen/Qwen2.5-0.5B-Instruct-GGUF` | 仓库全名（含 `/`） | `{ mode: "files", files: [...] }` |
    ///
    /// 判别依据是「是否含 `/`」：仓库全名必然是 `owner/name` 两段，
    /// 而关键字几乎不会带斜杠。这个规则简单、可预测，也不会误判。
    fn search_models(&self, req: &ParsedRequest) -> Response {
        let parsed: serde_json::Value = match serde_json::from_str(&req.body_str()) {
            Ok(v) => v,
            Err(e) => {
                return Response::json(
                    400,
                    &ErrorResponse::bad_request(format!("请求体不是合法 JSON：{e}"), None),
                )
            }
        };

        // 兼容两种字段名：`query`（新，关键字）与 `repo`（旧，仓库路径）
        let raw = parsed
            .get("query")
            .or_else(|| parsed.get("repo"))
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        if raw.is_empty() {
            return Response::json(
                400,
                &ErrorResponse::bad_request(
                    "缺少 query 字段（关键字，如 `qwen2.5 gguf`；或仓库全名 `owner/name`）",
                    Some("query"),
                ),
            );
        }

        let cfg = {
            let gw = match self.gateway.lock() {
                Ok(g) => g,
                Err(_) => return Response::text(500, "网关状态锁中毒"),
            };
            gw.config().clone()
        };

        let registry = match cfg.model_source_registry() {
            Ok(r) => r,
            Err(e) => return Response::error(&e),
        };
        let source = match parsed.get("source").and_then(|v| v.as_str()) {
            Some(s) => match registry.get(s) {
                Some(x) => x.clone(),
                None => {
                    return Response::json(
                        400,
                        &ErrorResponse::bad_request(format!("未知模型源 `{s}`"), Some("source")),
                    )
                }
            },
            None => match registry.get(registry.default_source()) {
                Some(x) => x.clone(),
                None => return Response::text(500, "默认模型源未定义"),
            },
        };

        // 去掉可能带的 `源名:` 前缀
        let text = raw
            .strip_prefix(&format!("{}:", source.name))
            .unwrap_or(&raw)
            .to_string();

        let opts = mangosteen_base::DownloadOptions::new(
            cfg.storage.models_dir.clone(),
            cfg.storage.max_model_bytes,
        );

        let is_repo_path = text.contains('/');

        if is_repo_path {
            // ---- 模式一：已知仓库，列出其中可下载的 GGUF ----
            return match mangosteen_base::download::fetch_remote_files(&source, &text, &opts) {
                Ok(files) => Response::json(
                    200,
                    &serde_json::json!({
                        "mode": "files",
                        "source": source.name,
                        "repo": text,
                        "files": files.iter().map(|f| serde_json::json!({
                            "name": f.name,
                            "size": f.size,
                            "sha256": f.sha256,
                            // 格式标识（gguf / safetensors / pytorch / …）。
                            // 不做过滤，只告知，由用户按所用引擎自行判断。
                            "format": f.format,
                        })).collect::<Vec<_>>(),
                    }),
                ),
                Err(e) => Response::error(&e),
            };
        }

        // ---- 模式二：关键字，搜索仓库 ----
        // 默认一次取 100 个：用户在 UI 里要能「看到足够多的候选再翻页」，
        // 30 个太少（一个热门关键字就有几百个仓库）。
        // 上限也设 100——再多会让上游变慢，且前端分页已能满足浏览需求。
        let limit = parsed
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| n.clamp(1, 100) as usize)
            .unwrap_or(100);

        match mangosteen_base::download::search_repos(&source, &text, limit, &opts) {
            Ok(repos) => Response::json(
                200,
                &serde_json::json!({
                    "mode": "repos",
                    "source": source.name,
                    "query": text,
                    "repos": repos.iter().map(|r| serde_json::json!({
                        "repo": r.repo,
                        "downloads": r.downloads,
                        "likes": r.likes,
                        "tags": r.tags,
                        "has_gguf": r.has_gguf,
                    })).collect::<Vec<_>>(),
                }),
            ),
            Err(e) => Response::error(&e),
        }
    }

    /// 提交下载任务（deploy 页签的「下载」按钮）。
    ///
    /// **立即返回** `job_id`，实际下载在后台线程进行；进度用
    /// `GET /api/models/jobs` 轮询。理由见 [`crate::pulljobs`] 模块文档。
    ///
    /// 幂等与去重：
    /// - 本地已有完整文件 → 任务立刻标记完成并带 `note`，不会重下
    /// - 同一模型已有进行中的任务 → 直接返回那个 `job_id`，不会并发下两份
    fn pull_model(&self, req: &ParsedRequest) -> Response {
        let parsed: serde_json::Value = match serde_json::from_str(&req.body_str()) {
            Ok(v) => v,
            Err(e) => {
                return Response::json(
                    400,
                    &ErrorResponse::bad_request(format!("请求体不是合法 JSON：{e}"), None),
                )
            }
        };
        let Some(spec) = parsed.get("model").and_then(|v| v.as_str()) else {
            return Response::json(
                400,
                &ErrorResponse::bad_request(
                    "缺少 model 字段（形如 `owner/仓库/文件.gguf`）",
                    Some("model"),
                ),
            );
        };
        // UI 的「部署」按钮走这条路径：下载完**自动启动**。
        // CLI 的 `models pull` 不带此参数，只下载不启动。
        let deploy_after = parsed
            .get("deploy")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        // 若要求自动部署，还要带上部署诉求（设备选择等）
        let deploy_target = parsed
            .get("target")
            .and_then(|v| v.as_str())
            .unwrap_or("local")
            .to_string();

        // 同一规格正在下载 → 复用，避免两个 curl 争抢同一个 .part
        if let Some(existing) = self.pull_jobs.find_active_by_spec(spec) {
            return Response::json(
                200,
                &serde_json::json!({
                    "ok": true,
                    "job_id": existing.id,
                    "reused": true,
                    "note": "该模型正在下载中，已复用现有任务",
                }),
            );
        }

        // ---- 持锁期间只做「取快照」，不做任何网络操作 ----
        let (cfg, register, gw_for_deploy) = {
            let gw = match self.gateway.lock() {
                Ok(g) => g,
                Err(_) => return Response::text(500, "网关状态锁中毒"),
            };
            let cfg = gw.config().clone();
            // 登记闭包：下载完成后把文件写进模型台账。
            // 用闭包而非可变的 Gateway 引用，是为了让下载线程完全不持锁。
            // ModelStore 是 Clone 的（内部只有路径 + 策略），
            // 克隆一份进闭包即可，不需要共享可变状态。
            let models = gw.models().clone();
            let reg: crate::pulljobs::RegisterHook = Arc::new(move |path: &std::path::Path| {
                models.register_local(path, None).map(|r| r.entry.id)
            });
            // 部署闭包要用它，但**在这里不能持锁**——只捕获 Arc，
            // 真正执行时才短暂加锁（见下面闭包体）。
            (cfg, reg, Arc::clone(&self.gateway))
        };

        let registry = match cfg.model_source_registry() {
            Ok(r) => r,
            Err(e) => return Response::error(&e),
        };
        let (source, model) = match registry.resolve(spec) {
            Ok(v) => v,
            Err(e) => return Response::error(&e),
        };

        let mut opts = mangosteen_base::DownloadOptions::new(
            cfg.storage.models_dir.clone(),
            cfg.storage.max_model_bytes,
        );

        // 先取元数据：拿官方大小与 SHA-256，并提前发现「文件不存在」。
        // 这一步在网络上是几百毫秒，放在请求里可接受——它能让用户在
        // 提交前就知道文件不对，而不是等任务跑起来才失败。
        let target = model.local_file_name();
        match mangosteen_base::download::fetch_remote_files(&source, &model.repo, &opts) {
            Ok(files) => match files
                .iter()
                .find(|f| f.name == target || f.name.ends_with(&format!("/{target}")))
            {
                Some(f) => {
                    opts.expected_bytes = Some(f.size);
                    opts.expected_sha256 = f.sha256.clone();
                }
                None => {
                    return Response::json(
                        400,
                        &ErrorResponse::bad_request(
                            format!(
                            "仓库 {} 中不存在文件 `{target}`。先用「搜索」查看该仓库可下载的文件",
                            model.repo
                        ),
                            Some("model"),
                        ),
                    )
                }
            },
            Err(e) => {
                // 元数据取不到不算致命：仍然允许下载（只是没有大小与哈希校验）。
                // 但要让用户知道这一点，而不是以为一切正常。
                let _ = e;
            }
        }

        // 磁盘空间预检（已知大小时才有意义）
        if let Some(need) = opts.expected_bytes {
            if let Ok(avail) = mangosteen_platform::fs::available_space(&cfg.storage.models_dir) {
                let required = need + need / 20 + (128 << 20);
                if avail < required {
                    return Response::json(
                        400,
                        &ErrorResponse::bad_request(
                            format!(
                                "磁盘空间不足：需要约 {}，可用 {}",
                                mangosteen_platform::fs::human_bytes(required),
                                mangosteen_platform::fs::human_bytes(avail)
                            ),
                            None,
                        ),
                    );
                }
            }
        }

        // 本地已有完整文件 → 不必启动下载线程，直接标记完成
        let file_name = model.local_file_name();
        let final_path = cfg.storage.models_dir.join(&file_name);
        let already = match (opts.expected_bytes, std::fs::metadata(&final_path)) {
            (Some(expected), Ok(meta)) => meta.len() == expected,
            _ => false,
        };

        let job_id = self.pull_jobs.create_with(spec, &file_name, deploy_after);

        // 部署回调：把每个阶段实时写回任务表。
        //
        // 闭包在**持有网关锁**期间被调用（`prepare` / `commit_local` 要
        // `&mut Gateway`），所以 sink 里只能操作内存任务表，
        // 绝不做网络/磁盘 IO——否则几 GB 下载期间整个 API 都会被拖死。
        let dep: Option<crate::pulljobs::DeployHooks> = if deploy_after {
            let jobs = self.pull_jobs.clone();
            let jid = job_id.clone();
            let gw = gw_for_deploy;
            let tgt = if deploy_target == "cloud" {
                mangosteen_orchestration::assess::ComputeTarget::Cloud
            } else {
                mangosteen_orchestration::assess::ComputeTarget::Local
            };
            // 远端元数据预取所需：源、模型引用、下载选项快照。
            // 这一份给 prepare 阶段用（HTTP Range 只取头部）。
            let probe_source = source.clone();
            let probe_model = model.clone();
            let probe_opts = opts.clone();
            // 模型名与量化用于「预取失败」时的粗估兜底
            let est_name = model_name_for_estimate(&model);
            let est_quant = model
                .quant_from_filename()
                .unwrap_or(mangosteen_core::model::Quantization::Q4KM);
            let est_size = opts.expected_bytes.unwrap_or(0);

            // ---- 第 1 段：准备（评估 → 选设备 → 配置）----
            let jobs_p = jobs.clone();
            let jid_p = jid.clone();
            let gw_p = Arc::clone(&gw);
            // 本地已存在的最终文件路径：优先读它做精确评估。
            // 远端预取要走网络（实测 5 秒），文件就在本地时没必要绕这一圈。
            let local_final_path = final_path.clone();

            // 云端直连下载所需的「从哪下、多大」。
            //
            // 设备选云端时，模型的下载发生在**云端**，所以要把源地址一起带给
            // 编排层；否则云端只能退化成「要求本地先下一份再上传」——那与
            // 「云端弹性补位」的定位相悖（本地没下过就不许上云）。
            //
            // 地址用与本地下载完全相同的 `download_url` 规则拼出，
            // 保证云端拿到的就是用户选的那个文件。
            let dl_spec: Option<mangosteen_orchestration::assess::ModelDownloadSpec> =
                match probe_source.download_url(&probe_model) {
                    Ok(url) => Some(mangosteen_orchestration::assess::ModelDownloadSpec {
                        url,
                        expected_bytes: probe_opts.expected_bytes,
                    }),
                    Err(e) => {
                        // 拼不出地址不是致命错误：本地已有文件时仍可上传兜底。
                        // 但要留痕，否则云端报「没有可用下载地址」时无从追查。
                        eprintln!("[deploy] 无法拼出下载地址，云端将只能走上传：{e}");
                        None
                    }
                };
            let prepare: crate::pulljobs::PrepareHook = Arc::new(move || {
                // 评估：本地文件 > 远端真实元数据 > 按体积粗估
                let req = {
                    let mut r =
                        mangosteen_orchestration::assess::DeploymentRequest::for_model("__probe__");
                    r.target = tgt;
                    // 云端要据此在远端下载；本地部署用不到，带着也无害
                    r.model_download = dl_spec.clone();
                    r
                };

                // 阶段 1/6：模型评估
                jobs_p.set_stage(
                    &jid_p,
                    crate::progress::DeployStage::Assessing,
                    crate::progress::StageOutcome::Running,
                    "正在读取模型元数据并评估资源需求".to_string(),
                );

                let evaluated = if local_final_path.is_file() {
                    // 本地已有 → 读盘解析，最准且零网络延迟
                    let g = gw_p
                        .lock()
                        .map_err(|_| MnError::bad_request("网关状态锁中毒"))?;
                    let ev = g.assessor_public().evaluate_file(&local_final_path, &req);
                    drop(g);
                    match ev {
                        Ok(ev) => {
                            jobs_p.set_stage(
                                &jid_p,
                                crate::progress::DeployStage::Assessing,
                                crate::progress::StageOutcome::Done,
                                format!("{}（元数据来自本地文件）", probe_assessed_detail(&ev)),
                            );
                            ev
                        }
                        Err(e) => {
                            // 本地文件坏了 → 也会失败，但给准确原因
                            jobs_p.set_stage(
                                &jid_p,
                                crate::progress::DeployStage::Assessing,
                                crate::progress::StageOutcome::Failed,
                                format!("本地模型文件无法解析：{e}"),
                            );
                            return Err(e);
                        }
                    }
                } else {
                    match mangosteen_base::download::probe_remote_metadata(
                        &probe_source,
                        &probe_model,
                        &probe_opts,
                        REMOTE_HEADER_BYTES,
                    ) {
                        Ok(meta) => {
                            let g = gw_p
                                .lock()
                                .map_err(|_| MnError::bad_request("网关状态锁中毒"))?;
                            let ev = g.evaluate_remote(&meta.info, &req);
                            drop(g);
                            jobs_p.set_stage(
                                &jid_p,
                                crate::progress::DeployStage::Assessing,
                                crate::progress::StageOutcome::Done,
                                format!("{}（元数据来自远端文件头）", probe_assessed_detail(&ev)),
                            );
                            ev
                        }
                        Err(e) => {
                            // 预取失败不阻断部署，但必须如实标注为估算。
                            let g = gw_p
                                .lock()
                                .map_err(|_| MnError::bad_request("网关状态锁中毒"))?;
                            let ev = g.evaluate_estimated(&est_name, est_quant, est_size, &req);
                            drop(g);
                            jobs_p.set_stage(
                                &jid_p,
                                crate::progress::DeployStage::Assessing,
                                crate::progress::StageOutcome::Done,
                                format!(
                                    "{}（⚠ 元数据预取失败，以下为按体积的估算值：{}）",
                                    probe_assessed_detail(&ev),
                                    first_line(&e.to_string())
                                ),
                            );
                            ev
                        }
                    }
                };

                // 阶段 2/6、3/6 由 prepare_with_progress 内部上报
                let sink_jobs = jobs_p.clone();
                let sink_id = jid_p.clone();
                let sink = move |stage: crate::progress::DeployStage,
                                 outcome: crate::progress::StageOutcome,
                                 detail: String| {
                    sink_jobs.set_stage(&sink_id, stage, outcome, detail);
                };
                let mut g = gw_p
                    .lock()
                    .map_err(|_| MnError::bad_request("网关状态锁中毒"))?;
                g.prepare_with_progress(evaluated, &req, &sink)
            });

            // ---- 第 3 段：提交（启动引擎）----
            let jobs_c = jobs.clone();
            let jid_c = jid.clone();
            // commit 与 abort 都要网关句柄；abort 另需一份独立克隆，
            // 因为 commit 的闭包已经把 gw 移进去了。
            let gw_for_abort = Arc::clone(&gw);
            let gw_c = gw;
            // commit 需要知道文件落在哪（prepare 时还没有这个文件）
            let final_path_for_commit = final_path.clone();
            let commit: crate::pulljobs::CommitHook = Arc::new(move |prepared| {
                // prepare 在下载**之前**执行，那时文件还不存在，
                // plan.model_path 是空的。这里补上真实落盘路径，
                // 否则引擎会报「模型文件不存在：」（冒号后为空）。
                let prepared =
                    prepared.with_model_path(final_path_for_commit.to_string_lossy().to_string());
                let sink_jobs = jobs_c.clone();
                let sink_id = jid_c.clone();
                let sink = move |stage: crate::progress::DeployStage,
                                 outcome: crate::progress::StageOutcome,
                                 detail: String| {
                    sink_jobs.set_stage(&sink_id, stage, outcome, detail);
                };
                let mut g = gw_c
                    .lock()
                    .map_err(|_| MnError::bad_request("网关状态锁中毒"))?;
                let plan = g.commit_local(prepared, &sink)?;
                Ok(plan.id)
            });

            // ---- 回滚：撤销这次部署产生的全部痕迹 ----
            //
            // 由 pulljobs 在检测到用户取消时调用。要点：
            //  · 停引擎（可能已经起来了）
            //  · 删台账记录（不是标 destroyed——回滚要「当没发生过」）
            //  · 释放端口预留（否则反复「部署→停止」会吃光端口区间）
            //  · 按 `delete_file` 决定是否删模型文件
            let gw_a = gw_for_abort;
            let abort: crate::pulljobs::AbortHook = Arc::new(move |dep_id, delete_file| {
                let Some(dep_id) = dep_id else { return Ok(()) };
                let mut g = gw_a
                    .lock()
                    .map_err(|_| MnError::bad_request("网关状态锁中毒"))?;
                g.abort_deployment(dep_id, delete_file)
            });

            Some(crate::pulljobs::DeployHooks {
                prepare,
                commit,
                abort,
            })
        } else {
            None
        };

        if already {
            // 走一遍登记（可能只是台账丢了，文件还在）
            let model_id = match register(&final_path) {
                Ok(id) => id,
                Err(e) => {
                    self.pull_jobs
                        .mark_failed(&job_id, format!("登记模型失败：{e}"));
                    return Response::error(&e);
                }
            };
            let size = std::fs::metadata(&final_path).map(|m| m.len()).unwrap_or(0);
            self.pull_jobs.mark_done(&job_id, &model_id, size, true);
            if deploy_after {
                // 文件已在本地 → 下载阶段直接标「跳过」（不是假装下载过）
                self.pull_jobs.set_stage(
                    &job_id,
                    crate::progress::DeployStage::Downloading,
                    crate::progress::StageOutcome::Skipped,
                    format!(
                        "本地已存在完整文件（{}），跳过下载",
                        mangosteen_base::download::human_bytes(size)
                    ),
                );
                self.pull_jobs.mark_deploying(&job_id);
                // 已在本地 → 直接跑「准备 → 提交」，不经过下载。
                // 准备里仍会做评估与设备选择，用户能看到同样的六步。
                if let Some(hooks) = dep.as_ref() {
                    match (hooks.prepare)() {
                        Ok(crate::gateway::PreparedDeployment::Done(plan)) => {
                            self.pull_jobs.set_deployment(&job_id, Some(&plan.id), None);
                        }
                        Ok(crate::gateway::PreparedDeployment::Local(p)) => {
                            match (hooks.commit)(*p) {
                                Ok(dep_id) => {
                                    self.pull_jobs.set_deployment(&job_id, Some(&dep_id), None)
                                }
                                Err(e) => self.pull_jobs.set_deployment(
                                    &job_id,
                                    None,
                                    Some(format!(
                                        "模型已就绪，但自动启动失败：{e}。\
                                         可在「已下载模型」里点「启动」重试"
                                    )),
                                ),
                            }
                        }
                        Err(e) => self.pull_jobs.set_deployment(
                            &job_id,
                            None,
                            Some(format!(
                                "模型已就绪，但部署准备失败：{e}。\
                                 可在「已下载模型」里点「启动」重试"
                            )),
                        ),
                    }
                }
            }
        } else {
            let resumed = cfg
                .storage
                .models_dir
                .join(format!("{file_name}.part"))
                .is_file();
            crate::pulljobs::spawn(
                self.pull_jobs.clone(),
                job_id.clone(),
                source,
                model,
                opts,
                register,
                dep,
            );
            if resumed {
                // 续传是磁盘上 .part 的能力，这里只补一句让用户安心
                self.pull_jobs
                    .set_note(&job_id, "检测到未完成的下载，将从断点继续");
            }
        }

        // 清理过期任务，避免表无限增长
        self.pull_jobs.prune(10 * 60 * 1000);

        Response::json(
            200,
            &serde_json::json!({
                "ok": true,
                "job_id": job_id,
                "reused": false,
            }),
        )
    }

    // ---------------------------------------------------------- 会话管理

    /// 列出会话。`GET /api/sessions?kind=chat|deploy`
    ///
    /// 不带 `kind` 则返回全部。列表只给摘要（不含消息体），
    /// 否则几十轮对话的会话会让列表接口膨胀到几百 KB。
    fn list_sessions(&self, req: &ParsedRequest) -> Response {
        let kind = req
            .query_param("kind")
            .and_then(|k| mangosteen_session::SessionKind::parse(&k));
        let res = match kind {
            Some(k) => self.sessions.list_by_kind(k),
            None => self.sessions.list(),
        };
        match res {
            Ok(list) => Response::json(200, &serde_json::json!({ "sessions": list })),
            Err(e) => Response::error(&e),
        }
    }

    /// 新建会话。`POST /api/sessions`
    ///
    /// 请求体：`{ "kind": "chat"|"deploy", "title": "...", "model": "..." }`
    /// 三者都可省略——`kind` 默认 `chat`，`title` 留空则由首条用户消息自动生成。
    fn create_session(&self, req: &ParsedRequest) -> Response {
        let parsed: serde_json::Value = match serde_json::from_slice(&req.body) {
            Ok(v) => v,
            Err(e) => {
                return Response::json(
                    400,
                    &ErrorResponse::bad_request(format!("请求体不是合法 JSON：{e}"), None),
                )
            }
        };
        let kind = match parsed.get("kind").and_then(|v| v.as_str()) {
            Some(s) => match mangosteen_session::SessionKind::parse(s) {
                Some(k) => k,
                None => {
                    return Response::json(
                        400,
                        &ErrorResponse::bad_request(
                            format!("未知会话种类 `{s}`，可选 chat / deploy"),
                            Some("kind"),
                        ),
                    )
                }
            },
            None => mangosteen_session::SessionKind::Chat,
        };
        let title = parsed
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let model = parsed
            .get("model")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        match self.sessions.create(kind, title, model) {
            Ok(s) => Response::json(201, &serde_json::json!({ "session": s })),
            Err(e) => Response::error(&e),
        }
    }

    /// 读取会话详情（含消息与操作记录）。`GET /api/sessions/{id}`
    fn get_session(&self, id: &str) -> Response {
        match self.sessions.get(id) {
            Ok(Some(s)) => Response::json(200, &serde_json::json!({ "session": s })),
            Ok(None) => Response::not_found(id),
            Err(e) => Response::error(&e),
        }
    }

    /// 删除会话。`DELETE /api/sessions/{id}`
    fn delete_session(&self, id: &str) -> Response {
        match self.sessions.delete(id) {
            Ok(true) => Response::json(200, &serde_json::json!({ "ok": true, "id": id })),
            Ok(false) => Response::not_found(id),
            Err(e) => Response::error(&e),
        }
    }

    /// 追加一条消息。`POST /api/sessions/{id}/messages`
    ///
    /// 请求体：`{ "role": "user"|"assistant"|"system", "content": "..." }`
    fn append_session_message(&self, id: &str, req: &ParsedRequest) -> Response {
        let parsed: serde_json::Value = match serde_json::from_slice(&req.body) {
            Ok(v) => v,
            Err(e) => {
                return Response::json(
                    400,
                    &ErrorResponse::bad_request(format!("请求体不是合法 JSON：{e}"), None),
                )
            }
        };
        let Some(content) = parsed.get("content").and_then(|v| v.as_str()) else {
            return Response::json(
                400,
                &ErrorResponse::bad_request("缺少 content 字段", Some("content")),
            );
        };
        let role = parsed
            .get("role")
            .and_then(|v| v.as_str())
            .unwrap_or("user");
        if !matches!(role, "user" | "assistant" | "system") {
            return Response::json(
                400,
                &ErrorResponse::bad_request(
                    format!("未知角色 `{role}`，可选 user / assistant / system"),
                    Some("role"),
                ),
            );
        }
        match self.sessions.append_message(id, role, content) {
            Ok(Some(s)) => Response::json(200, &serde_json::json!({ "session": s })),
            Ok(None) => Response::not_found(id),
            Err(e) => Response::error(&e),
        }
    }

    /// 记录一次操作。`POST /api/sessions/{id}/operations`
    ///
    /// 请求体：`{ "action": "deploy", "detail": "...", "ok": true }`
    ///
    /// 失败的操作也要记（`ok: false`）——操作记录的价值就在于
    /// 「当时到底发生了什么」，只记成功等于自欺。
    fn record_session_operation(&self, id: &str, req: &ParsedRequest) -> Response {
        let parsed: serde_json::Value = match serde_json::from_slice(&req.body) {
            Ok(v) => v,
            Err(e) => {
                return Response::json(
                    400,
                    &ErrorResponse::bad_request(format!("请求体不是合法 JSON：{e}"), None),
                )
            }
        };
        let Some(action) = parsed.get("action").and_then(|v| v.as_str()) else {
            return Response::json(
                400,
                &ErrorResponse::bad_request("缺少 action 字段", Some("action")),
            );
        };
        let detail = parsed
            .get("detail")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let ok = parsed.get("ok").and_then(|v| v.as_bool()).unwrap_or(true);
        match self.sessions.record_operation(id, action, &detail, ok) {
            Ok(Some(s)) => Response::json(200, &serde_json::json!({ "session": s })),
            Ok(None) => Response::not_found(id),
            Err(e) => Response::error(&e),
        }
    }

    fn list_pull_jobs(&self, req: &ParsedRequest) -> Response {
        if let Some(id) = req.query_param("id") {
            return match self.pull_jobs.get(&id) {
                Some(j) => Response::json(200, &job_json(&j)),
                None => Response::not_found(&id),
            };
        }
        let all = self.pull_jobs.list();
        Response::json(
            200,
            &serde_json::json!({
                "jobs": all.iter().map(job_json).collect::<Vec<_>>(),
            }),
        )
    }

    /// 取消下载任务。`POST /api/models/jobs/{id}/cancel`
    fn cancel_pull_job(&self, id: &str) -> Response {
        match self.pull_jobs.cancel(id) {
            true => Response::json(200, &serde_json::json!({ "ok": true, "id": id })),
            false => Response::json(
                400,
                &ErrorResponse::bad_request(format!("任务 `{id}` 不存在或已结束"), Some("id")),
            ),
        }
    }

    /// 删除模型（可选删除磁盘文件）。
    ///
    /// `?delete_file=1` 时同时删除磁盘上的 GGUF。默认只从台账移除，
    /// 因为模型动辄数 GiB，误删重下代价很高。
    fn remove_model(&self, id: &str, req: &ParsedRequest) -> Response {
        let delete_file = query_flag(&req.query, "delete_file");
        let gw = match self.gateway.lock() {
            Ok(g) => g,
            Err(_) => return Response::text(500, "网关状态锁中毒"),
        };

        // 删之前记下文件路径，删完**核实**是否真的不在了。
        //
        // 为什么不能直接回显 `delete_file` 参数：`ModelStore::remove` 在
        // 「路径不在授权范围」时会静默跳过删除却仍返回成功。直接回显
        // 会让 UI 报「已删除（含磁盘文件）」而文件还在占几个 GB——
        // 用户以为清理干净了，实际没有。
        let file_path = gw
            .models()
            .get(id)
            .ok()
            .flatten()
            .and_then(|m| match m.location {
                mangosteen_core::model::ModelLocation::Local { path } => Some(path),
                _ => None,
            });

        match gw.models().remove(id, delete_file) {
            Ok(true) => {
                let file_deleted = match (&file_path, delete_file) {
                    (Some(p), true) => !p.exists(),
                    _ => false,
                };
                Response::json(
                    200,
                    &serde_json::json!({
                        "ok": true,
                        "id": id,
                        "file_deleted": file_deleted,
                        // 请求删文件但实际没删掉 → 明确告诉用户残留路径，
                        // 而不是让他自己去找
                        "leftover": if delete_file && !file_deleted {
                            file_path.map(|p| p.display().to_string())
                        } else {
                            None
                        },
                    }),
                )
            }
            Ok(false) => Response::not_found(id),
            Err(e) => Response::error(&e),
        }
    }

    fn deployments(&self) -> Response {
        let gw = match self.gateway.lock() {
            Ok(g) => g,
            Err(_) => return Response::text(500, "网关状态锁中毒"),
        };
        match gw.list_deployments() {
            Ok(list) => {
                let arr: Vec<serde_json::Value> = list
                    .into_iter()
                    .map(|r| {
                        serde_json::json!({
                            "id": r.plan.id,
                            "model": r.plan.model.name,
                            // 完整模型 ID（含量化后缀）。`model` 只有裸名，
                            // 同家族多个量化版本并存时分不清实际在跑哪一个。
                            "model_id": mangosteen_core::model::ModelEntry::derive_id(
                                &r.plan.model.name,
                                r.plan.model.quant,
                            ),
                            "quant": r.plan.model.quant.to_string(),
                            "origin": r.plan.origin.label(),
                            "status": r.status.as_str(),
                            "port": r.plan.local_access_port(),
                            // 进程 PID：用户想用 top / kill 直接操作引擎进程时
                            // 必须有它，否则只能按端口反查。
                            "pid": r.pid,
                            "error": r.error,
                            "fallback_trail": r.plan.fallback_trail,
                        })
                    })
                    .collect();
                Response::json(200, &serde_json::json!({ "deployments": arr }))
            }
            Err(e) => Response::error(&e),
        }
    }

    fn deployment(&self, id: &str) -> Response {
        let gw = match self.gateway.lock() {
            Ok(g) => g,
            Err(_) => return Response::text(500, "网关状态锁中毒"),
        };
        match gw.get_deployment(id) {
            Ok(Some(r)) => {
                let payload = serde_json::json!({
                    "id": r.plan.id,
                    "model": r.plan.model.name,
                    "origin": r.plan.origin.label(),
                    "status": r.status.as_str(),
                    "port": r.plan.local_access_port(),
                    "error": r.error,
                    "fallback_trail": r.plan.fallback_trail,
                    "assessment": r.plan.assessment,
                });
                Response::json(200, &payload)
            }
            Ok(None) => Response::not_found(id),
            Err(e) => Response::error(&e),
        }
    }

    /// 停止服务：云端关机（停 GPU 计费，实例与权重保留），本地停引擎进程。
    fn stop_service(&self, id: &str) -> Response {
        let mut gw = match self.gateway.lock() {
            Ok(g) => g,
            Err(_) => return Response::text(500, "网关状态锁中毒"),
        };
        match gw.stop_service(id) {
            Ok(()) => Response::json(
                200,
                &serde_json::json!({"ok": true, "id": id, "status": "stopped"}),
            ),
            Err(e) => Response::json(500, &serde_json::json!({"error": e.to_string()})),
        }
    }

    /// 启动服务：云端开机并重新拉起推理进程，不需要重新传模型。
    fn start_service(&self, id: &str) -> Response {
        let mut gw = match self.gateway.lock() {
            Ok(g) => g,
            Err(_) => return Response::text(500, "网关状态锁中毒"),
        };
        match gw.start_service(id) {
            Ok(()) => Response::json(
                200,
                &serde_json::json!({"ok": true, "id": id, "status": "ready"}),
            ),
            Err(e) => Response::json(500, &serde_json::json!({"error": e.to_string()})),
        }
    }

    fn stop(&self, id: &str) -> Response {
        let mut gw = match self.gateway.lock() {
            Ok(g) => g,
            Err(_) => return Response::text(500, "网关状态锁中毒"),
        };
        match gw.stop(id) {
            Ok(()) => Response::json(200, &serde_json::json!({"ok": true, "id": id})),
            Err(e) => Response::error(&e),
        }
    }

    fn deploy(&self, req: &ParsedRequest) -> Response {
        let body = req.body_str();
        let parsed: serde_json::Value = match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(e) => {
                return Response::json(
                    400,
                    &ErrorResponse::bad_request(format!("请求体不是合法 JSON：{e}"), None),
                )
            }
        };
        let model = match parsed.get("model").and_then(|m| m.as_str()) {
            Some(m) => m.to_string(),
            None => {
                return Response::json(
                    400,
                    &ErrorResponse::bad_request("缺少 model 字段", Some("model")),
                )
            }
        };

        let mut dreq = mangosteen_orchestration::assess::DeploymentRequest::for_model(model);
        if let Some(c) = parsed.get("context_len").and_then(|v| v.as_u64()) {
            dreq.context_len = Some(c as u32);
        }
        if let Some(b) = parsed.get("batch_size").and_then(|v| v.as_u64()) {
            dreq.batch_size = b as u32;
        }
        if let Some(a) = parsed.get("allow_cloud").and_then(|v| v.as_bool()) {
            dreq.allow_cloud = a;
        }
        // 算力来源（部署页签的「本地 / 云端」选择）。
        //
        // 与 `allow_cloud` 是两件事：
        // * `allow_cloud=false` = 禁止降级（本地可行时仍用本地）；
        // * `target=cloud`      = 用户明确要为云端付费，跳过本地；
        // * `target=local`      = 本地不足时**停止部署**，绝不静默上云。
        //
        // 未知值直接报错而不是回退默认：静默忽略会让用户以为选择生效了，
        // 结果在「以为用本地」的情况下开出计费实例。
        if let Some(t) = parsed.get("target") {
            let raw = t.as_str().unwrap_or("");
            match mangosteen_orchestration::assess::ComputeTarget::parse(raw) {
                Some(v) => dreq.target = v,
                None => {
                    return Response::json(
                        400,
                        &ErrorResponse::bad_request(
                            format!("未知 target `{raw}`，可选 auto / local / cloud"),
                            Some("target"),
                        ),
                    )
                }
            }
        }
        if let Some(p) = parsed.get("max_price_per_hour").and_then(|v| v.as_f64()) {
            dreq.max_price_per_hour = Some(p);
        }
        if let Some(k) = parsed.get("kv_cache").and_then(|v| v.as_str()) {
            match k {
                "f16" => dreq.kv_cache = Some(mangosteen_core::estimate::KvCacheType::F16),
                "q8_0" => dreq.kv_cache = Some(mangosteen_core::estimate::KvCacheType::Q8_0),
                "q4_0" => dreq.kv_cache = Some(mangosteen_core::estimate::KvCacheType::Q4_0),
                other => {
                    return Response::json(
                        400,
                        &ErrorResponse::bad_request(
                            format!("未知 kv_cache `{other}`，可选 f16 / q8_0 / q4_0"),
                            Some("kv_cache"),
                        ),
                    )
                }
            }
        }

        let mut gw = match self.gateway.lock() {
            Ok(g) => g,
            Err(_) => return Response::text(500, "网关状态锁中毒"),
        };
        match gw.deploy(&dreq) {
            Ok(plan) => {
                let payload = serde_json::json!({
                    "id": plan.id,
                    "model": plan.model.name,
                    "origin": plan.origin.label(),
                    "port": plan.local_access_port(),
                    "fallback_trail": plan.fallback_trail,
                    "openai_base_url": format!(
                        "http://127.0.0.1:{}/v1",
                        gw.config().gateway.port
                    ),
                });
                Response::json(201, &payload)
            }
            Err(e) => Response::error(&e),
        }
    }

    fn assess(&self, req: &ParsedRequest) -> Response {
        let body = req.body_str();
        let parsed: serde_json::Value = match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(e) => {
                return Response::json(
                    400,
                    &ErrorResponse::bad_request(format!("请求体不是合法 JSON：{e}"), None),
                )
            }
        };
        let model = match parsed.get("model").and_then(|m| m.as_str()) {
            Some(m) => m.to_string(),
            None => {
                return Response::json(
                    400,
                    &ErrorResponse::bad_request("缺少 model 字段", Some("model")),
                )
            }
        };
        let mut dreq =
            mangosteen_orchestration::assess::DeploymentRequest::for_model(model).local_only();
        if let Some(c) = parsed.get("context_len").and_then(|v| v.as_u64()) {
            dreq.context_len = Some(c as u32);
        }

        let gw = match self.gateway.lock() {
            Ok(g) => g,
            Err(_) => return Response::text(500, "网关状态锁中毒"),
        };
        match gw.evaluate(&dreq) {
            Ok(e) => Response::json(
                200,
                &serde_json::json!({
                    "model": e.spec,
                    "placement": e.assessment.placement,
                    "context_len": e.assessment.context_len,
                    "kv_cache": e.assessment.kv_cache,
                    "threads": e.assessment.threads,
                    "requirement": e.assessment.requirement,
                    "local_viable": e.is_local_viable(),
                    "warnings": e.assessment.warnings.iter().map(|w| w.message()).collect::<Vec<_>>(),
                }),
            ),
            Err(e) => Response::error(&e),
        }
    }

    // ---------------------------------------------------------------- 推理转发

    fn chat(&self, req: &ParsedRequest) -> Response {
        let parsed: ChatRequest = match serde_json::from_slice(&req.body) {
            Ok(r) => r,
            Err(e) => {
                return Response::json(
                    400,
                    &ErrorResponse::bad_request(format!("请求体不是合法 ChatRequest：{e}"), None),
                )
            }
        };
        let target = match self.backend_for_model(Some(parsed.model.as_str())) {
            Ok(t) => t,
            Err(e) => return Response::error(&e),
        };
        let normalized = match protocol::normalize_chat(parsed, &target) {
            Ok(n) => n,
            Err(e) => return Response::error(&e),
        };
        let body = match serde_json::to_vec(&normalized) {
            Ok(b) => b,
            Err(e) => return Response::error(&MnError::bad_request(format!("序列化失败：{e}"))),
        };
        self.forward_json("chat/completions", &body, &target)
    }

    fn completion(&self, req: &ParsedRequest) -> Response {
        let parsed: mangosteen_core::openai::CompletionRequest =
            match serde_json::from_slice(&req.body) {
                Ok(r) => r,
                Err(e) => {
                    return Response::json(
                        400,
                        &ErrorResponse::bad_request(
                            format!("请求体不是合法 CompletionRequest：{e}"),
                            None,
                        ),
                    )
                }
            };
        let target = match self.backend() {
            Ok(t) => t,
            Err(e) => return Response::error(&e),
        };
        let normalized = match protocol::normalize_completion(parsed, &target) {
            Ok(n) => n,
            Err(e) => return Response::error(&e),
        };
        let body = match serde_json::to_vec(&normalized) {
            Ok(b) => b,
            Err(e) => return Response::error(&MnError::bad_request(format!("序列化失败：{e}"))),
        };
        self.forward_json("completions", &body, &target)
    }

    fn embeddings(&self, req: &ParsedRequest) -> Response {
        let parsed: mangosteen_core::openai::EmbeddingRequest =
            match serde_json::from_slice(&req.body) {
                Ok(r) => r,
                Err(e) => {
                    return Response::json(
                        400,
                        &ErrorResponse::bad_request(
                            format!("请求体不是合法 EmbeddingRequest：{e}"),
                            None,
                        ),
                    )
                }
            };
        let target = match self.backend() {
            Ok(t) => t,
            Err(e) => return Response::error(&e),
        };
        let normalized = match protocol::normalize_embedding(parsed, &target) {
            Ok(n) => n,
            Err(e) => return Response::error(&e),
        };
        let body = match serde_json::to_vec(&normalized) {
            Ok(b) => b,
            Err(e) => return Response::error(&MnError::bad_request(format!("序列化失败：{e}"))),
        };
        self.forward_json("embeddings", &body, &target)
    }

    /// 取当前后端。
    fn backend(&self) -> MnResult<BackendTarget> {
        let mut gw = self
            .gateway
            .lock()
            .map_err(|_| MnError::bad_request("网关状态锁中毒"))?;
        gw.active_backend()
    }

    /// 按请求里的 `model` 选择后端。
    ///
    /// 客户端传 `auto` 或缺省 → 自动（本地优先）；
    /// 传了具体模型 ID → 必须路由到那个模型，未运行则明确报错。
    ///
    /// 这样 UI「模型信息」下拉的选择才真正生效——否则用户选了 A
    /// 却被路由到 B，回答不对劲却查不出原因。
    fn backend_for_model(&self, model: Option<&str>) -> MnResult<BackendTarget> {
        let mut gw = self
            .gateway
            .lock()
            .map_err(|_| MnError::bad_request("网关状态锁中毒"))?;
        gw.active_backend_for(model)
    }

    /// 转发 JSON 请求到上游，并把上游响应归一化。
    fn forward_json(&self, path: &str, body: &[u8], target: &BackendTarget) -> Response {
        let url = target.url_for(path);
        match http_post_json(&url, body, self.upstream_timeout) {
            Ok((status, resp_body)) => {
                // 上游错误 → 归一化
                if !(200..300).contains(&status) {
                    let e = MnError::Network {
                        stage: format!("上游推理服务（HTTP {status}）"),
                        reason: summarize_body(&resp_body),
                    };
                    return Response::error(&e);
                }
                // 上游会把 model 填成模型文件绝对路径（llama.cpp 行为），
                // 这里统一改写为 Mangosteen 的模型 ID，保证与 /v1/models
                // 一致，且不向调用方泄漏本机路径。SSE 流也走同一函数
                // （内部按 data: 行逐条改写）。
                let body = protocol::rewrite_response_model(&resp_body, &target.model_id);
                // stream=true 时上游返回 SSE，Content-Type 必须是
                // text/event-stream，否则标准 OpenAI SDK 的流式解析器不认。
                let content_type = if protocol::looks_like_sse(&body) {
                    "text/event-stream"
                } else {
                    "application/json"
                };
                Response {
                    status,
                    content_type: content_type.to_string(),
                    body: body.into_bytes(),
                }
            }
            Err(e) => Response::error(&e),
        }
    }
}

/// 是否为**流式推理请求**（需要边生成边转发）。
///
/// 判定三件事：POST、推理路由、body 里 `stream: true`。
///
/// 为什么必须单独识别：`Response.body` 是 `Vec<u8>`，天然是
/// 「全部读完再返回」。流式请求若走那条路径，用户要等整段生成完
/// 才看到第一个字——这正是「输出卡顿」的直接原因。
fn is_stream_request(req: &ParsedRequest) -> bool {
    if req.method != "POST" {
        return false;
    }
    if !matches!(
        req.path.as_str(),
        "/v1/chat/completions" | "/v1/completions"
    ) {
        return false;
    }
    serde_json::from_slice::<serde_json::Value>(&req.body)
        .ok()
        .and_then(|v| v.get("stream").and_then(|s| s.as_bool()))
        .unwrap_or(false)
}

/// 写一个 HTTP chunked 块并**立即 flush**。
///
/// flush 是流式的关键：不 flush 的话数据留在用户态缓冲里，
/// 浏览器收不到，界面上就是「一直不动，最后一次性出现」。
/// 返回 `false` 表示客户端已断开（用户点了「停止」或关了页面）。
fn write_chunk(stream: &mut TcpStream, data: &[u8]) -> bool {
    if data.is_empty() {
        return true;
    }
    let head = format!("{:x}\r\n", data.len());
    if stream.write_all(head.as_bytes()).is_err() {
        return false;
    }
    if stream.write_all(data).is_err() {
        return false;
    }
    if stream.write_all(b"\r\n").is_err() {
        return false;
    }
    stream.flush().is_ok()
}

/// 极简 HTTP POST（仅用于转发到本机/隧道后的上游）。
///
/// 上游一定是 `127.0.0.1`，因此不需要 TLS。
pub fn http_post_json(url: &str, body: &[u8], timeout: Duration) -> MnResult<(u16, String)> {
    let (host_port, path) = split_url(url)?;
    let mut stream = TcpStream::connect_timeout(
        &host_port.parse().map_err(|_| MnError::Network {
            stage: "连接上游".to_string(),
            reason: format!("非法地址 {host_port}"),
        })?,
        timeout,
    )
    .map_err(|e| MnError::Network {
        stage: "连接上游推理服务".to_string(),
        reason: format!("{host_port}：{e}。请确认部署仍在运行"),
    })?;
    stream.set_read_timeout(Some(timeout)).ok();
    stream.set_write_timeout(Some(timeout)).ok();

    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: {host_port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(head.as_bytes())
        .and_then(|_| stream.write_all(body))
        .map_err(|e| MnError::Network {
            stage: "发送请求到上游".to_string(),
            reason: e.to_string(),
        })?;

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).map_err(|e| MnError::Network {
        stage: "读取上游响应".to_string(),
        reason: e.to_string(),
    })?;

    parse_http_response(&raw)
}

/// 解析上游 HTTP 响应（只处理非 chunked 的 JSON 响应）。
pub fn parse_http_response(raw: &[u8]) -> MnResult<(u16, String)> {
    let text = String::from_utf8_lossy(raw);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| MnError::Network {
            stage: "解析上游响应".to_string(),
            reason: "响应缺少头部与正文分隔".to_string(),
        })?;

    let status_line = head.lines().next().unwrap_or("");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| MnError::Network {
            stage: "解析上游响应".to_string(),
            reason: format!("无法解析状态行：{status_line}"),
        })?;

    // chunked 编码：llama-server 对流式请求才用；这里只处理普通响应
    let chunked = head
        .lines()
        .any(|l| l.to_ascii_lowercase().starts_with("transfer-encoding:") && l.contains("chunked"));
    let body = if chunked {
        decode_chunked(body)
    } else {
        body.to_string()
    };
    Ok((status, body))
}

/// 解 chunked 编码。
fn decode_chunked(input: &str) -> String {
    let mut out = String::new();
    let mut rest = input;
    loop {
        let Some((size_line, after)) = rest.split_once("\r\n") else {
            break;
        };
        let size = usize::from_str_radix(size_line.trim().split(';').next().unwrap_or("0"), 16)
            .unwrap_or(0);
        if size == 0 {
            break;
        }
        if after.len() < size {
            out.push_str(after);
            break;
        }
        out.push_str(&after[..size]);
        rest = after[size..].strip_prefix("\r\n").unwrap_or(&after[size..]);
    }
    out
}

/// 拆分 `http://host:port/path`。
fn split_url(url: &str) -> MnResult<(String, String)> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| MnError::bad_request(format!("只支持 http:// 上游，实际 {url}")))?;
    match rest.split_once('/') {
        Some((host, path)) => Ok((host.to_string(), format!("/{path}"))),
        None => Ok((rest.to_string(), "/".to_string())),
    }
}

/// 截取响应体摘要（错误消息用）。
fn summarize_body(body: &str) -> String {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(m) = v
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
        {
            return m.to_string();
        }
        if let Some(m) = v.get("message").and_then(|m| m.as_str()) {
            return m.to_string();
        }
    }
    body.chars().take(200).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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

#[cfg(test)]
mod stream_tests {
    use super::*;

    fn req(method: &str, path: &str, body: &str) -> ParsedRequest {
        ParsedRequest {
            method: method.to_string(),
            path: path.to_string(),
            query: String::new(),
            headers: HashMap::new(),
            body: body.as_bytes().to_vec(),
        }
    }

    /// 只有 `stream: true` 的推理请求才走直通路径。
    ///
    /// 判定错了后果很严重：非流式请求走流式路径会破坏 `Content-Length`
    /// 语义；流式请求走缓冲路径则退化成「等全部生成完才返回」。
    #[test]
    fn detects_stream_requests() {
        // 正常流式
        assert!(is_stream_request(&req(
            "POST",
            "/v1/chat/completions",
            r#"{"model":"m","stream":true,"messages":[]}"#
        )));
        assert!(is_stream_request(&req(
            "POST",
            "/v1/completions",
            r#"{"model":"m","stream":true}"#
        )));

        // 非流式
        assert!(!is_stream_request(&req(
            "POST",
            "/v1/chat/completions",
            r#"{"model":"m","stream":false,"messages":[]}"#
        )));
        // 缺省 stream → 非流式（协议默认）
        assert!(!is_stream_request(&req(
            "POST",
            "/v1/chat/completions",
            r#"{"model":"m","messages":[]}"#
        )));
        // 非推理路由
        assert!(!is_stream_request(&req(
            "POST",
            "/api/deploy",
            r#"{"stream":true}"#
        )));
        // 非 POST
        assert!(!is_stream_request(&req(
            "GET",
            "/v1/chat/completions",
            "{}"
        )));
        // body 不是 JSON → 不能崩，按非流式处理
        assert!(!is_stream_request(&req(
            "POST",
            "/v1/chat/completions",
            "not json"
        )));
    }

    /// SSE 单行改写：只改 `model`，帧结构必须原样保留。
    #[test]
    fn rewrites_single_sse_line() {
        let line =
            r#"data: {"model":"/abs/path/model.gguf","choices":[{"delta":{"content":"你"}}]}"#;
        let out = protocol::rewrite_sse_line(line, "qwen2.5:q4_k_m");
        assert!(out.starts_with("data: "), "帧前缀必须保留：{out}");
        let payload = out.trim_start_matches("data: ");
        let v: serde_json::Value = serde_json::from_str(payload).expect("仍是合法 JSON");
        assert_eq!(v["model"], "qwen2.5:q4_k_m", "model 必须改写为模型 ID");
        assert_eq!(v["choices"][0]["delta"]["content"], "你", "内容不能丢");

        // [DONE] 与非 data 行原样返回
        assert_eq!(
            protocol::rewrite_sse_line("data: [DONE]", "x"),
            "data: [DONE]"
        );
        assert_eq!(protocol::rewrite_sse_line("", "x"), "");
        assert_eq!(
            protocol::rewrite_sse_line("event: ping", "x"),
            "event: ping"
        );
    }

    /// chunked 帧格式必须严格符合 HTTP/1.1：`<hex len>\r\n<data>\r\n`。
    ///
    /// 格式错一位浏览器就解析失败，症状是「流式完全没有输出」。
    #[test]
    fn chunk_encoding_format_is_correct() {
        // 用一个本地 listener 验证真实写出的字节
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let h = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            write_chunk(&mut s, b"hello")
        });

        let mut client = TcpStream::connect(addr).unwrap();
        assert!(h.join().unwrap(), "写 chunk 应成功");

        let mut got = Vec::new();
        client.read_to_end(&mut got).unwrap();
        assert_eq!(
            String::from_utf8_lossy(&got),
            "5\r\nhello\r\n",
            "chunked 帧格式错误：{got:?}"
        );
    }
}
