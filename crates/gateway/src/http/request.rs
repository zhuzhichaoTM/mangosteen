//! HTTP 请求解析：请求行、头部、请求体、路径解码与请求级工具。
//!
//! 归属：**第二层 · API Server**。只用标准库实现（理由见
//! [`super::server`] 的模块文档）。

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};

use mangosteen_core::{MnError, MnResult};

pub(crate) const MAX_BODY: usize = 32 * 1024 * 1024;

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
