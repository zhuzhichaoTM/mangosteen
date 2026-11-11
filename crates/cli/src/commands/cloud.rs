//! `mangosteen cloud`
//!
//! 归属：**用户交互层 → 命令 `cloud`**。
//!
//! 管理云端节点（设置页「云端节点」的 CLI 对应）。
//! 云端节点是云厂商托管的实例，支持开机/关机/删除等生命周期管理。
//!
//! 子命令：
//! | 子命令 | 作用 |
//! |--------|------|
//! | `instances` | 列出云端实例（含已关机），支持 start/stop/destroy 子命令 |
//! | `key` | 管理云厂商密钥 |
//! | `pick` | 智能选点（四维打分） |
//! | `cheapest` | 最便宜优先（纯价格序） |
//! | `providers` | 列出云服务商及其启用状态 |

use mangosteen_core::MnResult;
use mangosteen_core::policy::SelectionConstraints;

use crate::cli::Args;
use crate::commands::{build_gateway, reject_unknown_options};
use crate::output;

/// 允许的选项。
const ALLOWED: &[&str] = &[
    "config",
    "json",
    "yes",
    "quiet",
    "secret",
    "public-key",
    "enable",
    "disable",
    "min-vram",
    "max-price",
    "explain",
    "create",
    "name",
    "require-stock",
    "no-require-stock",
];

/// 入口。
pub fn run(args: Args) -> MnResult<i32> {
    let sub = args.pos(1).unwrap_or("").to_string();
    if args.has("help") && sub.is_empty() {
        print_cloud_help();
        return Ok(0);
    }
    reject_unknown_options(&args, ALLOWED)?;

    let sub = args.pos(1).unwrap_or("help").to_string();
    match sub.as_str() {
        "instances" => instances(args),
        "key" => key(args),
        "pick" => pick(args),
        "cheapest" => cheapest(args),
        "providers" => providers(args),
        other => Err(mangosteen_core::MnError::bad_request(format!(
            "未知子命令 `cloud {other}`。可用：instances / key / pick / cheapest / providers"
        ))),
    }
}

/// `cloud instances` - 列出云端实例（含已关机），支持 start/stop/destroy 子命令。
fn instances(args: Args) -> MnResult<i32> {
    let sub = args.pos(2).unwrap_or("").to_string();
    if args.has("help") && sub.is_empty() {
        print_cloud_instances_help();
        return Ok(0);
    }
    match sub.as_str() {
        "list" | "ls" => {
            if args.has("help") {
                print_cloud_instances_list_help();
                return Ok(0);
            }
            instances_list(args)
        }
        "start" => {
            if args.has("help") {
                print_cloud_instances_start_help();
                return Ok(0);
            }
            instances_start(args)
        }
        "stop" => {
            if args.has("help") {
                print_cloud_instances_stop_help();
                return Ok(0);
            }
            instances_stop(args)
        }
        "destroy" => {
            if args.has("help") {
                print_cloud_instances_destroy_help();
                return Ok(0);
            }
            instances_destroy(args)
        }
        other => Err(mangosteen_core::MnError::bad_request(format!(
            "未知子命令 `cloud instances {other}`。可用：list / start / stop / destroy"
        ))),
    }
}

/// `cloud instances list` - 列出云端实例（含已关机）。
///
/// 与 UI 设置页「云端节点」列表功能一致。
fn instances_list(args: Args) -> MnResult<i32> {
    if args.has("help") {
        print_cloud_instances_list_help();
        return Ok(0);
    }
    let gw = build_gateway(&args)?;
    let instances = gw.cloud_instance_admin_briefs();

    if args.has("json") {
        let arr: Vec<serde_json::Value> = instances
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

    if instances.is_empty() {
        println!("{}", output::dim("（当前没有云端实例）"));
        println!();
        println!(
            "  {}",
            output::dim("执行 `mangosteen deploy <模型>` 创建云端实例")
        );
        return Ok(0);
    }

    let rows: Vec<Vec<String>> = instances
        .iter()
        .map(|x| {
            vec![
                format!("{}/{}", x.provider, x.instance_id),
                x.gpu_model.clone(),
                x.gpu_count.to_string(),
                format!("{} GB", x.vram_bytes / 1024 / 1024 / 1024),
                x.state.clone(),
                format!("¥{:.2}/小时", x.price_per_hour.unwrap_or(0.0)),
                x.occupied_models.join(", "),
            ]
        })
        .collect();
    output::print_table(
        &["实例ID", "GPU型号", "GPU数量", "显存", "状态", "价格", "已部署模型"],
        &rows,
    );
    println!();
    println!(
        "  {}",
        output::dim("用 `mangosteen cloud instances start <厂商>/<实例ID>` 启动实例")
    );
    println!(
        "  {}",
        output::dim("用 `mangosteen cloud instances stop <厂商>/<实例ID>` 关闭实例")
    );
    println!(
        "  {}",
        output::dim("用 `mangosteen cloud instances destroy <厂商>/<实例ID>` 删除实例")
    );
    Ok(0)
}

/// `cloud instances start <provider>/<instance_id>` - 启动实例。
///
/// 与 UI 设置页「云端节点」开机按钮功能一致。
fn instances_start(args: Args) -> MnResult<i32> {
    if args.has("help") {
        print_cloud_instances_start_help();
        return Ok(0);
    }
    let instance_ref = args.pos(3).ok_or_else(|| {
        mangosteen_core::MnError::bad_request(
            "缺少实例 ID。用法：mangosteen cloud instances start <厂商>/<实例ID>",
        )
    })?;

    let parts: Vec<&str> = instance_ref.split('/').collect();
    if parts.len() != 2 {
        return Err(mangosteen_core::MnError::bad_request(
            "实例 ID 格式应为 <厂商>/<实例ID>",
        ));
    }
    let provider = parts[0];
    let instance_id = parts[1];

    let gw = build_gateway(&args)?;
    gw.cloud_power_on(provider, instance_id)?;
    output::print_success(&format!(
        "实例 {}/{} 已开机并进入运行状态",
        provider, instance_id
    ));
    Ok(0)
}

/// `cloud instances stop <provider>/<instance_id>` - 关闭实例。
///
/// 与 UI 设置页「云端节点」关机按钮功能一致。
fn instances_stop(args: Args) -> MnResult<i32> {
    if args.has("help") {
        print_cloud_instances_stop_help();
        return Ok(0);
    }
    let instance_ref = args.pos(3).ok_or_else(|| {
        mangosteen_core::MnError::bad_request(
            "缺少实例 ID。用法：mangosteen cloud instances stop <厂商>/<实例ID>",
        )
    })?;

    let parts: Vec<&str> = instance_ref.split('/').collect();
    if parts.len() != 2 {
        return Err(mangosteen_core::MnError::bad_request(
            "实例 ID 格式应为 <厂商>/<实例ID>",
        ));
    }
    let provider = parts[0];
    let instance_id = parts[1];

    let gw = build_gateway(&args)?;
    gw.cloud_power_off(provider, instance_id)?;
    output::print_success(&format!(
        "实例 {}/{} 已关机（停止 GPU 计费，实例与磁盘保留）",
        provider, instance_id
    ));
    Ok(0)
}

/// `cloud instances destroy <provider>/<instance_id>` - 删除实例。
///
/// 与 UI 设置页「云端节点」删除按钮功能一致。
/// 销毁实例与磁盘，不可恢复。
fn instances_destroy(args: Args) -> MnResult<i32> {
    if args.has("help") {
        print_cloud_instances_destroy_help();
        return Ok(0);
    }
    let instance_ref = args.pos(3).ok_or_else(|| {
        mangosteen_core::MnError::bad_request(
            "缺少实例 ID。用法：mangosteen cloud instances destroy <厂商>/<实例ID>",
        )
    })?;

    let parts: Vec<&str> = instance_ref.split('/').collect();
    if parts.len() != 2 {
        return Err(mangosteen_core::MnError::bad_request(
            "实例 ID 格式应为 <厂商>/<实例ID>",
        ));
    }
    let provider = parts[0];
    let instance_id = parts[1];

    if !args.has("yes") {
        return Err(mangosteen_core::MnError::bad_request(
            "此操作将销毁实例与磁盘，不可恢复。请添加 --yes 确认",
        ));
    }

    let gw = build_gateway(&args)?;
    let errs = gw.destroy_cloud_instance(provider, instance_id)?;

    if errs.is_empty() {
        output::print_success(&format!(
            "实例 {}/{} 已删除（实例与磁盘已销毁）",
            provider, instance_id
        ));
    } else {
        let error_msg = errs.join("；");
        return Err(mangosteen_core::MnError::bad_request(format!(
            "实例 {}/{} 删除时发生错误：{}",
            provider, instance_id, error_msg
        )));
    }
    Ok(0)
}

/// `cloud key <厂商>` - 管理云厂商密钥。
fn key(args: Args) -> MnResult<i32> {
    if args.has("help") {
        print_cloud_key_help();
        return Ok(0);
    }
    let provider = args.pos(2).ok_or_else(|| {
        mangosteen_core::MnError::bad_request(
            "缺少厂商名称。用法：mangosteen cloud key <厂商> [--secret <密钥>] [--public-key <公钥>] [--enable|--disable]",
        )
    })?;

    let gw = build_gateway(&args)?;
    let mut cfg = gw.config().as_ref().clone();
    let store = gw.config_store();

    // 先收集可用厂商列表
    let available_providers: Vec<String> = cfg.providers.keys().cloned().collect();

    // 查找提供商配置
    let provider_cfg = cfg
        .providers
        .get_mut(provider)
        .ok_or_else(|| {
            mangosteen_core::MnError::bad_request(format!(
                "未找到厂商 `{provider}`。可用厂商：{}",
                available_providers.join(", ")
            ))
        })?;

    // 处理 --enable / --disable
    if args.has("enable") {
        provider_cfg.enabled = true;
    } else if args.has("disable") {
        provider_cfg.enabled = false;
    }

    // 处理 secret 更新（私钥）
    if let Some(secret) = args.get("secret") {
        let secret = secret.trim();
        if secret.is_empty() {
            return Err(mangosteen_core::MnError::bad_request("密钥不能为空"));
        }
        let env = &mangosteen_platform::paths::real_env as &dyn Fn(&str) -> Option<String>;
        let dir = mangosteen_platform::paths::config_dir(env).join("secrets");
        mangosteen_platform::fs::ensure_private_dir(&dir)?;
        let path = dir.join(format!("cloud-{}", provider));
        mangosteen_platform::fs::write_private_file(&path, secret)?;
        provider_cfg.secret = mangosteen_core::config::SecretRef::File(path);
    }

    // 处理 public_key 更新（AccessKey ID，存储在 options 中）
    if let Some(public_key) = args.get("public-key") {
        let public_key = public_key.trim();
        if public_key.is_empty() {
            return Err(mangosteen_core::MnError::bad_request("公钥不能为空"));
        }
        provider_cfg.options.insert("public_key".to_string(), public_key.to_string());
    }

    cfg.validate()?;
    let written = store.save(&cfg)?;
    gw.replace_config(cfg);
    gw.reload_cloud_drivers();

    output::print_success(&format!("厂商 {provider} 配置已更新 → {}", written.display()));
    Ok(0)
}

/// `cloud pick` - 智能选点（四维打分）。
///
/// 查询可用规格，按配置的权重策略打分排序，显示最优候选。
/// 不创建任何资源，不产生费用。
fn pick(args: Args) -> MnResult<i32> {
    if args.has("help") {
        print_cloud_pick_help();
        return Ok(0);
    }
    let gw = build_gateway(&args)?;

    // 解析约束
    let mut constraints = SelectionConstraints::default();
    if let Some(min_vram_gb) = args.get_num::<u64>("min-vram")? {
        constraints.min_vram_bytes = min_vram_gb * 1024 * 1024 * 1024;
    }
    if let Some(max_price) = args.get_num::<f64>("max-price")? {
        constraints.max_price_per_hour = Some(max_price);
    }
    if args.has("no-require-stock") {
        constraints.require_stock = false;
    }

    let ranked = gw.pick_cloud(&constraints)?;

    if args.has("json") {
        println!(
            "{}",
            serde_json::to_string_pretty(&ranked).unwrap_or_default()
        );
        return Ok(0);
    }

    if ranked.is_empty() {
        println!("{}", output::dim("（无可用候选）"));
        println!();
        println!(
            "  {}",
            output::dim("建议：放宽约束（提高价格上限 / 降低显存要求），或启用更多服务商")
        );
        return Ok(0);
    }

    // 默认显示全部候选的综合评分
    println!("{}", output::bold("智能选点结果（四维打分）："));
    println!();

    if args.has("explain") {
        let best = &ranked[0];
        println!(
            "  {}",
            output::paint(&format!("最优：{}/{} [{}] ¥{:.3}/h · {:.1}GiB",
                best.provider,
                best.gpu_model,
                best.region,
                best.price_per_hour,
                best.vram_bytes as f64 / (1u64 << 30) as f64
            ), output::Color::Cyan)
        );
        println!();
        println!("{}", output::dim("打分明细："));
        println!(
            "  价格 {:.2} · 延迟 {:.2} · 可用 {:.2} · 硬件 {:.2} → 总分 {:.4}",
            best.score_price,
            best.score_latency,
            best.score_availability,
            best.score_hardware,
            best.score_total
        );
        println!();
    }

    println!("{}", output::dim("全部候选（按综合得分降序）："));
    for (i, c) in ranked.iter().enumerate() {
        let price_str = if c.price_per_hour >= f64::MAX {
            "未知".to_string()
        } else {
            format!("¥{:.3}/h", c.price_per_hour)
        };
        println!(
            "  {}. {}/{} [{}] {} · {:.1}GiB → {:.4}",
            i + 1,
            c.provider,
            c.gpu_model,
            c.region,
            price_str,
            c.vram_bytes as f64 / (1u64 << 30) as f64,
            c.score_total
        );
    }

    Ok(0)
}

/// `cloud cheapest` - 最便宜优先（纯价格序）。
///
/// 查询可用规格，按价格升序排序，显示最便宜的候选。
/// 不创建任何资源，不产生费用。
fn cheapest(args: Args) -> MnResult<i32> {
    if args.has("help") {
        print_cloud_cheapest_help();
        return Ok(0);
    }
    let gw = build_gateway(&args)?;

    // 解析约束
    let mut constraints = SelectionConstraints::default();
    if let Some(min_vram_gb) = args.get_num::<u64>("min-vram")? {
        constraints.min_vram_bytes = min_vram_gb * 1024 * 1024 * 1024;
    }
    if args.has("no-require-stock") {
        constraints.require_stock = false;
    }

    let ordered = gw.cheapest_cloud(&constraints)?;

    if args.has("json") {
        let arr: Vec<serde_json::Value> = ordered
            .iter()
            .map(|(provider, spec)| {
                serde_json::json!({
                    "provider": provider,
                    "spec_id": spec.spec_id,
                    "gpu_model": spec.spec.gpu.model,
                    "gpu_count": spec.spec.gpu.count,
                    "vram_bytes": spec.spec.gpu.total_vram_bytes(),
                    "memory_bytes": spec.spec.memory_bytes,
                    "price_per_hour": spec.price.per_hour_cny,
                    "region": spec.region,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({"candidates": arr}))
                .unwrap_or_default()
        );
        return Ok(0);
    }

    if ordered.is_empty() {
        println!("{}", output::dim("（无可用候选）"));
        println!();
        println!(
            "  {}",
            output::dim("建议：放宽约束（提高价格上限 / 降低显存要求），或启用更多服务商")
        );
        return Ok(0);
    }

    println!("{}", output::bold("最便宜候选（按价格升序）："));
    println!();

    let rows: Vec<Vec<String>> = ordered
        .iter()
        .map(|(provider, spec)| {
            let price_str = if spec.price.per_hour_cny >= f64::MAX {
                "未知".to_string()
            } else {
                format!("¥{:.3}/h", spec.price.per_hour_cny)
            };
            vec![
                provider.clone(),
                spec.spec.gpu.model.clone(),
                format!("{}x", spec.spec.gpu.count),
                format!("{:.1} GiB", spec.spec.gpu.total_vram_bytes() as f64 / (1u64 << 30) as f64),
                price_str,
                spec.region.clone(),
            ]
        })
        .collect();

    output::print_table(
        &["服务商", "GPU型号", "数量", "显存", "价格", "地域"],
        &rows,
    );

    Ok(0)
}

/// `cloud providers` - 列出云服务商及其启用状态。
///
/// 与 UI 设置页「云服务商」列表功能一致。
fn providers(args: Args) -> MnResult<i32> {
    if args.has("help") {
        print_cloud_providers_help();
        return Ok(0);
    }
    let gw = build_gateway(&args)?;
    let cfg = gw.config();

    let providers: Vec<_> = cfg
        .providers
        .iter()
        .map(|(name, p)| {
            (
                name.clone(),
                p.display_name.clone().unwrap_or_else(|| name.clone()),
                p.driver.clone(),
                p.enabled,
                p.priority,
            )
        })
        .collect();

    if args.has("json") {
        let arr: Vec<serde_json::Value> = providers
            .iter()
            .map(|(name, display_name, driver, enabled, priority)| {
                serde_json::json!({
                    "name": name,
                    "display_name": display_name,
                    "driver": driver,
                    "enabled": enabled,
                    "priority": priority,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({"providers": arr}))
                .unwrap_or_default()
        );
        return Ok(0);
    }

    if providers.is_empty() {
        println!("{}", output::dim("（未配置任何云服务商）"));
        println!();
        println!(
            "  {}",
            output::dim("在 config.toml 中配置 [providers.<name>] 后即可使用")
        );
        return Ok(0);
    }

    let rows: Vec<Vec<String>> = providers
        .iter()
        .map(|(name, display_name, driver, enabled, priority)| {
            vec![
                name.clone(),
                display_name.clone(),
                driver.clone(),
                if *enabled { "启用".to_string() } else { "禁用".to_string() },
                format!("{:.1}", priority),
            ]
        })
        .collect();

    output::print_table(
        &["名称", "显示名", "驱动", "状态", "权重"],
        &rows,
    );
    println!();
    println!(
        "  {}",
        output::dim("用 `mangosteen cloud key <厂商> --enable|--disable` 启用/禁用服务商")
    );
    println!(
        "  {}",
        output::dim("用 `mangosteen cloud key <厂商> --secret <密钥> --public-key <公钥>` 更新密钥")
    );

    Ok(0)
}

/// 打印 cloud 命令帮助。
fn print_cloud_help() {
    println!("mangosteen cloud <子命令> [选项]");
    println!();
    println!("云端算力管理。");
    println!();
    println!("{}", output::bold("子命令"));
    println!("  {:<w$}  {}", "instances", "云端实例管理", w = 18);
    println!("  {:<w$}  {}", "key", "云厂商密钥管理", w = 18);
    println!("  {:<w$}  {}", "pick", "智能选点", w = 18);
    println!("  {:<w$}  {}", "cheapest", "最便宜优先", w = 18);
    println!("  {:<w$}  {}", "providers", "云服务商管理", w = 18);
    println!();
    println!("{}", output::bold("全局选项"));
    println!("  {:<w$}  {}", "--config <path>", "指定配置文件", w = 22);
    println!("  {:<w$}  {}", "--json", "以 JSON 输出（便于脚本处理）", w = 22);
    println!("  {:<w$}  {}", "--quiet", "抑制 banner 与进度提示", w = 22);
    println!();
    println!("{}", output::bold("示例"));
    println!("  mangosteen cloud instances list");
    println!("  mangosteen cloud key compshare --secret <密钥> --public-key <公钥>");
    println!("  mangosteen cloud pick --max-price 1.5 --explain");
    println!("  mangosteen cloud cheapest --min-vram 1");
    println!("  mangosteen cloud providers");
    println!();
    println!("更多信息：docs/USAGE.md");
}

/// 打印 cloud instances 子命令帮助。
fn print_cloud_instances_help() {
    println!("mangosteen cloud instances <list|start|stop|destroy> [选项]");
    println!();
    println!("云端实例管理。");
    println!();
    println!("{}", output::bold("子命令"));
    println!("  {:<w$}  {}", "list", "列出云端实例（含已关机）", w = 18);
    println!("  {:<w$}  {}", "start", "启动实例", w = 18);
    println!("  {:<w$}  {}", "stop", "关闭实例", w = 18);
    println!("  {:<w$}  {}", "destroy", "删除实例（需 --yes 确认）", w = 18);
    println!();
    println!("{}", output::bold("示例"));
    println!("  mangosteen cloud instances list");
    println!("  mangosteen cloud instances start compshare/uhost-xxx");
    println!("  mangosteen cloud instances stop compshare/uhost-xxx");
    println!("  mangosteen cloud instances destroy compshare/uhost-xxx --yes");
    println!();
    println!("更多信息：docs/USAGE.md");
}

/// 打印 cloud instances list 子命令帮助。
fn print_cloud_instances_list_help() {
    println!("mangosteen cloud instances list [选项]");
    println!();
    println!("列出云端实例（含已关机）。");
    println!();
    println!("{}", output::bold("选项"));
    println!("  {:<w$}  {}", "--json", "以 JSON 输出（便于脚本处理）", w = 22);
    println!("  {:<w$}  {}", "--quiet", "抑制 banner 与进度提示", w = 22);
    println!();
    println!("{}", output::bold("示例"));
    println!("  mangosteen cloud instances list");
    println!("  mangosteen cloud instances list --json");
    println!();
    println!("更多信息：docs/USAGE.md");
}

/// 打印 cloud instances start 子命令帮助。
fn print_cloud_instances_start_help() {
    println!("mangosteen cloud instances start <厂商>/<实例ID> [选项]");
    println!();
    println!("启动实例（开机）。");
    println!();
    println!("{}", output::bold("位置参数"));
    println!("  {:<w$}  {}", "<厂商>/<实例ID>", "实例标识，如 compshare/uhost-xxx", w = 22);
    println!();
    println!("{}", output::bold("选项"));
    println!("  {:<w$}  {}", "--config <path>", "指定配置文件", w = 22);
    println!("  {:<w$}  {}", "--json", "以 JSON 输出（便于脚本处理）", w = 22);
    println!("  {:<w$}  {}", "--quiet", "抑制 banner 与进度提示", w = 22);
    println!();
    println!("{}", output::bold("示例"));
    println!("  mangosteen cloud instances start compshare/uhost-1xkb7e7x9c28");
    println!();
    println!("更多信息：docs/USAGE.md");
}

/// 打印 cloud instances stop 子命令帮助。
fn print_cloud_instances_stop_help() {
    println!("mangosteen cloud instances stop <厂商>/<实例ID> [选项]");
    println!();
    println!("关闭实例（停止 GPU 计费，实例与磁盘保留）。");
    println!();
    println!("{}", output::bold("位置参数"));
    println!("  {:<w$}  {}", "<厂商>/<实例ID>", "实例标识，如 compshare/uhost-xxx", w = 22);
    println!();
    println!("{}", output::bold("选项"));
    println!("  {:<w$}  {}", "--config <path>", "指定配置文件", w = 22);
    println!("  {:<w$}  {}", "--json", "以 JSON 输出（便于脚本处理）", w = 22);
    println!("  {:<w$}  {}", "--quiet", "抑制 banner 与进度提示", w = 22);
    println!();
    println!("{}", output::bold("示例"));
    println!("  mangosteen cloud instances stop compshare/uhost-1xkb7e7x9c28");
    println!();
    println!("更多信息：docs/USAGE.md");
}

/// 打印 cloud instances destroy 子命令帮助。
fn print_cloud_instances_destroy_help() {
    println!("mangosteen cloud instances destroy <厂商>/<实例ID> --yes [选项]");
    println!();
    println!("删除实例（销毁实例与磁盘，不可恢复）。");
    println!();
    println!("{}", output::bold("位置参数"));
    println!("  {:<w$}  {}", "<厂商>/<实例ID>", "实例标识，如 compshare/uhost-xxx", w = 22);
    println!();
    println!("{}", output::bold("选项"));
    println!("  {:<w$}  {}", "--yes", "免确认直接执行（必须提供）", w = 22);
    println!("  {:<w$}  {}", "--config <path>", "指定配置文件", w = 22);
    println!("  {:<w$}  {}", "--json", "以 JSON 输出（便于脚本处理）", w = 22);
    println!("  {:<w$}  {}", "--quiet", "抑制 banner 与进度提示", w = 22);
    println!();
    println!("{}", output::bold("示例"));
    println!("  mangosteen cloud instances destroy compshare/uhost-1xkb7e7x9c28 --yes");
    println!();
    println!("警告：删除操作不可逆，实例与磁盘将被永久销毁。");
    println!("更多信息：docs/USAGE.md");
}

/// 打印 cloud key 子命令帮助。
fn print_cloud_key_help() {
    println!("mangosteen cloud key <厂商> [选项]");
    println!();
    println!("管理云厂商密钥。");
    println!();
    println!("{}", output::bold("位置参数"));
    println!("  {:<w$}  {}", "<厂商>", "云服务商名称（如 compshare、aliyun、autodl）", w = 22);
    println!();
    println!("{}", output::bold("选项"));
    println!("  {:<w$}  {}", "--secret <密钥>", "更新厂商私钥（Secret Key，存储为加密文件）", w = 22);
    println!("  {:<w$}  {}", "--public-key <公钥>", "更新厂商公钥（AccessKey ID，存储在 options 中）", w = 22);
    println!("  {:<w$}  {}", "--enable", "启用该厂商", w = 22);
    println!("  {:<w$}  {}", "--disable", "禁用该厂商", w = 22);
    println!("  {:<w$}  {}", "--config <path>", "指定配置文件", w = 22);
    println!("  {:<w$}  {}", "--json", "以 JSON 输出（便于脚本处理）", w = 22);
    println!("  {:<w$}  {}", "--quiet", "抑制 banner 与进度提示", w = 22);
    println!();
    println!("{}", output::bold("示例"));
    println!("  mangosteen cloud key compshare --secret <密钥> --public-key <公钥>");
    println!("  mangosteen cloud key compshare --enable");
    println!("  mangosteen cloud key aliyun --disable");
    println!();
    println!("更多信息：docs/USAGE.md");
}

/// 打印 cloud pick 子命令帮助。
fn print_cloud_pick_help() {
    println!("mangosteen cloud pick [选项]");
    println!();
    println!("智能选点（四维打分：价格、延迟、可用性、硬件）。");
    println!();
    println!("{}", output::bold("选项"));
    println!("  {:<w$}  {}", "--min-vram <GB>", "最小显存要求（GB）", w = 22);
    println!("  {:<w$}  {}", "--max-price <元/小时>", "最高价格上限（元/小时）", w = 22);
    println!("  {:<w$}  {}", "--explain", "显示打分明细", w = 22);
    println!("  {:<w$}  {}", "--json", "以 JSON 输出（便于脚本处理）", w = 22);
    println!("  {:<w$}  {}", "--quiet", "抑制 banner 与进度提示", w = 22);
    println!("  {:<w$}  {}", "--require-stock", "要求库存充足", w = 22);
    println!("  {:<w$}  {}", "--no-require-stock", "不要求库存", w = 22);
    println!();
    println!("{}", output::bold("示例"));
    println!("  mangosteen cloud pick --max-price 1.5 --explain");
    println!("  mangosteen cloud pick --min-vram 8 --json");
    println!();
    println!("更多信息：docs/USAGE.md");
}

/// 打印 cloud cheapest 子命令帮助。
fn print_cloud_cheapest_help() {
    println!("mangosteen cloud cheapest [选项]");
    println!();
    println!("最便宜优先（纯价格序）。");
    println!();
    println!("{}", output::bold("选项"));
    println!("  {:<w$}  {}", "--min-vram <GB>", "最小显存要求（GB）", w = 22);
    println!("  {:<w$}  {}", "--json", "以 JSON 输出（便于脚本处理）", w = 22);
    println!("  {:<w$}  {}", "--quiet", "抑制 banner 与进度提示", w = 22);
    println!("  {:<w$}  {}", "--require-stock", "要求库存充足", w = 22);
    println!("  {:<w$}  {}", "--no-require-stock", "不要求库存", w = 22);
    println!();
    println!("{}", output::bold("示例"));
    println!("  mangosteen cloud cheapest --min-vram 1");
    println!("  mangosteen cloud cheapest --json");
    println!();
    println!("更多信息：docs/USAGE.md");
}

/// 打印 cloud providers 子命令帮助。
fn print_cloud_providers_help() {
    println!("mangosteen cloud providers [选项]");
    println!();
    println!("列出云服务商及其启用状态。");
    println!();
    println!("{}", output::bold("选项"));
    println!("  {:<w$}  {}", "--json", "以 JSON 输出（便于脚本处理）", w = 22);
    println!("  {:<w$}  {}", "--quiet", "抑制 banner 与进度提示", w = 22);
    println!();
    println!("{}", output::bold("示例"));
    println!("  mangosteen cloud providers");
    println!("  mangosteen cloud providers --json");
    println!();
    println!("更多信息：docs/USAGE.md");
}
