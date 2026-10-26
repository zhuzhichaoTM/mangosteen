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

use std::net::TcpListener;
use std::time::Duration;

use mangosteen_core::MnResult;

use crate::cli::Args;
use crate::commands::reject_unknown_options;
use crate::output;

/// 允许的选项。
const ALLOWED: &[&str] = &["config", "port", "host", "no-open", "quiet"];

/// 入口。
pub fn run(args: Args) -> MnResult<i32> {
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
        cfg.gateway.api_key.resolve(&mangosteen_platform::paths::real_env)?
    } else {
        String::new()
    };

    let url = browser_url(port);

    if !args.has("quiet") {
        output::print_success(&format!("Mangosteen 控制台已启动：{url}"));
        output::print_kv(&[
            ("控制台".into(), url.clone()),
            (
                "OpenAI 兼容".into(),
                format!("http://127.0.0.1:{port}/v1"),
            ),
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
                output::print_warning(&format!(
                    "无法自动打开浏览器（{e}）。请手工访问：{url}"
                ));
            }
        }
    }

    let shared = std::sync::Arc::new(std::sync::Mutex::new(gw));

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
    .with_cloud_monitor();

    server.serve(listener)?;
    Ok(0)
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
        let a = crate::cli::parse_args(&[
            "ui".to_string(),
            "--port".to_string(),
            "80".to_string(),
        ])
        .unwrap();
        let e = run(a).unwrap_err();
        assert!(e.to_string().contains("特权端口"), "got {e}");
    }

    #[test]
    fn rejects_unknown_option() {
        let a = crate::cli::parse_args(&[
            "ui".to_string(),
            "--wat".to_string(),
            "1".to_string(),
        ])
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
