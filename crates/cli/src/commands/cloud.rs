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

use mangosteen_core::MnResult;

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
    "enable",
    "disable",
];

/// 入口。
pub fn run(args: Args) -> MnResult<i32> {
    reject_unknown_options(&args, ALLOWED)?;

    let sub = args.pos(1).unwrap_or("help").to_string();
    match sub.as_str() {
        "instances" => instances(args),
        "key" => key(args),
        other => Err(mangosteen_core::MnError::bad_request(format!(
            "未知子命令 `cloud {other}`。可用：instances / key"
        ))),
    }
}

/// `cloud instances` - 列出云端实例（含已关机），支持 start/stop/destroy 子命令。
fn instances(args: Args) -> MnResult<i32> {
    let sub = args.pos(2).unwrap_or("list").to_string();
    match sub.as_str() {
        "list" | "ls" => instances_list(args),
        "start" => instances_start(args),
        "stop" => instances_stop(args),
        "destroy" => instances_destroy(args),
        other => Err(mangosteen_core::MnError::bad_request(format!(
            "未知子命令 `cloud instances {other}`。可用：list / start / stop / destroy"
        ))),
    }
}

/// `cloud instances list` - 列出云端实例（含已关机）。
///
/// 与 UI 设置页「云端节点」列表功能一致。
fn instances_list(args: Args) -> MnResult<i32> {
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
    let provider = args.pos(2).ok_or_else(|| {
        mangosteen_core::MnError::bad_request(
            "缺少厂商名称。用法：mangosteen cloud key <厂商> [--secret <密钥>] [--enable|--disable]",
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

    // 处理密钥更新
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

    cfg.validate()?;
    let written = store.save(&cfg)?;
    gw.replace_config(cfg);
    gw.reload_cloud_drivers();

    output::print_success(&format!("厂商 {provider} 配置已更新 → {}", written.display()));
    Ok(0)
}
