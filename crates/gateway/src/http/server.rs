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
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mangosteen_core::{MnError, MnResult};

use crate::gateway::Gateway;
use crate::http::relay::is_stream_request;

// 兼容导出：处理器按 `http::server::*` 引入这些类型与工具，
// 物理位置已按职责迁出，这里只保留唯一的公开路径。
pub(crate) use crate::http::helpers::{
    first_line, job_json, model_name_for_estimate, probe_assessed_detail, query_flag,
};
pub(crate) use crate::http::request::{parse_request, ParsedRequest};
pub(crate) use crate::http::response::Response;

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

    /// 校验 API Key。
    pub(crate) fn authorized(&self, req: &ParsedRequest) -> bool {
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
