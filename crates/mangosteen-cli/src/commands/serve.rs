//! `mangosteen serve`
//!
//! 归属：**用户交互层 → 命令 `serve`**。
//!
//! 只启动 API 网关的 HTTP 服务，不做部署。
//!
//! ## 典型用法
//!
//! * 一个终端跑 `mangosteen deploy <模型>`（或 `run`）；
//! * 另一个终端跑 `mangosteen serve`，把统一 API 入口挂到固定端口上；
//! * IDE 插件 / 脚本 / OpenAI SDK 只连 `serve` 的端口。
//!
//! 这正是「统一 API 网关」的落地形态：上层应用不需要知道模型在本地还是云端。

use std::net::TcpListener;
use std::time::Duration;

use mangosteen_core::MnResult;

use crate::cli::Args;
use crate::commands::reject_unknown_options;
use crate::output;

/// 允许的选项。
const ALLOWED: &[&str] = &["config", "port", "host", "quiet"];

/// 入口。
pub fn run(args: Args) -> MnResult<i32> {
    reject_unknown_options(&args, ALLOWED)?;

    let gw = crate::commands::build_gateway(&args)?;
    let cfg = gw.config().clone();

    let host = args.get("host").unwrap_or(&cfg.gateway.host).to_string();
    let port = args
        .get_num::<u16>("port")?
        .unwrap_or(cfg.gateway.port);

    if port < 1024 {
        return Err(mangosteen_core::MnError::config(
            "gateway.port",
            "不得使用特权端口（<1024）",
        ));
    }

    let addr = format!("{host}:{port}");
    let listener = TcpListener::bind(&addr).map_err(|e| {
        mangosteen_core::MnError::Network {
            stage: "绑定网关端口".to_string(),
            reason: format!(
                "{addr}：{e}。可能端口已被占用，用 --port 换一个，\
                 或执行 `mangosteen stop --all` 清理残留进程"
            ),
        }
    })?;

    // 鉴权配置
    let require_key = cfg.gateway.require_api_key;
    let api_key = if require_key {
        cfg.gateway.api_key.resolve(&mangosteen_platform::paths::real_env)?
    } else {
        String::new()
    };

    output::print_success(&format!("Mangosteen API 网关已启动：http://{addr}"));
    output::print_kv(&[
        ("健康检查".into(), format!("http://{addr}/health")),
        ("OpenAI 兼容".into(), format!("http://{addr}/v1")),
        ("模型列表".into(), format!("http://{addr}/v1/models")),
        ("部署接口".into(), format!("POST http://{addr}/api/deploy")),
        (
            "鉴权".into(),
            if require_key {
                "已开启（Authorization: Bearer <key>）".to_string()
            } else {
                output::dim("未开启").to_string()
            },
        ),
    ]);
    println!();
    output::print_info("按 Ctrl-C 停止网关（不会影响已部署的推理服务）");

    let shared = std::sync::Arc::new(std::sync::Mutex::new(gw));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_privileged_port() {
        let a = crate::cli::parse_args(
            &["serve".to_string(), "--port".to_string(), "80".to_string()],
        )
        .unwrap();
        let e = run(a).unwrap_err();
        assert!(e.to_string().contains("特权端口"), "got {e}");
    }

    #[test]
    fn rejects_unknown_option() {
        let a = crate::cli::parse_args(
            &["serve".to_string(), "--weird".to_string(), "1".to_string()],
        )
        .unwrap();
        let e = run(a).unwrap_err();
        assert!(e.to_string().contains("--weird"));
    }
}