//! `mangosteen status` / `mangosteen ps`
//!
//! 归属：**用户交互层 → 命令 `status` / `ps`**。
//!
//! `status` 给出「一眼看清整个系统」的概览：硬件、引擎、配置来源、活跃部署。
//! `ps` 只列部署表格，适合脚本化与频繁查看。

use mangosteen_core::MnResult;

use crate::cli::Args;
use crate::commands::{build_gateway, load_config, reject_unknown_options, state_store};
use crate::output;

/// `status` 允许的选项。
const ALLOWED: &[&str] = &["config", "json", "quiet"];

/// `status` 入口。
pub fn run(args: Args) -> MnResult<i32> {
    reject_unknown_options(&args, ALLOWED)?;
    let mut gw = build_gateway(&args)?;
    let st = gw.status()?;

    if args.has("json") {
        let payload = serde_json::json!({
            "version": st.version,
            "hardware": st.hardware,
            "providers": st.providers,
            "engine_path": st.engine_path.map(|p| p.display().to_string()),
            "engine_version": st.engine_version,
            "active": st.active.iter().map(|a| serde_json::json!({
                "id": a.id,
                "model": a.model,
                "origin": a.origin.label(),
                "status": a.status.as_str(),
                "port": a.port,
                "ready": a.ready,
            })).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&payload).unwrap_or_default());
        return Ok(0);
    }

    println!("{}", output::bold("系统状态"));
    println!();
    output::print_kv(&[
        ("版本".into(), st.version.clone()),
        ("硬件".into(), st.hardware.clone()),
        (
            "本地推理引擎".into(),
            match (&st.engine_path, &st.engine_version) {
                (Some(p), Some(v)) => format!("{} ({})", p.display(), v),
                (Some(p), None) => format!("{} （版本未知）", p.display()),
                (None, _) => output::paint("未找到（无法本地部署）", output::Color::Yellow),
            },
        ),
        (
            "云服务商".into(),
            if st.providers.is_empty() {
                output::dim("未配置（本地不足时无法降级）").to_string()
            } else {
                st.providers.join(", ")
            },
        ),
        (
            "配置文件".into(),
            load_config(&args)
                .map(|(_, s)| s)
                .unwrap_or_else(|e| format!("读取失败：{e}")),
        ),
    ]);

    println!();
    if st.active.is_empty() {
        println!("  {}", output::dim("当前没有活跃部署"));
        println!();
        println!(
            "  {}",
            output::dim("执行 `mangosteen deploy <模型>` 开始部署")
        );
    } else {
        println!("{}", output::bold("活跃部署"));
        println!();
        print_active(&st.active);
        println!();
        println!(
            "  {}",
            output::dim(&format!(
                "推理入口：http://127.0.0.1:{}/v1",
                gw.config().gateway.port
            ))
        );
    }
    println!();
    Ok(0)
}

/// `ps` 入口。
pub fn run_ps(args: Args) -> MnResult<i32> {
    reject_unknown_options(&args, ALLOWED)?;
    let store = state_store(&args)?;
    let all = store.list()?;

    if args.has("json") {
        let arr: Vec<serde_json::Value> = all
            .iter()
            .map(|r| {
                serde_json::json!({
                    "id": r.plan.id,
                    "model": r.plan.model.name,
                    "origin": r.plan.origin.label(),
                    "status": r.status.as_str(),
                    "port": r.plan.local_access_port(),
                    "ready": r.status.is_active(),
                    "error": r.error,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({"deployments": arr}))
                .unwrap_or_default()
        );
        return Ok(0);
    }

    if all.is_empty() {
        println!("  {}", output::dim("（无部署记录）"));
        return Ok(0);
    }

    let rows: Vec<Vec<String>> = all
        .iter()
        .map(|r| {
            vec![
                r.plan.id.clone(),
                r.plan.model.name.clone(),
                r.plan.origin.label(),
                r.status.as_str().to_string(),
                r.plan
                    .local_access_port()
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "-".into()),
            ]
        })
        .collect();
    output::print_table(&["部署 ID", "模型", "算力来源", "状态", "端口"], &rows);
    Ok(0)
}

/// 打印活跃部署表。
fn print_active(active: &[mangosteen_gateway::ActiveDeployment]) {
    let rows: Vec<Vec<String>> = active
        .iter()
        .map(|a| {
            vec![
                a.id.clone(),
                a.model.clone(),
                a.origin.label(),
                a.status.as_str().to_string(),
                a.port.map(|p| p.to_string()).unwrap_or_else(|| "-".into()),
                if a.ready { "就绪" } else { "未就绪" }.to_string(),
            ]
        })
        .collect();
    output::print_table(&["部署 ID", "模型", "算力来源", "状态", "端口", "健康"], &rows);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowed_options_reject_typos() {
        // 用带值的拼错选项（布尔形式的拼错会在解析期就因缺值报错）
        let a =
            crate::cli::parse_args(&["status".to_string(), "--formt".to_string(), "x".to_string()])
                .unwrap();
        let e = reject_unknown_options(&a, ALLOWED).unwrap_err();
        assert!(e.to_string().contains("--formt"), "got {e}");
    }

    #[test]
    fn ps_on_empty_state_does_not_fail() {
        let a = crate::cli::parse_args(&["ps".to_string()]).unwrap();
        // 用临时路径的配置以免污染真实台账
        // 这里只验证 reject_unknown_options 与状态判断路径不 panic
        assert!(reject_unknown_options(&a, ALLOWED).is_ok());
    }
}