//! 网页正文抓取：把 URL 转成可喂给模型的纯文本。
//!
//! 归属：**基础能力层**。
//!
//! ## 为什么这是「联网搜索」的第一层
//!
//! 真正的关键词搜索需要第三方搜索 API（要密钥、要计费）。而「让模型读
//! 一个网页」只需要 HTTP + HTML 清洗，**零密钥、零依赖**即可实现。
//!
//! 因此这里的定位是：**可插拔搜索的兜底路径**。用户没配搜索 API 时，
//! 仍可以粘贴 URL 让模型读内容；配了搜索 API 时，搜索结果由上层转换为
//! URL 列表后同样走本模块抓取正文。
//!
//! ## 依赖
//!
//! 用系统 `curl`（与云端驱动、模型下载一致）：
//!
//! | 需求 | curl |
//! |------|------|
//! | HTTPS | 自带 TLS |
//! | 跟随重定向 | `--location` |
//! | 超时 | `--max-time` / `--connect-timeout` |
//! | 自定义 UA | `--user-agent` |
//! | 限制体积 | `--max-filesize` |
//!
//! 不引入 HTML 解析库：目标是「去掉标签留文字」，用状态机足够，
//! 而完整的 DOM 解析器（html5ever 等）会带来显著体积。

use std::process::Command;

use mangosteen_core::{MnError, MnResult};

/// 抓取结果。
#[derive(Debug, Clone, PartialEq)]
pub struct FetchedPage {
    /// 最终 URL（跟随重定向后）
    pub final_url: String,
    /// 页面标题（能提取到时）
    pub title: Option<String>,
    /// 正文纯文本
    pub text: String,
    /// HTTP 状态码
    pub status: u16,
    /// 原始内容类型
    pub content_type: String,
    /// 原始字节数
    pub bytes: u64,
    /// 是否被截断
    pub truncated: bool,
    /// 提示
    pub warnings: Vec<String>,
}

/// 抓取选项。
#[derive(Debug, Clone)]
pub struct FetchOptions {
    /// 单次超时（秒）
    pub timeout_secs: u64,
    /// 响应体体积上限（字节）。防止误抓大文件把内存吃光
    pub max_bytes: u64,
    /// 提取文本的字符上限
    pub max_chars: usize,
    /// curl 可执行文件
    pub curl_bin: String,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            timeout_secs: 20,
            max_bytes: 8 * 1024 * 1024,
            max_chars: 60_000,
            curl_bin: "curl".to_string(),
        }
    }
}

/// 抓取并提取正文。
pub fn fetch(url: &str, opts: &FetchOptions) -> MnResult<FetchedPage> {
    validate_url(url)?;

    let mut cmd = Command::new(&opts.curl_bin);
    cmd.arg("--fail")
        .arg("--location")
        .arg("--silent")
        .arg("--show-error")
        .arg("--compressed")
        .arg("--connect-timeout")
        .arg("10")
        .arg("--max-time")
        .arg(opts.timeout_secs.to_string())
        .arg("--max-filesize")
        .arg(opts.max_bytes.to_string())
        // 声明为普通浏览器：部分站点对默认 curl UA 直接 403
        .arg("--user-agent")
        .arg("Mozilla/5.0 (compatible; Mangosteen/0.1; +https://github.com/)")
        .arg("--write-out")
        .arg("\n__META__%{http_code}|%{content_type}|%{size_download}|%{url_effective}")
        .arg(url);

    let out = cmd.output().map_err(|e| MnError::Network {
        stage: "抓取网页".to_string(),
        reason: format!("无法启动 {}：{e}", opts.curl_bin),
    })?;

    if !out.status.success() {
        let code = out.status.code().unwrap_or(-1);
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(MnError::Network {
            stage: "抓取网页".to_string(),
            reason: match code {
                22 => format!(
                    "服务器返回错误（HTTP 4xx/5xx）：{err}。\
                     若目标站点需要登录或反爬，可换一个公开页面"
                ),
                6 | 7 => format!("无法连接：{err}。检查网络或代理设置"),
                28 => format!("超时（{}s）：{err}", opts.timeout_secs),
                63 => format!("响应体超过 {} 字节上限，已中止：{err}", opts.max_bytes),
                _ => format!("curl 退出码 {code}：{err}"),
            },
        });
    }

    // 从 stdout 尾部解析 meta 行
    let raw = out.stdout;
    let (body_bytes, meta) = split_meta(&raw);
    let (status, content_type, bytes, final_url) =
        meta.unwrap_or((0, String::new(), 0, url.to_string()));

    let mut warnings = Vec::new();

    // 非 HTML/文本内容：明确拒绝而不是塞乱码给模型
    let ct_lower = content_type.to_ascii_lowercase();
    if !ct_lower.is_empty()
        && !ct_lower.contains("text")
        && !ct_lower.contains("json")
        && !ct_lower.contains("xml")
        && !ct_lower.contains("html")
    {
        return Err(MnError::bad_request(format!(
            "该 URL 返回的是 `{content_type}`，不是可读文本。\
             只支持 text/html、text/plain、application/json、application/xml"
        )));
    }

    let body = String::from_utf8_lossy(&body_bytes).to_string();
    let (title, text) = if ct_lower.contains("html")
        || body.trim_start().starts_with("<!DOCTYPE")
        || body.trim_start().starts_with("<html")
    {
        html_to_text(&body)
    } else {
        (None, body)
    };

    if text.trim().is_empty() {
        warnings.push(
            "页面正文为空：可能是纯前端渲染（JS 动态加载）或需要登录。\
             可尝试该站点的「阅读版」或 RSS 地址"
                .to_string(),
        );
    }

    let (text, truncated) = truncate_chars(text, opts.max_chars);
    Ok(FetchedPage {
        final_url,
        title,
        text,
        status,
        content_type,
        bytes,
        truncated,
        warnings,
    })
}

/// 校验 URL 基本形状。
fn validate_url(url: &str) -> MnResult<()> {
    let u = url.trim();
    if u.is_empty() {
        return Err(MnError::bad_request("URL 为空"));
    }
    if !u.starts_with("http://") && !u.starts_with("https://") {
        return Err(MnError::bad_request(format!(
            "URL 必须以 http:// 或 https:// 开头，实际：`{u}`"
        )));
    }
    // 拒绝把非 HTTP 协议塞进来（file://、ftp:// 等会被 curl 支持，
    // 但那是本地文件读取，应走附件上传路径并受访问控制约束）
    if let Some(scheme_end) = u.find("://") {
        let scheme = &u[..scheme_end];
        if !matches!(scheme, "http" | "https") {
            return Err(MnError::bad_request(format!(
                "只允许 http/https，`{scheme}` 不受支持"
            )));
        }
    }
    Ok(())
}

/// 拆分 stdout 的正文与 meta 行。
fn split_meta(raw: &[u8]) -> (Vec<u8>, Option<(u16, String, u64, String)>) {
    const MARK: &[u8] = b"\n__META__";
    // 从尾部找标记（正文里理论上不会出现该串，但仍从后往前找更稳）
    let pos = raw.windows(MARK.len()).rposition(|w| w == MARK);
    let Some(pos) = pos else {
        return (raw.to_vec(), None);
    };
    let body = raw[..pos].to_vec();
    let meta_line = String::from_utf8_lossy(&raw[pos + MARK.len()..]).to_string();
    let parts: Vec<&str> = meta_line.trim().split('|').collect();
    if parts.len() < 4 {
        return (body, None);
    }
    let status = parts[0].parse().unwrap_or(0);
    let ct = parts[1].to_string();
    let bytes = parts[2].parse().unwrap_or(0);
    let url = parts[3].to_string();
    (body, Some((status, ct, bytes, url)))
}

/// 按字符截断。
fn truncate_chars(s: String, max: usize) -> (String, bool) {
    if s.chars().count() <= max {
        return (s, false);
    }
    (s.chars().take(max).collect(), true)
}

/// HTML → (标题, 正文纯文本)。
///
/// 状态机实现，处理四类必须丢弃的区域：
///
/// | 标签 | 为什么必须丢 |
/// |------|-------------|
/// | `<script>` | JS 源码对模型是噪声，且可能极长 |
/// | `<style>` | CSS 同上 |
/// | `<noscript>` | 提示文案，无信息量 |
/// | `<svg>` | 路径数据完全没有语义 |
///
/// 其余标签只作为「块边界」插入换行，不保留任何标签本身——
/// 保留标签会浪费大量 token 且降低模型对内容的理解。
pub fn html_to_text(html: &str) -> (Option<String>, String) {
    // 注意：不包含 "head"。`<title>` 在 head 内，若整体跳过 head 就取不到标题。
    // head 里除 title 外只有 meta/link（不产生文本）与 style/script（已单独跳过），
    // 因此保留 head 不会引入噪声。
    const SKIP: &[&str] = &["script", "style", "noscript", "svg", "iframe", "canvas"];
    const BLOCK: &[&str] = &[
        "p",
        "div",
        "br",
        "tr",
        "li",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "section",
        "article",
        "blockquote",
        "pre",
        "hr",
        "table",
        "ul",
        "ol",
        "dt",
        "dd",
    ];

    let mut out = String::with_capacity(html.len() / 2);
    let mut title: Option<String> = None;
    let mut chars = html.chars().peekable();
    let mut skip_depth = 0usize;
    let mut in_title = false;
    let mut title_buf = String::new();

    while let Some(c) = chars.next() {
        if c != '<' {
            if skip_depth == 0 {
                if in_title {
                    title_buf.push(c);
                } else if c == '&' {
                    // 实体解码（常用几个）
                    let mut ent = String::new();
                    while let Some(&n) = chars.peek() {
                        if n == ';' || ent.len() > 8 {
                            break;
                        }
                        ent.push(n);
                        chars.next();
                    }
                    if chars.peek() == Some(&';') {
                        chars.next();
                    }
                    out.push(decode_entity(&ent));
                } else {
                    out.push(c);
                }
            }
            continue;
        }

        // 收集标签
        let mut tag = String::new();
        for n in chars.by_ref() {
            if n == '>' {
                break;
            }
            tag.push(n);
        }
        let is_close = tag.starts_with('/');
        let name: String = tag
            .trim_start_matches('/')
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();

        if name == "title" {
            if is_close {
                if in_title && title.is_none() {
                    let t = collapse_ws(&title_buf);
                    if !t.is_empty() {
                        title = Some(t);
                    }
                }
                in_title = false;
            } else {
                in_title = true;
            }
            continue;
        }

        if SKIP.contains(&name.as_str()) {
            if is_close {
                skip_depth = skip_depth.saturating_sub(1);
            } else if !tag.trim_end().ends_with('/') {
                skip_depth += 1;
            }
            continue;
        }

        if skip_depth > 0 {
            continue;
        }

        if BLOCK.contains(&name.as_str()) {
            out.push('\n');
        }
    }

    // 清理：合并空白、去掉空行过多
    let text = out
        .lines()
        .map(|l| collapse_ws(l))
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    (title, text)
}

/// 折叠行内空白。
fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for c in s.chars() {
        let is_ws = c == ' ' || c == '\t' || c == '\r' || c == '\n';
        if is_ws {
            if !prev_space && !out.is_empty() {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    out.trim_end().to_string()
}

/// 常用 HTML 实体解码。
fn decode_entity(name: &str) -> char {
    match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => ' ',
        "mdash" => '—',
        "ndash" => '–',
        "hellip" => '…',
        "ldquo" => '“',
        "rdquo" => '”',
        "lsquo" => '‘',
        "rsquo" => '’',
        _ => {
            // 数字实体 &#123; / &#x1F600;
            if let Some(rest) = name.strip_prefix('#') {
                let code =
                    if let Some(hex) = rest.strip_prefix('x').or_else(|| rest.strip_prefix('X')) {
                        u32::from_str_radix(hex, 16).ok()
                    } else {
                        rest.parse::<u32>().ok()
                    };
                if let Some(c) = code.and_then(char::from_u32) {
                    return c;
                }
            }
            ' '
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_scheme() {
        assert!(validate_url("https://example.com").is_ok());
        assert!(validate_url("http://example.com").is_ok());
        assert!(validate_url("ftp://example.com").is_err());
        assert!(validate_url("file:///etc/passwd").is_err());
        assert!(validate_url("example.com").is_err());
        assert!(validate_url("").is_err());
    }

    #[test]
    fn strips_script_and_style() {
        let html = "<html><head><title>T</title><style>body{color:red}</style></head>\
                    <body><script>var x=1;</script><p>正文一</p><p>正文二</p></body></html>";
        let (title, text) = html_to_text(html);
        assert_eq!(title.as_deref(), Some("T"));
        assert!(text.contains("正文一"));
        assert!(text.contains("正文二"));
        assert!(!text.contains("script"), "脚本内容必须丢弃：{text}");
        assert!(!text.contains("color:red"), "样式内容必须丢弃：{text}");
    }

    #[test]
    fn skips_nested_svg_and_iframe() {
        let html = "<body><svg><path d='M0 0'/></svg><iframe src='x'></iframe><p>保留</p></body>";
        let (_, text) = html_to_text(html);
        assert_eq!(text.trim(), "保留");
    }

    #[test]
    fn inserts_block_breaks() {
        let html = "<div>行1</div><div>行2</div><br><p>行3</p>";
        let (_, text) = html_to_text(html);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines, vec!["行1", "行2", "行3"]);
    }

    #[test]
    fn decodes_entities() {
        let html = "<p>a &amp; b &lt;tag&gt; &quot;q&quot; &#65;</p>";
        let (_, text) = html_to_text(html);
        assert!(text.contains("a & b <tag> \"q\" A"), "got {text}");
    }

    #[test]
    fn title_only_from_first_title_tag() {
        let html = "<title>第一个</title><title>第二个</title>";
        let (t, _) = html_to_text(html);
        assert_eq!(t.as_deref(), Some("第一个"));
    }

    #[test]
    fn meta_split_handles_absent_marker() {
        let (body, meta) = split_meta(b"hello");
        assert_eq!(body, b"hello");
        assert!(meta.is_none());
    }

    #[test]
    fn meta_split_extracts_fields() {
        let raw = b"<html>x</html>\n__META__200|text/html; charset=utf-8|123|https://a.com/b";
        let (body, meta) = split_meta(raw);
        assert_eq!(body, b"<html>x</html>");
        let (status, ct, bytes, url) = meta.unwrap();
        assert_eq!(status, 200);
        assert!(ct.contains("text/html"));
        assert_eq!(bytes, 123);
        assert_eq!(url, "https://a.com/b");
    }

    #[test]
    fn truncate_by_chars() {
        let (s, t) = truncate_chars("汉".repeat(50), 10);
        assert!(t);
        assert_eq!(s.chars().count(), 10);
    }

    #[test]
    fn collapse_ws_merges_spaces() {
        assert_eq!(collapse_ws("a   b\t\tc\n"), "a b c");
    }

    #[test]
    fn plain_text_passthrough_without_html() {
        // 非 HTML 内容不应被 html_to_text 破坏（由 fetch 里的判断保证）
        let (_, text) = html_to_text("纯文本无标签");
        assert!(text.contains("纯文本无标签"));
    }
}
