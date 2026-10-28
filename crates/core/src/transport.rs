//! 跨层 HTTP 传输契约。
//!
//! 归属：**第五层·基础支撑**。只定义 DTO 与 trait，不做任何 IO；
//! 各云驱动与厂商协议适配共享同一份请求/响应类型，避免重复抽象。

use crate::{MnError, MnResult};

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

    /// 构造表单 POST（`application/x-www-form-urlencoded`）。
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

    /// 构造 JSON POST（`application/json`）。
    ///
    /// 与 [`Self::post`] 等价，保留独立命名让「按 JSON 发送」在调用点显式可读。
    pub fn json_post(url: impl Into<String>, body: impl Into<String>) -> Self {
        Self::post(url, body)
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

/// `&T` 也是 `Transport`（`T: Transport`）。
///
/// 跨可用区批量查询需要「同一份凭证与传输、不同 region/zone」的多个客户端；
/// 要求 `T: Clone` 会把 `Arc<dyn Transport>` 之外的实现挡在门外（`dyn` 不可 Clone）。
impl<T: Transport + ?Sized> Transport for &T {
    fn execute(&self, req: &HttpRequest) -> MnResult<HttpResponse> {
        (**self).execute(req)
    }
}

/// 测试用传输替身：按预设队列返回响应，并记录收到的请求。
///
/// 支持「同一次调用按顺序返回不同响应」，用于验证重试逻辑。
pub struct MockTransport {
    responses: std::sync::Mutex<std::collections::VecDeque<MnResult<HttpResponse>>>,
    /// 收到过的请求（按顺序）
    pub recorded: std::sync::Mutex<Vec<HttpRequest>>,
}

impl MockTransport {
    fn with_queue(items: impl IntoIterator<Item = MnResult<HttpResponse>>) -> Self {
        Self {
            responses: std::sync::Mutex::new(items.into_iter().collect()),
            recorded: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// 按顺序给出响应（字符串 → 200）。
    pub fn sequence(bodies: impl IntoIterator<Item = String>) -> Self {
        Self::with_queue(
            bodies
                .into_iter()
                .map(|b| Ok(HttpResponse::new(200, b)))
                .collect::<Vec<_>>(),
        )
    }

    /// 给出带状态码的响应序列。
    pub fn statuses(codes: impl IntoIterator<Item = u16>) -> Self {
        Self::with_queue(
            codes
                .into_iter()
                .map(|c| Ok(HttpResponse::new(c, "{}")))
                .collect::<Vec<_>>(),
        )
    }

    /// 始终返回同一个响应。
    pub fn always(status: u16, body: impl Into<String>) -> Self {
        let body = body.into();
        Self::with_queue((0..32).map(|_| Ok(HttpResponse::new(status, body.clone()))))
    }

    /// 始终返回网络错误。
    pub fn always_fail(reason: &str) -> Self {
        let reason = reason.to_string();
        Self::with_queue((0..32).map(|_| {
            Err(MnError::Network {
                stage: "mock".to_string(),
                reason: reason.clone(),
            })
        }))
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

        let d = HttpRequest::delete("https://x/z");
        assert_eq!(d.method, "DELETE");
    }

    #[test]
    fn response_success_range() {
        assert!(HttpResponse::new(200, "").is_success());
        assert!(HttpResponse::new(204, "").is_success());
        assert!(HttpResponse::new(299, "").is_success());
        assert!(!HttpResponse::new(300, "").is_success());
        assert!(!HttpResponse::new(500, "").is_success());
    }

    #[test]
    fn mock_returns_sequence_in_order_and_records() {
        let m = MockTransport::sequence(vec!["a".to_string(), "b".to_string()]);
        let t: &dyn Transport = &m;
        assert_eq!(t.execute(&HttpRequest::get("u")).unwrap().body, "a");
        assert_eq!(t.execute(&HttpRequest::get("u")).unwrap().body, "b");
        assert_eq!(m.call_count(), 2);
        assert!(m
            .execute(&HttpRequest::get("u"))
            .unwrap_err()
            .to_string()
            .contains("超出预期"));
    }

    #[test]
    fn mock_fail_mode_and_statuses() {
        let m = MockTransport::always_fail("boom");
        assert!(m
            .execute(&HttpRequest::get("u"))
            .unwrap_err()
            .to_string()
            .contains("boom"));
        let s = MockTransport::statuses(vec![429, 200]);
        assert_eq!(s.execute(&HttpRequest::get("u")).unwrap().status, 429);
        assert_eq!(s.execute(&HttpRequest::get("u")).unwrap().status, 200);
    }
}
