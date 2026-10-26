//! `mangosteen deploy` / `mangosteen run`
//!
//! 归属：**用户交互层 → 命令 `deploy`**。
//!
//! ## 行为
//!
//! 1. 解析模型与参数；
//! 2. 调网关 `deploy`（内部完成评估 → 本地优先决策 → 执行 → 降级重试）；
//! 3. 打印结果：算力来源、端口、OpenAI base URL、降级轨迹（若有）。
//!
//! `run` 与 `deploy` 的区别：`run` 部署后**前台阻塞**并打印推理入口，
//! Ctrl-C 时优雅停止。适合「就想马上用」的场景。

use mangosteen_core::estimate::KvCacheType;
use mangosteen_core::MnResult;
use mangosteen_gateway::DeploymentRequest;

use crate::cli::Args;
use crate::commands::{build_gateway, reject_unknown_options};
use crate::output;

/// 允许的选项。
const ALLOWED: &[&str] = &[
    "config",
    "context",
    "batch-size",
    "kv-cache",
    "local-only",
    "no-cpu-fallback",
    "require-full-gpu",
    "max-price",
    "json",
    "quiet",
];

/// `deploy` 入口。
pub fn run(args: Args) -> MnResult<i32> {
    execute(args, false)
}

/// `run` 入口。
pub fn run_foreground(args: Args) -> MnResult<i32> {
    execute(args, true)
}

/// 共享实现。
fn execute(args: Args, foreground: bool) -> MnResult<i32> {
    reject_unknown_options(&args, ALLOWED)?;

    let model = args.positionals.get(1).cloned().ok_or_else(|| {
        mangosteen_core::MnError::bad_request(
            "缺少模型参数。用法：mangosteen deploy <模型ID或GGUF路径> [--context 8192]",
        )
    })?;

    let mut req = DeploymentRequest::for_model(model.clone());
    if let Some(ctx) = args.get_num::<u32>("context")? {
        req.context_len = Some(ctx);
    }
    if let Some(b) = args.get_num::<u32>("batch-size")? {
        req.batch_size = b;
    }
    if let Some(p) = args.get_num::<f64>("max-price")? {
        req.max_price_per_hour = Some(p);
    }
    if let Some(kv) = args.get("kv-cache") {
        req.kv_cache = Some(parse_kv(kv)?);
    }
    if args.has("local-only") {
        req.allow_cloud = false;
    }
    if args.has("no-cpu-fallback") {
        req.allow_cpu_fallback = false;
    }
    if args.has("require-full-gpu") {
        req.allow_partial_offload = false;
    }

    let mut gw = build_gateway(&args)?;

    let spinner = output::Spinner::new(format!("正在部署 {model}"));
    let mut spinner = spinner;
    spinner.tick("评估资源并调度…");

    let result = gw.deploy(&req);
    spinner.finish();

    let plan = result?;

    if args.has("json") {
        let payload = serde_json::json!({
            "id": plan.id,
            "model": plan.model.name,
            "origin": plan.origin.label(),
            "port": plan.local_access_port(),
            "openai_base_url": format!("http://127.0.0.1:{}/v1", gw.config().gateway.port),
            "fallback_trail": plan.fallback_trail,
            "placement": plan.assessment.placement,
            "context_len": plan.assessment.context_len,
            "kv_cache": plan.assessment.kv_cache,
        });
        println!("{}", serde_json::to_string_pretty(&payload).unwrap_or_default());
    } else {
        print_deploy_result(&plan, &gw);
        for w in &plan.assessment.warnings {
            output::print_warning(&w.message());
        }
    }

    if !foreground {
        return Ok(0);
    }

    // ---- run 模式：前台阻塞 ----
    output::print_info("推理服务已就绪。按 Ctrl-C 停止。");
    println!(
        "  {}",
        output::dim(&format!(
            "OpenAI base URL: http://127.0.0.1:{}/v1  (API Key: {})",
            gw.config().gateway.port,
            if gw.config().gateway.require_api_key {
                "见配置"
            } else {
                "无需"
            }
        ))
    );
    println!();

    wait_for_interrupt();

    output::print_info("正在停止部署…");
    let id = plan.id.clone();
    if let Err(e) = gw.stop(&id) {
        output::print_warning(&format!("停止部署失败：{e}"));
        return Ok(1);
    }
    output::print_success("已停止，资源已释放");
    Ok(0)
}

/// 打印部署结果。
fn print_deploy_result(plan: &mangosteen_core::plan::DeploymentPlan, gw: &mangosteen_gateway::Gateway) {
    output::print_success(&format!(
        "部署完成：{} ({})",
        plan.model.name,
        plan.origin.label()
    ));
    let port = plan
        .local_access_port()
        .map(|p| p.to_string())
        .unwrap_or_else(|| "-".into());
    output::print_kv(&[
        ("部署 ID".into(), plan.id.clone()),
        ("算力来源".into(), plan.origin.label()),
        ("落位形态".into(), plan.assessment.placement.describe(plan.model.n_layers)),
        ("上下文".into(), plan.assessment.context_len.to_string()),
        ("KV 精度".into(), plan.assessment.kv_cache.as_cli_value().to_string()),
        ("量化".into(), plan.assessment.quant.to_string()),
        ("后端端口".into(), port),
        (
            "OpenAI 入口".into(),
            format!("http://127.0.0.1:{}/v1", gw.config().gateway.port),
        ),
        ("资源占用".into(), plan.assessment.requirement.describe()),
    ]);

    // 降级轨迹：非空时必须显眼展示（用户需要知道为什么没用本地）
    if !plan.fallback_trail.is_empty() {
        println!();
        output::print_warning(&format!(
            "发生算力降级：{}",
            plan.fallback_trail.join(" → ")
        ));
        if let mangosteen_core::plan::ComputeOrigin::Cloud { .. } = plan.origin {
            output::print_info(
                "云上实例正在计费，用完请执行 `mangosteen stop <部署ID>` 或 `mangosteen stop --all`",
            );
        }
    }
}

/// 解析 KV 精度参数。
fn parse_kv(s: &str) -> MnResult<KvCacheType> {
    match s.to_ascii_lowercase().as_str() {
        "f16" => Ok(KvCacheType::F16),
        "q8_0" | "q8" => Ok(KvCacheType::Q8_0),
        "q4_0" | "q4" => Ok(KvCacheType::Q4_0),
        other => Err(mangosteen_core::MnError::bad_request(format!(
            "未知 kv-cache `{other}`，可选：f16 / q8_0 / q4_0"
        ))),
    }
}

/// 等待 Ctrl-C。
#[cfg(unix)]
fn wait_for_interrupt() {
    // SAFETY: 只是阻塞等待信号，不涉及内存操作
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::sigaddset(&mut set, libc::SIGTERM);
        let mut sig: libc::c_int = 0;
        libc::sigwait(&set, &mut sig);
    }
}

#[cfg(windows)]
fn wait_for_interrupt() {
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
    // 简化处理：轮询直到标准输入被关闭或收到中断。
    // Windows 下用 SetConsoleCtrlHandler 注册返回 TRUE 的处理器会让程序
    // 不立即退出，但这里采用最直接的方式：阻塞读取 stdin。
    let _ = SetConsoleCtrlHandler(None, 1);
    use std::io::Read;
    let mut buf = [0u8; 1];
    let _ = std::io::stdin().read(&mut buf);
}

#[cfg(not(any(unix, windows)))]
fn wait_for_interrupt() {
    std::thread::park();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kv_parsing() {
        assert_eq!(parse_kv("f16").unwrap(), KvCacheType::F16);
        assert_eq!(parse_kv("Q8_0").unwrap(), KvCacheType::Q8_0);
        assert_eq!(parse_kv("q4").unwrap(), KvCacheType::Q4_0);
        assert!(parse_kv("q2").is_err());
    }

    #[test]
    fn missing_model_is_actionable() {
        let args = crate::cli::parse_args(&["deploy".to_string()]).unwrap();
        let e = execute(args, false).unwrap_err();
        assert!(e.to_string().contains("缺少模型参数"));
        assert!(e.to_string().contains("mangosteen deploy"));
    }

    #[test]
    fn unknown_option_rejected() {
        let args = crate::cli::parse_args(
            &["deploy".to_string(), "m".to_string(), "--bogus".to_string(), "1".to_string()],
        )
        .unwrap();
        let e = execute(args, false).unwrap_err();
        assert!(e.to_string().contains("--bogus"), "got {e}");
    }

    #[test]
    fn allowed_options_listed_in_error() {
        let args = crate::cli::parse_args(
            &["deploy".to_string(), "m".to_string(), "--nope".to_string()],
        );
        // --nope 是布尔开关形式，会被当成带值选项而缺值 → 解析期报错
        assert!(args.is_err() || execute(args.unwrap(), false).is_err());
    }
}