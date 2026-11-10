//! `mangosteen cloud`
//!
//! 归属：**用户交互层 → 命令 `cloud`**。
//!
//! 子命令：
//!
//! | 子命令 | 作用 |
//! |--------|------|
//! | `providers` | 列出已配置/已加载的服务商驱动 |
//! | `specs` | 查询各服务商当前可创建规格与报价 |
//! | `pick` | 按约束打分排序并解释选择依据（**不创建资源**） |
//! | `cheapest` | 查当前可用的**最低价**节点；`--create` 直接创建 |
//! | `instances` | 列出所有云厂商托管的实例（含已关机），与 UI "云端节点" 对应 |
//! | `destroy` | 销毁指定实例（部署 ID，或 `--instance 服务商/实例ID`） |
//! | `key` | 管理云厂商访问凭证（与 UI 设置页同一套后端） |
//!
//! `pick --explain` 是本命令最有价值的部分：让用户在花钱前看清
//! 「为什么选了这个节点」——价格/延迟/可用性/硬件四维分数都摊开。

use mangosteen_core::config::SecretRef;
use mangosteen_core::MnError;
use mangosteen_core::policy::SelectionConstraints;
use mangosteen_core::MnResult;

use crate::cli::Args;
use crate::commands::{build_gateway, load_config, reject_unknown_options};
use crate::output;

use mangosteen_platform::fs::{ensure_private_dir, write_private_file};
use mangosteen_platform::paths;

/// 允许的选项。
const ALLOWED: &[&str] = &[
    "config",
    "json",
    "explain",
    "min-vram",
    "max-price",
    "provider",
    "region",
    "all",
    "yes",
    "quiet",
    "create",
    "name",
    "instance",
    "running",
    "stopped",
    // 云厂商密钥管理（与 UI 设置页同一套后端）
    "password",
    "env",
    "access-key",
    "secret-key",
    "set",
    "enable",
    "disable",
];

/// 入口。
pub fn run(args: Args) -> MnResult<i32> {
    reject_unknown_options(&args, ALLOWED)?;
    let sub = args.pos(1).unwrap_or("providers").to_string();
    match sub.as_str() {
        "providers" => providers(args),
        "specs" => specs(args),
        "pick" => pick(args),
        "cheapest" | "cheap" => cheapest(args),
        "instances" | "ls" => instances(args),
        "destroy" | "rm" => destroy(args),
        "key" => cloud_key(args),
        other => Err(mangosteen_core::MnError::bad_request(format!(
            "未知子命令 `cloud {other}`。可用：providers / specs / pick / cheapest / instances / destroy / key"
        ))),
    }
}

/// 构造约束。
///
/// `require_stock` 取自配置：部分厂商（优云智算）的库存接口是模拟调度，
/// 对可用机型也返回 false，强制要求库存会把候选清空。
fn constraints(args: &Args, require_stock: bool) -> MnResult<SelectionConstraints> {
    let mut c = SelectionConstraints {
        require_stock,
        ..SelectionConstraints::default()
    };
    if let Some(gib) = args.get_num::<u64>("min-vram")? {
        c.min_vram_bytes = gib * (1u64 << 30);
    }
    if let Some(p) = args.get_num::<f64>("max-price")? {
        c.max_price_per_hour = Some(p);
    }
    if let Some(p) = args.get("provider") {
        c.allowed_providers = p.split(',').map(|s| s.trim().to_string()).collect();
    }
    if let Some(r) = args.get("region") {
        c.allowed_regions = r.split(',').map(|s| s.trim().to_string()).collect();
    }
    Ok(c)
}

/// `cloud providers`
fn providers(args: Args) -> MnResult<i32> {
    let (cfg, _) = load_config(&args)?;
    let (registered, skipped) = mangosteen_gateway::probe_providers(&cfg);

    if args.has("json") {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "builtin": mangosteen_gateway::builtin_providers(),
                "configured": cfg.providers.keys().collect::<Vec<_>>(),
                "registered": registered,
                "skipped": skipped.iter().map(|(n, r)| serde_json::json!({"name": n, "reason": r}))
                    .collect::<Vec<_>>(),
            }))
            .unwrap_or_default()
        );
        return Ok(0);
    }

    println!("{}", output::bold("云端服务商"));
    println!();
    if cfg.providers.is_empty() {
        println!("  {}", output::dim("（未配置任何服务商）"));
        println!();
        println!(
            "  {}",
            output::dim("执行 `mangosteen config init` 生成配置模板，然后填写凭证")
        );
        return Ok(0);
    }

    let rows: Vec<Vec<String>> = cfg
        .providers
        .iter()
        .map(|(name, p)| {
            let state = if registered.contains(name) {
                output::paint("已就绪", output::Color::Green)
            } else if !p.enabled {
                output::dim("已停用")
            } else {
                output::paint("不可用", output::Color::Yellow)
            };
            let reason = skipped
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, r)| r.clone())
                .unwrap_or_else(|| "-".into());
            vec![name.clone(), p.driver.clone(), state, reason]
        })
        .collect();
    output::print_table(&["名称", "驱动", "状态", "备注"], &rows);
    println!();
    println!(
        "  {}",
        output::dim(&format!(
            "内置可用驱动：{}",
            mangosteen_gateway::builtin_providers().join(", ")
        ))
    );
    Ok(0)
}

/// `cloud specs`
fn specs(args: Args) -> MnResult<i32> {
    let (cfg, _) = load_config(&args)?;
    let (registered, skipped) = mangosteen_gateway::probe_providers(&cfg);

    if registered.is_empty() {
        for (name, reason) in &skipped {
            output::print_warning(&format!("{name}：{reason}"));
        }
        return Err(mangosteen_core::MnError::NoViableCandidate {
            reason: "没有已就绪的云服务商，无法查询规格。请检查凭证配置".to_string(),
        });
    }

    let gw = crate::commands::build_gateway(&args)?;
    let specs = gw.list_cloud_specs()?;

    // 过滤：仅保留符合约束的
    let cons = constraints(&args, cfg.scheduler.require_stock)?;
    let filtered: Vec<_> = specs.iter().filter(|(p, sp)| cons.admits(p, sp)).collect();

    if args.has("json") {
        let arr: Vec<serde_json::Value> = filtered
            .iter()
            .map(|(p, sp)| {
                serde_json::json!({
                    "provider": p,
                    "spec_id": sp.spec_id,
                    "gpu_model": sp.spec.gpu.model,
                    "gpu_count": sp.spec.gpu.count,
                    "vram_bytes": sp.spec.gpu.total_vram_bytes(),
                    "cpu_cores": sp.spec.cpu_cores,
                    "memory_bytes": sp.spec.memory_bytes,
                    "disk_bytes": sp.spec.disk_bytes,
                    "image": sp.spec.image,
                    "price_per_hour": sp.price.per_hour_cny,
                    "region": sp.region,
                    "stock": sp.stock,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({"specs": arr})).unwrap_or_default()
        );
        return Ok(0);
    }

    if filtered.is_empty() {
        println!("  {}", output::dim("（没有符合约束的规格）"));
        return Ok(0);
    }

    let rows: Vec<Vec<String>> = filtered
        .iter()
        .map(|(p, sp)| {
                let price = if sp.price.per_hour_cny >= f64::MAX / 2.0 {
                    "-".to_string()
                } else {
                    format!("¥{:.3}", sp.price.per_hour_cny)
                };
                vec![
                    (*p).clone(),
                    sp.spec_id.clone(),
                    sp.spec.gpu.model.clone(),
                    format!(
                        "{:.0} GiB",
                        sp.spec.gpu.total_vram_bytes() as f64 / (1u64 << 30) as f64
                    ),
                    format!(
                        "{:.0} GiB",
                        sp.spec.memory_bytes as f64 / (1u64 << 30) as f64
                    ),
                    price,
                    sp.region.clone(),
                    sp.stock
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| "-".into()),
            ]
        })
        .collect();
    output::print_table(
        &[
            "服务商",
            "规格 ID",
            "GPU",
            "显存",
            "内存",
            "时价",
            "地域",
            "库存",
        ],
        &rows,
    );
    Ok(0)
}

/// `cloud pick`
fn pick(args: Args) -> MnResult<i32> {
    let (cfg, _) = load_config(&args)?;
    let mut cons = constraints(&args, cfg.scheduler.require_stock)?;
    // 强制过滤掉库存为 0 的规格，只返回库存 >= 1 的数据项
    cons.require_stock = true;

    // 通过网关的只读评估：构造 gateway 后使用其 Picker 能力
    // 由于 Picker 依赖驱动注册表，这里用 combine 通道
    let (registered, skipped) = mangosteen_gateway::probe_providers(&cfg);
    if registered.is_empty() {
        for (name, reason) in &skipped {
            output::print_warning(&format!("{name}：{reason}"));
        }
        return Err(mangosteen_core::MnError::NoViableCandidate {
            reason: "没有已就绪的云服务商".to_string(),
        });
    }

    output::print_info(&format!("已就绪服务商：{}", registered.join(", ")));
    println!();

    // 约束摘要
    let mut lines: Vec<(String, String)> = Vec::new();
    if cons.min_vram_bytes > 0 {
        lines.push((
            "最低显存".into(),
            format!(
                "{:.1} GiB",
                cons.min_vram_bytes as f64 / (1u64 << 30) as f64
            ),
        ));
    }
    if let Some(p) = cons.max_price_per_hour {
        lines.push(("价格上限".into(), format!("¥{p:.2}/h")));
    }
    if !cons.allowed_providers.is_empty() {
        lines.push(("限定服务商".into(), cons.allowed_providers.join(", ")));
    }
    if lines.is_empty() {
        println!("  {}", output::dim("（未指定约束，将按配置权重排序）"));
    } else {
        output::print_kv(&lines);
    }
    println!();

    let gw = crate::commands::build_gateway(&args)?;
    let ranked = gw.pick_cloud(&cons)?;

    if ranked.is_empty() {
        return Err(mangosteen_core::MnError::NoViableCandidate {
            reason: "所有候选规格都不满足约束".to_string(),
        });
    }

    if args.has("json") {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "best": ranked[0],
                "ranked": ranked,
            }))
            .unwrap_or_default()
        );
        return Ok(0);
    }

    println!("{}", output::bold("推荐顺序（分维度得分越高越好）"));
    println!();
    if args.has("explain") {
        for (i, c) in ranked.iter().enumerate() {
            if i == 0 {
                println!(
                    "  {} {}",
                    output::paint("★", output::Color::Green),
                    c.explain()
                );
            } else {
                println!("    {}", c.explain());
            }
        }
    } else {
        let rows: Vec<Vec<String>> = ranked
            .iter()
            .map(|c| {
                    let price = if c.price_per_hour >= f64::MAX / 2.0 {
                        "-".to_string()
                    } else {
                        format!("¥{:.3}", c.price_per_hour)
                    };
                vec![
                    c.provider.clone(),
                    c.gpu_model.clone(),
                    price,
                    c.latency_ms
                        .map(|v| format!("{v}ms"))
                        .unwrap_or_else(|| "-".into()),
                    format!("{:.1} GiB", c.vram_bytes as f64 / (1u64 << 30) as f64),
                    format!("{:.4}", c.score_total),
                ]
            })
            .collect();
        output::print_table(&["服务商", "GPU", "时价", "延迟", "显存", "总分"], &rows);
        println!();
        println!("  {}", output::dim("加 --explain 查看各维度得分明细"));
    }

    println!();
    println!(
        "  {}",
        output::dim(
            "`mangosteen deploy <模型>` 会自动完成同样的选点流程并创建实例。\
             本命令仅查看决策依据，不产生费用。"
        )
    );
    Ok(0)
}

/// `cloud cheapest`：查「当前可用的最低价」节点，可选直接创建。
///
/// ## 为什么不复用 `pick`
///
/// `pick` 走四维打分（默认 balanced），「综合最优」未必最便宜——延迟与库存
/// 分会把贵的顶上来。用户明确说「要最便宜的」时，必须给纯价格序，
/// 否则他会看到一台 ¥2.15 的 4090 被推荐，而库里 ¥1.34 的 V100S 排在下面。
///
/// ## 「可用」由创建结果定义
///
/// 厂商库存接口不可信（优云对能开的机器也返回 false），所以这里不做
/// 「库存为 0 就淘汰」。`--create` 时按价格顺序逐个真开，谁先开出来
/// 谁就是「当前可用的最低价」。
fn cheapest(args: Args) -> MnResult<i32> {
    let (cfg, _) = load_config(&args)?;
    let mut cons = constraints(&args, cfg.scheduler.require_stock)?;
    // 强制过滤掉库存为 0 的规格，只返回库存 >= 1 的数据项
    cons.require_stock = true;

    let (registered, skipped) = mangosteen_gateway::probe_providers(&cfg);
    if registered.is_empty() {
        for (name, reason) in &skipped {
            output::print_warning(&format!("{name}：{reason}"));
        }
        return Err(mangosteen_core::MnError::NoViableCandidate {
            reason: "没有已就绪的云服务商".to_string(),
        });
    }

    let gw = crate::commands::build_gateway(&args)?;
    let ordered = gw.cheapest_cloud(&cons)?;
    let create = args.has("create");

    // ---- 只查不建 ----
    if !create {
        if args.has("json") {
            let arr: Vec<serde_json::Value> =
                ordered.iter().map(|(p, sp)| spec_json(p, sp)).collect();
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "cheapest": arr.first().cloned(),
                    "ordered": arr,
                }))
                .unwrap_or_default()
            );
            return Ok(0);
        }

        let best = &ordered[0];
        println!("{}", output::bold("当前可用最低价节点"));
        println!();
        output::print_kv(&[
            ("服务商".into(), best.0.clone()),
            ("规格 ID".into(), best.1.spec_id.clone()),
            ("GPU".into(), gpu_label(&best.1)),
            (
                "时价".into(),
                if best.1.price.per_hour_cny >= f64::MAX / 2.0 {
                    "-".to_string()
                } else {
                    format!("¥{:.3}/h", best.1.price.per_hour_cny)
                },
            ),
            (
                "日费用".into(),
                if best.1.price.per_hour_cny >= f64::MAX / 2.0 {
                    "-".to_string()
                } else {
                    format!("¥{:.2}", best.1.price.per_hour_cny * 24.0)
                },
            ),
            ("地域".into(), best.1.region.clone()),
        ]);
        println!();

        if ordered.len() > 1 {
            println!(
                "{}",
                output::bold(&format!("价格顺序（共 {} 个候选）", ordered.len()))
            );
            println!();
            let rows: Vec<Vec<String>> = ordered
                .iter()
                .take(if args.has("all") { ordered.len() } else { 8 })
                .map(|(p, sp)| {
                        let price = if sp.price.per_hour_cny >= f64::MAX / 2.0 {
                            "-".to_string()
                        } else {
                            format!("¥{:.3}", sp.price.per_hour_cny)
                        };
                        vec![
                            p.clone(),
                            sp.spec_id.clone(),
                            gpu_label(sp),
                            price,
                            sp.region.clone(),
                            sp.stock
                                .map(|s| s.to_string())
                                .unwrap_or_else(|| "-".into()),
                    ]
                })
                .collect();
            output::print_table(&["服务商", "规格 ID", "GPU", "时价", "地域", "库存"], &rows);
            println!();
        }

        println!(
            "  {}",
            output::dim("`--create --yes` 直接创建这台（会按价格顺序自动跳过开不出来的）")
        );
        return Ok(0);
    }

    // ---- 创建前必须确认 ----
    let best = &ordered[0];
    let name = args.get("name").map(|s| s.to_string());
    if !args.has("yes") {
        output::print_warning(&format!(
            "⚠ 即将创建**真实计费实例**：约 ¥{:.2}/天（¥{:.3}/小时）",
            best.1.price.per_hour_cny * 24.0,
            best.1.price.per_hour_cny
        ));
        output::print_kv(&[
            ("服务商".into(), best.0.clone()),
            ("规格 ID".into(), best.1.spec_id.clone()),
            ("GPU".into(), gpu_label(&best.1)),
            ("地域".into(), best.1.region.clone()),
            (
                "实例名".into(),
                name.clone().unwrap_or_else(|| "(厂商默认)".into()),
            ),
            (
                "候选数".into(),
                format!("{}（最便宜的开不出来会自动换下一个）", ordered.len()),
            ),
        ]);
        println!();
        println!(
            "  确认创建请加 {}：{}",
            output::paint("--yes", output::Color::Cyan),
            output::bold("mangosteen cloud cheapest --create --yes")
        );
        return Ok(2);
    }

    let (provider, inst) = gw.create_cheapest_cloud(&cons, name.as_deref())?;

    if args.has("json") {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "provider": provider,
                "instance_id": inst.id,
                "state": inst.state.to_string(),
                "price_per_hour": inst.price.as_ref().map(|p| p.per_hour_cny),
                "ssh_host": inst.network.ssh_host,
                "ssh_port": inst.network.ssh_port,
            }))
            .unwrap_or_default()
        );
        return Ok(0);
    }

    println!();
    output::print_success(&format!("实例已创建并进入 {} 状态", inst.state));
    output::print_kv(&[
        ("服务商".into(), provider.clone()),
        ("实例 ID".into(), inst.id.clone()),
        (
            "SSH".into(),
            match (&inst.network.ssh_host, inst.network.ssh_port) {
                (Some(h), Some(p)) => format!("{h}:{p}"),
                (Some(h), None) => h.clone(),
                _ => "-".into(),
            },
        ),
    ]);
    println!();
    // 裸实例不在部署台账里 → 必须给出**能直接复制执行**的销毁命令，
    // 否则就是「开了台机器没人记得」，持续计费到用户发现为止。
    println!(
        "  {}",
        output::paint(
            "⚠ 这台实例不在部署台账里，不会被 stop/abort 自动回收",
            output::Color::Yellow
        )
    );
    println!(
        "  用完请销毁：{}",
        output::bold(&format!(
            "mangosteen cloud destroy --instance {}/{} --yes",
            provider, inst.id
        ))
    );
    Ok(0)
}

/// 规格的展示字段（`--json` 用）。
fn spec_json(provider: &str, sp: &mangosteen_core::cloud::AvailableSpec) -> serde_json::Value {
    serde_json::json!({
        "provider": provider,
        "spec_id": sp.spec_id,
        "gpu_model": sp.spec.gpu.model,
        "gpu_count": sp.spec.gpu.count,
        "vram_bytes": sp.spec.gpu.total_vram_bytes(),
        "cpu_cores": sp.spec.cpu_cores,
        "memory_bytes": sp.spec.memory_bytes,
        "price_per_hour": sp.price.per_hour_cny,
        "region": sp.region,
        "stock": sp.stock,
    })
}

/// GPU 展示文案：`RTX 4090 ×1（24 GiB）`。
fn gpu_label(sp: &mangosteen_core::cloud::AvailableSpec) -> String {
    format!(
        "{} ×{}（{:.0} GiB）",
        sp.spec.gpu.model,
        sp.spec.gpu.count,
        sp.spec.gpu.total_vram_bytes() as f64 / (1u64 << 30) as f64
    )
}

/// `cloud instances`
fn instances(args: Args) -> MnResult<i32> {
    let gw = build_gateway(&args)?;
    
    // 查询所有云厂商托管的实例（含已关机），与 UI "云端节点" 对应
    let list = gw.cloud_instance_briefs();
    
    // 状态过滤
    let filter_running = args.has("running");
    let filter_stopped = args.has("stopped");
    let filtered: Vec<_> = list
        .into_iter()
        .filter(|x| {
            if filter_running {
                x.state == "running"
            } else if filter_stopped {
                x.state == "stopped"
            } else {
                true
            }
        })
        .collect();

    if args.has("json") {
        let arr: Vec<serde_json::Value> = filtered
            .iter()
            .map(|x| {
                serde_json::json!({
                    "provider": x.provider,
                    "instance_id": x.instance_id,
                    "gpu_model": x.gpu_model,
                    "gpu_count": x.gpu_count,
                    "vram_bytes": x.vram_bytes,
                    "status": x.state,
                    "ssh_target": x.ssh_target,
                    "price_per_hour": x.price_per_hour,
                    "occupied_models": x.occupied_models,
                    "display_name": x.display_name,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({"instances": arr}))
                .unwrap_or_default()
        );
        return Ok(0);
    }

    if filtered.is_empty() {
        if filter_running {
            println!("  {}", output::dim("（没有运行中的云端实例）"));
        } else if filter_stopped {
            println!("  {}", output::dim("（没有已停机的云端实例）"));
        } else {
            println!("  {}", output::dim("（当前云厂商下没有 GPU 实例）"));
        }
        return Ok(0);
    }

    let rows: Vec<Vec<String>> = filtered
        .iter()
        .map(|x| {
            vec![
                x.display_name.clone().unwrap_or_default(),
                x.instance_id.clone(),
                x.provider.clone(),
                format!("{} × {}", x.gpu_model, x.gpu_count),
                x.state.clone(),
                x.ssh_target.clone().unwrap_or_default(),
                x.price_per_hour.map(|p| format!("¥{:.2}/h", p)).unwrap_or_default(),
            ]
        })
        .collect();
    output::print_table(
        &["实例名称", "实例ID", "云厂商", "规格", "运行状态", "SSH目标", "价格"],
        &rows,
    );
    println!();
    println!(
        "  {}",
        output::dim("用 `mangosteen cloud destroy --instance <服务商>/<实例ID> --yes` 销毁裸实例")
    );
    Ok(0)
}

/// `cloud destroy`
///
/// 两种用法：
/// * `cloud destroy <部署ID>` —— 销毁某个部署的云端实例（走台账）
/// * `cloud destroy --instance <服务商>/<实例ID>` —— 销毁**裸实例**
///   （由 `cloud cheapest --create` 创建，不在台账里）
fn destroy(args: Args) -> MnResult<i32> {
    if let Some(target) = args.get("instance").map(|s| s.to_string()) {
        return destroy_bare_instance(&args, &target);
    }

    let id = args.pos(2).ok_or_else(|| {
        mangosteen_core::MnError::bad_request(
            "缺少部署 ID。用法：mangosteen cloud destroy <部署ID> [--yes]\n\
             销毁裸实例用：mangosteen cloud destroy --instance <服务商>/<实例ID> --yes",
        )
    })?;

    let store = crate::commands::state_store(&args)?;
    let record = store
        .get(id)?
        .ok_or_else(|| mangosteen_core::MnError::bad_request(format!("部署 `{id}` 不存在")))?;

    let (provider, instance_id) = match &record.plan.origin {
        mangosteen_core::plan::ComputeOrigin::Cloud {
            provider,
            instance_id,
        } => (provider.clone(), instance_id.clone()),
    };

    if !args.has("yes") {
        output::print_warning(
            "⚠ 此操作非常危险，可能导致不可逆的数据丢失！销毁云端实例后磁盘数据不可恢复。",
        );
        output::print_kv(&[
            ("服务商".into(), provider),
            ("实例 ID".into(), instance_id),
            ("模型".into(), record.plan.model.name.clone()),
        ]);
        println!();
        println!(
            "  确认执行请加 {}：{}",
            output::paint("--yes", output::Color::Cyan),
            output::bold(&format!("mangosteen cloud destroy {id} --yes"))
        );
        return Ok(2);
    }

    let gw = crate::commands::build_gateway(&args)?;
    gw.stop(id)?;
    output::print_success(&format!("云端实例已销毁（部署 {id}）"));
    Ok(0)
}

/// 销毁**不在台账里**的裸实例。
///
/// ## 为什么必须有这条路径
///
/// `cloud cheapest --create` 建出来的实例没有部署记录，`stop`/`abort` 都
/// 找不到它。若只提供「按部署 ID 销毁」，用户想清理就只能去厂商控制台——
/// 而「忘了去控制台」正是持续计费最常见的成因。
fn destroy_bare_instance(args: &Args, target: &str) -> MnResult<i32> {
    let (provider, instance_id) = target.split_once('/').ok_or_else(|| {
        mangosteen_core::MnError::bad_request(format!(
            "--instance 需要写成 <服务商>/<实例ID>，例如 compshare/uhost-abc123，收到 `{target}`"
        ))
    })?;
    if provider.is_empty() || instance_id.is_empty() {
        return Err(mangosteen_core::MnError::bad_request(format!(
            "--instance 的服务商与实例 ID 都不能为空，收到 `{target}`"
        )));
    }

    if !args.has("yes") {
        output::print_warning(
            "⚠ 此操作非常危险，可能导致不可逆的数据丢失！销毁云端实例后磁盘数据不可恢复。",
        );
        output::print_kv(&[
            ("服务商".into(), provider.to_string()),
            ("实例 ID".into(), instance_id.to_string()),
            ("来源".into(), "裸实例（不在部署台账里）".into()),
        ]);
        println!();
        println!(
            "  确认执行请加 {}：{}",
            output::paint("--yes", output::Color::Cyan),
            output::bold(&format!(
                "mangosteen cloud destroy --instance {provider}/{instance_id} --yes"
            ))
        );
        return Ok(2);
    }

    let gw = crate::commands::build_gateway(args)?;
    let errors = gw.destroy_cloud_instance(provider, instance_id)?;
    if errors.is_empty() {
        output::print_success(&format!(
            "实例已销毁（{provider}/{instance_id}），计费已停止"
        ));
        return Ok(0);
    }

    // 销毁失败意味着**仍在计费**，必须当作错误而不是警告
    for e in &errors {
        output::print_warning(e);
    }
    Err(mangosteen_core::MnError::Network {
        stage: "销毁云端实例".to_string(),
        reason: format!(
            "实例 {instance_id} 可能仍在运行并计费：{}。请到厂商控制台确认",
            errors.join("；")
        ),
    })
}

/// `cloud key` - 管理云厂商访问凭证（与 UI 设置页同一套后端）
///
/// ## 密钥存储方式（与 UI 完全一致）
///
/// 密钥明文写入用户 secrets 目录（0700 目录 / 0600 文件），配置文件里
/// 只存 `SecretRef::File(path)` 路径引用，**绝不落明文**。写入后自动
/// 启用该厂商，并热重建云端驱动（无需重启网关）。
///
/// ## 用法
///
/// * `mangosteen cloud key <厂商> --password <值>`  —— 写入通用密码/密钥（自动启用）
/// * `mangosteen cloud key <厂商> --env <ENV_VAR>`     —— 引用环境变量
/// * `mangosteen cloud key <厂商> --access-key <值>`   —— 写入 AccessKeyId（优云智算等）
/// * `mangosteen cloud key <厂商> --secret-key <值>`   —— 写入 AccessKeySecret（优云智算等）
/// * `mangosteen cloud key <厂商> --set <k=v>`         —— 写入厂商专属选项（可重复）
/// * `mangosteen cloud key <厂商> --enable`            —— 启用该厂商
/// * `mangosteen cloud key <厂商> --disable`           —— 禁用该厂商
/// * `mangosteen cloud key <厂商>`                      —— 查看当前状态（脱敏）
fn cloud_key(args: Args) -> MnResult<i32> {
    let provider = match args.pos(2) {
        Some(p) => p,
        None => {
            return Err(MnError::bad_request(
                "缺少云厂商名。用法：mangosteen cloud key <厂商> [--password <值> | --env <ENV>] [--access-key <值>] [--secret-key <值>] [--set <k=v>] [--enable | --disable]",
            ))
        }
    };

    let gw = build_gateway(&args)?;
    let mut cfg = gw.config().as_ref().clone();
    let store = gw.config_store();

    // 先收集可用厂商名（用于错误提示），避免与下面的可变借用冲突
    let available: Vec<String> = cfg.providers.keys().cloned().collect();

    // ---- 获取目标厂商的可变引用并修改 ----
    let slot = cfg
        .providers
        .get_mut(provider)
        .ok_or_else(|| {
            MnError::bad_request(format!(
                "云厂商 `{provider}` 未配置。可用：{}",
                available.join(", ")
            ))
        })?;

    let mut changed = false;

    // ---- 写入单个密钥文件的辅助闭包 ----
    let write_secret_file = |secret_name: &str, secret_value: &str| -> MnResult<std::path::PathBuf> {
        let secret_value = secret_value.trim();
        if secret_value.is_empty() {
            return Err(MnError::bad_request(format!("{secret_name} 不能为空")));
        }
        let env = &mangosteen_platform::paths::real_env as &dyn Fn(&str) -> Option<String>;
        let dir = mangosteen_platform::paths::config_dir(env).join("secrets");
        ensure_private_dir(&dir)?;
        let path = dir.join(format!("{provider}_{secret_name}"));
        write_private_file(&path, secret_value)?;
        Ok(path)
    };

    // 设置密码/密钥（写入 secrets 文件，复用 UI 设置页方式）
    if let Some(password) = args.get("password") {
        let path = write_secret_file("private_key", password)?;
        slot.secret = SecretRef::File(path);
        if !slot.enabled {
            slot.enabled = true;
        }
        changed = true;
    }

    // 设置 AccessKeyId（写入 secrets 文件，记录到 options）
    if let Some(access_key) = args.get("access-key") {
        let path = write_secret_file("access_key", access_key)?;
        slot.options.insert("access_key_id".to_string(), format!("file:{}", path.display()));
        if !slot.enabled {
            slot.enabled = true;
        }
        changed = true;
    }

    // 设置 AccessKeySecret（写入 secrets 文件，记录到 options）
    if let Some(secret_key) = args.get("secret-key") {
        let path = write_secret_file("secret_key", secret_key)?;
        slot.options.insert("access_key_secret".to_string(), format!("file:{}", path.display()));
        if !slot.enabled {
            slot.enabled = true;
        }
        changed = true;
    }

    // 设置环境变量引用
    if let Some(env_var) = args.get("env") {
        let env_var = env_var.trim();
        if env_var.is_empty() {
            return Err(MnError::bad_request("环境变量名不能为空"));
        }
        slot.secret = SecretRef::Env(env_var.to_string());
        if !slot.enabled {
            slot.enabled = true;
        }
        changed = true;
    }

    // 设置厂商专属选项（--set k=v，可重复；args.options 取最后一个值）
    if let Some(set_value) = args.options.get("set") {
        let set_value = set_value.trim();
        if let Some((k, v)) = set_value.split_once('=') {
            let k = k.trim();
            let v = v.trim();
            if k.is_empty() || v.is_empty() {
                return Err(MnError::bad_request("--set 格式应为 key=value"));
            }
            slot.options.insert(k.to_string(), v.to_string());
            if !slot.enabled {
                slot.enabled = true;
            }
            changed = true;
        } else {
            return Err(MnError::bad_request("--set 格式应为 key=value"));
        }
    }

    // 启用 / 禁用
    if args.has("enable") {
        if !slot.enabled {
            slot.enabled = true;
            changed = true;
        }
    }
    if args.has("disable") {
        if slot.enabled {
            slot.enabled = false;
            changed = true;
        }
    }

    // 显式释放 slot 的可变借用，随后才能不可变借用 cfg
    drop(slot);

    if changed {
        cfg.validate()?;
        let written = store.save(&cfg)?;
        gw.replace_config(cfg);
        let (hot, skipped) = gw.reload_cloud_drivers();
        output::print_success(&format!(
            "云厂商 `{provider}` 凭证已更新 → {}",
            written.display()
        ));
        if hot {
            output::print_info(&format!(
                "云端驱动已热重建（跳过 {} 个厂商）",
                skipped.len()
            ));
            for (name, reason) in &skipped {
                output::print_warning(&format!("  {name}: {reason}"));
            }
        } else {
            output::print_warning(
                "云端驱动未热重建，请重启网关后生效：mangosteen ui restart",
            );
        }
    } else {
        // 仅查询状态（脱敏）：重新获取引用（此时已无其他借用）
        let slot = cfg
            .providers
            .get(provider)
            .ok_or_else(|| {
                MnError::bad_request(format!(
                    "云厂商 `{provider}` 未配置。可用：{}",
                    available.join(", ")
                ))
            })?;
        print_provider_status(slot, provider);
    }

    Ok(0)
}

/// 打印单个云厂商的状态（脱敏，不回显凭证明文）。
fn print_provider_status(slot: &mangosteen_core::config::ProviderConfig, provider: &str) {
    let title = format!("云厂商：{provider}");
    println!("{}", output::bold(&title));
    println!("  驱动：{}", slot.driver);
    println!("  状态：{}", if slot.enabled { "启用" } else { "停用" });
    println!("  凭证：{}", slot.secret.redacted());
    println!("  优先级：{:.1}", slot.priority);
    if !slot.options.is_empty() {
        let opts: Vec<&str> = slot.options.keys().map(|s| s.as_str()).collect();
        println!("  选项：{}", opts.join(", "));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_subcommand_is_actionable() {
        let a = crate::cli::parse_args(&["cloud".to_string(), "xyz".to_string()]).unwrap();
        let e = run(a).unwrap_err();
        assert!(e.to_string().contains("cloud xyz"));
        assert!(e.to_string().contains("providers"));
    }

    #[test]
    fn destroy_requires_id() {
        let a = crate::cli::parse_args(&["cloud".to_string(), "destroy".to_string()]).unwrap();
        let e = run(a).unwrap_err();
        assert!(e.to_string().contains("缺少部署 ID"));
    }

    #[test]
    fn constraints_built_from_args() {
        let a = crate::cli::parse_args(&[
            "cloud".to_string(),
            "pick".to_string(),
            "--min-vram".to_string(),
            "24".to_string(),
            "--max-price".to_string(),
            "3.5".to_string(),
            "--provider".to_string(),
            "autodl,ucloud".to_string(),
        ])
        .unwrap();
        let c = constraints(&a, true).unwrap();
        assert_eq!(c.min_vram_bytes, 24 << 30);
        assert_eq!(c.max_price_per_hour, Some(3.5));
        assert_eq!(c.allowed_providers, vec!["autodl", "ucloud"]);
    }

    #[test]
    fn constraints_default_when_no_flags() {
        let a = crate::cli::parse_args(&["cloud".to_string(), "pick".to_string()]).unwrap();
        let c = constraints(&a, true).unwrap();
        assert_eq!(c.min_vram_bytes, 0);
        assert_eq!(c.max_price_per_hour.is_none(), true);
        assert_eq!(c.allowed_providers.is_empty(), true);
        assert_eq!(c.require_stock, true, "默认要求有库存");
    }
    #[test]
    fn destroy_instance_requires_provider_slash_id() {
        let a = crate::cli::parse_args(&[
            "cloud".to_string(),
            "destroy".to_string(),
            "--instance".to_string(),
            "uhost-abc".to_string(),
        ])
        .unwrap();
        let e = run(a).unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("服务商"), "{msg}");
        assert!(msg.contains("uhost-abc"), "错误里要回显用户输入：{msg}");
    }

    #[test]
    fn destroy_instance_rejects_empty_parts() {
        let a = crate::cli::parse_args(&[
            "cloud".to_string(),
            "destroy".to_string(),
            "--instance".to_string(),
            "/uhost-abc".to_string(),
        ])
        .unwrap();
        let e = run(a).unwrap_err();
        assert!(e.to_string().contains("不能为空"), "{e}");
    }

    /// `--create` 必须是开关，不能吃下一个参数。
    ///
    /// 实测踩到：`--create --yes` 里 `--yes` 被当成 create 的值，于是
    /// `has("create")` 为假 —— 命令只查询不创建，还不报错。
    #[test]
    fn create_flag_does_not_swallow_yes() {
        let a = crate::cli::parse_args(&[
            "cloud".to_string(),
            "cheapest".to_string(),
            "--create".to_string(),
            "--yes".to_string(),
            "--name".to_string(),
            "mn".to_string(),
        ])
        .unwrap();
        assert!(a.has("create"), "--create 必须是布尔开关");
        assert!(a.has("yes"), "--yes 不能被 --create 吃掉");
        assert_eq!(a.get("name"), Some("mn"));
    }
}
