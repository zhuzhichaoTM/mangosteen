//! 上游推理转发：chunked 写出、直连 HTTP 客户端与响应解码。
//!
//! 归属：**第二层 · API Server**。上游一定是本机或 SSH 隧道后的
//! `127.0.0.1`，因此不需要 TLS。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use mangosteen_core::{MnError, MnResult};

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
