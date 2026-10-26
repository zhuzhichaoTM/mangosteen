//! 多云统一适配层的 HTTP 传输抽象。
//!
//! 归属：**算力执行层 · 多云统一适配层**（驱动内部设施）。
//!
//! ## 为什么要抽这一层
//!
//! 服务商驱动的**业务逻辑**（如何拼 URL、如何映射字段、如何归一化状态）
//! 才是本项目的价值所在，也才是需要被测试覆盖的部分。若驱动直接调用
//! HTTP 客户端，测试就必须联网或起 mock 服务器，覆盖率会因此塌陷。
//!
//! 因此：所有驱动只依赖 [`Transport`] trait，单测注入 [`MockTransport`]，
//! 生产注入 [`crate::drivers::curl::CurlTransport`]（基于系统 curl，无新增依赖）。

use std::collections::VecDeque;
use std::sync::Mutex;

use mangosteen_core::{MnError, MnResult};

/// HTTP 请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    /// 方法（GET / POST / DELETE）
    pub method: String,
    /// 完整 URL
    pub url: String,
    /// 请求头
    pub headers: Vec<(String, String)>,
    /// 请求体（可为空）
    pub body: Option<String>,
}

impl HttpRequest {
    /// 构造 GET。
    pub fn get(url: impl Into<String>) -> Self {
        Self {
            method: "GET".to_string(),
            url: url.into(),
            headers: Vec::new(),
            body: None,
        }
    }

    /// 构造 POST。
    pub fn post(url: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            method: "POST".to_string(),
            url: url.into(),
            headers: vec![("Content-Type".to_string(), "application/json".to_string())],
            body: Some(body.into()),
        }
    }

    /// 构造 DELETE。
    pub fn delete(url: impl Into<String>) -> Self {
        Self {
            method: "DELETE".to_string(),
            url: url.into(),
            headers: Vec::new(),
            body: None,
        }
    }

    /// 添加请求头。
    pub fn with_header(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.headers.push((k.into(), v.into()));
        self
    }

    /// 取请求头。
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// HTTP 响应。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    /// 状态码
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

/// 传输抽象。实现须为 `Send + Sync` 以便驱动可跨线程。
pub trait Transport: Send + Sync {
    /// 执行请求。
    fn execute(&self, req: &HttpRequest) -> MnResult<HttpResponse>;
}

/// `Arc<dyn Transport>` 也是 `Transport`，便于驱动注册表共享同一传输后端。
impl Transport for std::sync::Arc<dyn Transport + Send + Sync> {
    fn execute(&self, req: &HttpRequest) -> MnResult<HttpResponse> {
        (**self).execute(req)
    }
}

/// 测试用传输替身：按预设队列返回响应，并记录收到的请求。
///
/// 支持「同一次调用按顺序返回不同响应」，用于验证重试逻辑。
pub struct MockTransport {
    responses: Mutex<VecDeque<MnResult<HttpResponse>>>,
    /// 收到过的请求（按顺序）
    pub recorded: Mutex<Vec<HttpRequest>>,
}

impl MockTransport {
    /// 按顺序给出响应（字符串 → 200）。
    pub fn sequence(bodies: impl IntoIterator<Item = String>) -> Self {
        let responses = bodies
            .into_iter()
            .map(|b| Ok(HttpResponse::new(200, b)))
            .collect();
        Self {
            responses: Mutex::new(responses),
            recorded: Mutex::new(Vec::new()),
        }
    }

    /// 给出带状态码的响应序列。
    pub fn statuses(codes: impl IntoIterator<Item = u16>) -> Self {
        let responses = codes
            .into_iter()
            .map(|c| Ok(HttpResponse::new(c, "{}")))
            .collect();
        Self {
            responses: Mutex::new(responses),
            recorded: Mutex::new(Vec::new()),
        }
    }

    /// 始终返回同一个响应。
    pub fn always(status: u16, body: impl Into<String>) -> Self {
        let body = body.into();
        Self {
            responses: Mutex::new(VecDeque::from(vec![
                Ok(HttpResponse::new(
                    status,
                    body.clone()
                ));
                16
            ])),
            recorded: Mutex::new(Vec::new()),
        }
    }

    /// 始终返回网络错误。
    pub fn always_fail(reason: &str) -> Self {
        let reason = reason.to_string();
        Self {
            responses: Mutex::new(
                (0..16)
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
        q.pop_front().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_builders() {
        let g = HttpRequest::get("https://x/y").with_header("Authorization", "Bearer t");
        assert_eq!(g.method, "GET");
        assert_eq!(g.header("authorization"), Some("Bearer t"));
        assert!(g.body.is_none());

        let p = HttpRequest::post("https://x/y", r#"{"a":1}"#);
        assert_eq!(p.method, "POST");
        assert_eq!(p.header("Content-Type"), Some("application/json"));
        assert!(p.body.is_some());

        let d = HttpRequest::delete("https://x/z");
        assert_eq!(d.method, "DELETE");
    }

    #[test]
    fn response_success_range() {
        assert!(HttpResponse::new(200, "").is_success());
        assert!(HttpResponse::new(204, "").is_success());
        assert!(HttpResponse::new(299, "").is_success());
        assert!(!HttpResponse::new(300, "").is_success());
        assert!(!HttpResponse::new(404, "").is_success());
        assert!(!HttpResponse::new(500, "").is_success());
    }

    #[test]
    fn mock_returns_sequence_in_order() {
        let m = MockTransport::sequence(vec!["a".to_string(), "b".to_string()]);
        let t: &dyn Transport = &m;
        let r1 = t.execute(&HttpRequest::get("u")).unwrap();
        let r2 = t.execute(&HttpRequest::get("u")).unwrap();
        assert_eq!(r1.body, "a");
        assert_eq!(r2.body, "b");
        assert_eq!(m.call_count(), 2);
    }

    #[test]
    fn mock_exhaustion_is_an_error() {
        let m = MockTransport::sequence(vec!["a".to_string()]);
        assert!(m.execute(&HttpRequest::get("u")).is_ok());
        let e = m.execute(&HttpRequest::get("u")).unwrap_err();
        assert!(e.to_string().contains("超出预期"));
    }

    #[test]
    fn mock_fail_mode() {
        let m = MockTransport::always_fail("boom");
        let e = m.execute(&HttpRequest::get("u")).unwrap_err();
        assert!(e.to_string().contains("boom"));
    }

    #[test]
    fn mock_records_full_request() {
        let m = MockTransport::always(200, "{}");
        m.execute(&HttpRequest::post("https://api/x", "{}").with_header("X-A", "1"))
            .unwrap();
        let reqs = m.requests();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].url, "https://api/x");
        assert_eq!(reqs[0].header("X-A"), Some("1"));
    }
}
