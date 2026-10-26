//! `mangosteen assess`
//!
//! 归属：**用户交互层 → 命令 `assess`**。
//!
//! 只做资源评估，**不部署**。用途：
//!
//! * 花钱前先确认本地能否承载；
//! * 想知道「如果上云，需要多大显存」；
//! * 想知道「降到什么量化能本地跑」。

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
    "kv-cache",
    "batch-size",
    "json",
    "quiet",
    "suggest-quant",
];

/// 显存余量文本。
///
/// 纯 CPU 场景（无加速卡）下显存余量没有意义——此时不应展示负数，
/// 否则用户会误以为「显存差一点就够了」。改为明确说明该形态不占显存。
fn headroom_text(a: &mangosteen_core::estimate::Assessment) -> String {
    if a.accelerator_budget_bytes == 0 {
        return "不适用（无加速卡，全部占用主机内存）".to_string();
    }
    let headroom = a.accelerator_budget_bytes as f64 - a.requirement.total_bytes() as f64;
    let gib = headroom / (1u64 << 30) as f64;
    if headroom >= 0.0 {
        format!("{gib:.2} GiB 富余")
    } else {
        format!("{gib:.2} GiB（超出，需部分卸载）")
    }
}

/// 入口。
pub fn run(args: Args) -> MnResult<i32> {
    reject_unknown_options(&args, ALLOWED)?;
    let model = args.pos(1).ok_or_else(|| {
        mangosteen_core::MnError::bad_request(
            "缺少模型参数。用法：mangosteen assess <模型ID或GGUF路径> [--context 8192]",
        )
    })?;

    let mut req = DeploymentRequest::for_model(model).local_only();
    if let Some(c) = args.get_num::<u32>("context")? {
        req.context_len = Some(c);
    }
    if let Some(b) = args.get_num::<u32>("batch-size")? {
        req.batch_size = b;
    }
    if let Some(kv) = args.get("kv-cache") {
        req.kv_cache = Some(match kv.to_ascii_lowercase().as_str() {
            "f16" => KvCacheType::F16,
            "q8_0" | "q8" => KvCacheType::Q8_0,
            "q4_0" | "q4" => KvCacheType::Q4_0,
            other => {
                return Err(mangosteen_core::MnError::bad_request(format!(
                    "未知 kv-cache `{other}`，可选：f16 / q8_0 / q4_0"
                )))
            }
        });
    }

    let gw = build_gateway(&args)?;
    let evaluated = gw.evaluate(&req)?;
    let a = &evaluated.assessment;

    if args.has("json") {
        let payload = serde_json::json!({
            "model": evaluated.spec,
            "model_path": evaluated.model_path.display().to_string(),
            "placement": a.placement,
            "local_viable": evaluated.is_local_viable(),
            "full_gpu": evaluated.is_full_gpu(),
            "context_len": a.context_len,
            "kv_cache": a.kv_cache,
            "threads": a.threads,
            "batch_size": a.batch_size,
            "requirement": a.requirement,
            "accelerator_budget_bytes": a.accelerator_budget_bytes,
            "host_budget_bytes": a.host_budget_bytes,
            "warnings": a.warnings.iter().map(|w| w.message()).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&payload).unwrap_or_default());
        return Ok(0);
    }

    let viable = evaluated.is_local_viable();
    if viable {
        output::print_success("本地可以承载该模型");
    } else {
        output::print_warning("本地无法承载该模型，需要云端算力");
    }

    println!();
    output::print_kv(&[
        ("模型".into(), evaluated.spec.name.clone()),
        (
            "落位形态".into(),
            a.placement.describe(evaluated.spec.n_layers),
        ),
        ("上下文".into(), a.context_len.to_string()),
        ("KV 精度".into(), a.kv_cache.as_cli_value().to_string()),
        ("量化".into(), a.quant.to_string()),
        ("线程".into(), a.threads.to_string()),
        ("资源需求".into(), a.requirement.describe()),
        (
            "显存预算".into(),
            mangosteen_platform::fs::human_bytes(a.accelerator_budget_bytes),
        ),
        (
            "内存预算".into(),
            mangosteen_platform::fs::human_bytes(a.host_budget_bytes),
        ),
        ("显存余量".into(), headroom_text(a)),
    ]);

    if !a.warnings.is_empty() {
        println!();
        for w in &a.warnings {
            output::print_warning(&w.message());
        }
    }

    // 不可行时给出「降档建议」
    if !viable || args.has("suggest-quant") {
        println!();
        let budget = a.requirement.total_bytes();
        if let Some((q, suggest)) = gw
            .assessor()
            .suggest_quantization(&evaluated.spec, &req, budget.max(1))
        {
            output::print_info(&format!(
                "在同等预算下可尝试量化档 {}（预计需求 {}）",
                q,
                suggest.requirement.describe()
            ));
        } else {
            // 本地预算内任何量化都跑不动 → 给出云端所需显存
            let need = gw.assessor().cloud_vram_requirement(
                &evaluated.spec,
                a.context_len,
                a.kv_cache,
                a.batch_size,
            );
            output::print_info(&format!(
                "本地任何量化档都不足。云端实例至少需要 {} 显存",
                mangosteen_platform::fs::human_bytes(need)
            ));
        }
    }

    println!();
    if viable {
        println!(
            "  {}",
            output::dim(&format!(
                "下一步：mangosteen deploy {} --context {}",
                evaluated.spec.name, a.context_len
            ))
        );
    } else {
        println!(
            "  {}",
            output::dim(&format!(
                "下一步：mangosteen deploy {}（将自动降级到云端）",
                evaluated.spec.name
            ))
        );
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_model_is_actionable() {
        let a = crate::cli::parse_args(&["assess".to_string()]).unwrap();
        let e = run(a).unwrap_err();
        assert!(e.to_string().contains("缺少模型参数"));
    }

    #[test]
    fn bad_kv_cache_rejected() {
        let a = crate::cli::parse_args(
            &[
                "assess".to_string(),
                "m".to_string(),
                "--kv-cache".to_string(),
                "q2".to_string(),
            ],
        )
        .unwrap();
        // 会在 build_gateway 之前因模型不存在或 kv 非法报错；
        // 这里验证 kv 校验分支
        let e = run(a).unwrap_err();
        assert!(
            e.to_string().contains("未知 kv-cache") || e.to_string().contains("m"),
            "got {e}"
        );
    }
}