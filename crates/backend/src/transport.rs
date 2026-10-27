//! HTTP 传输抽象与实现。
//!
//! 归属：**算力执行层 · 云端后端管理**（协议适配器的传输后端）。
//!
//! ## 为什么驱动只依赖 trait
//!
//! 本 crate 的价值在**协议适配逻辑**：参数扁平化、签名、字段映射、状态归一化。
//! 这些必须能在无网络、无账号的情况下被完整测试。所以驱动只认识
//! [`Transport`]，单测注入 [`MockTransport`]，生产注入 [`CurlTransport`]。
//!
//! ## 为什么用系统 curl（不引入 HTTP 客户端库）
//!
//! 遵循工作区「依赖极简」原则：HTTPS、超时、状态码这几项需求 curl 全部满足，
//! macOS / Linux 自带，Windows 10 1803+ 自带 `curl.exe`。云端管理接口是
//! 分钟级低频操作，进程启动开销可忽略。
//!
//! ## 安全要点（与普通 REST 调用不同）
//!
//! 优云智算使用 UCloud V1 签名，认证信息（`PublicKey` / `Signature`）
//! 位于**请求体参数**中，而不是请求头。若用 `--data <body>` 传参，
//! 这些值会出现在进程参数列表（同机其它用户可 `ps` 看到）。
//!
//! 因此这里固定用 `--data-binary @-` 从 **stdin** 喂入请求体：
//! 既不进进程参数列表，也不需要落地临时文件（因而没有文件权限、
//! 残留清理、跨平台权限模型这些问题）。

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Mutex;

use mangosteen_core::{MnError, MnResult};

/// 一次 HTTP 请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    /// 方法（本 crate 只用到 `POST`，保留通用性）
    pub method: String,
    /// 完整 URL
    pub url: String,
    /// 请求头
    pub headers: Vec<(String, String)>,
    /// 请求体
    pub body: Option<String>,
}

impl HttpRequest {
    /// 构造表单 POST。
    ///
    /// 优云智算的所有接口都是 `POST` + `application/x-www-form-urlencoded`，
    /// 因此这里直接把 Content-Type 设成表单类型，避免调用方重复书写。
    pub fn form_post(url: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            method: "POST".to_string(),
            url: url.into(),
            headers: vec![(
                "Content-Type".to_string(),
                "application/x-www-form-urlencoded".to_string(),
            )],
            body: Some(body.into()),
        }
    }

    /// 构造 JSON POST。
    ///
    /// 绝大多数优云智算接口用表单编码，但 `GetCompShareInstanceMonitor`
    /// 例外：它的 `UHostIds` 是**数组**，表单编码无论用扁平索引键
    /// （`UHostIds.0`）还是 JSON 字符串都会被打回
    /// （实测 `210 Missing params` / `230 ... not available`），
    /// 只有 `application/json` + 真正的 JSON 数组才被接受。
    pub fn json_post(url: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            method: "POST".to_string(),
            url: url.into(),
            headers: vec![("Content-Type".to_string(), "application/json".to_string())],
            body: Some(body.into()),
        }
    }

    /// 追加请求头。
    pub fn with_header(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.headers.push((k.into(), v.into()));
        self
    }

    /// 取请求头（名不区分大小写）。
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// 一次 HTTP 响应。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    /// HTTP 状态码
    pub status: u16,
    /// 响应体
    pub body: String,
}

impl HttpResponse {
    /// 构造。
    pub fn new(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            body: body.into(),
        }
    }

    /// 是否 2xx。
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// 传输抽象。
pub trait Transport: Send + Sync {
    /// 执行请求。
    fn execute(&self, req: &HttpRequest) -> MnResult<HttpResponse>;
}

/// `Arc<dyn Transport>` 也是 `Transport`，便于在驱动间共享同一后端。
impl Transport for std::sync::Arc<dyn Transport + Send + Sync> {
    fn execute(&self, req: &HttpRequest) -> MnResult<HttpResponse> {
        (**self).execute(req)
    }
}

/// `&T` 也是 `Transport`（`T: Transport`）。
///
/// 用途：跨可用区批量查询需要「同一份凭证与传输、不同 region/zone」的多个客户端。
/// 若要求 `T: Clone` 会把 `Arc<dyn Transport>` 之外的实现挡在门外（dyn 不可 Clone），
/// 因此提供引用形式，让派生客户端借用外部传输。
impl<T: Transport + ?Sized> Transport for &T {
    fn execute(&self, req: &HttpRequest) -> MnResult<HttpResponse> {
        (**self).execute(req)
    }
}

// 说明：`Transport: Send + Sync`，而 `&T` 的 `Send + Sync` 由 `T: Sync` 自动成立，
// 因此上面的 impl 无需额外约束即可满足 trait 要求。

/// 基于系统 `curl` 的真实传输实现。
#[derive(Debug, Clone)]
pub struct CurlTransport {
    /// 单次请求超时（秒）
    pub timeout_secs: u64,
    /// curl 可执行文件（默认 `curl`）
    pub curl_bin: String,
}

impl Default for CurlTransport {
    fn default() -> Self {
        Self {
            timeout_secs: 30,
            curl_bin: "curl".to_string(),
        }
    }
}

impl CurlTransport {
    /// 新建。
    pub fn new(timeout_secs: u64) -> Self {
        Self {
            timeout_secs,
            ..Default::default()
        }
    }

    /// 构建 curl 命令行参数。
    pub fn build_args(&self, req: &HttpRequest) -> Vec<String> {
        let mut args: Vec<String> = vec![
            "--silent".into(),
            "--show-error".into(),
            "--max-time".into(),
            self.timeout_secs.to_string(),
            "--request".into(),
            req.method.clone(),
            // 云 API 不应重定向；禁止跟随可避免把请求体投递到别的地址
            "--location".into(),
            "--max-redirs".into(),
            "0".into(),
            "--write-out".into(),
            "\n%{http_code}".into(),
        ];
        if req.body.is_some() {
            // 从 stdin 读请求体：认证参数不进入进程参数列表
            args.push("--data-binary".into());
            args.push("@-".into());
        }
        for (k, v) in &req.headers {
            args.push("--header".into());
            args.push(format!("{k}: {v}"));
        }
        args.push(req.url.clone());
        args
    }

    /// 解析 curl 输出。
    ///
    /// 约定：`--write-out "\n%{http_code}"` 让状态码位于最后一行。
    pub fn parse_output(stdout: &str, exit_code: i32, stderr: &str) -> MnResult<HttpResponse> {
        if exit_code != 0 {
            let stage = match exit_code {
                6 => "DNS 解析",
                7 => "TCP 连接",
                28 => "请求超时",
                35 | 60 => "TLS 证书校验",
                52 => "服务端空响应",
                _ => "HTTP 请求",
            };
            return Err(MnError::Network {
                stage: stage.to_string(),
                reason: format!(
                    "curl 退出码 {exit_code}：{}",
                    stderr.trim().lines().next().unwrap_or("（无错误输出）")
                ),
            });
        }
        let (body, code_line) = match stdout.rfind('\n') {
            Some(pos) => (&stdout[..pos], &stdout[pos + 1..]),
            None => (stdout, ""),
        };
        let status: u16 = code_line.trim().parse().map_err(|_| MnError::Network {
            stage: "解析响应".to_string(),
            reason: format!("无法解析 HTTP 状态码：{code_line:?}"),
        })?;
        Ok(HttpResponse::new(status, body.to_string()))
    }
}

impl Transport for CurlTransport {
    fn execute(&self, req: &HttpRequest) -> MnResult<HttpResponse> {
        let mut cmd = Command::new(&self.curl_bin);
        cmd.args(self.build_args(req));
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        cmd.stdin(if req.body.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });

        let mut child = cmd.spawn().map_err(|e| MnError::Network {
            stage: "启动 curl".to_string(),
            reason: format!(
                "无法执行 `{}`：{e}。Mangosteen 依赖系统 curl 访问云厂商 API。",
                self.curl_bin
            ),
        })?;

        // 写完必须显式 drop stdin，curl 才会读到 EOF 并发出请求
        if let Some(body) = &req.body {
            let mut stdin = child.stdin.take().ok_or_else(|| MnError::Network {
                stage: "写入请求体".to_string(),
                reason: "curl 子进程的 stdin 不可用".to_string(),
            })?;
            stdin
                .write_all(body.as_bytes())
                .map_err(|e| MnError::Network {
                    stage: "写入请求体".to_string(),
                    reason: format!("管道写入失败：{e}"),
                })?;
        }
        let out = child.wait_with_output().map_err(|e| MnError::Network {
            stage: "等待 curl".to_string(),
            reason: e.to_string(),
        })?;

        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        let code = out.status.code().unwrap_or(-1);
        CurlTransport::parse_output(&stdout, code, &stderr)
    }
}

/// 测试用传输替身：按预设队列返回响应，并记录收到的请求。
pub struct MockTransport {
    responses: Mutex<Vec<MnResult<HttpResponse>>>,
    /// 收到过的请求（按顺序）
    pub recorded: Mutex<Vec<HttpRequest>>,
}

impl MockTransport {
    /// 始终返回同一个响应。
    pub fn always(status: u16, body: impl Into<String>) -> Self {
        let body = body.into();
        Self {
            responses: Mutex::new(
                (0..32)
                    .map(|_| Ok(HttpResponse::new(status, body.clone())))
                    .collect(),
            ),
            recorded: Mutex::new(Vec::new()),
        }
    }

    /// 按顺序给出响应体（均视为 200）。
    pub fn sequence(bodies: impl IntoIterator<Item = String>) -> Self {
        Self {
            responses: Mutex::new(
                bodies
                    .into_iter()
                    .map(|b| Ok(HttpResponse::new(200, b)))
                    .collect(),
            ),
            recorded: Mutex::new(Vec::new()),
        }
    }

    /// 始终返回网络错误。
    pub fn always_fail(reason: &str) -> Self {
        let reason = reason.to_string();
        Self {
            responses: Mutex::new(
                (0..32)
                    .map(|_| {
                        Err(MnError::Network {
                            stage: "mock".to_string(),
                            reason: reason.clone(),
                        })
                    })
                    .collect(),
            ),
            recorded: Mutex::new(Vec::new()),
        }
    }

    /// 收到的请求数。
    pub fn call_count(&self) -> usize {
        self.recorded.lock().map(|v| v.len()).unwrap_or(0)
    }

    /// 复制收到的请求列表。
    pub fn requests(&self) -> Vec<HttpRequest> {
        self.recorded.lock().map(|v| v.clone()).unwrap_or_default()
    }
}

impl Transport for MockTransport {
    fn execute(&self, req: &HttpRequest) -> MnResult<HttpResponse> {
        self.recorded
            .lock()
            .map_err(|_| MnError::io("mock lock", &std::io::Error::other("poisoned")))?
            .push(req.clone());

        let mut q = self
            .responses
            .lock()
            .map_err(|_| MnError::io("mock lock", &std::io::Error::other("poisoned")))?;
        if q.is_empty() {
            return Err(MnError::Network {
                stage: "mock".to_string(),
                reason: "没有更多预设响应（调用次数超出预期）".to_string(),
            });
        }
        q.remove(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_post_sets_content_type() {
        let r = HttpRequest::form_post("https://api/x", "Action=Y");
        assert_eq!(r.method, "POST");
        assert_eq!(
            r.header("content-type"),
            Some("application/x-www-form-urlencoded")
        );
        assert_eq!(r.body.as_deref(), Some("Action=Y"));
    }

    #[test]
    fn response_success_range() {
        assert!(HttpResponse::new(200, "").is_success());
        assert!(HttpResponse::new(204, "").is_success());
        assert!(!HttpResponse::new(300, "").is_success());
        assert!(!HttpResponse::new(500, "").is_success());
    }

    #[test]
    fn parse_output_splits_status() {
        let r = CurlTransport::parse_output("{\"a\":1}\n200", 0, "").unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body, "{\"a\":1}");
    }

    #[test]
    fn parse_output_multiline_body() {
        let out = "line1\nline2\n404";
        let r = CurlTransport::parse_output(out, 0, "").unwrap();
        assert_eq!(r.status, 404);
        assert_eq!(r.body, "line1\nline2");
    }

    #[test]
    fn network_exit_codes_map_to_stage() {
        let e = CurlTransport::parse_output("", 7, "Connection refused").unwrap_err();
        assert!(e.to_string().contains("TCP 连接"));
        assert!(e.is_retryable());
        let e = CurlTransport::parse_output("", 28, "timed out").unwrap_err();
        assert!(e.to_string().contains("超时"));
    }

    #[test]
    fn args_use_stdin_for_body_and_never_inline_it() {
        let t = CurlTransport::new(12);
        let req = HttpRequest::form_post("https://api.compshare.cn", "Signature=secret-value");
        let args = t.build_args(&req);
        // body 必须走 stdin，绝不出现在参数里（否则 ps 可见）
        assert!(args.iter().any(|a| a == "@-"), "{args:?}");
        assert!(
            !args.iter().any(|a| a.contains("secret-value")),
            "请求体内容不得进入进程参数：{args:?}"
        );
        assert_eq!(
            args.last().map(|s| s.as_str()),
            Some("https://api.compshare.cn")
        );
    }

    #[test]
    fn args_without_body_do_not_read_stdin() {
        let t = CurlTransport::default();
        let req = HttpRequest {
            method: "POST".into(),
            url: "https://x/y".into(),
            headers: vec![],
            body: None,
        };
        let args = t.build_args(&req);
        assert!(!args.iter().any(|a| a == "@-"));
    }

    #[test]
    fn mock_records_and_sequences() {
        let m = MockTransport::sequence(vec!["a".to_string(), "b".to_string()]);
        let t: &dyn Transport = &m;
        assert_eq!(
            t.execute(&HttpRequest::form_post("u", "x")).unwrap().body,
            "a"
        );
        assert_eq!(
            t.execute(&HttpRequest::form_post("u", "x")).unwrap().body,
            "b"
        );
        assert_eq!(m.call_count(), 2);
        assert_eq!(m.requests()[0].body.as_deref(), Some("x"));
    }

    #[test]
    fn mock_failure_is_retryable() {
        let m = MockTransport::always_fail("boom");
        let e = m.execute(&HttpRequest::form_post("u", "x")).unwrap_err();
        assert!(e.to_string().contains("boom"));
        assert!(e.is_retryable());
    }

    #[test]
    fn missing_curl_is_actionable() {
        let t = CurlTransport {
            timeout_secs: 3,
            curl_bin: "/nonexistent/curl-xyz".to_string(),
        };
        let e = t
            .execute(&HttpRequest::form_post("https://x/y", "a=b"))
            .unwrap_err();
        assert!(e.to_string().contains("curl"), "got {e}");
    }

    #[test]
    fn real_curl_roundtrip_when_available() {
        // 用本地文件 URL 之外的方式难以离线验证，这里只验证「能启动 curl」，
        // 真正联网的验证由 MANGOSTEEN_LIVE_NETWORK_TESTS 控制。
        if std::env::var("MANGOSTEEN_LIVE_NETWORK_TESTS").is_err() {
            return;
        }
        let t = CurlTransport::new(20);
        let r = t
            .execute(&HttpRequest::form_post(
                "https://httpbin.org/post",
                "hello=world",
            ))
            .expect("live request");
        assert!(r.status == 200 || r.status == 503);
    }
}
