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
    http_post_json_with_key(url, body, timeout, None)
}

/// 带上游 API Key 的 JSON POST（safety 分支，改动点 2 的网关侧）。
///
/// `api_key` 为 `Some` 时附 `Authorization: Bearer <key>`——适配启用
/// `--api-key-file` 的 llama-server。`None` 时行为与旧版逐字节一致
/// （兼容未加固的部署与本地 Ollama 链路）。
pub fn http_post_json_with_key(
    url: &str,
    body: &[u8],
    timeout: Duration,
    api_key: Option<&str>,
) -> MnResult<(u16, String)> {
    http_post_json_with_key_tls(url, body, timeout, api_key, false, None)
}

/// 带 TLS 标志的版本（P1 传输加密）：tls=true 时 URL 是 https（自签
/// 证书），裸 TcpStream 无法完成 TLS 握手——改走系统 curl（`-k` 跳过
/// 证书链校验，证书是引导链自签的）。tls=false 时保持裸 TCP 快路径。
pub fn http_post_json_with_key_tls(
    url: &str,
    body: &[u8],
    timeout: Duration,
    api_key: Option<&str>,
    tls: bool,
    cacert: Option<&std::path::Path>,
) -> MnResult<(u16, String)> {
    if tls {
        // curl 快路径：--silent 拿状态码+体（-w 分隔）。
        // P0 证书钉扎：有本地钉扎证书时 --cacert 强校验，无则 --insecure 降级。
        let mut cmd = std::process::Command::new("curl");
        cmd.arg("--silent")
            .arg("--show-error")
            .arg("--noproxy")
            .arg("*");
        if let Some(cert) = cacert {
            cmd.arg("--cacert").arg(cert);
        } else {
            cmd.arg("--insecure");
        }
        cmd
            .arg("--max-time")
            .arg(timeout.as_secs().to_string())
            .arg("-X")
            .arg("POST")
            .arg("-H")
            .arg("Content-Type: application/json")
            .arg("--data-binary")
            .arg("@-")
            .arg("--write-out")
            .arg("\n%{http_code}")
            .arg(url);
        // P1-4（2026-11-19）：与 relay.rs 流式路径同款——密钥走 curl
        // @file 语法（临时 600 文件），argv/ps 不含明文 token，用完即删。
        // 写临时文件失败才退回 argv（可用性优先）。
        if let Some(key) = api_key {
            let key_path = std::env::temp_dir().join(format!("mn-key-up-{}.tmp", std::process::id()));
            match std::fs::write(&key_path, key.as_bytes()) {
                Ok(()) => {
                    let _ = mangosteen_platform::fs::set_private_permissions(&key_path);
                    cmd.arg("-H")
                        .arg(format!("Authorization: Bearer @{}", key_path.display()));
                }
                Err(_) => {
                    cmd.arg("-H").arg(format!("Authorization: Bearer {key}"));
                }
            }
        }
        use std::io::Write as _;
        let mut child = cmd
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| MnError::Network {
                stage: "启动 curl".to_string(),
                reason: format!("{e}"),
            })?;
        // curl 已启动（header 已读取）→ 立即删除临时 key 文件
        if let Some(key) = api_key {
            let _ = std::fs::remove_file(
                std::env::temp_dir().join(format!("mn-key-up-{}.tmp", std::process::id())),
            );
        }
        child
            .stdin
            .as_mut()
            .ok_or_else(|| MnError::Network {
                stage: "curl stdin".to_string(),
                reason: "stdin 不可用".to_string(),
            })?
            .write_all(body)
            .map_err(|e| MnError::Network {
                stage: "curl stdin 写入".to_string(),
                reason: format!("{e}"),
            })?;
        let out = child
            .wait_with_output()
            .map_err(|e| MnError::Network {
                stage: "curl 等待".to_string(),
                reason: format!("{e}"),
            })?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let mut lines = stdout.rsplitn(2, '\n');
        let code = lines
            .next()
            .and_then(|c| c.trim().parse::<u16>().ok())
            .unwrap_or(0);
        let resp_body = lines.next().unwrap_or("").to_string();
        if code == 0 {
            return Err(MnError::Network {
                stage: "上游推理服务".to_string(),
                reason: format!(
                    "curl 失败：{}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
            });
        }
        return Ok((code, resp_body));
    }
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

    let auth_header = api_key
        .map(|k| format!("Authorization: Bearer {k}\r\n"))
        .unwrap_or_default();
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: {host_port}\r\nContent-Type: application/json\r\n{auth_header}Content-Length: {}\r\nConnection: close\r\n\r\n",
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
