//! `mangosteen status` / `mangosteen ps`
//!
//! 归属：**用户交互层 → 命令 `status` / `ps`**。
//!
//! `status` 给出「一眼看清整个系统」的概览：硬件、引擎、配置来源、全部部署状态。
//! `ps` 只列部署表格，适合脚本化与频繁查看。

use mangosteen_core::MnResult;

use crate::cli::Args;
use crate::commands::{build_gateway, load_config, reject_unknown_options};
use crate::output;

/// `status` 允许的选项。
const ALLOWED: &[&str] = &["config", "json", "quiet"];

/// `status` 入口。
pub fn run(args: Args) -> MnResult<i32> {
    if args.has("help") {
        print_status_help();
        return Ok(0);
    }
    reject_unknown_options(&args, ALLOWED)?;
    let gw = build_gateway(&args)?;
    let st = gw.status()?;
    // 获取全部部署记录（包括非 active 状态），用于显示全部部署
    let all_records = gw.list_deployments()?;

    if args.has("json") {
        let payload = serde_json::json!({
            "version": st.version,
            "hardware": st.hardware,
            "providers": st.providers,
            "active": st.active.iter().map(|a| serde_json::json!({
                "id": a.id,
                "model": a.model,
                "origin": a.origin.label(),
                "status": a.status.as_str(),
                "run_status": a.run_status.as_str(),
                "port": a.port,
                "remote_port": a.remote_port,
                "ready": a.ready,
            })).collect::<Vec<_>>(),
            "all": all_records.iter().map(|r| serde_json::json!({
                "id": r.plan.id,
                "model": mangosteen_core::model::ModelEntry::derive_id(&r.plan.model.name, r.plan.model.quant),
                "origin": r.plan.origin.label(),
                "status": r.status.as_str(),
                "run_status": r.run_status.as_str(),
                "port": r.plan.local_access_port(),
                "remote_port": r.plan.cloud.as_ref().map(|c| c.engine_port),
                "ready": r.run_status == mangosteen_base::state::RunStatus::Active,
            })).collect::<Vec<_>>(),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).unwrap_or_default()
        );
        return Ok(0);
    }

    println!("{}", output::bold("系统状态"));
    println!();
    output::print_kv(&[
        ("版本".into(), st.version.clone()),
        ("硬件".into(), st.hardware.clone()),
        (
            "云服务商".into(),
            if st.providers.is_empty() {
                output::dim("未配置（无法部署云端算力）").to_string()
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
    println!("{}", output::bold("全部部署"));
    println!();
    print_all_deployments(&st.active, &all_records);
    println!();
    println!(
        "  {}",
        output::dim(&format!(
            "推理入口：http://127.0.0.1:{}/v1",
            gw.config().gateway.port
        ))
    );
    println!();
    Ok(0)
}

/// 打印全部部署表（包含 active 和非 active 部署）。
fn print_all_deployments(
    active: &[mangosteen_gateway::ActiveDeployment],
    all_records: &[mangosteen_base::state::DeploymentRecord],
) {
    if all_records.is_empty() {
        println!("  {}", output::dim("当前没有部署记录"));
        println!("  {}", output::dim("执行 `mangosteen deploy <模型>` 开始部署"));
        return;
    }

    // 先显示 active 部署（带完整信息）
    if !active.is_empty() {
        println!("{}", output::bold("活跃部署（正在运行）："));
        println!();
        let rows: Vec<Vec<String>> = active
            .iter()
            .map(|a| {
                vec![
                    a.id.clone(),
                    a.model.clone(),
                    a.origin.label(),
                    a.status.as_str().to_string(),
                    a.run_status.as_str().to_string(),
                    a.port.map(|p| p.to_string()).unwrap_or_else(|| "-".into()),
                    a.remote_port.map(|p| p.to_string()).unwrap_or_else(|| "-".into()),
                    if a.ready { "就绪" } else { "未就绪" }.to_string(),
                ]
            })
            .collect();
        output::print_table(
            &["部署 ID", "模型", "算力来源", "状态", "运行状态", "本地端口", "远端端口", "健康"],
            &rows,
        );
        println!();
    }

    // 再显示非 active 部署（已停止/失败/销毁等）
    let inactive: Vec<&mangosteen_base::state::DeploymentRecord> = all_records
        .iter()
        .filter(|r| {
            !matches!(
                r.status,
                mangosteen_base::state::DeploymentStatus::Provisioning
                    | mangosteen_base::state::DeploymentStatus::Ready
            )
        })
        .collect();

    if !inactive.is_empty() {
        println!("{}", output::bold("历史部署（已停止/失败/销毁）："));
        println!();
        let rows: Vec<Vec<String>> = inactive
            .iter()
            .map(|r| {
                let model_id =
                    mangosteen_core::model::ModelEntry::derive_id(&r.plan.model.name, r.plan.model.quant);
                let local_port = r.plan.local_access_port();
                let remote_port = r.plan.cloud.as_ref().map(|c| c.engine_port);
                vec![
                    r.plan.id.clone(),
                    model_id,
                    r.plan.origin.label(),
                    r.status.as_str().to_string(),
                    r.run_status.as_str().to_string(),
                    local_port.map(|p| p.to_string()).unwrap_or_else(|| "-".into()),
                    remote_port.map(|p| p.to_string()).unwrap_or_else(|| "-".into()),
                    if r.run_status == mangosteen_base::state::RunStatus::Active {
                        "就绪".to_string()
                    } else {
                        "未就绪".to_string()
                    },
                ]
            })
            .collect();
        output::print_table(
            &["部署 ID", "模型", "算力来源", "状态", "运行状态", "本地端口", "远端端口", "健康"],
            &rows,
        );
    }
}

/// 打印 status 子命令帮助。
fn print_status_help() {
    println!("mangosteen status [选项]");
    println!();
    println!("查看网关与部署状态。");
    println!();
    println!("{}", output::bold("选项"));
    println!("  {:<w$}  {}", "--json", "以 JSON 输出（便于脚本处理）", w = 22);
    println!("  {:<w$}  {}", "--quiet", "抑制 banner 与进度提示", w = 22);
    println!();
    println!("{}", output::bold("示例"));
    println!("  mangosteen status");
    println!("  mangosteen status --json");
    println!();
    println!("输出包含：版本、硬件信息、云服务商、活跃部署列表（ID、模型、算力来源、状态、端口、健康）。");
    println!("更多信息：docs/USAGE.md");
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
}
