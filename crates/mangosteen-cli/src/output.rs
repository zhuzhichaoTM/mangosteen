//! 终端输出格式化。
//!
//! 归属：**用户交互层**。
//!
//! ## 设计约束
//!
//! * **自动降级**：检测到输出被重定向（非 TTY）或设置了 `NO_COLOR` 时，
//!   自动关闭 ANSI 颜色，保证管道/重定向场景输出干净文本；
//! * **不引入颜色 crate**：ANSI 转义序列只有几个，手写足够；
//! * **错误带建议**：每个错误都尽量给出「下一步该做什么」，而不是只报现象。

use mangosteen_core::MnError;

/// 是否启用颜色。
pub fn color_enabled() -> bool {
    // 显式的 MANGOSTEEN_COLOR 是用户对本工具的明确指令，优先级最高
    if let Ok(v) = std::env::var("MANGOSTEEN_COLOR") {
        match v.as_str() {
            "0" | "never" | "off" => return false,
            "1" | "always" | "on" => return true,
            _ => {}
        }
    }
    // NO_COLOR 是跨工具的通用约定（https://no-color.org）
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    is_tty_stdout()
}

/// 标准输出是否为终端。
pub fn is_tty_stdout() -> bool {
    std::io::IsTerminal::is_terminal(&std::io::stdout())
}

/// 标准输入是否为终端（TUI 需要）。
pub fn is_tty_stdin() -> bool {
    std::io::IsTerminal::is_terminal(&std::io::stdin())
}

/// 终端宽度（取不到时给 100）。
pub fn term_width() -> usize {
    #[cfg(unix)]
    {
        // SAFETY: ioctl 的参数是纯值，失败返回 -1，不会造成内存问题
        unsafe {
            let mut ws: libc::winsize = std::mem::zeroed();
            if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
                return ws.ws_col as usize;
            }
        }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Console::{
            GetConsoleScreenBufferInfo, GetStdHandle, CONSOLE_SCREEN_BUFFER_INFO,
            STD_OUTPUT_HANDLE,
        };
        unsafe {
            let h = GetStdHandle(STD_OUTPUT_HANDLE);
            let mut info: CONSOLE_SCREEN_BUFFER_INFO = std::mem::zeroed();
            if GetConsoleScreenBufferInfo(h, &mut info) != 0 {
                let w = (info.srWindow.Right - info.srWindow.Left + 1) as usize;
                if w > 0 {
                    return w;
                }
            }
        }
    }
    100
}

/// 终端高度。
pub fn term_height() -> usize {
    #[cfg(unix)]
    {
        unsafe {
            let mut ws: libc::winsize = std::mem::zeroed();
            if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_row > 0 {
                return ws.ws_row as usize;
            }
        }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Console::{
            GetConsoleScreenBufferInfo, GetStdHandle, CONSOLE_SCREEN_BUFFER_INFO,
            STD_OUTPUT_HANDLE,
        };
        unsafe {
            let h = GetStdHandle(STD_OUTPUT_HANDLE);
            let mut info: CONSOLE_SCREEN_BUFFER_INFO = std::mem::zeroed();
            if GetConsoleScreenBufferInfo(h, &mut info) != 0 {
                let ht = (info.srWindow.Bottom - info.srWindow.Top + 1) as usize;
                if ht > 0 {
                    return ht;
                }
            }
        }
    }
    30
}

/// ANSI 颜色。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    /// 重置
    Reset,
    /// 粗体
    Bold,
    /// 暗色
    Dim,
    /// 红（错误 / 涨）
    Red,
    /// 绿（成功 / 跌）
    Green,
    /// 黄（警告）
    Yellow,
    /// 蓝（信息）
    Blue,
    /// 青（强调）
    Cyan,
    /// 品红
    Magenta,
}

impl Color {
    /// ANSI 码。
    fn code(self) -> &'static str {
        match self {
            Color::Reset => "\x1b[0m",
            Color::Bold => "\x1b[1m",
            Color::Dim => "\x1b[2m",
            Color::Red => "\x1b[31m",
            Color::Green => "\x1b[32m",
            Color::Yellow => "\x1b[33m",
            Color::Blue => "\x1b[34m",
            Color::Cyan => "\x1b[36m",
            Color::Magenta => "\x1b[35m",
        }
    }
}

/// 给文本上色（未启用颜色时原样返回）。
pub fn paint(text: &str, color: Color) -> String {
    if color_enabled() {
        format!("{}{}{}", color.code(), text, Color::Reset.code())
    } else {
        text.to_string()
    }
}

/// 粗体。
pub fn bold(text: &str) -> String {
    paint(text, Color::Bold)
}

/// 暗色。
pub fn dim(text: &str) -> String {
    paint(text, Color::Dim)
}

/// 成功消息。
pub fn print_success(msg: &str) {
    println!("{} {}", paint("✓", Color::Green), msg);
}

/// 信息消息。
pub fn print_info(msg: &str) {
    println!("{} {}", paint("·", Color::Blue), msg);
}

/// 警告消息。带 `⚠` 前缀，符合 personal_files_safety 的显著提示要求。
pub fn print_warning(msg: &str) {
    eprintln!("{} {}", paint("⚠", Color::Yellow), msg);
}

/// 错误消息（输出到 stderr）。
///
/// 会附带「下一步建议」，把 MnError 的分类翻译成可操作的动作。
pub fn print_error(e: &MnError) {
    eprintln!("{} {}", paint("✗", Color::Red), bold(&e.to_string()));
    if let Some(hint) = suggest(e) {
        eprintln!("  {} {}", dim("建议："), hint);
    }
}

/// 根据错误类型给出下一步建议。
pub fn suggest(e: &MnError) -> Option<String> {
    Some(match e {
        MnError::ModelNotFound { .. } => {
            "确认路径正确，或先执行 `mangosteen models add <path>` 登记模型".to_string()
        }
        MnError::GgufFormat { .. } => {
            "Mangosteen 只支持 GGUF 格式；请用 llama.cpp 的 convert 工具转换后重试".to_string()
        }
        MnError::HashMismatch { .. } => {
            "文件可能与登记时不符。若确认是正常更新，执行 `mangosteen models verify --reset <id>`"
                .to_string()
        }
        MnError::InsufficientLocalResource { .. } => {
            "降低上下文长度或改用更低比特量化；也可以确认云端服务商配置后让它自动降级"
                .to_string()
        }
        MnError::EngineLaunch { .. } => {
            "先执行 `mangosteen doctor` 检查本地推理引擎是否就绪".to_string()
        }
        MnError::EngineExited { .. } => {
            "查看日志文件排查原因；常见为显存不足。也可降低 --ctx 或 --n-gpu-layers".to_string()
        }
        MnError::DriverUnavailable { .. } => {
            "该驱动未注册。检查 `mangosteen config show` 里的 providers 配置".to_string()
        }
        MnError::CloudDownloadFailed { .. } => {
            "云端下载没成功，实例已回收。可换个下载源（如 hf-mirror）或稍后重试".to_string()
        }
        MnError::Provider { .. } => {
            "服务商接口报错。核对密钥与账户余额，或换其它服务商".to_string()
        }
        MnError::NoViableCandidate { .. } => {
            "放宽约束（提高价格上限 / 降低显存要求），或启用更多服务商".to_string()
        }
        MnError::Network { .. } => {
            "检查网络连通性与实例状态；云端隧道断开时执行 `mangosteen status` 查看".to_string()
        }
        MnError::AccessDenied { .. } => {
            "该路径不在授权范围内。若确需访问，把它加入配置的 storage.allowed_read_roots"
                .to_string()
        }
        MnError::Config { .. } | MnError::ConfigIo { .. } => {
            "执行 `mangosteen config init` 生成模板，或 `mangosteen config show` 检查现状"
                .to_string()
        }
        MnError::InvalidTransition { .. } => {
            "该状态下的操作不被允许。执行 `mangosteen status` 查看当前状态".to_string()
        }
        MnError::BadRequest { .. } => "执行 `mangosteen help <命令>` 查看正确用法".to_string(),
        // 取消是用户的主动选择，不需要「建议下一步」
        MnError::Cancelled { .. } => return None,
        MnError::Io { .. } => "检查文件权限与磁盘空间".to_string(),
    })
}

/// 打印一个键值表格。
pub fn print_kv(pairs: &[(String, String)]) {
    let width = pairs.iter().map(|(k, _)| k.chars().count()).max().unwrap_or(0);
    for (k, v) in pairs {
        // 必须先按显示宽度补齐，再上色。若先上色，ANSI 转义序列会被
        // `{:<width$}` 计入宽度，导致对齐错乱、甚至把 `\x1b[0m` 挤到内容里。
        let padded = format!("{k:<width$}");
        println!("  {}  {}", paint(&padded, Color::Dim), v);
    }
}

/// 打印一个简单的两列表格（标题 + 行）。
pub fn print_table(headers: &[&str], rows: &[Vec<String>]) {
    if rows.is_empty() {
        println!("  {}", dim("（无数据）"));
        return;
    }
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i < widths.len() {
                widths[i] = widths[i].max(cell.chars().count());
            }
        }
    }
    // 表头
    let head: Vec<String> = headers
        .iter()
        .enumerate()
        .map(|(i, h)| format!("{:<w$}", h, w = widths[i]))
        .collect();
    println!("  {}", bold(&head.join("  ")));
    println!("  {}", dim(&widths.iter().map(|w| "-".repeat(*w)).collect::<Vec<_>>().join("  ")));
    for row in rows {
        let cells: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{:<w$}", c, w = widths.get(i).copied().unwrap_or(0)))
            .collect();
        println!("  {}", cells.join("  ").trim_end());
    }
}

/// 打印产品 banner。仅在交互式终端且非 quiet 模式下输出。
pub fn print_banner() {
    if !color_enabled() {
        return;
    }
    let title = format!("{} {}", mangosteen_core::identity::PRODUCT_NAME, mangosteen_core::identity::VERSION);
    println!();
    println!("  {}  {}", paint("●", Color::Magenta), bold(&title));
    println!(
        "  {}",
        dim("本地算力优先 · 云端算力弹性补位")
    );
    println!();
}

/// 进度提示（简单实现：打印一行然后原地覆盖）。
pub struct Spinner {
    label: String,
    enabled: bool,
    frame: usize,
}

impl Spinner {
    /// 新建。
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            enabled: color_enabled() && is_tty_stdout(),
            frame: 0,
        }
    }

    /// 刷新一帧。
    pub fn tick(&mut self, detail: &str) {
        if !self.enabled {
            return;
        }
        const FRAMES: [char; 4] = ['⠋', '⠙', '⠹', '⠸'];
        let f = FRAMES[self.frame % FRAMES.len()];
        self.frame += 1;
        print!(
            "\r  {} {} {}",
            paint(&f.to_string(), Color::Cyan),
            self.label,
            dim(detail)
        );
        use std::io::Write;
        std::io::stdout().flush().ok();
    }

    /// 结束并清理该行。
    pub fn finish(&self) {
        if self.enabled {
            print!("\r{}\r", " ".repeat(term_width().min(120)));
            use std::io::Write;
            std::io::stdout().flush().ok();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// 环境变量是进程级共享状态，并行测试会互相干扰，故串行化。
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvReset {
        no_color: Option<std::ffi::OsString>,
        mangosteen_color: Option<std::ffi::OsString>,
    }

    impl EnvReset {
        fn capture() -> Self {
            Self {
                no_color: std::env::var_os("NO_COLOR"),
                mangosteen_color: std::env::var_os("MANGOSTEEN_COLOR"),
            }
        }
    }

    impl Drop for EnvReset {
        fn drop(&mut self) {
            match &self.no_color {
                Some(v) => std::env::set_var("NO_COLOR", v),
                None => std::env::remove_var("NO_COLOR"),
            }
            match &self.mangosteen_color {
                Some(v) => std::env::set_var("MANGOSTEEN_COLOR", v),
                None => std::env::remove_var("MANGOSTEEN_COLOR"),
            }
        }
    }

    #[test]
    fn color_respects_no_color_env() {
        let _g = ENV_LOCK.lock().unwrap();
        let _r = EnvReset::capture();
        std::env::remove_var("MANGOSTEEN_COLOR");
        std::env::set_var("NO_COLOR", "1");
        assert!(!color_enabled());
        let s = paint("x", Color::Red);
        assert_eq!(s, "x", "关闭颜色时不应含 ANSI 序列");
        assert!(!s.contains('\x1b'));
    }

    #[test]
    fn explicit_color_override_wins() {
        let _g = ENV_LOCK.lock().unwrap();
        let _r = EnvReset::capture();
        std::env::set_var("NO_COLOR", "1");
        std::env::set_var("MANGOSTEEN_COLOR", "always");
        assert!(color_enabled(), "显式 always 应压过 NO_COLOR");
    }

    #[test]
    fn paint_contains_reset_when_enabled() {
        let _g = ENV_LOCK.lock().unwrap();
        let _r = EnvReset::capture();
        std::env::set_var("MANGOSTEEN_COLOR", "always");
        let s = paint("hi", Color::Green);
        assert!(s.contains("\x1b[32m"), "got {s:?}");
        assert!(s.contains("\x1b[0m"));
    }

    #[test]
    fn every_error_has_a_suggestion() {
        use std::path::PathBuf;
        let cases = vec![
            MnError::ModelNotFound { path: PathBuf::from("/m") },
            MnError::GgufFormat { path: PathBuf::from("/m"), reason: "x".into() },
            MnError::HashMismatch { path: PathBuf::from("/m"), expected: "a".into(), actual: "b".into() },
            MnError::InsufficientLocalResource { detail: "x".into() },
            MnError::EngineLaunch { binary: PathBuf::from("x"), reason: "y".into() },
            MnError::EngineExited { pid: 1, code: Some(1), stderr_tail: "z".into() },
            MnError::DriverUnavailable { provider: "p".into() },
            MnError::Provider { provider: "p".into(), code: "c".into(), message: "m".into() },
            MnError::NoViableCandidate { reason: "r".into() },
            MnError::Network { stage: "s".into(), reason: "r".into() },
            MnError::AccessDenied { path: PathBuf::from("/x"), reason: "r".into() },
            MnError::Config { field: "f".into(), reason: "r".into() },
            MnError::ConfigIo { path: PathBuf::from("/c"), reason: "r".into() },
            MnError::InvalidTransition { instance: "i".into(), from: "a".into(), action: "b".into() },
            MnError::BadRequest { reason: "r".into() },
            MnError::Io { context: "c".into(), reason: "r".into() },
        ];
        for e in cases {
            let s = suggest(&e);
            assert!(s.is_some(), "错误缺少建议：{e:?}");
            assert!(!s.unwrap().trim().is_empty());
        }
    }

    #[test]
    fn term_size_is_plausible() {
        assert!(term_width() > 0);
        assert!(term_height() > 0);
    }

    #[test]
    fn table_prints_without_panic() {
        // 主要验证不 panic 且空行处理正确
        print_table(&["A", "B"], &[]);
        print_table(
            &["名字", "状态"],
            &[vec!["m1".into(), "ready".into()], vec!["m2".into(), "stopped".into()]],
        );
    }

    #[test]
    fn kv_prints_without_panic() {
        print_kv(&[("a".into(), "1".into()), ("longer".into(), "2".into())]);
        print_kv(&[]);
    }

    #[test]
    fn spinner_disabled_when_not_tty() {
        let mut s = Spinner::new("test");
        // 在测试环境通常非 TTY，tick 应为无操作且不 panic
        s.tick("x");
        s.finish();
    }
}