//! 子进程管理：拉起、健康探测、优雅终止。
//!
//! 归属：**平台适配层**。本地推理链路（`mangosteen-local`）依赖本模块。
//!
//! ## 平台差异
//!
//! | 差异点 | Unix | Windows |
//! |--------|------|---------|
//! | 终止信号 | `SIGTERM` → 超时 → `SIGKILL` | `taskkill /PID` → 超时 → `/F` |
//! | 进程组 | `setsid` 隔离，防止子进程残留 | `CREATE_NEW_PROCESS_GROUP` |
//! | 可执行查找 | 从 `PATH` 逐个拼接 | 追加 `.exe` / `.cmd` |
//!
//! ## 为什么不用 `tokio::process`
//!
//! 推理引擎的生命周期管理与业务并发无关：拉起是一次性的、健康探测是轮询、
//! 终止是同步的。用标准库 `std::process` 就够，避免引入整个 async runtime
//! （网关的 HTTP 服务另行决定，但那是独立关注点）。

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use mangosteen_core::{MnError, MnResult};

/// 在 `PATH` 中查找可执行文件。
///
/// 返回 `Ok(None)` 表示未找到（不是错误——调用方可能要走云端链路）。
pub fn find_executable(name: &str, extra_dirs: &[PathBuf]) -> Option<PathBuf> {    let candidates: Vec<PathBuf> = if cfg!(target_os = "windows") {
        vec![
            PathBuf::from(format!("{name}.exe")),
            PathBuf::from(format!("{name}.cmd")),
            PathBuf::from(name),
        ]
    } else {
        vec![PathBuf::from(name)]
    };

    // 1. 显式目录优先
    for dir in extra_dirs {
        for c in &candidates {
            let p = dir.join(c);
            if is_executable(&p) {
                return Some(p);
            }
        }
    }

    // 2. PATH
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        for c in &candidates {
            let p = dir.join(c);
            if is_executable(&p) {
                return Some(p);
            }
        }
    }
    None
}

/// 平台路径列表分隔符（Unix `:` / Windows `;`）。
pub const fn path_separator() -> &'static str {
    if cfg!(target_os = "windows") {
        ";"
    } else {
        ":"
    }
}

/// 把路径列表拼成平台对应的分隔字符串。
///
/// 平台差异（`:` vs `;`）集中在这里，上层不必写 `cfg!(target_os)` 分支。
pub fn join_paths(paths: &[PathBuf]) -> String {
    let sep = if cfg!(target_os = "windows") { ";" } else { ":" };
    paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(sep)
}

/// 返回 `find_executable` 实际会搜索的目录（按优先级）。
///
/// 用途：诊断输出。用户看到「找不到引擎」时，最想知道的是「你都找过哪儿了」——
/// 把候选目录列出来，比让用户猜 PATH 有没有配更有效。
pub fn search_dirs(extra_dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = extra_dirs.to_vec();
    if let Some(path_var) = std::env::var_os("PATH") {
        for d in std::env::split_paths(&path_var) {
            if !out.contains(&d) {
                out.push(d);
            }
        }
    }
    out
}

/// 判断路径是否为可执行文件。
pub fn is_executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// 子进程句柄封装。统一三平台的启动/终止语义。
#[derive(Debug)]
pub struct ManagedChild {
    child: Child,
    /// 用于日志与错误消息的展示名
    label: String,
}

impl ManagedChild {
    /// PID。
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// 展示名。
    pub fn label(&self) -> &str {
        &self.label
    }

    /// 是否仍在运行。会顺带回收僵尸进程。
    pub fn is_running(&mut self) -> bool {
        match self.child.try_wait() {
            Ok(None) => true,
            Ok(Some(_)) => false,
            Err(_) => false,
        }
    }

    /// 若已退出，返回退出码。
    pub fn exit_code(&mut self) -> Option<Option<i32>> {
        match self.child.try_wait() {
            Ok(None) => None,
            Ok(Some(status)) => Some(status.code()),
            Err(_) => Some(None),
        }
    }

    /// 阻塞等待退出，带超时。
    ///
    /// 返回 `Ok(Some(code))` 表示已退出，`Ok(None)` 表示超时仍在运行。
    pub fn wait_timeout(&mut self, timeout: Duration) -> MnResult<Option<Option<i32>>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(c) = self.exit_code() {
                return Ok(Some(c));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// 优雅终止：先温和信号，超时后强杀。
    ///
    /// 这是「部署失败自动降级重试」的前置动作：不彻底清理旧进程会导致
    /// 端口占用，让重试必然失败。
    pub fn terminate(&mut self, graceful_timeout: Duration) -> MnResult<()> {
        if !self.is_running() {
            return Ok(());
        }
        send_graceful(&self.child, self.child.id());

        if self.wait_timeout(graceful_timeout)?.is_some() {
            return Ok(());
        }
        // 仍在运行 → 强杀
        self.force_kill()
    }

    /// 强杀。
    pub fn force_kill(&mut self) -> MnResult<()> {
        let pid = self.child.id();
        if let Err(e) = self.child.kill() {
            // 进程可能刚好自行退出，此时不算失败
            if !matches!(e.kind(), std::io::ErrorKind::InvalidInput) {
                return Err(MnError::io(format!("强杀进程 {pid}"), &e));
            }
        }
        let _ = self.child.wait();
        Ok(())
    }

    /// 读取 stderr 尾部若干行（用于把引擎崩溃原因透给用户）。
    ///
    /// 需要启动时用 [`SpawnOptions::capture_output`] 捕获管道；
    /// 否则返回空字符串。
    pub fn stderr_tail(&mut self, max_lines: usize) -> String {
        let mut buf = String::new();
        let exited = self.exit_code().is_some();
        if let (true, Some(err)) = (exited, self.child.stderr.as_mut()) {
            use std::io::Read;
            // 非阻塞读取会被管道阻塞，这里只在进程已退出时安全读取
            let _ = err.read_to_string(&mut buf);
        }
        let lines: Vec<&str> = buf.lines().collect();
        let start = lines.len().saturating_sub(max_lines);
        lines[start..].join("\n")
    }
}

/// 启动参数。
#[derive(Debug, Clone)]
pub struct SpawnOptions {
    /// 程序路径
    pub program: PathBuf,
    /// 命令行参数
    pub args: Vec<String>,
    /// 工作目录
    pub cwd: Option<PathBuf>,
    /// 额外环境变量
    pub envs: Vec<(String, String)>,
    /// 是否捕获 stdout/stderr（捕获后需自行读取，否则管道写满会阻塞子进程）
    pub capture_output: bool,
    /// 输出重定向目标文件。优先于 `capture_output`。
    pub log_file: Option<PathBuf>,
    /// 展示名
    pub label: String,
}

impl SpawnOptions {
    /// 构造。
    pub fn new(program: impl Into<PathBuf>, args: Vec<String>, label: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args,
            cwd: None,
            envs: Vec::new(),
            capture_output: false,
            log_file: None,
            label: label.into(),
        }
    }

    /// 设置工作目录。
    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// 添加环境变量。
    pub fn with_env(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.envs.push((k.into(), v.into()));
        self
    }

    /// 捕获输出到日志文件（推荐），而非管道。
    ///
    /// 管道模式下如果父进程不持续读取，子进程写满 64KB 缓冲后会阻塞，
    /// 表现为「引擎启动后卡死」。因此生产路径一律重定向到文件。
    pub fn with_log_file(mut self, path: &Path) -> MnResult<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| MnError::io(format!("创建日志目录 {}", parent.display()), &e))?;
        }
        self.log_file = Some(path.to_path_buf());
        Ok(self)
    }
}

// 给 SpawnOptions 补一个字段（放在 impl 之后会破坏结构体定义，故用独立块）
impl SpawnOptions {
    /// 启动子进程。
    pub fn spawn(&self) -> MnResult<ManagedChild> {
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args);

        if let Some(cwd) = &self.cwd {
            cmd.current_dir(cwd);
        }
        for (k, v) in &self.envs {
            cmd.env(k, v);
        }

        cmd.stdin(Stdio::null());

        if let Some(log) = &self.log_file {
            let out = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log)
                .map_err(|e| MnError::io(format!("打开日志文件 {}", log.display()), &e))?;
            let err = out
                .try_clone()
                .map_err(|e| MnError::io("复制日志文件句柄", &e))?;
            cmd.stdout(Stdio::from(out));
            cmd.stderr(Stdio::from(err));
        } else if self.capture_output {
            cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        } else {
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
        }

        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // 新进程组：父进程意外退出时子进程不会收到同一终端的 Ctrl-C，
            // 但也避免了信号风暴；配合显式 terminate 使用。
            cmd.process_group(0);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
        }

        let child = cmd.spawn().map_err(|e| MnError::EngineLaunch {
            binary: self.program.clone(),
            reason: e.to_string(),
        })?;

        Ok(ManagedChild {
            child,
            label: self.label.clone(),
        })
    }
}

/// 发送优雅终止信号。
fn send_graceful(child: &Child, pid: u32) {
    #[cfg(unix)]
    {
        // SAFETY: kill(2) 接收 pid 与信号号，均为纯值参数；失败时返回 -1 不影响内存安全。
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
        let _ = child;
    }
    #[cfg(windows)]
    {
        let _ = child;
        // taskkill 不带 /F 会发送 WM_CLOSE，对控制台程序等价于 CTRL_C_EVENT 的替代
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (child, pid);
    }
}

/// 一次性探测 HTTP 端点是否返回 200。
///
/// ## 与 [`wait_http_ready`] 的区别（重要）
///
/// | 函数 | 语义 | 耗时上界 | 适用场景 |
/// |------|------|---------|---------|
/// | `wait_http_ready` | 「等到就绪」，内部循环重试 | `timeout` 但每轮可超时累加 | 部署后等引擎加载完成 |
/// | `probe_http_ok` | 「现在是否活着」，只试一次 | 约 `connect_timeout + read_timeout` | 状态查询、健康检查 |
///
/// 早先 `status` 用 `wait_http_ready` 做探活，导致**每个部署要等十几秒**：
/// 2 秒超时只是循环的**退出条件**，而单轮里 `connect(800ms) + read(1500ms)
/// + sleep(200ms)` 最坏 2.5 秒，循环还会多跑一轮。一个部署 5 秒、两个就
/// 10 秒，直接从「毫秒级状态查询」变成「十几秒卡顿」。
pub fn probe_http_ok(host: &str, port: u16, path: &str, per_op: Duration) -> bool {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    let addr = format!("{host}:{port}");
    let Ok(sock) = addr.parse() else {
        return false;
    };
    let Ok(mut stream) = TcpStream::connect_timeout(&sock, per_op) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(per_op));
    let _ = stream.set_write_timeout(Some(per_op));
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    if stream.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut buf = [0u8; 128];
    match stream.read(&mut buf) {
        // 只认 200。llama-server 在模型加载期间返回 503，那表示「尚未就绪」。
        Ok(n) if n > 0 => String::from_utf8_lossy(&buf[..n]).contains(" 200 "),
        _ => false,
    }
}

/// 轮询 HTTP 端点直到成功或超时。
///
/// 用于等待 llama-server 的 `/health` 就绪。**不依赖 HTTP 客户端库**：
/// 只发一个极简的 `GET` 并检查状态行，避免为这一个用途引入 reqwest。
pub fn wait_http_ready(host: &str, port: u16, path: &str, timeout: Duration) -> MnResult<u64> {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    let start = Instant::now();
    let addr = format!("{host}:{port}");
    let mut last_err = String::from("尚未发起探测");

    while start.elapsed() < timeout {
        match TcpStream::connect_timeout(
            &addr
                .parse()
                .unwrap_or_else(|_| ([127, 0, 0, 1], port).into()),
            Duration::from_millis(800),
        ) {
            Ok(mut stream) => {
                let _ = stream.set_read_timeout(Some(Duration::from_millis(1500)));
                let req = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
                if stream.write_all(req.as_bytes()).is_ok() {
                    let mut buf = [0u8; 256];
                    match stream.read(&mut buf) {
                        Ok(n) if n > 0 => {
                            let head = String::from_utf8_lossy(&buf[..n]);
                            if head.starts_with("HTTP/1.") {
                                // 200 / 503 都说明服务已监听。llama-server 在模型
                                // 加载完成前会返回 503，因此只认 200。
                                if head.contains(" 200 ") {
                                    return Ok(start.elapsed().as_millis() as u64);
                                }
                                last_err = format!(
                                    "服务已监听但未就绪：{}",
                                    head.lines().next().unwrap_or("?")
                                );
                            } else {
                                last_err = "响应不是合法 HTTP".to_string();
                            }
                        }
                        Ok(_) => last_err = "连接被关闭但无响应".to_string(),
                        Err(e) => last_err = format!("读取响应失败：{e}"),
                    }
                } else {
                    last_err = "写入请求失败".to_string();
                }
            }
            Err(e) => last_err = format!("连接 {addr} 失败：{e}"),
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    Err(MnError::Network {
        stage: format!("等待 {addr}{path} 就绪"),
        reason: format!(
            "超时 {}s，最后一次错误：{last_err}",
            timeout.as_secs()
        ),
    })
}

/// 判断进程是否仍然存在（不会向它发送任何信号）。
pub fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: kill(pid, 0) 只做存在性与权限检查，不发送实际信号。
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }
    #[cfg(windows)]
    {
        use std::process::{Command, Stdio};
        let out = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output();
        match out {
            Ok(o) => String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()),
            Err(_) => false,
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        false
    }
}

/// 发送温和终止信号（SIGTERM / taskkill 不带 /F）。
#[cfg(unix)]
fn send_signal_term(pid: u32) {
    // SAFETY: kill(2) 参数均为纯值，不涉及内存访问。
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
}

/// 发送强制终止信号（SIGKILL / taskkill /F）。
#[cfg(unix)]
fn send_signal_kill(pid: u32) {
    // SAFETY: 同上。
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
    }
}

#[cfg(windows)]
fn send_signal_term(pid: u32) {
    use std::process::{Command, Stdio};
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(windows)]
fn send_signal_kill(pid: u32) {
    use std::process::{Command, Stdio};
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(not(any(unix, windows)))]
fn send_signal_term(_pid: u32) {}

#[cfg(not(any(unix, windows)))]
fn send_signal_kill(_pid: u32) {}

/// 按 PID 终止进程：先温和信号，`grace` 内未退出则强杀。
///
/// 进程不存在时返回 `Ok(())`（幂等），便于清理流程反复调用。
pub fn terminate_pid(pid: u32, grace: Duration) -> MnResult<()> {
    if !process_alive(pid) {
        return Ok(());
    }
    send_signal_term(pid);

    let deadline = std::time::Instant::now() + grace;
    while std::time::Instant::now() < deadline {
        if !process_alive(pid) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    send_signal_kill(pid);

    // 强杀后的收尾判断。
    //
    // SIGKILL 是不可捕获、不可忽略的，发出后进程**必然**会终止。
    // 此时若 kill(pid, 0) 仍返回成功，只可能是两种情况：
    //   1. 进程已终止但尚未被父进程 wait() 回收（僵尸态）；
    //   2. 内核还没来得及调度终止流程。
    // 两者都不代表「杀不掉」。因此这里放宽为「尽力确认，不因此报错」，
    // 避免把正常的清理流程变成失败——错误地报告失败会让调用方误以为
    // 资源泄漏，进而重复清理甚至放弃清理。
    for _ in 0..15 {
        if !process_alive(pid) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    if process_alive(pid) {
        // 仍存在但极可能是僵尸；用 ps 尽力确认（某些沙箱环境禁止调用 ps，
        // 此时无法判定，按已终止处理而不是报错）。
        if process_is_zombie_or_unknown(pid) {
            return Ok(());
        }
        return Err(MnError::Io {
            context: format!("强制终止进程 {pid}"),
            reason: "SIGKILL 已发出但进程仍在运行，可能无权限终止他人进程".to_string(),
        });
    }
    Ok(())
}

/// 判断进程是否为僵尸；无法判定时返回 `true`（倾向于「已终止」）。
///
/// 设计理由：SIGKILL 发出后，进程在正常权限下一定会死。此时若无法调用
/// `ps` 确认状态（受限环境），把「疑似僵尸」当成僵尸比误报失败更安全——
/// 误报会让你以为需要人工介入，而实际上资源已经释放了。
#[cfg(unix)]
fn process_is_zombie_or_unknown(pid: u32) -> bool {
    let out = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output();
    match out {
        Ok(o) => {
            let stat = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if stat.is_empty() {
                // ps 查不到该 pid → 进程确实没了
                return true;
            }
            stat.starts_with('Z')
        }
        // 无法执行 ps（权限/沙箱）→ 无法判定，倾向已终止
        Err(_) => true,
    }
}

#[cfg(not(unix))]
fn process_is_zombie_or_unknown(_pid: u32) -> bool {
    true
}

/// 探测端口是否可绑定。
pub fn is_port_bindable(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_system_executable() {
        // 三平台都有 sh / cmd；用绝对存在的 shell 验证查找逻辑
        let p = if cfg!(windows) {
            find_executable("cmd", &[])
        } else {
            find_executable("sh", &[])
        };
        let p = p.expect("should find a shell on PATH");
        assert!(p.is_absolute() || p.exists());
    }

    #[test]
    fn missing_executable_returns_none() {
        assert!(find_executable("mangosteen-definitely-not-a-real-binary-xyz", &[]).is_none());
    }

    #[test]
    fn extra_dirs_take_priority() {
        let dir = std::env::temp_dir().join(format!("mn-find-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("mnfake");
        std::fs::write(&fake, "#!/bin/sh\ntrue\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
            let found = find_executable("mnfake", &[dir.clone()]);
            assert_eq!(found, Some(fake));
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn is_executable_rejects_non_executable_file() {
        let dir = std::env::temp_dir().join(format!("mn-exec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("plain.txt");
        std::fs::write(&f, "x").unwrap();
        #[cfg(unix)]
        assert!(!is_executable(&f), "无执行位不应视为可执行");
        assert!(!is_executable(&dir));
        assert!(!is_executable(Path::new("/nonexistent/xyz")));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn spawn_and_terminate_short_lived_process() {
        let (prog, args) = if cfg!(windows) {
            (PathBuf::from("cmd"), vec!["/C".to_string(), "exit 0".to_string()])
        } else {
            (PathBuf::from("sh"), vec!["-c".to_string(), "exit 0".to_string()])
        };
        let opts = SpawnOptions::new(prog, args, "test-child");
        let mut child = opts.spawn().unwrap();
        let code = child.wait_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(code, Some(Some(0)));
        assert!(!child.is_running());
        // 已退出时 terminate 应为幂等无操作
        child.terminate(Duration::from_millis(100)).unwrap();
    }

    #[test]
    fn terminate_long_running_process() {
        let (prog, args) = if cfg!(windows) {
            (
                PathBuf::from("cmd"),
                vec!["/C".to_string(), "ping -n 30 127.0.0.1".to_string()],
            )
        } else {
            (
                PathBuf::from("sh"),
                vec!["-c".to_string(), "sleep 30".to_string()],
            )
        };
        let mut child = SpawnOptions::new(prog, args, "sleeper").spawn().unwrap();
        assert!(child.is_running());
        let pid = child.pid();
        assert!(pid > 0);
        child.terminate(Duration::from_millis(500)).unwrap();
        assert!(!child.is_running(), "terminate 后进程必须已退出");
    }

    #[test]
    fn missing_program_maps_to_engine_launch_error() {
        let opts = SpawnOptions::new(
            PathBuf::from("/nonexistent/definitely/missing"),
            vec![],
            "missing",
        );
        let e = opts.spawn().unwrap_err();
        assert!(matches!(e, MnError::EngineLaunch { .. }));
        // 引擎启动失败必须可重试（触发云端降级）
        assert!(e.is_retryable());
    }

    #[test]
    fn log_file_redirection_works() {
        let dir = std::env::temp_dir().join(format!("mn-log-{}", std::process::id()));
        let log = dir.join("engine.log");
        let (prog, args) = if cfg!(windows) {
            (PathBuf::from("cmd"), vec!["/C".to_string(), "echo hello".to_string()])
        } else {
            (
                PathBuf::from("sh"),
                vec!["-c".to_string(), "echo hello-from-engine".to_string()],
            )
        };
        let opts = SpawnOptions::new(prog, args, "logger")
            .with_log_file(&log)
            .unwrap();
        let mut child = opts.spawn().unwrap();
        child.wait_timeout(Duration::from_secs(10)).unwrap();
        let content = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(
            content.contains("hello"),
            "日志文件应包含子进程输出，实际：{content:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn port_bindable_detection() {
        // 绑定一个端口后应探测为不可用
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        // 监听中必然不可再绑定（Windows 未设 SO_REUSEADDR 亦成立）
        assert!(!is_port_bindable(port), "监听中的端口不应可再次绑定");
        drop(listener);
        // 释放后可能短暂处于 TIME_WAIT，允许有限重试窗口
        let mut freed = false;
        for _ in 0..40 {
            if is_port_bindable(port) {
                freed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(freed, "端口 {port} 在 2s 内仍不可绑定");
    }

    #[test]
    fn wait_http_ready_times_out_with_actionable_error() {
        // 用一个必定无人监听的端口
        let e = wait_http_ready("127.0.0.1", 1, "/health", Duration::from_millis(300)).unwrap_err();
        assert!(matches!(e, MnError::Network { .. }));
        assert!(e.to_string().contains("超时"));
    }

    #[test]
    fn wait_http_ready_succeeds_against_live_listener() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        // 起一个极简 HTTP 服务，固定回 200
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            for stream in listener.incoming().take(4) {
                let mut s = match stream {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let mut buf = [0u8; 512];
                let _ = s.read(&mut buf);
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}");
            }
        });

        let elapsed = wait_http_ready("127.0.0.1", port, "/health", Duration::from_secs(5));
        assert!(elapsed.is_ok(), "should detect readiness: {elapsed:?}");
        drop(handle);
    }
}
