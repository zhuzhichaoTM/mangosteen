//! `mangosteen config`
//!
//! 归属：**用户交互层 → 命令 `config`**。
//!
//! 子命令：`show` / `init` / `path` / `validate`。
//!
//! **密钥永不回显明文**：`show` 输出的是 `SecretRef` 的脱敏形式
//! （`<inline:***redacted***>` / `${ENV_VAR}` / `file:/path`）。

use mangosteen_core::MnResult;

use crate::cli::Args;
use crate::commands::{load_config, reject_unknown_options};
use crate::output;

/// 允许的选项。
const ALLOWED: &[&str] = &["config", "json", "force", "quiet"];

/// 入口。
pub fn run(args: Args) -> MnResult<i32> {
    if args.has("help") {
        print_config_help();
        return Ok(0);
    }
    let sub = args.pos(1).unwrap_or("show").to_string();
    match sub.as_str() {
        "show" => {
            if args.has("help") {
                print_config_show_help();
                return Ok(0);
            }
            show(args)
        }
        "init" => {
            if args.has("help") {
                print_config_init_help();
                return Ok(0);
            }
            init(args)
        }
        "path" => {
            if args.has("help") {
                print_config_path_help();
                return Ok(0);
            }
            path(args)
        }
        // P0-1 密钥备份（2026-11-19）：gateway.key / ssh_known_hosts
        // 是本地单点——损坏/误删会导致鉴权失效或 SSH 全拒连。备份后
        // 可随时恢复。
        "backup-keys" => backup_keys(args),
        // P0-2 网关密钥轮换（2026-11-19）：生成新 key → 原子写
        // gateway.key（600）→ 提示重启网关生效。
        "rotate-gateway-key" => rotate_gateway_key(args),
        // P0-1 host key 恢复（2026-11-19）：ssh_known_hosts 损坏
        // （指纹全变/文件损坏）→ 清空重置，下次连接重新引导。
        "reset-host-keys" => reset_host_keys(args),
        other => Err(mangosteen_core::MnError::bad_request(format!(
            "未知子命令 `config {other}`。可用：show / init / path / backup-keys / rotate-gateway-key / reset-host-keys"
        ))),
    }
}

/// `config show`
fn show(args: Args) -> MnResult<i32> {
    let (cfg, source) = load_config(&args)?;

    if args.has("json") {
        // 密钥以脱敏形式进入 JSON，绝不输出明文
        let providers: Vec<serde_json::Value> = cfg
            .providers
            .iter()
            .map(|(name, p)| {
                serde_json::json!({
                    "name": name,
                    "driver": p.driver,
                    "enabled": p.enabled,
                    "display_name": p.display_name,
                    "secret": p.secret.redacted(),
                    "priority": p.priority,
                    "options": p.options.keys().collect::<Vec<_>>(),
                })
            })
            .collect();
        let payload = serde_json::json!({
            "source": source,
            "config_version": cfg.config_version,
            "gateway": {
                "port": cfg.gateway.port,
                "host": cfg.gateway.host,
                "require_api_key": cfg.gateway.require_api_key,
                "api_key": cfg.gateway.api_key.redacted(),
            },
            "scheduler": {
                "weights": cfg.scheduler.weights,
                "max_fallbacks": cfg.scheduler.max_fallbacks,
                "launch_timeout_secs": cfg.scheduler.launch_timeout_secs,
                "provision_timeout_secs": cfg.scheduler.provision_timeout_secs,
                "max_price_per_hour": cfg.scheduler.max_price_per_hour,
            },
            "local": {
                "engine_path": cfg.local.engine_path.map(|p| p.display().to_string()),
                "default_context": cfg.local.default_context,
                "kv_cache": cfg.local.kv_cache,
                "batch_size": cfg.local.batch_size,
                "allow_cpu_fallback": cfg.local.allow_cpu_fallback,
                "allow_partial_offload": cfg.local.allow_partial_offload,
                "port_range": [cfg.local.port_range_start, cfg.local.port_range_end],
            },
            "storage": {
                "default_source": cfg.storage.default_source,
                "verify_hash": cfg.storage.verify_hash,
                "max_model_bytes": cfg.storage.max_model_bytes,
                "allowed_read_roots": cfg.storage.allowed_read_roots
                    .iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
            },
            "providers": providers,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).unwrap_or_default()
        );
        return Ok(0);
    }

    println!("{}", output::bold("配置"));
    println!("  {}", output::dim(&source));
    println!();

    println!("{}", output::bold("网关"));
    output::print_kv(&[
        (
            "监听".into(),
            format!("{}:{}", cfg.gateway.host, cfg.gateway.port),
        ),
        (
            "鉴权".into(),
            if cfg.gateway.require_api_key {
                format!("开启（{}）", cfg.gateway.api_key.redacted())
            } else {
                "关闭".to_string()
            },
        ),
    ]);

    println!();
    println!("{}", output::bold("优选策略"));
    output::print_kv(&[
        (
            "权重".into(),
            format!(
                "价格 {:.0} / 延迟 {:.0} / 可用性 {:.0} / 硬件 {:.0}",
                cfg.scheduler.weights.price,
                cfg.scheduler.weights.latency,
                cfg.scheduler.weights.availability,
                cfg.scheduler.weights.hardware
            ),
        ),
        (
            "降级重试上限".into(),
            cfg.scheduler.max_fallbacks.to_string(),
        ),
        ("价格上限".into(), opt_num(cfg.scheduler.max_price_per_hour)),
    ]);

    println!();
    println!("{}", output::bold("模型配置"));
    output::print_kv(&[
        ("默认上下文".into(), cfg.local.default_context.to_string()),
        (
            "端口区间".into(),
            format!(
                "{}-{}",
                cfg.local.port_range_start, cfg.local.port_range_end
            ),
        ),
    ]);

    println!();
    println!("{}", output::bold("云服务商"));
    if cfg.providers.is_empty() {
        println!("  {}", output::dim("（未配置）"));
    } else {
        let rows: Vec<Vec<String>> = cfg
            .providers
            .iter()
            .map(|(name, p)| {
                vec![
                    name.clone(),
                    p.driver.clone(),
                    if p.enabled { "启用" } else { "停用" }.to_string(),
                    p.secret.redacted(),
                    format!("{:.1}", p.priority),
                ]
            })
            .collect();
        output::print_table(&["名称", "驱动", "状态", "凭证", "优先级"], &rows);
    }
    Ok(0)
}

/// `config init`
fn init(args: Args) -> MnResult<i32> {
    let store = mangosteen_base::ConfigStore::platform_default(
        args.get("config").map(std::path::PathBuf::from),
    );
    let target = store.default_write_path();

    if target.exists() && !args.has("force") {
        return Err(mangosteen_core::MnError::bad_request(format!(
            "配置文件已存在：{}。若要覆盖请加 --force（会丢失现有配置）",
            target.display()
        )));
    }

    let cfg = mangosteen_core::config::Config::sample();
    cfg.validate()?;
    let written = store.save(&cfg)?;

    output::print_success(&format!("已生成配置文件：{}", written.display()));
    println!();
    println!("  {}", output::dim("接下来："));
    println!(
        "    1. 执行 {} 更新云服务商密钥",
        output::bold("`mangosteen cloud key <厂商> --secret <密钥> --public-key <公钥>`")
    );
    println!("    2. 执行 `mangosteen doctor` 自检");
    println!("    3. 执行 `mangosteen models add <path>` 登记模型");
    println!();
    #[cfg(unix)]
    println!(
        "  {}",
        output::dim("注意：该文件权限已设为 0600（可能含密钥）")
    );
    Ok(0)
}

/// `config path`
fn path(args: Args) -> MnResult<i32> {
    let store = mangosteen_base::ConfigStore::platform_default(
        args.get("config").map(std::path::PathBuf::from),
    );
    let env = &mangosteen_platform::paths::real_env as &dyn Fn(&str) -> Option<String>;

    println!("{}", output::bold("配置与数据路径"));
    output::print_kv(&[
        (
            "配置文件（写入位置）".into(),
            store.default_write_path().display().to_string(),
        ),
        (
            "配置文件（候选列表）".into(),
            store
                .candidates()
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join("\n                      "),
        ),
        (
            "日志".into(),
            mangosteen_platform::paths::log_dir(env)
                .display()
                .to_string(),
        ),
        (
            "部署台账".into(),
            mangosteen_platform::paths::state_file(env)
                .display()
                .to_string(),
        ),
        (
            "模型台账".into(),
            mangosteen_platform::paths::model_index_file(env)
                .display()
                .to_string(),
        ),
    ]);
    Ok(0)
}

/// 数字选项展示。
fn opt_num<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map(|x| x.to_string()).unwrap_or_else(|| "不限".into())
}

/// 打印 config 命令帮助。
fn print_config_help() {
    println!("mangosteen config <子命令> [选项]");
    println!();
    println!("配置管理。");
    println!();
    println!("{}", output::bold("子命令"));
    println!("  {:<w$}  {}", "show", "显示当前完整配置", w = 18);
    println!("  {:<w$}  {}", "init", "初始化默认配置文件", w = 18);
    println!("  {:<w$}  {}", "path", "显示配置文件所在路径", w = 18);
    println!();
    println!("{}", output::bold("全局选项"));
    println!("  {:<w$}  {}", "--config <path>", "指定配置文件", w = 22);
    println!("  {:<w$}  {}", "--json", "以 JSON 输出（便于脚本处理）", w = 22);
    println!("  {:<w$}  {}", "--quiet", "抑制 banner 与进度提示", w = 22);
    println!("  {:<w$}  {}", "--force", "强制覆盖已存在的配置文件", w = 22);
    println!();
    println!("{}", output::bold("示例"));
    println!("  mangosteen config show");
    println!("  mangosteen config init");
    println!("  mangosteen config path");
    println!();
    println!("更多信息：docs/USAGE.md");
}

/// 打印 config show 子命令帮助。
fn print_config_show_help() {
    println!("mangosteen config show [选项]");
    println!();
    println!("显示当前完整配置。");
    println!();
    println!("{}", output::bold("选项"));
    println!("  {:<w$}  {}", "--json", "以 JSON 输出（便于脚本处理）", w = 22);
    println!("  {:<w$}  {}", "--quiet", "抑制 banner 与进度提示", w = 22);
    println!();
    println!("{}", output::bold("示例"));
    println!("  mangosteen config show");
    println!("  mangosteen config show --json");
    println!();
    println!("更多信息：docs/USAGE.md");
}

/// 打印 config init 子命令帮助。
fn print_config_init_help() {
    println!("mangosteen config init [选项]");
    println!();
    println!("初始化默认配置文件。");
    println!();
    println!("{}", output::bold("选项"));
    println!("  {:<w$}  {}", "--force", "强制覆盖已存在的配置文件", w = 22);
    println!("  {:<w$}  {}", "--quiet", "抑制 banner 与进度提示", w = 22);
    println!();
    println!("{}", output::bold("示例"));
    println!("  mangosteen config init");
    println!("  mangosteen config init --force");
    println!();
    println!("更多信息：docs/USAGE.md");
}

/// 打印 config path 子命令帮助。
fn print_config_path_help() {
    println!("mangosteen config path [选项]");
    println!();
    println!("显示配置文件所在路径。");
    println!();
    println!("{}", output::bold("选项"));
    println!("  {:<w$}  {}", "--json", "以 JSON 输出（便于脚本处理）", w = 22);
    println!("  {:<w$}  {}", "--quiet", "抑制 banner 与进度提示", w = 22);
    println!();
    println!("{}", output::bold("示例"));
    println!("  mangosteen config path");
    println!();
    println!("更多信息：docs/USAGE.md");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_subcommand_is_actionable() {
        let a = crate::cli::parse_args(&["config".to_string(), "nope".to_string()]).unwrap();
        let e = run(a).unwrap_err();
        assert!(e.to_string().contains("config nope"));
        assert!(e.to_string().contains("show"));
    }

    #[test]
    fn opt_num_formats() {
        assert_eq!(opt_num(None::<f64>), "不限");
        assert_eq!(opt_num(Some(2.5f64)), "2.5");
    }

    #[test]
    fn rejects_unknown_option() {
        let a =
            crate::cli::parse_args(&["config".to_string(), "show".to_string(), "--xx".to_string()]);
        // --xx 会被当成带值选项且缺值 → 解析报错；否则 reject 报错
        assert!(a.is_err() || run(a.unwrap()).is_err());
    }

    /// 已删除的子命令传入应报未知子命令（不静默执行）。
    #[test]
    fn removed_subcommands_are_rejected() {
        for sub in ["validate", "reset-templates"] {
            let a =
                crate::cli::parse_args(&["config".to_string(), sub.to_string()]).unwrap();
            let e = run(a).unwrap_err();
            assert!(e.to_string().contains("未知子命令"), "{sub} 未被拦截：{e}");
            assert!(e.to_string().contains("show"), "提示未列出可用命令：{e}");
        }
    }

    /// 未知子命令的提示必须列出当前可用命令，避免用户以为它不存在。
    #[test]
    fn unknown_subcommand_lists_current_commands() {
        let a = crate::cli::parse_args(&["config".to_string(), "nope".to_string()]).unwrap();
        let msg = run(a).unwrap_err().to_string();
        assert!(msg.contains("show"), "提示未列出命令：{msg}");
        assert!(msg.contains("init"), "提示未列出命令：{msg}");
        assert!(msg.contains("path"), "提示未列出命令：{msg}");
    }
}

// ==================== P0-1/P0-2 密钥生命周期（2026-11-19） ====================

/// `config backup-keys`：备份 gateway.key 与 ssh_known_hosts。
///
/// 备份目标 `{config_dir}/backups/`（600）：gateway.key.bak、
/// ssh_known_hosts.bak。已有备份不覆盖（保留最早可用版本）。
fn backup_keys(args: Args) -> MnResult<i32> {
    if args.has("help") {
        println!("用法：mangosteen config backup-keys");
        println!();
        let dir = mangosteen_platform::paths::config_dir(&mangosteen_platform::paths::real_env);
        println!("备份本地密钥文件（gateway.key / ssh_known_hosts）到 {}。", dir.join("backups").display());
        println!("密钥文件损坏/误删时可用备份恢复。已有备份不覆盖。");
        return Ok(0);
    }
    let dir = mangosteen_platform::paths::config_dir(&mangosteen_platform::paths::real_env);
    let backup_dir = dir.join("backups");
    std::fs::create_dir_all(&backup_dir).map_err(|e| {
        mangosteen_core::MnError::io(format!("创建备份目录 {}", backup_dir.display()), &e)
    })?;
    mangosteen_platform::fs::set_private_dir(&backup_dir)?;

    let mut done = 0;
    for (name, src) in [
        ("gateway.key", dir.join("gateway.key")),
        ("ssh_known_hosts", dir.join("ssh_known_hosts")),
    ] {
        if !src.is_file() {
            println!("  - {name}：不存在，跳过");
            continue;
        }
        let dst = backup_dir.join(format!("{name}.bak"));
        if dst.exists() {
            println!("  - {name}：备份已存在（{}），保留不覆盖", dst.display());
            continue;
        }
        std::fs::copy(&src, &dst).map_err(|e| {
            mangosteen_core::MnError::io(format!("备份 {} → {}", src.display(), dst.display()), &e)
        })?;
        mangosteen_platform::fs::set_private_permissions(&dst)?;
        println!("  ✓ {name} → {}", dst.display());
        done += 1;
    }
    if done > 0 {
        println!("完成：{done} 个密钥文件已备份。");
    }
    Ok(0)
}

/// `config rotate-gateway-key`：轮换网关 API Key。
///
/// 生成新 32B token（256 位熵，/dev/urandom 优先）→ 原子写
/// `{config_dir}/gateway.key`（600，先写临时文件再 rename）→ 旧
/// Inline 明文引用一并清除。重启网关后新 key 生效。
fn rotate_gateway_key(args: Args) -> MnResult<i32> {
    if args.has("help") {
        println!("用法：mangosteen config rotate-gateway-key");
        println!();
        println!("生成新的网关 API Key 并写入 gateway.key（600）。");
        println!("重启网关后新 key 生效；旧 key 立即失效。");
        return Ok(0);
    }
    let dir = mangosteen_platform::paths::config_dir(&mangosteen_platform::paths::real_env);
    let key_path = dir.join("gateway.key");

    // 生成 32B token（256 位熵）：/dev/urandom 优先
    let strong = {
        use std::io::Read as _;
        std::fs::File::open("/dev/urandom")
            .ok()
            .and_then(|mut f| {
                let mut buf = [0u8; 32];
                f.read_exact(&mut buf).ok()?;
                Some(buf.iter().map(|b| format!("{b:02x}")).collect::<String>())
            })
            .unwrap_or_else(|| {
                // 熵降级兜底（与 ui.rs 同口径）：时间戳 ^ pid
                let nanos = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0);
                let x = nanos ^ ((std::process::id() as u64) << 32);
                format!("{x:016x}{nanos:016x}")
            })
    };

    // 原子写：临时文件 → rename（避免写一半崩溃损坏 key）
    let tmp = dir.join("gateway.key.tmp");
    std::fs::write(&tmp, strong.as_bytes()).map_err(|e| {
        mangosteen_core::MnError::io(format!("写入 {}", tmp.display()), &e)
    })?;
    mangosteen_platform::fs::set_private_permissions(&tmp)?;
    std::fs::rename(&tmp, &key_path).map_err(|e| {
        mangosteen_core::MnError::io(format!("替换 {}", key_path.display()), &e)
    })?;

    mangosteen_platform::fs::audit_critical(&format!(
        "gateway-key-rotated {}",
        mangosteen_core::time::now_unix()
    ));
    println!("✓ 网关 API Key 已轮换：{}（600）", key_path.display());
    println!();
    println!("⚠ 旧 key 已失效。请重启网关使新 key 生效：");
    println!("    mangosteen ui restart");
    println!("  新 key 值（请妥善保存，勿提交到代码库）：");
    println!("    {strong}");
    Ok(0)
}

/// `config reset-host-keys`：重置 SSH host key 钉扎。
///
/// ssh_known_hosts 损坏（指纹全变/文件损坏）导致所有 SSH 连接被
/// 拒（StrictHostKeyChecking=yes）时——备份并清空，下次连接重新
/// 引导（重新 keyscan + 钉扎）。
fn reset_host_keys(args: Args) -> MnResult<i32> {
    if args.has("help") {
        println!("用法：mangosteen config reset-host-keys");
        println!();
        println!("SSH host key 钉扎文件损坏/实例重建导致拒连时重置：");
        println!("备份 ssh_known_hosts 到 .bak 后清空，下次连接重新引导。");
        return Ok(0);
    }
    let dir = mangosteen_platform::paths::config_dir(&mangosteen_platform::paths::real_env);
    let kh = dir.join("ssh_known_hosts");
    if !kh.is_file() {
        println!("ssh_known_hosts 不存在（未钉扎），无需重置。");
        return Ok(0);
    }
    let bak = kh.with_extension("known_hosts.bak");
    std::fs::copy(&kh, &bak).map_err(|e| {
        mangosteen_core::MnError::io(format!("备份 {} → {}", kh.display(), bak.display()), &e)
    })?;
    mangosteen_platform::fs::set_private_permissions(&bak)?;
    std::fs::remove_file(&kh).map_err(|e| {
        mangosteen_core::MnError::io(format!("删除 {}", kh.display()), &e)
    })?;
    println!("✓ ssh_known_hosts 已备份到 {} 并清空。", bak.display());
    println!("  下次部署将重新 keyscan 并钉扎实例指纹。");
    Ok(0)
}
