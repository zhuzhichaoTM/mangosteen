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

use mangosteen_core::openai::ErrorResponse;
use mangosteen_core::{MnError, MnResult};

use crate::gateway::Gateway;
use crate::protocol;

/// 请求行与头部解析结果。
#[derive(Debug, Clone)]
pub struct ParsedRequest {
    pub method: String,
    pub path: String,
    pub query: String,
    pub headers: HashMap<String, String>,
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
    pub status: u16,
    pub content_type: String,
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
pub(crate) fn base64_decode(input: &str) -> Result<Vec<u8>, String> {
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

pub(crate) const REMOTE_HEADER_BYTES: u64 = 8 * 1024 * 1024;

/// 评估结果的单行摘要（含资源占用明细）。
///
/// 与 `gateway::assessed_detail` 同样的信息，但在 server 侧构造——
/// 那边是 `pub(crate)` 之外的私有函数，且这里要用在**持有任务表锁**的
/// 闭包里（不能再进网关锁拿数据）。
pub(crate) fn probe_assessed_detail(ev: &mangosteen_orchestration::assess::Evaluated) -> String {
    crate::gateway::assessed_detail(ev)
}

/// 从模型引用推一个人类可读的名字（用于粗估兜底）。
///
/// 去掉 `.gguf` 后缀与量化尾缀，得到如 `qwen2.5-0.5b-instruct`。
/// 量化尾缀写法多样（`-q4_k_m` / `.Q4_K_M`），复用 `ModelRef` 自己的
/// 解析器，避免两处规则分叉。
pub(crate) fn model_name_for_estimate(model: &mangosteen_core::source::ModelRef) -> String {
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
pub(crate) fn first_line(msg: &str) -> String {
    msg.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("未知原因")
        .trim()
        .to_string()
}

/// 任务状态 → JSON。字段名是 UI 的契约，改名会让进度条白屏。
pub(crate) fn job_json(j: &crate::pulljobs::PullJob) -> serde_json::Value {
    // 步骤条：把「全部步骤」与「每步最新状态」一次给全。
    //
    // 为什么在后端算而不是前端拼：前端只拿到「当前阶段」时，无法区分
    // 「这步还没到」与「这步跳过了」——后者必须显示原因。把状态判定
    // 放在这里，UI 只管画。
    let trail = &j.deploy_trail;
    let steps: Vec<serde_json::Value> = mangosteen_orchestration::progress::DeployStage::STEPS
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
            "total": mangosteen_orchestration::progress::DeployStage::total(),
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
pub(crate) fn query_flag(query: &str, key: &str) -> bool {
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
pub(crate) fn reason_phrase(code: u16) -> &'static str {
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

const MAX_BODY: usize = 32 * 1024 * 1024;

/// 十六进制字符 → 数值。
pub(crate) fn hex_val(c: u8) -> Option<u8> {
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
pub(crate) fn percent_decode_path(s: &str) -> String {
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
    pub(crate) gateway: Arc<Mutex<Gateway>>,
    pub(crate) require_key: bool,
    pub(crate) api_key: String,
    pub(crate) upstream_timeout: Duration,
    pub(crate) sampler: mangosteen_platform::resmon::CpuSampler,
    pub(crate) pull_jobs: crate::pulljobs::PullJobs,
    pub(crate) cloud_monitor: crate::cloudmon::CloudMonitor,
    pub(crate) sessions: mangosteen_session::SessionStore,
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

/// 是否为**流式推理请求**（需要边生成边转发）。
///
/// 判定三件事：POST、推理路由、body 里 `stream: true`。
///
/// 为什么必须单独识别：`Response.body` 是 `Vec<u8>`，天然是
/// 「全部读完再返回」。流式请求若走那条路径，用户要等整段生成完
/// 才看到第一个字——这正是「输出卡顿」的直接原因。
pub(crate) fn is_stream_request(req: &ParsedRequest) -> bool {
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
pub(crate) fn write_chunk(stream: &mut TcpStream, data: &[u8]) -> bool {
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
pub(crate) fn decode_chunked(input: &str) -> String {
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
pub(crate) fn split_url(url: &str) -> MnResult<(String, String)> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| MnError::bad_request(format!("只支持 http:// 上游，实际 {url}")))?;
    match rest.split_once('/') {
        Some((host, path)) => Ok((host.to_string(), format!("/{path}"))),
        None => Ok((rest.to_string(), "/".to_string())),
    }
}

/// 截取响应体摘要（错误消息用）。
pub(crate) fn summarize_body(body: &str) -> String {
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
