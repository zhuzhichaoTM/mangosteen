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
    reject_unknown_options(&args, ALLOWED)?;
    let sub = args.pos(1).unwrap_or("show").to_string();
    match sub.as_str() {
        "show" => show(args),
        "init" => init(args),
        "path" => path(args),
        "validate" => validate(args),
        other => Err(mangosteen_core::MnError::bad_request(format!(
            "未知子命令 `config {other}`。可用：show / init / path / validate"
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
                "local_first": cfg.scheduler.local_first,
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
                "models_dir": cfg.storage.models_dir.display().to_string(),
                "cache_dir": cfg.storage.cache_dir.display().to_string(),
                "verify_hash": cfg.storage.verify_hash,
                "max_model_bytes": cfg.storage.max_model_bytes,
                "allowed_read_roots": cfg.storage.allowed_read_roots
                    .iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
            },
            "providers": providers,
        });
        println!("{}", serde_json::to_string_pretty(&payload).unwrap_or_default());
        return Ok(0);
    }

    println!("{}", output::bold("配置"));
    println!("  {}", output::dim(&source));
    println!();

    println!("{}", output::bold("网关"));
    output::print_kv(&[
        ("监听".into(), format!("{}:{}", cfg.gateway.host, cfg.gateway.port)),
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
    println!("{}", output::bold("调度策略"));
    output::print_kv(&[
        (
            "本地优先".into(),
            if cfg.scheduler.local_first {
                output::paint("是", output::Color::Green)
            } else {
                output::paint("否（违反产品基线，建议改回）", output::Color::Red)
            },
        ),
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
        ("降级重试上限".into(), cfg.scheduler.max_fallbacks.to_string()),
        ("价格上限".into(), opt_num(cfg.scheduler.max_price_per_hour)),
    ]);

    println!();
    println!("{}", output::bold("本地部署"));
    output::print_kv(&[
        (
            "推理引擎".into(),
            cfg.local
                .engine_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| output::dim("自动在 PATH 中查找").to_string()),
        ),
        ("默认上下文".into(), cfg.local.default_context.to_string()),
        ("KV 精度".into(), cfg.local.kv_cache.as_cli_value().to_string()),
        ("批大小".into(), cfg.local.batch_size.to_string()),
        (
            "CPU 回退".into(),
            if cfg.local.allow_cpu_fallback { "允许" } else { "禁止" }.to_string(),
        ),
        (
            "部分卸载".into(),
            if cfg.local.allow_partial_offload { "允许" } else { "禁止" }.to_string(),
        ),
        (
            "端口区间".into(),
            format!("{}-{}", cfg.local.port_range_start, cfg.local.port_range_end),
        ),
    ]);

    println!();
    println!("{}", output::bold("存储与访问控制"));
    output::print_kv(&[
        ("模型库".into(), cfg.storage.models_dir.display().to_string()),
        ("缓存".into(), cfg.storage.cache_dir.display().to_string()),
        (
            "哈希校验".into(),
            if cfg.storage.verify_hash { "开启" } else { "关闭" }.to_string(),
        ),
        (
            "单文件上限".into(),
            mangosteen_platform::fs::human_bytes(cfg.storage.max_model_bytes),
        ),
        (
            "额外授权目录".into(),
            if cfg.storage.allowed_read_roots.is_empty() {
                output::dim("（无）").to_string()
            } else {
                cfg.storage
                    .allowed_read_roots
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            },
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

    let env = &mangosteen_platform::paths::real_env as &dyn Fn(&str) -> Option<String>;
    let cfg = mangosteen_core::config::Config::sample(
        mangosteen_platform::paths::models_dir(env),
        mangosteen_platform::paths::cache_dir(env),
    );
    cfg.validate()?;
    let written = store.save(&cfg)?;

    output::print_success(&format!("已生成配置文件：{}", written.display()));
    println!();
    println!("  {}", output::dim("接下来："));
    println!("    1. 编辑该文件，填写云服务商凭证（推荐用 env 形式）");
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
            "模型库".into(),
            mangosteen_platform::paths::models_dir(env).display().to_string(),
        ),
        (
            "下载缓存".into(),
            mangosteen_platform::paths::cache_dir(env).display().to_string(),
        ),
        (
            "日志".into(),
            mangosteen_platform::paths::log_dir(env).display().to_string(),
        ),
        (
            "部署台账".into(),
            mangosteen_platform::paths::state_file(env).display().to_string(),
        ),
        (
            "模型台账".into(),
            mangosteen_platform::paths::model_index_file(env).display().to_string(),
        ),
    ]);
    Ok(0)
}

/// `config validate`
fn validate(args: Args) -> MnResult<i32> {
    let (cfg, source) = load_config(&args)?;
    cfg.validate()?;
    output::print_success(&format!("配置合法（{source}）"));

    // 额外做云端能力诊断
    let notes = mangosteen_gateway::diagnose_cloud(&cfg);
    if !notes.is_empty() {
        println!();
        for n in notes {
            output::print_warning(&n);
        }
    }
    Ok(0)
}

/// 数字选项展示。
fn opt_num<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map(|x| x.to_string()).unwrap_or_else(|| "不限".into())
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
}