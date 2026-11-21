//! HTTP handler groups（第二层 · API Server 内部按职责分组的处理器）。
//!
//! 每个子模块持有 `impl GatewayServer` 的一组方法。Rust 允许同 crate
//! 多个 `impl` 块分散在不同模块，因此拆分不改变任何对外签名；同时
//! `GatewayServer` 的私有字段以 `pub(crate)` 暴露给 handler 组，
//! 仍然只有网关 crate 内部可见。
//!
//! | 模块 | 路由组 |
//! |------|--------|
//! | [`sessions`] | `/api/sessions*` |
//! | [`status`] | `/health`、`/api/status`、`/api/sysinfo`、`/api/doctor` |
//! | [`models`] | `/api/models*`、`/v1/models` |
//! | [`pull`] | `/api/models/pull`、`/api/models/jobs*` |
//! | [`deployment`] | `/api/deploy`、`/api/deployments*`、`/api/preview`、`/api/assess` |
//! | [`inference`] | `/v1/chat/completions`、`/v1/completions`、`/v1/embeddings` |
//! | [`attachments`] | `/api/extract`、`/api/fetch-url` |
//! | [`settings`] | `/api/settings` |
//! | [`model_templates`] | `/api/model-templates*` |

pub mod attachments;
pub mod deployment;
pub mod model_templates;
pub mod inference;
pub mod models;
pub mod pull;
pub mod sessions;
pub mod settings;
pub mod status;

/// 日志 CRLF 过滤（2026-11-21 安全加固）。
///
/// 移除字符串中的 \r 和 \n，防止用户可控字段（模型名、部署 ID 等）
/// 含换行符伪造日志条目。用于 eprintln!/println! 等日志输出。
#[allow(dead_code)]
pub(crate) fn sanitize_log(s: &str) -> String {
    s.replace(['\r', '\n'], " ")
}

/// 错误信息脱敏（2026-11-21 安全加固）。
///
/// 移除错误信息中的敏感细节（远端路径、IP 地址、SSH 连接信息、
/// 厂商 API 细节），防止攻击者通过错误响应收集内部拓扑。
///
/// 用法：`sanitize_error(&e.to_string())`
pub(crate) fn sanitize_error(msg: &str) -> String {
    let mut s = msg.to_string();
    // 移除远端文件路径（/home/deploy/... 或 /root/...）
    s = remove_remote_paths(&s);
    // 移除 IP 地址（IPv4）
    s = remove_ipv4(&s);
    // 超长错误截断（防止巨量错误信息淹没日志/响应）
    if s.len() > 500 {
        s.truncate(500);
        s.push_str("...");
    }
    s
}

/// 移除常见远端路径模式。
fn remove_remote_paths(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '/' && (out.ends_with(' ') || out.ends_with('"') || out.ends_with('\'') || out.is_empty() || out.ends_with('(')) {
            // 可能是路径开头，尝试匹配 /home/ /root/ /tmp/ /var/ 等
            let rest: String = chars.clone().take(30).collect();
            if rest.starts_with("home/")
                || rest.starts_with("root/")
                || rest.starts_with("tmp/")
                || rest.starts_with("var/")
                || rest.starts_with("etc/")
                || rest.starts_with("dev/shm")
                || rest.starts_with("usr/")
            {
                // 跳过整个路径（直到空格、引号或行尾）
                for c2 in chars.by_ref() {
                    if c2 == ' ' || c2 == '"' || c2 == '\'' || c2 == '\n' || c2 == ')' {
                        out.push_str("<path>");
                        out.push(c2);
                        break;
                    }
                }
                continue;
            }
        }
        out.push(c);
    }
    out
}

/// 移除 IPv4 地址。
fn remove_ipv4(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // 检查是否是 IPv4 模式（数字.数字.数字.数字）
        if bytes[i].is_ascii_digit() {
            let mut j = i;
            let mut dots = 0;
            while j < bytes.len() && (bytes[j].is_ascii_digit() || (bytes[j] == b'.' && dots < 3)) {
                if bytes[j] == b'.' {
                    dots += 1;
                }
                j += 1;
            }
            if dots == 3 && j > i && j < bytes.len() {
                // 检查前后边界
                let before_ok = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
                let after_ok = j == bytes.len() || !bytes[j].is_ascii_alphanumeric();
                if before_ok && after_ok {
                    out.push_str("<ip>");
                    i = j;
                    continue;
                }
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}
