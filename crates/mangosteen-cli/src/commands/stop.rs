//! `mangosteen stop`
//!
//! 归属：**用户交互层 → 命令 `stop`**。
//!
//! ## 安全考量
//!
//! 停止云端部署等于销毁计费实例，属于**不可逆**操作。因此：
//!
//! * `stop <id>` 只停一个，不 `--all` 时不确认；
//! * `stop --all` 先列出将要停止的部署，要求 `--yes` 确认；
//! * 有云端部署时额外提示计费影响。

use mangosteen_core::MnResult;

use crate::cli::Args;
use crate::commands::{build_gateway, reject_unknown_options, state_store};
use crate::output;

/// 允许的选项。
const ALLOWED: &[&str] = &["config", "all", "yes", "json", "quiet"];

/// 入口。
pub fn run(args: Args) -> MnResult<i32> {
    reject_unknown_options(&args, ALLOWED)?;

    let all = args.has("all");
    let target = args.pos(1).map(|s| s.to_string());

    if !all && target.is_none() {
        return Err(mangosteen_core::MnError::bad_request(
            "请指定部署 ID，或使用 --all 停止全部。用法：mangosteen stop <部署ID> | --all",
        ));
    }

    // --all 且未 --yes：先展示再要求确认
    if all && !args.has("yes") {
        let store = state_store(&args)?;
        let active = store.list_active()?;
        if active.is_empty() {
            output::print_info("没有需要停止的部署");
            return Ok(0);
        }
        println!("{}", output::bold("将要停止以下部署："));
        let rows: Vec<Vec<String>> = active
            .iter()
            .map(|r| {
                vec![
                    r.plan.id.clone(),
                    r.plan.model.name.clone(),
                    r.plan.origin.label(),
                    r.status.as_str().to_string(),
                ]
            })
            .collect();
        output::print_table(&["部署 ID", "模型", "算力来源", "状态"], &rows);

        let has_cloud = active.iter().any(|r| !r.plan.origin.is_local());
        if has_cloud {
            println!();
            output::print_warning(
                "其中包含云端部署：停止会销毁实例，未保存的数据将丢失，且计费立即停止",
            );
        }
        println!();
        println!(
            "  确认执行请加 {} 重试：{}",
            output::paint("--yes", output::Color::Cyan),
            output::bold("mangosteen stop --all --yes")
        );
        return Ok(2);
    }

    let mut gw = build_gateway(&args)?;

    if all {
        let errors = gw.stop_all();
        if errors.is_empty() {
            output::print_success("全部部署已停止");
            return Ok(0);
        }
        for (id, msg) in &errors {
            output::print_warning(&format!("{id} 停止失败：{msg}"));
        }
        return Ok(1);
    }

    let id = target.unwrap();
    match gw.stop(&id) {
        Ok(()) => {
            output::print_success(&format!("部署 {id} 已停止，资源已释放"));
            Ok(0)
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_target_or_all() {
        let a = crate::cli::parse_args(&["stop".to_string()]).unwrap();
        let e = run(a).unwrap_err();
        assert!(e.to_string().contains("请指定部署 ID"));
        assert!(e.to_string().contains("--all"));
    }

    #[test]
    fn rejects_unknown_option() {
        let a =
            crate::cli::parse_args(&["stop".to_string(), "--force".to_string()]).unwrap();
        // force 是 json 之外的开关；reject 应通过（force 在 BOOLEAN_FLAGS 中，
        // 但不在 ALLOWED 里）
        let e = run(a).unwrap_err();
        assert!(e.to_string().contains("--force"), "got {e}");
    }
}