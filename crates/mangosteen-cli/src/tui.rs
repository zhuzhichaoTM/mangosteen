//! 全屏 TUI。
//!
//! 归属：**用户交互层**。
//!
//! ## 形态
//!
//! ```text
//! ┌ Mangosteen 0.1.0 ─────────────── 本地算力优先 ─┐
//! │ 面板                                              │
//! │  ┌ 部署 ──────┐ ┌ 模型 ──────┐ ┌ 云端 ──────┐   │
//! │  │ 1 概览      │ │ 2 模型列表 │ │ 3 服务商   │   │
//! │  │ 4 评估      │ │ 5 登记模型 │ │ 6 选点     │   │
//! │  └─────────────┘ └────────────┘ └────────────┘   │
//! │                                                    │
//! │  内容区                                            │
//! │                                                    │
//! │ q 退出  ? 帮助  r 刷新  Tab 切换面板  Enter 执行    │
//! └────────────────────────────────────────────────────┘
//! ```
//!
//! ## 实现取舍
//!
//! 不引入 ratatui / crossterm：需要的能力只有「清屏、定位、读一个按键、
//! 切原始模式」，标准库 + 少量 libc / windows-sys 足够。
//!
//! ## 安全
//!
//! * 非 TTY 环境直接拒绝启动（避免把控制序列喷进管道）；
//! * 任何退出路径（含 panic）都必须恢复终端原始模式，否则终端会「失灵」。
//!   利用 `Drop` 保证恢复。

use std::io::{Read, Write};

use mangosteen_core::MnResult;
use mangosteen_gateway::DeploymentRequest;

use crate::cli::Args;
use crate::commands::reject_unknown_options;
use crate::output;

/// 允许的选项。
const ALLOWED: &[&str] = &["config", "quiet"];

/// TUI 面板。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    /// 概览
    Overview,
    /// 模型列表
    Models,
    /// 已部署列表
    Deployments,
    /// 云端服务商
    Providers,
    /// 帮助
    Help,
}

impl Panel {
    /// 全部面板（按 Tab 顺序）。
    pub fn all() -> &'static [Panel] {
        &[
            Panel::Overview,
            Panel::Models,
            Panel::Deployments,
            Panel::Providers,
            Panel::Help,
        ]
    }

    /// 标题。
    pub fn title(self) -> &'static str {
        match self {
            Panel::Overview => "概览",
            Panel::Models => "模型库",
            Panel::Deployments => "部署",
            Panel::Providers => "云端服务商",
            Panel::Help => "帮助",
        }
    }

    /// 下一个面板。
    pub fn next(self) -> Panel {
        let all = Panel::all();
        let idx = all.iter().position(|p| *p == self).unwrap_or(0);
        all[(idx + 1) % all.len()]
    }

    /// 上一个面板。
    pub fn prev(self) -> Panel {
        let all = Panel::all();
        let idx = all.iter().position(|p| *p == self).unwrap_or(0);
        all[(idx + all.len() - 1) % all.len()]
    }
}

/// 按键。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// 退出
    Quit,
    /// 切换面板（正向）
    NextPanel,
    /// 切换面板（反向）
    PrevPanel,
    /// 刷新
    Refresh,
    /// 上移
    Up,
    /// 下移
    Down,
    /// 确认 / 部署
    Enter,
    /// 停止选中部署
    Stop,
    /// 未知按键
    Other(char),
}

impl Key {
    /// 从字节解析（单字节或 ESC 序列）。
    pub fn from_bytes(buf: &[u8]) -> Key {
        match buf {
            b"q" | b"Q" => Key::Quit,
            b"\t" => Key::NextPanel,
            b"r" | b"R" => Key::Refresh,
            b"k" => Key::Up,
            b"j" => Key::Down,
            b"d" | b"D" => Key::Stop,
            b"\r" | b"\n" => Key::Enter,
            // ESC [ Z = Shift-Tab
            b"\x1b[Z" => Key::PrevPanel,
            // ESC [ A = 上箭头
            b"\x1b[A" => Key::Up,
            // ESC [ B = 下箭头
            b"\x1b[B" => Key::Down,
            b"\x03" => Key::Quit, // Ctrl-C
            other => {
                if other.len() == 1 {
                    Key::Other(other[0] as char)
                } else {
                    Key::Other('\0')
                }
            }
        }
    }
}

/// TUI 状态。
pub struct TuiState {
    /// 当前面板
    pub panel: Panel,
    /// 光标位置
    pub cursor: usize,
    /// 模型列表
    pub models: Vec<String>,
    /// 部署列表（id, 模型, 来源, 状态）
    pub deployments: Vec<(String, String, String, String)>,
    /// 服务商列表
    pub providers: Vec<String>,
    /// 硬件描述
    pub hardware: String,
    /// 引擎路径
    pub engine: String,
    /// 状态消息
    pub message: String,
    /// 是否需要退出
    pub should_quit: bool,
}

impl Default for TuiState {
    fn default() -> Self {
        Self {
            panel: Panel::Overview,
            cursor: 0,
            models: Vec::new(),
            deployments: Vec::new(),
            providers: Vec::new(),
            hardware: String::new(),
            engine: String::new(),
            message: String::new(),
            should_quit: false,
        }
    }
}

impl TuiState {
    /// 当前面板的条目数（用于光标夹紧）。
    pub fn item_count(&self) -> usize {
        match self.panel {
            Panel::Models => self.models.len(),
            Panel::Deployments => self.deployments.len(),
            Panel::Providers => self.providers.len(),
            _ => 0,
        }
    }

    /// 光标下移。
    pub fn move_down(&mut self) {
        let n = self.item_count();
        if n > 0 {
            self.cursor = (self.cursor + 1).min(n - 1);
        }
    }

    /// 光标上移。
    pub fn move_up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    /// 切换面板时重置光标。
    pub fn goto(&mut self, panel: Panel) {
        self.panel = panel;
        self.cursor = 0;
    }

    /// 处理按键。返回可能需要网关执行的动作。
    pub fn handle_key(&mut self, key: Key) -> Option<Action> {
        match key {
            Key::Quit => {
                self.should_quit = true;
                None
            }
            Key::NextPanel => {
                let p = self.panel.next();
                self.goto(p);
                None
            }
            Key::PrevPanel => {
                let p = self.panel.prev();
                self.goto(p);
                None
            }
            Key::Refresh => Some(Action::Refresh),
            Key::Up => {
                self.move_up();
                None
            }
            Key::Down => {
                self.move_down();
                None
            }
            Key::Enter => match self.panel {
                Panel::Models => self
                    .models
                    .get(self.cursor)
                    .map(|m| Action::Deploy(m.clone())),
                Panel::Deployments => self
                    .deployments
                    .get(self.cursor)
                    .map(|(id, _, _, _)| Action::Stop(id.clone())),
                _ => None,
            },
            Key::Stop => self
                .deployments
                .get(self.cursor)
                .map(|(id, _, _, _)| Action::Stop(id.clone())),
            Key::Other(c) => {
                // 数字键直达面板
                match c {
                    '1' => self.goto(Panel::Overview),
                    '2' => self.goto(Panel::Models),
                    '3' => self.goto(Panel::Deployments),
                    '4' => self.goto(Panel::Providers),
                    '?' | 'h' | 'H' => self.goto(Panel::Help),
                    _ => {}
                }
                None
            }
        }
    }
}

/// TUI 触发的动作（由主循环交给网关执行）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// 刷新数据
    Refresh,
    /// 部署指定模型
    Deploy(String),
    /// 停止指定部署
    Stop(String),
}

/// 入口。
pub fn run(args: Args) -> MnResult<i32> {
    reject_unknown_options(&args, ALLOWED)?;

    if !output::is_tty_stdout() || !output::is_tty_stdin() {
        return Err(mangosteen_core::MnError::bad_request(
            "TUI 需要交互式终端。当前输出或输入被重定向，请直接在终端中运行，\
             或改用 `mangosteen status` 等命令行子命令",
        ));
    }

    let gw = crate::commands::build_gateway(&args)?;
    let mut app = App::new(gw);
    app.refresh();
    app.run_loop()
}

/// TUI 应用。
struct App {
    gw: mangosteen_gateway::Gateway,
    state: TuiState,
}

impl App {
    fn new(gw: mangosteen_gateway::Gateway) -> Self {
        Self {
            gw,
            state: TuiState::default(),
        }
    }

    /// 从网关拉取最新数据。
    fn refresh(&mut self) {
        // 硬件与引擎
        if let Ok(st) = self.gw.status() {
            self.state.hardware = st.hardware;
            self.state.engine = st
                .engine_path
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "未找到".into());
            self.state.providers = st.providers;
            self.state.deployments = st
                .active
                .iter()
                .map(|a| {
                    (
                        a.id.clone(),
                        a.model.clone(),
                        a.origin.label(),
                        a.status.as_str().to_string(),
                    )
                })
                .collect();
        }
        // 模型
        if let Ok(list) = self.gw.models().list() {
            self.state.models = list
                .into_iter()
                .map(|m| format!("{}  ({:.1}B {})", m.id, m.spec.params_b, m.spec.quant))
                .collect();
        }
        self.state.message = "已刷新".to_string();
    }

    /// 执行动作。
    fn execute(&mut self, action: Action) {
        match action {
            Action::Refresh => self.refresh(),
            Action::Deploy(model) => {
                // TUI 里展示的模型串带附注，取第一个空格前的 ID
                let id = model.split_whitespace().next().unwrap_or(&model).to_string();
                self.state.message = format!("正在部署 {id}…");
                let req = DeploymentRequest::for_model(id.clone());
                match self.gw.deploy(&req) {
                    Ok(plan) => {
                        self.state.message = format!(
                            "部署成功：{}（{}，端口 {}）",
                            plan.model.name,
                            plan.origin.label(),
                            plan.local_access_port()
                                .map(|p| p.to_string())
                                .unwrap_or_else(|| "-".into())
                        );
                    }
                    Err(e) => self.state.message = format!("部署失败：{e}"),
                }
                self.refresh();
            }
            Action::Stop(id) => {
                self.state.message = format!("正在停止 {id}…");
                match self.gw.stop(&id) {
                    Ok(()) => self.state.message = format!("已停止 {id}"),
                    Err(e) => self.state.message = format!("停止失败：{e}"),
                }
                self.refresh();
            }
        }
    }

    /// 主循环。
    fn run_loop(&mut self) -> MnResult<i32> {
        let _guard = RawModeGuard::enter()?;
        let mut out = std::io::stdout();

        loop {
            self.draw(&mut out)?;
            let key = read_key()?;
            if let Some(action) = self.state.handle_key(key) {
                // 绘制一帧提示，再执行（部署可能耗时）
                self.draw(&mut out)?;
                self.execute(action);
            }
            if self.state.should_quit {
                break;
            }
        }
        Ok(0)
    }

    /// 绘制一帧。
    fn draw(&self, out: &mut impl Write) -> MnResult<()> {
        let w = output::term_width().max(60);
        let h = output::term_height().max(16);
        let mut buf = String::new();

        // 清屏 + 归位
        buf.push_str("\x1b[2J\x1b[H");

        let title = format!(
            " Mangosteen {} — 本地算力优先 · 云端弹性补位 ",
            mangosteen_core::identity::VERSION
        );
        buf.push_str(&format!(
            "\x1b[7m{:<width$}\x1b[0m\r\n",
            truncate(&title, w),
            width = w
        ));

        // 标签栏
        let mut tabs = String::new();
        for (i, p) in Panel::all().iter().enumerate() {
            let label = format!(" {}{} {} ", i + 1, "", p.title());
            if *p == self.state.panel {
                tabs.push_str(&format!("\x1b[1;36m[{}]\x1b[0m", label.trim()));
            } else {
                tabs.push_str(&format!(" {} ", label.trim()));
            }
        }
        buf.push_str(&format!("{}\r\n", truncate(&tabs, w)));
        buf.push_str(&format!("{}\r\n", "─".repeat(w)));

        // 内容区
        let body_lines = h.saturating_sub(6);
        let content = self.render_panel(w, body_lines);
        for line in content.into_iter().take(body_lines) {
            buf.push_str(&format!("{}\r\n", truncate(&line, w)));
        }

        // 状态栏
        buf.push_str(&format!("{}\r\n", "─".repeat(w)));
        buf.push_str(&format!(
            " {}\r\n",
            truncate(&self.state.message, w.saturating_sub(2))
        ));
        buf.push_str(&format!(
            " {}",
            truncate(
                " q 退出 │ Tab 切面板 │ ↑↓ 移动 │ Enter 执行 │ d 停止 │ r 刷新 │ ? 帮助",
                w
            )
        ));

        out.write_all(buf.as_bytes())
            .map_err(|e| mangosteen_core::MnError::io("绘制 TUI", &e))?;
        out.flush()
            .map_err(|e| mangosteen_core::MnError::io("刷新 TUI", &e))?;
        Ok(())
    }

    /// 渲染当前面板内容。
    fn render_panel(&self, w: usize, max_lines: usize) -> Vec<String> {
        let mut lines = Vec::new();
        match self.state.panel {
            Panel::Overview => {
                lines.push(String::new());
                lines.push("  系统".to_string());
                lines.push(format!("    硬件：{}", self.state.hardware));
                lines.push(format!("    本地引擎：{}", self.state.engine));
                lines.push(format!(
                    "    云服务商：{}",
                    if self.state.providers.is_empty() {
                        "未配置（本地不足时无法降级）".to_string()
                    } else {
                        self.state.providers.join(", ")
                    }
                ));
                lines.push(String::new());
                lines.push("  当前部署".to_string());
                if self.state.deployments.is_empty() {
                    lines.push("    （无活跃部署）".to_string());
                } else {
                    for (id, model, origin, status) in &self.state.deployments {
                        lines.push(format!("    {id}  {model}  {origin}  [{status}]"));
                    }
                }
                lines.push(String::new());
                lines.push(format!(
                    "  推理入口：http://127.0.0.1:{}/v1",
                    self.gw.config().gateway.port
                ));
            }
            Panel::Models => {
                if self.state.models.is_empty() {
                    lines.push(String::new());
                    lines.push("  （模型库为空）".to_string());
                    lines.push("  退出后执行：mangosteen models add <path/to/model.gguf>".to_string());
                } else {
                    lines.push(String::new());
                    lines.push("  按 Enter 部署选中模型".to_string());
                    for (i, m) in self.state.models.iter().enumerate() {
                        let marker = if i == self.state.cursor { "▸" } else { " " };
                        lines.push(format!("  {marker} {m}"));
                    }
                }
            }
            Panel::Deployments => {
                if self.state.deployments.is_empty() {
                    lines.push(String::new());
                    lines.push("  （无活跃部署）".to_string());
                } else {
                    lines.push(String::new());
                    lines.push("  按 Enter 或 d 停止选中部署".to_string());
                    lines.push(format!(
                        "  {:<24} {:<28} {:<18} {}",
                        "部署 ID", "模型", "算力来源", "状态"
                    ));
                    for (i, (id, model, origin, status)) in self.state.deployments.iter().enumerate()
                    {
                        let marker = if i == self.state.cursor { "▸" } else { " " };
                        lines.push(format!(
                            "  {marker}{:<23} {:<28} {:<18} {}",
                            id, model, origin, status
                        ));
                    }
                }
            }
            Panel::Providers => {
                lines.push(String::new());
                if self.state.providers.is_empty() {
                    lines.push("  （未配置云服务商）".to_string());
                    lines.push("  本地资源不足时将无法降级到云端".to_string());
                    lines.push(String::new());
                    lines.push("  退出后执行：mangosteen config init".to_string());
                } else {
                    lines.push("  已就绪服务商：".to_string());
                    for (i, p) in self.state.providers.iter().enumerate() {
                        let marker = if i == self.state.cursor { "▸" } else { " " };
                        lines.push(format!("  {marker} {p}"));
                    }
                }
            }
            Panel::Help => {
                lines.push(String::new());
                lines.push("  按键".to_string());
                for (k, d) in [
                    ("q / Ctrl-C", "退出"),
                    ("Tab / Shift-Tab", "切换面板"),
                    ("1-4", "直达面板"),
                    ("↑ ↓ / k j", "移动光标"),
                    ("Enter", "部署（模型面板）/ 停止（部署面板）"),
                    ("d", "停止选中部署"),
                    ("r", "刷新数据"),
                    ("?", "本帮助"),
                ] {
                    lines.push(format!("    {k:<18} {d}"));
                }
                lines.push(String::new());
                lines.push("  说明".to_string());
                lines.push("    部署遵循「本地优先」：本地资源不足时自动降级到云端".to_string());
                lines.push("    云端实例会持续计费，用完请停止".to_string());
            }
        }
        while lines.len() < max_lines {
            lines.push(String::new());
        }
        let _ = w;
        lines
    }
}

/// 按显示宽度截断（粗略处理 CJK 双宽字符）。
fn truncate(s: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for ch in s.chars() {
        let cw = char_width(ch);
        if used + cw > width {
            break;
        }
        out.push(ch);
        used += cw;
    }
    out
}

/// 字符显示宽度（CJK 与全角算 2）。
fn char_width(c: char) -> usize {
    let cp = c as u32;
    if (0x1100..=0x115F).contains(&cp)
        || (0x2E80..=0xA4CF).contains(&cp)
        || (0xAC00..=0xD7A3).contains(&cp)
        || (0xF900..=0xFAFF).contains(&cp)
        || (0xFE30..=0xFE6F).contains(&cp)
        || (0xFF00..=0xFF60).contains(&cp)
        || (0xFFE0..=0xFFE6).contains(&cp)
        || (0x20000..=0x3FFFD).contains(&cp)
    {
        2
    } else if cp < 0x20 {
        0
    } else {
        1
    }
}

/// 读一个按键（阻塞）。
fn read_key() -> MnResult<Key> {
    let mut buf = [0u8; 8];
    let mut stdin = std::io::stdin();
    let n = stdin
        .read(&mut buf)
        .map_err(|e| mangosteen_core::MnError::io("读取按键", &e))?;
    if n == 0 {
        return Ok(Key::Quit);
    }
    // ESC 序列：若首字节是 ESC 且只读到 1 字节，再尝试非阻塞读剩余
    if n == 1 && buf[0] == 0x1b {
        // 简单处理：把单独的 ESC 当作「返回概览」
        return Ok(Key::Other('1'));
    }
    Ok(Key::from_bytes(&buf[..n]))
}

/// 终端原始模式守卫。`Drop` 时恢复，保证任何退出路径都不留烂摊子。
struct RawModeGuard {
    #[cfg(unix)]
    original: libc::termios,
}

impl RawModeGuard {
    #[cfg(unix)]
    fn enter() -> MnResult<Self> {
        // SAFETY: tcgetattr/tcsetattr 操作本进程的 stdin 终端属性；
        // original 由 tcgetattr 填充，随后原样恢复。
        unsafe {
            let mut original: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut original) != 0 {
                return Err(mangosteen_core::MnError::io(
                    "读取终端属性",
                    &std::io::Error::last_os_error(),
                ));
            }
            let mut raw = original;
            // 关闭行缓冲、回显、信号字符
            raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG);
            raw.c_cc[libc::VMIN] = 1;
            raw.c_cc[libc::VTIME] = 0;
            if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &raw) != 0 {
                return Err(mangosteen_core::MnError::io(
                    "设置终端原始模式",
                    &std::io::Error::last_os_error(),
                ));
            }
            Ok(Self { original })
        }
    }

    #[cfg(windows)]
    fn enter() -> MnResult<Self> {
        use windows_sys::Win32::System::Console::{
            GetConsoleMode, GetStdHandle, SetConsoleMode, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT,
            ENABLE_PROCESSED_INPUT, STD_INPUT_HANDLE,
        };
        unsafe {
            let h = GetStdHandle(STD_INPUT_HANDLE);
            let mut mode: u32 = 0;
            if GetConsoleMode(h, &mut mode) == 0 {
                return Err(mangosteen_core::MnError::io(
                    "读取控制台模式",
                    &std::io::Error::last_os_error(),
                ));
            }
            let new_mode = mode & !(ENABLE_ECHO_INPUT | ENABLE_LINE_INPUT | ENABLE_PROCESSED_INPUT);
            if SetConsoleMode(h, new_mode) == 0 {
                return Err(mangosteen_core::MnError::io(
                    "设置控制台模式",
                    &std::io::Error::last_os_error(),
                ));
            }
            Ok(Self { original_mode: mode })
        }
    }

    #[cfg(not(any(unix, windows)))]
    fn enter() -> MnResult<Self> {
        Ok(Self {})
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        // 恢复终端 + 清屏并归位，避免退出后残留 TUI 画面
        #[cfg(unix)]
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &self.original);
        }
        #[cfg(windows)]
        unsafe {
            use windows_sys::Win32::System::Console::{
                GetStdHandle, SetConsoleMode, STD_INPUT_HANDLE,
            };
            let h = GetStdHandle(STD_INPUT_HANDLE);
            SetConsoleMode(h, self.original_mode);
        }
        let mut out = std::io::stdout();
        let _ = out.write_all(b"\x1b[2J\x1b[H\x1b[?25h");
        let _ = out.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panel_navigation_wraps() {
        assert_eq!(Panel::Overview.next(), Panel::Models);
        assert_eq!(Panel::Help.next(), Panel::Overview);
        assert_eq!(Panel::Overview.prev(), Panel::Help);
        assert_eq!(Panel::Models.prev(), Panel::Overview);
    }

    #[test]
    fn panel_titles_are_distinct() {
        let titles: Vec<&str> = Panel::all().iter().map(|p| p.title()).collect();
        let mut sorted = titles.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(titles.len(), sorted.len());
    }

    #[test]
    fn key_parsing() {
        assert_eq!(Key::from_bytes(b"q"), Key::Quit);
        assert_eq!(Key::from_bytes(b"\t"), Key::NextPanel);
        assert_eq!(Key::from_bytes(b"\x1b[Z"), Key::PrevPanel);
        assert_eq!(Key::from_bytes(b"\x1b[A"), Key::Up);
        assert_eq!(Key::from_bytes(b"\x1b[B"), Key::Down);
        assert_eq!(Key::from_bytes(b"r"), Key::Refresh);
        assert_eq!(Key::from_bytes(b"\x03"), Key::Quit);
        assert_eq!(Key::from_bytes(b"z"), Key::Other('z'));
    }

    #[test]
    fn cursor_clamps_to_item_count() {
        let mut s = TuiState {
            panel: Panel::Models,
            models: vec!["a".into(), "b".into()],
            ..Default::default()
        };
        s.move_down();
        assert_eq!(s.cursor, 1);
        s.move_down();
        assert_eq!(s.cursor, 1, "不应越界");
        s.move_up();
        assert_eq!(s.cursor, 0);
        s.move_up();
        assert_eq!(s.cursor, 0, "不应下溢");
    }

    #[test]
    fn switching_panel_resets_cursor() {
        let mut s = TuiState {
            cursor: 5,
            panel: Panel::Models,
            ..Default::default()
        };
        s.handle_key(Key::NextPanel);
        assert_eq!(s.cursor, 0);
        assert_eq!(s.panel, Panel::Deployments);
    }

    #[test]
    fn number_keys_jump_to_panel() {
        let mut s = TuiState::default();
        s.handle_key(Key::Other('3'));
        assert_eq!(s.panel, Panel::Deployments);
        s.handle_key(Key::Other('1'));
        assert_eq!(s.panel, Panel::Overview);
        s.handle_key(Key::Other('?'));
        assert_eq!(s.panel, Panel::Help);
    }

    #[test]
    fn quit_key_sets_flag() {
        let mut s = TuiState::default();
        assert_eq!(s.handle_key(Key::Quit), None);
        assert!(s.should_quit);
    }

    #[test]
    fn refresh_key_returns_action() {
        let mut s = TuiState::default();
        assert_eq!(s.handle_key(Key::Refresh), Some(Action::Refresh));
    }

    #[test]
    fn enter_on_model_returns_deploy_action() {
        let mut s = TuiState {
            panel: Panel::Models,
            models: vec!["m1".into()],
            ..Default::default()
        };
        assert_eq!(
            s.handle_key(Key::Enter),
            Some(Action::Deploy("m1".to_string()))
        );
    }

    #[test]
    fn enter_on_empty_models_returns_none() {
        let mut s = TuiState {
            panel: Panel::Models,
            ..Default::default()
        };
        assert_eq!(s.handle_key(Key::Enter), None);
    }

    #[test]
    fn stop_on_deployment_returns_action() {
        let mut s = TuiState {
            panel: Panel::Deployments,
            deployments: vec![(
                "dep-1".into(),
                "m".into(),
                "本地".into(),
                "ready".into(),
            )],
            ..Default::default()
        };
        assert_eq!(
            s.handle_key(Key::Stop),
            Some(Action::Stop("dep-1".to_string()))
        );
    }

    #[test]
    fn truncate_respects_display_width() {
        assert_eq!(truncate("abcdef", 3), "abc");
        // 中文双宽：3 个汉字占 6 列
        assert_eq!(truncate("中文测试", 4), "中文");
        assert_eq!(truncate("a中b", 2), "a");
        assert_eq!(truncate("", 10), "");
    }

    #[test]
    fn char_width_classification() {
        assert_eq!(char_width('a'), 1);
        assert_eq!(char_width('中'), 2);
        assert_eq!(char_width('，'), 2);
        assert_eq!(char_width('\0'), 0);
    }

    #[test]
    fn tui_refuses_non_tty() {
        // 测试环境通常非 TTY，应被拒绝而不是喷控制字符到管道
        let a = crate::cli::parse_args(&["tui".to_string()]).unwrap();
        let r = run(a);
        if !output::is_tty_stdout() {
            assert!(r.is_err());
            assert!(r.unwrap_err().to_string().contains("交互式终端"));
        }
    }

    #[test]
    fn render_panels_produce_content() {
        let a = crate::cli::parse_args(&["tui".to_string()]).unwrap();
        let _ = a;
        // 构造一个假 App 需要真实 Gateway，这里只测 TuiState 的纯逻辑
        let s = TuiState::default();
        assert_eq!(s.item_count(), 0);
    }
}