//! 富文档转换命令选择。
//!
//! 归属：**平台适配层**。
//!
//! ## 为什么这段逻辑必须在本层
//!
//! 各平台的可用转换工具完全不同：
//!
//! | 平台 | 工具 | 覆盖格式 |
//! |------|------|---------|
//! | macOS | 自带 `textutil` | doc / docx / rtf / html / odt |
//! | Linux | `pdftotext` / `pandoc` | pdf / docx / odt |
//! | Windows | PowerShell + Word COM | doc / docx / rtf |
//!
//! 「用哪个程序、传什么参数」是纯粹的平台差异，按「跨平台兼容原则」必须
//! 收敛在平台层。基础能力层只负责「调用并取回文本」，不判断操作系统。

use std::path::Path;

/// 一次转换调用的描述。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConvertCommand {
    /// 可执行程序
    pub program: String,
    /// 参数列表
    pub args: Vec<String>,
    /// 人类可读的方式说明（用于 UI 展示「怎么读出来的」）
    pub method: String,
}

/// 按文件扩展名选择转换命令。
///
/// 返回 `None` 表示当前平台没有可用工具处理该格式——调用方应把
/// 「为什么不行 + 该装什么」告诉用户，而不是静默失败。
pub fn convert_command(path: &Path, ext: &str) -> Option<ConvertCommand> {
    let p = path.display().to_string();

    #[cfg(target_os = "macos")]
    {
        // textutil 是 macOS 自带的富文本转换器
        if matches!(
            ext,
            "doc" | "docx" | "rtf" | "html" | "htm" | "odt" | "wordml"
        ) {
            return Some(ConvertCommand {
                program: "textutil".to_string(),
                args: vec![
                    "-convert".to_string(),
                    "txt".to_string(),
                    "-stdout".to_string(),
                    p,
                ],
                method: format!("textutil 转换 .{ext} → 纯文本"),
            });
        }
        // 表格 / 演示 / PDF：macOS 无自带命令行工具
        None
    }

    #[cfg(target_os = "linux")]
    {
        if ext == "pdf" {
            return Some(ConvertCommand {
                program: "pdftotext".to_string(),
                args: vec![
                    "-layout".to_string(),
                    "-enc".to_string(),
                    "UTF-8".to_string(),
                    p,
                    "-".to_string(),
                ],
                method: "pdftotext 转换 .pdf → 纯文本".to_string(),
            });
        }
        if matches!(ext, "doc" | "docx" | "odt" | "rtf") {
            return Some(ConvertCommand {
                program: "pandoc".to_string(),
                args: vec![p, "-t".to_string(), "plain".to_string()],
                method: format!("pandoc 转换 .{ext} → 纯文本"),
            });
        }
        None
    }

    #[cfg(target_os = "windows")]
    {
        if matches!(ext, "doc" | "docx" | "rtf") {
            return Some(ConvertCommand {
                program: "powershell".to_string(),
                args: vec![
                    "-NoProfile".to_string(),
                    "-Command".to_string(),
                    format!(
                        "$w=New-Object -ComObject Word.Application; $w.Visible=$false; \
                         $d=$w.Documents.Open('{p}'); $d.Content.Text; \
                         $d.Close(); $w.Quit()"
                    ),
                ],
                method: format!("Word COM 转换 .{ext} → 纯文本"),
            });
        }
        None
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = (path, ext);
        None
    }
}

/// 当前平台可能用于富文档转换的工具名（供 UI 提前告知能力边界）。
pub fn candidate_tools() -> &'static [&'static str] {
    #[cfg(target_os = "macos")]
    {
        &["textutil"]
    }
    #[cfg(target_os = "linux")]
    {
        &["pdftotext", "pandoc", "antiword"]
    }
    #[cfg(target_os = "windows")]
    {
        &["powershell"]
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        &[]
    }
}

/// 本平台支持的富文档扩展名。
pub fn supported_exts() -> &'static [&'static str] {
    #[cfg(target_os = "macos")]
    {
        &["doc", "docx", "rtf", "html", "htm", "odt", "wordml"]
    }
    #[cfg(target_os = "linux")]
    {
        &["pdf", "doc", "docx", "odt", "rtf"]
    }
    #[cfg(target_os = "windows")]
    {
        &["doc", "docx", "rtf"]
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        &[]
    }
}

/// 缺工具时给用户的可执行建议。
pub fn missing_tool_hint(ext: &str) -> String {
    #[cfg(target_os = "macos")]
    {
        format!(
            "无法提取 .{ext} 文件：当前系统缺少可用的转换工具。\n\
             · doc/docx/rtf/html/odt 应可用系统自带的 textutil\n\
             · pdf / 表格 / 演示文稿 macOS 无自带命令行工具，\
             可先另存为 .txt 或 .md 再上传"
        )
    }
    #[cfg(target_os = "linux")]
    {
        format!(
            "无法提取 .{ext} 文件：缺少转换工具。\n\
             · PDF 需要 poppler-utils：apt install poppler-utils\n\
             · Office 文档需要 pandoc：apt install pandoc\n\
             或把内容另存为 .txt / .md 后再上传"
        )
    }
    #[cfg(target_os = "windows")]
    {
        format!(
            "无法提取 .{ext} 文件：需要安装 Microsoft Word（走 COM 自动化）。\n\
             或把内容另存为 .txt / .md 后再上传"
        )
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        format!("当前平台不支持提取 .{ext} 文件")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_uses_textutil_for_office() {
        if !cfg!(target_os = "macos") {
            return;
        }
        let c = convert_command(Path::new("/tmp/a.docx"), "docx").unwrap();
        assert_eq!(c.program, "textutil");
        assert!(c.args.iter().any(|a| a == "-stdout"));
        assert!(c.method.contains("docx"));
    }

    #[test]
    fn macos_has_no_pdf_tool() {
        if !cfg!(target_os = "macos") {
            return;
        }
        // macOS 无自带 pdftotext：应明确返回 None，由调用方给出指引
        assert!(convert_command(Path::new("/tmp/a.pdf"), "pdf").is_none());
    }

    #[test]
    fn unsupported_ext_returns_none() {
        assert!(convert_command(Path::new("/tmp/a.xyz"), "xyz").is_none());
    }

    #[test]
    fn candidate_tools_not_empty_on_known_platforms() {
        if cfg!(any(
            target_os = "macos",
            target_os = "linux",
            target_os = "windows"
        )) {
            assert!(!candidate_tools().is_empty());
            assert!(!supported_exts().is_empty());
        }
    }

    #[test]
    fn hint_mentions_extension() {
        let h = missing_tool_hint("pdf");
        assert!(h.contains("pdf"));
    }
}
