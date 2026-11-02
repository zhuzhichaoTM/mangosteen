//! `mangosteen ui`
//!
//! 归属：**用户交互层 → 命令 `ui`**。
//!
//! 启动内置 Web 控制台并自动打开浏览器。
//!
//! ## 与 `serve` 的区别
//!
//! 两者**跑的是同一个网关服务**，界面也挂在同一端口；差别只在交互形态：
//!
//! | 命令 | 面向 | 额外行为 |
//! |------|------|---------|
//! | `serve` | 脚本 / IDE 插件 / OpenAI SDK | 只起服务，打印接口地址 |
//! | `ui` | 人 | 起服务 + 自动打开浏览器 + 打印可粘贴的地址 |
//!
//! 之所以不做成两个服务：前端要调 `/api/*` 与 `/v1/*`，同源最省事，
//! 也避免「静态服务器」被绕过鉴权的问题。
//!
//! ## 为什么不用 --open 之类的选项名
//!
//! `ui` 的默认行为就是「给人用」，自动打开浏览器符合直觉；
//! 需要纯服务形态时用 `serve`。因此提供 `--no-open` 用于
//! 远程/无桌面环境（如 SSH、容器）——那里打开浏览器会静默失败。

use std::fs;
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use mangosteen_core::MnResult;

use crate::cli::Args;
use crate::commands::reject_unknown_options;
use crate::output;

/// 允许的选项。
const ALLOWED: &[&str] = &["config", "port", "host", "no-open", "quiet", "foreground"];

/// UI 生命周期子命令：`start` / `restart` / `stop` / `status`。
pub fn run_lifecycle(args: Args) -> MnResult<i32> {
    reject_unknown_options(&args, ALLOWED)?;
    let action = args.pos(1).unwrap_or("start");
    match action {
        "start" => start_background(&args),
        "restart" => {
            let _ = stop_background(&args);
            start_background(&args)
        }
        "stop" => stop_background(&args),
        "status" => status_background(&args),
        other => Err(mangosteen_core::MnError::bad_request(format!(
            "未知 UI 操作 `{other}`。可用：start、restart、stop、status"
        ))),
    }
}

fn lifecycle_paths() -> (PathBuf, PathBuf) {
    let env = &mangosteen_platform::paths::real_env;
    (
        mangosteen_platform::paths::gateway_pid_file(env),
        mangosteen_platform::paths::gateway_log_file(env),
    )
}

fn read_pid() -> Option<u32> {
    let (pid, _) = lifecycle_paths();
    fs::read_to_string(pid).ok()?.trim().parse().ok()
}

fn effective_pid(port: u16) -> Option<u32> {
    read_pid()
        .filter(|pid| mangosteen_platform::process::process_alive(*pid))
        .or_else(|| mangosteen_platform::process::find_listener_pid(port))
}

fn healthy(port: u16) -> bool {
    let Ok(addr) = format!("127.0.0.1:{port}").parse() else {
        return false;
    };
    let Ok(mut s) = TcpStream::connect_timeout(&addr, Duration::from_millis(300)) else {
        return false;
    };
    use std::io::{Read, Write};
    let _ = s.write_all(b"GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    let mut b = [0u8; 128];
    s.read(&mut b)
        .map(|n| String::from_utf8_lossy(&b[..n]).contains(" 200 "))
        .unwrap_or(false)
}

fn start_background(args: &Args) -> MnResult<i32> {
    let (pid_path, log_path) = lifecycle_paths();
    let gw = crate::commands::build_gateway(args)?;
    let port = args
        .get_num::<u16>("port")?
        .unwrap_or(gw.config().gateway.port);
    if let Some(pid) = effective_pid(port) {
        if mangosteen_platform::process::process_alive(pid) && healthy(port) {
            output::print_info(&format!("UI 已运行（PID {pid}），无需重复启动"));
            return Ok(0);
        }
    }
    if let Some(parent) = pid_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| mangosteen_core::MnError::io("创建网关状态目录", &e))?;
    }
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| mangosteen_core::MnError::io("创建网关日志目录", &e))?;
    }
    let exe = std::env::current_exe()
        .map_err(|e| mangosteen_core::MnError::io("定位 mangosteen 可执行文件", &e))?;
    #[cfg(unix)]
    let mut cmd = {
        // macOS 没有默认 `setsid`，使用系统 `nohup` 脱离终端挂断信号；
        // stdout/stderr 已由下方重定向到持久日志。
        let mut c = Command::new("nohup");
        c.arg(&exe);
        c
    };
    #[cfg(not(unix))]
    let mut cmd = Command::new(&exe);
    cmd.arg("ui").arg("--foreground").arg("--no-open");
    // 后台网关必须脱离当前终端/对话会话，否则父命令退出后，
    // 宿主环境可能回收同一进程组，表现为 start 成功但服务立即消失。
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.stdin(Stdio::null());
    // 让子进程自己写 PID：包装命令（nohup/setsid）与实际监听进程的
    // PID 未必一致，父进程猜一次就可能写错，导致「status 说没运行、
    // 端口却被自己占着」。自注册后 PID 文件永远指向真实网关。
    let (pid_path_env, _) = lifecycle_paths();
    let _ = fs::remove_file(&pid_path_env);
    cmd.env("MANGOSTEEN_GATEWAY_PIDFILE", &pid_path_env);
    if let Some(v) = args.get("config") {
        cmd.arg("--config").arg(v);
    }
    if let Some(v) = args.get("host") {
        cmd.arg("--host").arg(v);
    }
    if let Some(v) = args.get("port") {
        cmd.arg("--port").arg(v);
    }
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| mangosteen_core::MnError::io("打开网关日志", &e))?;
    let err = log
        .try_clone()
        .map_err(|e| mangosteen_core::MnError::io("复制网关日志句柄", &e))?;
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(err))
        .spawn()
        .map_err(|e| mangosteen_core::MnError::io("启动后台 API 网关", &e))?;
    // PID 由网关进程自注册；这里只做兜底（老版本/自注册失败时仍有记录）。
    for _ in 0..30 {
        if healthy(port) {
            if read_pid().is_none() {
                let _ = fs::write(&pid_path, child.id().to_string());
            }
            output::print_success(&format!("UI/API 网关已启动：http://127.0.0.1:{port}"));
            return Ok(0);
        }
        if let Ok(Some(_)) = child.try_wait() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(mangosteen_core::MnError::Network {
        stage: "启动 API 网关".to_string(),
        reason: format!("服务未在 3 秒内就绪，日志：{}", log_path.display()),
    })
}

fn stop_background(args: &Args) -> MnResult<i32> {
    let (pid_path, _) = lifecycle_paths();
    let gw = crate::commands::build_gateway(args)?;
    let port = args
        .get_num::<u16>("port")?
        .unwrap_or(gw.config().gateway.port);
    let Some(pid) = effective_pid(port) else {
        output::print_info("UI/API 网关未运行");
        return Ok(0);
    };
    mangosteen_platform::process::terminate_pid(pid, Duration::from_secs(3))?;
    let _ = fs::remove_file(pid_path);
    output::print_success("UI/API 网关已关闭（不影响已部署的推理服务）");
    Ok(0)
}

fn status_background(args: &Args) -> MnResult<i32> {
    let gw = crate::commands::build_gateway(args)?;
    let port = args
        .get_num::<u16>("port")?
        .unwrap_or(gw.config().gateway.port);
    match effective_pid(port) {
        Some(pid) if mangosteen_platform::process::process_alive(pid) && healthy(port) => {
            output::print_success(&format!("UI/API 网关运行中（PID {pid}，端口 {port}）"));
            Ok(0)
        }
        _ => {
            output::print_info("UI/API 网关未运行");
            Ok(1)
        }
    }
}
/// 前台兼容入口：`ui --foreground --no-open`。
pub fn run(args: Args) -> MnResult<i32> {
    // 选项白名单校验必须在这里也做一次：`run_lifecycle` 只覆盖
    // `ui start|restart|stop|status` 这条子命令路径，前台直跑 `mangosteen ui`
    // 走的是本函数。缺了这一步，拼错的参数会被静默忽略，用户只会看到
    // 「服务起来了但参数没生效」这种更难排查的现象。
    reject_unknown_options(&args, ALLOWED)?;
    let gw = crate::commands::build_gateway(&args)?;
    let cfg = gw.config().clone();

    let host = args.get("host").unwrap_or(&cfg.gateway.host).to_string();
    let port = args.get_num::<u16>("port")?.unwrap_or(cfg.gateway.port);

    if port < 1024 {
        return Err(mangosteen_core::MnError::config(
            "gateway.port",
            "不得使用特权端口（<1024）",
        ));
    }

    let addr = format!("{host}:{port}");
    let listener = TcpListener::bind(&addr).map_err(|e| mangosteen_core::MnError::Network {
        stage: "绑定网关端口".to_string(),
        reason: format!(
            "{addr}：{e}。可能端口已被占用，用 --port 换一个，\
             或执行 `mangosteen stop --all` 清理残留进程"
        ),
    })?;

    let require_key = cfg.gateway.require_api_key;
    let api_key = if require_key {
        cfg.gateway
            .api_key
            .resolve(mangosteen_platform::paths::real_env)?
    } else {
        String::new()
    };

    let url = browser_url(port);

    if !args.has("quiet") {
        output::print_success(&format!("Mangosteen 控制台已启动：{url}"));
        output::print_kv(&[
            ("控制台".into(), url.clone()),
            ("OpenAI 兼容".into(), format!("http://127.0.0.1:{port}/v1")),
            (
                "鉴权".into(),
                if require_key {
                    "已开启（页面需在浏览器中带上 Bearer key）".to_string()
                } else {
                    output::dim("未开启").to_string()
                },
            ),
        ]);
        println!();

        if host == "0.0.0.0" || host == "::" {
            output::print_warning(
                "服务监听在所有网卡上，局域网内其他设备可访问。\
                 请开启 gateway.require_api_key，否则任何人都能用你的模型",
            );
        }
        if require_key {
            output::print_warning(
                "已开启鉴权：浏览器直接打开页面时不含 API Key，\
                 请求会返回 401。建议用 `mangosteen ui --no-open` 配合反向代理，\
                 或临时关闭 gateway.require_api_key",
            );
        }
        output::print_info("按 Ctrl-C 停止（不会影响已部署的推理服务）");
        println!();
    }

    // 打开浏览器放在服务开始接受连接之前：listen 已 bind，端口一定可用，
    // 浏览器不会遇到「连接被拒」。
    if !args.has("no-open") {
        match open_browser(&url) {
            Ok(()) => {}
            Err(e) => {
                // 打不开浏览器不是致命错误：用户可手工粘贴地址
                output::print_warning(&format!("无法自动打开浏览器（{e}）。请手工访问：{url}"));
            }
        }
    }

    let shared = std::sync::Arc::new(std::sync::Mutex::new(gw));

    // 后台模式（`ui start`）由本进程自注册 PID：包装命令与真实网关进程的
    // PID 未必一致，只有进程自己写才是准确记录。
    let self_pid_file = std::env::var_os("MANGOSTEEN_GATEWAY_PIDFILE").map(PathBuf::from);
    if let Some(path) = &self_pid_file {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(path, std::process::id().to_string());
    }

    // 后台预热引擎版本：`llama-server --version` 在部分安装上要 20~30 秒，
    // 若用户在首个请求里承担这段等待，页面会白屏半分钟。
    mangosteen_gateway::spawn_warm_up(std::sync::Arc::clone(&shared));

    let server = mangosteen_gateway::GatewayServer::new(
        shared,
        require_key,
        api_key,
        Duration::from_secs(300),
    )
    // 启用云端监控后台采集：设备页在「使用云端算力」时显示云端指标
    .with_cloud_monitor()
    // 启用部署运行态周期探测：部署页签区分「运行 / 停止」
    .with_runtime_probe();

    let result = server.serve(listener);
    if let Some(path) = self_pid_file {
        let _ = fs::remove_file(path);
    }
    result.map(|_| 0)
}

/// 浏览器要访问的地址。
///
/// 始终用 `127.0.0.1` 而不是 `host`：即便服务监听 `0.0.0.0`，
/// 本机访问回环地址也最可靠，不依赖 DNS 与主机名解析。
fn browser_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/")
}

/// 用系统默认浏览器打开 URL。
///
/// 平台差异集中在这里的决定：CLI 层允许处理终端/桌面交互，
/// 但具体命令必须按平台选（`open` / `xdg-open` / `start`）。
fn open_browser(url: &str) -> Result<(), String> {
    use std::process::{Command, Stdio};

    let (program, args): (&str, Vec<&str>) = if cfg!(target_os = "macos") {
        ("open", vec![url])
    } else if cfg!(target_os = "windows") {
        // `start` 是 cmd 内建命令，必须经 cmd /C 调用；
        // 空字符串是 start 的「窗口标题」参数占位，否则 URL 会被当标题。
        ("cmd", vec!["/C", "start", "", url])
    } else {
        ("xdg-open", vec![url])
    };

    Command::new(program)
        .args(&args)
        // 不等待浏览器进程（它通常常驻），也避免占用 stdout
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("启动 {program} 失败：{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_privileged_port() {
        let a = crate::cli::parse_args(&["ui".to_string(), "--port".to_string(), "80".to_string()])
            .unwrap();
        let e = run(a).unwrap_err();
        assert!(e.to_string().contains("特权端口"), "got {e}");
    }

    #[test]
    fn rejects_unknown_option() {
        let a = crate::cli::parse_args(&["ui".to_string(), "--wat".to_string(), "1".to_string()])
            .unwrap();
        let e = run(a).unwrap_err();
        assert!(e.to_string().contains("--wat"), "got {e}");
    }

    #[test]
    fn no_open_is_accepted() {
        // 只校验选项白名单，不真的起服务
        let a = crate::cli::parse_args(&["ui".to_string(), "--no-open".to_string()]).unwrap();
        assert!(reject_unknown_options(&a, ALLOWED).is_ok());
    }

    #[test]
    fn browser_url_targets_loopback() {
        // 浏览器必须访问回环地址：即便服务监听 0.0.0.0，
        // 用 127.0.0.1 也不依赖 DNS 解析。
        // 端口用常量而非字面量：默认端口改过一次（11434→11516），
        // 写死字面量会让这条测试与真实默认值脱节。
        let port = mangosteen_core::DEFAULT_GATEWAY_PORT;
        let url = browser_url(port);
        assert_eq!(url, format!("http://127.0.0.1:{port}/"));
    }
}
