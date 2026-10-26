//! 文档文本提取：把用户附件转成可喂给模型的纯文本。
//!
//! 归属：**基础能力层 → 模型管理模块的输入侧**。
//!
//! ## 为什么必须提取而不是直接塞给模型
//!
//! GGUF 模型只接受 token 序列。`.docx` / `.xlsx` / `.pdf` 是二进制容器
//! （OOXML 是 zip+XML，PDF 是对象流），直接按字节读取会得到乱码，
//! 既浪费上下文又可能让模型产生幻觉。所以必须先抽成纯文本。
//!
//! ## 两条路径
//!
//! | 类型 | 做法 | 依赖 |
//! |------|------|------|
//! | 纯文本（txt/md/csv/json/代码…） | 直接读，做编码校验 | 无 |
//! | 富文档（docx/xlsx/pdf/rtf/html） | 调系统工具转换 | 系统自带或提示安装 |
//!
//! 富文档走系统工具而不是引入解析库，理由与 HTTP 走 curl 一致：
//!
//! * `.docx` 需要 zip + XML 解析，`.pdf` 需要字体与流解码，两个库加起来
//!   的体积超过整个 Mangosteen 当前发布件；
//! * macOS 自带 `textutil`（支持 doc/docx/rtf/html/odt），Linux 通常有
//!   `pdftotext` / `antiword`，Windows 有 PowerShell 的 Word COM；
//! * 工具缺失时给出**可执行的安装建议**，而不是静默失败。
//!
//! ## 安全边界
//!
//! * 只读用户显式选择的文件，路径必须落在授权目录内（由调用方用
//!   [`AccessGuard`](crate::guard::AccessGuard) 校验）；
//! * 提取结果有长度上限，避免一个巨型文件撑爆上下文窗口；
//! * 富文档转换使用临时目录并在结束后清理。

use std::path::{Path, PathBuf};
use std::process::Command;

use mangosteen_core::{MnError, MnResult};

/// 提取结果。
#[derive(Debug, Clone, PartialEq)]
pub struct ExtractedText {
    /// 纯文本内容（可能被截断）
    pub text: String,
    /// 原始字节数
    pub original_bytes: u64,
    /// 提取方式描述（用于 UI 展示「怎么读出来的」）
    pub method: String,
    /// 是否因超长被截断
    pub truncated: bool,
    /// 提示（如编码猜测、工具缺失的降级说明）
    pub warnings: Vec<String>,
}

/// 单次提取的文本上限（字符数）。
///
/// 取 200k 字符：约等于 100k~150k token 的英文，远超常见上下文窗口。
/// 真正的裁剪交给调用方按模型上下文做，这里只防「一个 500MB 日志把内存吃光」。
pub const MAX_TEXT_CHARS: usize = 200_000;

/// 纯文本类扩展名（直接读取）。
const TEXT_EXTS: &[&str] = &[
    "txt", "md", "markdown", "csv", "tsv", "json", "jsonl", "yaml", "yml", "toml", "ini", "conf",
    "log", "xml", "html", "htm", "css", "js", "ts", "jsx", "tsx", "py", "rs", "go", "java", "c",
    "h", "cpp", "hpp", "cs", "rb", "php", "sh", "bash", "zsh", "sql", "r", "lua", "swift", "kt",
    "scala", "pl", "ex", "exs", "vue", "svelte", "tex", "rst", "org",
];

/// 富文档扩展名（需系统工具转换）。
const RICH_EXTS: &[&str] = &[
    "doc", "docx", "odt", "rtf", "pages", "xls", "xlsx", "ods", "numbers", "ppt", "pptx", "odp",
    "key", "pdf",
];

/// 提取一个文件的文本。
///
/// 调用方必须先做访问控制校验（见模块文档的安全边界）。
pub fn extract(path: &Path, max_chars: usize) -> MnResult<ExtractedText> {
    let meta = std::fs::metadata(path)
        .map_err(|e| MnError::io(format!("读取附件信息 {}", path.display()), &e))?;
    if !meta.is_file() {
        return Err(MnError::bad_request(format!(
            "{} 不是普通文件",
            path.display()
        )));
    }
    let original_bytes = meta.len();
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();

    if TEXT_EXTS.contains(&ext.as_str()) {
        return extract_plain(path, original_bytes, max_chars);
    }
    if RICH_EXTS.contains(&ext.as_str()) {
        return extract_rich(path, &ext, original_bytes, max_chars);
    }

    // 未知扩展名：尝试按文本读（很多配置文件没有扩展名），
    // 若含大量 NUL 字节则判定为二进制并拒绝
    extract_plain(path, original_bytes, max_chars)
}

/// 从内存字节提取文本（Web UI 上传路径）。
///
/// 与 [`extract`] 的差别：不碰服务端文件系统。浏览器只能给出文件内容
/// 而非路径，因此这条路径是 UI 的**唯一**可行方式。
///
/// 富文档需要落临时文件再调系统工具（textutil 等只接受路径参数），
/// 因此这里会写入临时目录并在结束时清理。
pub fn extract_bytes(filename: &str, bytes: &[u8], max_chars: usize) -> MnResult<ExtractedText> {
    let original_bytes = bytes.len() as u64;
    let ext = std::path::Path::new(filename)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();

    // 纯文本：直接处理，无需落盘
    if TEXT_EXTS.contains(&ext.as_str()) || !RICH_EXTS.contains(&ext.as_str()) {
        return plain_from_bytes(bytes, original_bytes, max_chars);
    }

    // 富文档：系统工具只接受路径，落临时文件
    let tmp = temp_input_path(filename, &ext);
    if let Some(parent) = tmp.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| MnError::io(format!("创建临时目录 {}", parent.display()), &e))?;
    }
    std::fs::write(&tmp, bytes)
        .map_err(|e| MnError::io(format!("写入临时文件 {}", tmp.display()), &e))?;

    let result = extract_rich(&tmp, &ext, original_bytes, max_chars);
    // 无论成功失败都清理临时文件：里面是用户文档，不该残留
    let _ = std::fs::remove_file(&tmp);
    result
}

/// 从内存字节解析纯文本。
fn plain_from_bytes(
    bytes: &[u8],
    original_bytes: u64,
    max_chars: usize,
) -> MnResult<ExtractedText> {
    let probe_len = bytes.len().min(8192);
    if bytes[..probe_len].contains(&0) {
        return Err(MnError::bad_request(
            "附件看起来是二进制文件（含 NUL 字节），无法作为文本读取。\
             若是 Office / PDF 文档，请确认扩展名正确"
                .to_string(),
        ));
    }

    let mut warnings = Vec::new();
    let text = match String::from_utf8(bytes.to_vec()) {
        Ok(s) => s,
        Err(e) => {
            let raw = e.into_bytes();
            match decode_gbk(&raw) {
                Some(s) => {
                    warnings.push("文件不是 UTF-8 编码，已按 GBK 解码".to_string());
                    s
                }
                None => {
                    warnings.push(
                        "文件编码无法识别，已按 UTF-8 有损转换（个别字符可能变成 \u{fffd}）"
                            .to_string(),
                    );
                    String::from_utf8_lossy(&raw).to_string()
                }
            }
        }
    };

    let (text, truncated) = truncate_chars(text, max_chars);
    Ok(ExtractedText {
        text,
        original_bytes,
        method: "直接读取（纯文本）".to_string(),
        truncated,
        warnings,
    })
}

/// 为富文档准备临时输入文件路径（保留原扩展名，工具靠它判断格式）。
fn temp_input_path(filename: &str, ext: &str) -> PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let base = std::path::Path::new(filename)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("upload");
    // 文件名里可能有空格/中文，统一下划线化，避免命令行转义问题
    let safe: String = base
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .take(40)
        .collect();
    std::env::temp_dir()
        .join("mangosteen-uploads")
        .join(format!(
            "{safe}-{}-{}-{seq}.{ext}",
            std::process::id(),
            mangosteen_core::time::now_unix_millis()
        ))
}

/// 纯文本读取。
fn extract_plain(path: &Path, original_bytes: u64, max_chars: usize) -> MnResult<ExtractedText> {
    let bytes =
        std::fs::read(path).map_err(|e| MnError::io(format!("读取 {}", path.display()), &e))?;

    // 二进制探测：前 8KB 里出现 NUL 基本可以确定不是文本
    let probe_len = bytes.len().min(8192);
    if bytes[..probe_len].contains(&0) {
        return Err(MnError::bad_request(format!(
            "{} 看起来是二进制文件（含 NUL 字节），无法作为文本读取。\
             若是 Office / PDF 文档，请确认扩展名正确",
            path.display()
        )));
    }

    let mut warnings = Vec::new();
    let text = match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => {
            // 不是合法 UTF-8：按 GBK 尝试（中文环境常见），再退回有损解码
            let raw = e.into_bytes();
            match decode_gbk(&raw) {
                Some(s) => {
                    warnings.push("文件不是 UTF-8 编码，已按 GBK 解码".to_string());
                    s
                }
                None => {
                    warnings.push(
                        "文件编码无法识别，已按 UTF-8 有损转换（个别字符可能变成 \u{fffd}）"
                            .to_string(),
                    );
                    String::from_utf8_lossy(&raw).to_string()
                }
            }
        }
    };

    let (text, truncated) = truncate_chars(text, max_chars);
    Ok(ExtractedText {
        text,
        original_bytes,
        method: "直接读取（纯文本）".to_string(),
        truncated,
        warnings,
    })
}

/// 富文档转换。
fn extract_rich(
    path: &Path,
    ext: &str,
    original_bytes: u64,
    max_chars: usize,
) -> MnResult<ExtractedText> {
    let mut warnings = Vec::new();

    // 平台差异（用哪个工具、传什么参数）由平台层决定。
    // 基础能力层不判断操作系统——这是「跨平台兼容原则」的要求。
    let cmd = mangosteen_platform::docconv::convert_command(path, ext).ok_or_else(|| {
        MnError::bad_request(mangosteen_platform::docconv::missing_tool_hint(ext))
    })?;
    let (program, args, method) = (cmd.program, cmd.args, cmd.method);

    let out = Command::new(&program)
        .args(&args)
        .output()
        .map_err(|e| MnError::Io {
            context: format!("调用 {program} 提取 {}", path.display()),
            reason: e.to_string(),
        })?;

    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(MnError::bad_request(format!(
            "{program} 转换失败（退出码 {:?}）：{}",
            out.status.code(),
            if err.is_empty() {
                "（无错误输出）"
            } else {
                &err
            }
        )));
    }

    // textutil 直接写 stdout；pdftotext 用 `-` 表示 stdout
    let raw = if out.stdout.is_empty() {
        // 部分工具（如 textutil 某些版本）需要输出到文件，这里兜底再读一次
        return Err(MnError::bad_request(format!(
            "{program} 没有输出任何文本。该文件可能为空、已加密，或格式不被支持"
        )));
    } else {
        out.stdout
    };

    let text = String::from_utf8_lossy(&raw).to_string();
    if text.trim().is_empty() {
        warnings.push("提取结果为空：文件可能是纯图片扫描件（需要 OCR）或已加密".to_string());
    }

    let (text, truncated) = truncate_chars(text, max_chars);
    Ok(ExtractedText {
        text,
        original_bytes,
        method,
        truncated,
        warnings,
    })
}

/// 按字符数截断（不按字节，避免切坏多字节字符）。
fn truncate_chars(s: String, max: usize) -> (String, bool) {
    if s.chars().count() <= max {
        return (s, false);
    }
    let cut: String = s.chars().take(max).collect();
    (cut, true)
}

/// 尝试把非 UTF-8 字节按 GBK 解码。
///
/// ## 为什么这里几乎总是返回 `None`
///
/// 完整的 GBK 映射表有 2 万多条，内嵌会让二进制增大数十 KB；引入
/// `encoding_rs` 则要 1.5MB 静态表。两笔代价都不便宜，而这条路径只在
/// 「用户给了 GBK 编码的旧文本文件」时才会走到——现代工具默认 UTF-8。
///
/// 因此当前实现**只处理 ASCII 子集**，遇到任何非 ASCII 字节就返回 `None`，
/// 由调用方降级为有损 UTF-8 并**明确提示用户**。
///
/// 这是刻意的取舍：与其内嵌不完整的表、在某些字上悄悄输出错字，
/// 不如承认不支持，让用户自己 `iconv -f gbk -t utf-8` 一下。
fn decode_gbk(bytes: &[u8]) -> Option<String> {
    if bytes.iter().all(|b| *b < 0x80) {
        // 纯 ASCII：UTF-8 与 GBK 完全一致，无需转换
        return Some(String::from_utf8_lossy(bytes).to_string());
    }
    None
}

/// 临时目录辅助：为需要输出文件的转换工具准备路径。
pub fn temp_output_path(tag: &str) -> PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "mangosteen-{tag}-{}-{}-{seq}.txt",
        std::process::id(),
        mangosteen_core::time::now_unix_millis()
    ))
}

/// 检测系统中可用的富文档工具（供 UI 提前告知用户能力边界）。
///
/// 工具清单是平台相关的，由 [`mangosteen_platform::docconv`] 给出；
/// 这里只做可用性探测。
pub fn available_rich_tools() -> Vec<(&'static str, bool)> {
    mangosteen_platform::docconv::candidate_tools()
        .iter()
        .map(|name| {
            let ok = Command::new(name)
                .arg("--version")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            (*name, ok)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpfile(name: &str, content: &[u8]) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "mn-extract-{}-{}-{seq}",
            std::process::id(),
            mangosteen_core::time::now_unix_millis()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, content).unwrap();
        p
    }

    #[test]
    fn reads_plain_text() {
        let p = tmpfile("a.md", "你好，世界\n第二行".as_bytes());
        let r = extract(&p, MAX_TEXT_CHARS).unwrap();
        assert_eq!(r.text, "你好，世界\n第二行");
        assert!(!r.truncated);
        assert!(r.method.contains("纯文本"));
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn truncates_by_chars_not_bytes() {
        let p = tmpfile("big.txt", "汉".repeat(100).as_bytes());
        let r = extract(&p, 10).unwrap();
        assert!(r.truncated);
        // 必须正好 10 个字符，且没有切坏多字节
        assert_eq!(r.text.chars().count(), 10);
        assert_eq!(r.text, "汉".repeat(10));
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn rejects_binary_with_nul() {
        let p = tmpfile("bin.dat", &[0x00, 0x01, 0x02, 0x03]);
        let e = extract(&p, MAX_TEXT_CHARS).unwrap_err();
        assert!(e.to_string().contains("二进制"), "got {e}");
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn handles_invalid_utf8_gracefully() {
        // 0xFF 不是合法 UTF-8 起始字节
        let p = tmpfile("bad.txt", &[0x41, 0xFF, 0x42]);
        let r = extract(&p, MAX_TEXT_CHARS).unwrap();
        // 不 panic，且有提示
        assert!(!r.warnings.is_empty(), "非法编码应给出提示");
        assert!(r.text.contains('A'));
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn unknown_ext_tries_plain_read() {
        let p = tmpfile("Makefile", b"all:\n\techo hi\n");
        let r = extract(&p, MAX_TEXT_CHARS).unwrap();
        assert!(r.text.contains("echo hi"));
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn missing_tool_gives_actionable_error() {
        // 造一个 .xlsx，在没有转换工具时应给出安装建议而非崩溃
        let p = tmpfile("a.xlsx", b"PK\x03\x04fake");
        match extract(&p, MAX_TEXT_CHARS) {
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("textutil") || msg.contains("pdftotext") || msg.contains("Word"),
                    "错误信息应给出可执行的工具建议：{msg}"
                );
            }
            Ok(_) => {
                // 若系统恰好有工具，也允许（不同环境差异）
            }
        }
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn missing_file_is_reported() {
        let e = extract(Path::new("/nonexistent/x.txt"), MAX_TEXT_CHARS).unwrap_err();
        assert!(!e.to_string().is_empty());
    }

    #[test]
    fn gbk_ascii_passthrough() {
        assert_eq!(decode_gbk(b"hello").unwrap(), "hello");
    }

    #[test]
    fn gbk_only_handles_ascii_subset() {
        // 含非 ASCII 字节 → 明确不支持，返回 None 让调用方降级
        assert!(decode_gbk(&[0xFF, 0x41]).is_none());
        assert!(decode_gbk("中文".as_bytes()).is_none());
    }

    #[test]
    fn rich_tool_selection_comes_from_platform_layer() {
        // 平台差异已下沉到 mangosteen_platform::docconv，
        // 本层不再判断操作系统（由 layering 测试守护）
        let hint = mangosteen_platform::docconv::missing_tool_hint("pdf");
        assert!(hint.contains("pdf"));
    }
}
