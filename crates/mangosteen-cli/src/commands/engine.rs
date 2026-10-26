//! `mangosteen engine`
//!
//! 归属：**用户交互层 → 命令 `engine`**。
//!
//! 本地推理引擎（llama.cpp / llama-server）的定位与运行时诊断。
//!
//! ## 为什么需要这个命令
//!
//! 「引擎找不到」和「引擎找到了但起不来」是两类完全不同的故障，用户的
//! 排查方向也不同。`doctor` 只给结论，本命令给出**依据**：
//!
//! * `engine locate` —— 到底定位到了哪个二进制，搜索过哪些目录
//! * `engine env`    —— 启动时会注入哪些环境变量（尤其动态库路径）
//! * `engine probe`  —— 实际执行 `--version`，验证「能不能真的跑」
//!
//! ## 与 Ollama 集成方式的对应关系
//!
//! Ollama 把 llama.cpp 的 runner 与动态库作为整体分发，启动时由宿主进程
//! 显式设置库搜索路径，因此不受系统 `LC_RPATH` / `RUNPATH` 是否正确影响。
//! Mangosteen 复用系统已装引擎以减小分发体积，但**同样由自己计算并注入**
//! 库搜索路径 —— 这正是 `engine env` 想让人看到的东西。
//!
//! 全部逻辑经网关层转发（网关唯一收口原则），CLI 不直接持有引擎对象。

use mangosteen_core::MnResult;

use crate::cli::Args;
use crate::commands::{load_config, reject_unknown_options};
use crate::output;

/// 允许的选项。
const ALLOWED: &[&str] = &["config", "json", "quiet"];

/// 入口。
pub fn run(args: Args) -> MnResult<i32> {
    reject_unknown_options(&args, ALLOWED)?;
    let sub = args.pos(1).unwrap_or("info").to_string();
    match sub.as_str() {
        "info" => info(args),
        "locate" => locate(args),
        "env" => env(args),
        "probe" | "test" => probe(args),
        other => Err(mangosteen_core::MnError::bad_request(format!(
            "未知子命令 `engine {other}`。可用：info / locate / env / probe"
        ))),
    }
}

/// `engine info` —— 运行时布局 + 诊断摘要。
fn info(args: Args) -> MnResult<i32> {
    let cfg = match load_config(&args) {
        Ok((c, _)) => Some(c),
        Err(_) => None,
    };
    let diag = mangosteen_gateway::diagnose_engine(cfg.as_ref());
    let layout = mangosteen_gateway::engine_runtime_layout();

    if args.has("json") {
        let payload = serde_json::json!({
            "ready": diag.is_ready(),
            "binary": diag.binary.as_ref().map(|p| p.display().to_string()),
            "version": diag.version,
            "error": diag.error,
            "library_path_var": diag.library_path_var,
            "library_path": diag.library_path,
            "search_dirs": diag.search_dirs.iter().map(|d| d.display().to_string()).collect::<Vec<_>>(),
            "layout": layout.iter().map(|(k, v)| serde_json::json!({"kind": k, "path": v})).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&payload).unwrap_or_default());
        return Ok(if diag.is_ready() { 0 } else { 1 });
    }

    println!("{}", output::bold("本地推理引擎（llama.cpp）"));
    println!();
    if let (Some(bin), Some(v)) = (&diag.binary, &diag.version) {
        output::print_kv(&[
            ("状态".into(), "就绪".to_string()),
            ("路径".into(), bin.display().to_string()),
            ("版本".into(), v.clone()),
        ]);
    } else {
        output::print_kv(&[
            ("状态".into(), "不可用".to_string()),
            (
                "原因".into(),
                diag.error.clone().unwrap_or_else(|| "未找到".into()),
            ),
        ]);
    }

    println!();
    println!("{}", output::bold("运行时布局（按查找优先级）"));
    for (kind, path) in &layout {
        println!("  {} {}", output::paint("·", output::Color::Dim), kind);
        println!("    {}", output::dim(path));
    }

    println!();
    println!("{}", output::bold("动态库搜索路径"));
    println!(
        "  变量 {}",
        diag.library_path_var
    );
    match &diag.library_path {
        Some(v) => println!("  值   {}", output::dim(v)),
        None => println!(
            "  值   {}",
            output::dim("（未注入：引擎未定位成功，或已关闭自动探测）")
        ),
    }

    Ok(if diag.is_ready() { 0 } else { 1 })
}

/// `engine locate` —— 只报定位结果。
fn locate(args: Args) -> MnResult<i32> {
    let cfg = match load_config(&args) {
        Ok((c, _)) => Some(c),
        Err(_) => None,
    };
    let diag = mangosteen_gateway::diagnose_engine(cfg.as_ref());

    if args.has("json") {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "found": diag.binary.is_some(),
                "binary": diag.binary.as_ref().map(|p| p.display().to_string()),
                "error": diag.error,
                "search_dirs": diag.search_dirs.iter().map(|d| d.display().to_string()).collect::<Vec<_>>(),
            }))
            .unwrap_or_default()
        );
        return Ok(if diag.binary.is_some() { 0 } else { 1 });
    }

    match &diag.binary {
        Some(p) => {
            println!("{}", p.display());
            Ok(0)
        }
        None => {
            output::print_warning(
                diag.error
                    .as_deref()
                    .unwrap_or("未找到 llama-server"),
            );
            println!();
            println!("  已搜索目录：");
            for d in &diag.search_dirs {
                println!("    {}", output::dim(&d.display().to_string()));
            }
            Ok(1)
        }
    }
}

/// `engine env` —— 打印启动时会注入的环境变量。
///
/// 输出可直接用于 shell：`eval "$(mangosteen engine env --shell)"`。
fn env(args: Args) -> MnResult<i32> {
    let cfg = match load_config(&args) {
        Ok((c, _)) => Some(c),
        Err(_) => None,
    };
    let diag = mangosteen_gateway::diagnose_engine(cfg.as_ref());

    let Some(bin) = &diag.binary else {
        output::print_warning(
            diag.error
                .as_deref()
                .unwrap_or("引擎未定位成功，无法计算运行环境"),
        );
        return Ok(1);
    };

    if args.has("json") {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "binary": bin.display().to_string(),
                "library_path_var": diag.library_path_var,
                "library_path": diag.library_path,
            }))
            .unwrap_or_default()
        );
        return Ok(0);
    }

    if args.has("shell") {
        // 便于用户复制后在当前 shell 里手工复现 Mangosteen 的启动条件
        if let Some(v) = &diag.library_path {
            // 变量名与拼接口径都来自平台层，CLI 不判断操作系统
            let sep_str = mangosteen_platform::process::path_separator();
            println!(
                "export {}=\"{v}{sep_str}${}\"",
                diag.library_path_var,
                diag.library_path_var
            );
        }
        return Ok(0);
    }

    println!("{}", output::bold("引擎启动环境"));
    println!();
    output::print_kv(&[
        ("二进制".into(), bin.display().to_string()),
        (
            diag.library_path_var.clone(),
            diag.library_path
                .clone()
                .unwrap_or_else(|| "（未注入）".into()),
        ),
    ]);
    println!();
    println!(
        "  {}",
        output::dim("加 --shell 输出可直接 eval 的 export 语句")
    );
    Ok(0)
}

/// `engine probe` —— 实际执行 `--version`。
fn probe(args: Args) -> MnResult<i32> {
    let cfg = match load_config(&args) {
        Ok((c, _)) => Some(c),
        Err(_) => None,
    };
    let diag = mangosteen_gateway::diagnose_engine(cfg.as_ref());

    if args.has("json") {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ok": diag.is_ready(),
                "binary": diag.binary.as_ref().map(|p| p.display().to_string()),
                "version": diag.version,
                "error": diag.error,
            }))
            .unwrap_or_default()
        );
        return Ok(if diag.is_ready() { 0 } else { 1 });
    }

    if diag.is_ready() {
        output::print_success(&format!(
            "引擎可用：{}",
            diag.version.clone().unwrap_or_default()
        ));
        Ok(0)
    } else {
        output::print_warning(&format!(
            "引擎不可用：{}",
            diag.error.clone().unwrap_or_else(|| "未知原因".into())
        ));
        Ok(1)
    }
}
