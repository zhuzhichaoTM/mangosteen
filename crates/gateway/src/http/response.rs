//! HTTP 响应值与序列化。
//!
//! 归属：**第二层 · API Server**。

use mangosteen_core::openai::ErrorResponse;
use mangosteen_core::MnError;

use crate::protocol;

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
