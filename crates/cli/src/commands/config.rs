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
///
/// `yes` 供 `config reset-templates` 确认覆盖内置模板槽位；
/// 覆盖用户配置属于需要二次确认的操作，不能不给确认开关就执行。
const ALLOWED: &[&str] = &["config", "json", "force", "quiet", "yes"];

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
        "validate" => {
            if args.has("help") {
                print_config_validate_help();
                return Ok(0);
            }
            validate(args)
        }
        "reset-templates" => {
            if args.has("help") {
                print_config_reset_templates_help();
                return Ok(0);
            }
            reset_templates(args)
        }
        other => Err(mangosteen_core::MnError::bad_request(format!(
            "未知子命令 `config {other}`。可用：show / init / path / validate / reset-templates"
        ))),
    }
}

/// `config reset-templates`
///
/// ## 为什么需要这条命令
///
/// 内置模型配置模板随版本升级而改进（例如修正量化精度、去掉无法加载的
/// 外置草稿模型）。但落盘到用户配置里的模板**优先于**内置版本，而网关
/// 的补种逻辑（`seed_builtin_templates`）只在「槽位缺失」时插入——
/// 已存在的槽位永不更新。
///
/// 结果是：升级后用户仍在跑旧模板，且毫无提示。这个设计对「保护用户
/// 自定义」是对的，但让「内置模板的修正」永远无法送达。
///
/// 因此提供显式的重置入口：把配置里的内置槽位改写为当前内置版本。
/// **只处理内置槽位**，用户自建的模板名（如 `uncensored-custom`）原样保留。
fn reset_templates(args: Args) -> MnResult<i32> {
    let store = mangosteen_base::ConfigStore::platform_default(
        args.get("config").map(std::path::PathBuf::from),
    );
    let (mut cfg, _source) = load_config(&args)?;

    // 汇总将要发生的变更，先展示后落盘：这是覆盖用户配置的操作，
    // 不能让用户「执行完才发现改了哪些」。
    let mut planned: Vec<(String, String)> = Vec::new();
    for (model, slot, display, text) in mangosteen_core::template::BUILTIN_TEMPLATES {
        let entry = cfg
            .model_whitelist
            .entry((*model).to_string())
            .or_insert_with(|| mangosteen_core::config::WhitelistedModel {
                display_name: (*display).to_string(),
                source_spec: String::new(),
                template: (*slot).to_string(),
                templates: Default::default(),
            });
        let current = entry.templates.get(*slot);
        if current.map(|c| c.as_str()) != Some(*text) {
            entry.templates.insert((*slot).to_string(), (*text).to_string());
            planned.push(((*model).to_string(), (*slot).to_string()));
        }
    }

    if planned.is_empty() {
        output::print_success("内置模板已是最新版本，无需重置");
        return Ok(0);
    }

    println!("{}", output::bold("将重置以下内置模板槽位："));
    for (model, slot) in &planned {
        println!("  · {model} / {slot}");
    }
    println!();
    println!(
        "  {}",
        output::dim("自定义模板（不叫这些名字的槽位）不会被动")
    );
    println!();

    if !args.has("yes") {
        println!(
            "  确认执行请加 {}",
            output::bold("--yes")
        );
        return Ok(0);
    }

    cfg.validate()?;
    let written = store.save(&cfg)?;
    output::print_success(&format!(
        "已重置 {} 个内置模板槽位 → {}",
        planned.len(),
        written.display()
    ));
    println!(
        "  {}",
        output::dim("若网关正在运行，重启后生效：mangosteen ui restart")
    );
    Ok(0)
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
    println!("{}", output::bold("节点策略"));
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
        (
            "推理引擎".into(),
            cfg.local
                .engine_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| output::dim("自动在 PATH 中查找").to_string()),
        ),
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
    println!("  {:<w$}  {}", "validate", "验证配置项合法性", w = 18);
    println!("  {:<w$}  {}", "reset-templates", "重置内置部署模板为默认值", w = 18);
    println!();
    println!("{}", output::bold("全局选项"));
    println!("  {:<w$}  {}", "--config <path>", "指定配置文件", w = 22);
    println!("  {:<w$}  {}", "--json", "以 JSON 输出（便于脚本处理）", w = 22);
    println!("  {:<w$}  {}", "--quiet", "抑制 banner 与进度提示", w = 22);
    println!("  {:<w$}  {}", "--force", "强制覆盖已存在的配置文件", w = 22);
    println!("  {:<w$}  {}", "--yes", "免确认直接执行（与 reset-templates 配合）", w = 22);
    println!();
    println!("{}", output::bold("示例"));
    println!("  mangosteen config show");
    println!("  mangosteen config path");
    println!("  mangosteen config validate");
    println!("  mangosteen config reset-templates --yes");
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

/// 打印 config validate 子命令帮助。
fn print_config_validate_help() {
    println!("mangosteen config validate [选项]");
    println!();
    println!("验证配置项合法性（如 image_id、region、zone 等）。");
    println!();
    println!("{}", output::bold("选项"));
    println!("  {:<w$}  {}", "--json", "以 JSON 输出（便于脚本处理）", w = 22);
    println!("  {:<w$}  {}", "--quiet", "抑制 banner 与进度提示", w = 22);
    println!();
    println!("{}", output::bold("示例"));
    println!("  mangosteen config validate");
    println!();
    println!("更多信息：docs/USAGE.md");
}

/// 打印 config reset-templates 子命令帮助。
fn print_config_reset_templates_help() {
    println!("mangosteen config reset-templates [选项]");
    println!();
    println!("重置内置部署模板为默认值。");
    println!("只处理内置槽位，用户自建的模板名原样保留。");
    println!();
    println!("{}", output::bold("选项"));
    println!("  {:<w$}  {}", "--yes", "免确认直接执行", w = 22);
    println!("  {:<w$}  {}", "--quiet", "抑制 banner 与进度提示", w = 22);
    println!();
    println!("{}", output::bold("示例"));
    println!("  mangosteen config reset-templates --yes");
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

    /// 重置命令必须被 `ALLOWED` 放行，否则 `config reset-templates` 会
    /// 因为选项校验直接失败——这条测试守的是「新命令可用」。
    #[test]
    fn reset_templates_option_is_allowed() {
        let a = crate::cli::parse_args(&[
            "config".to_string(),
            "reset-templates".to_string(),
            "--yes".to_string(),
        ])
        .expect("--yes 应被解析为布尔开关");
        assert!(a.has("yes"), "--yes 未进入 flags：{a:?}");
        // reject_unknown_options 不应报错（真正执行留给集成环境）
        assert!(reject_unknown_options(&a, ALLOWED).is_ok());
    }

    /// 未知子命令的提示必须列出新命令，避免用户以为它不存在。
    #[test]
    fn unknown_subcommand_lists_reset_templates() {
        let a = crate::cli::parse_args(&["config".to_string(), "nope".to_string()]).unwrap();
        let msg = run(a).unwrap_err().to_string();
        assert!(msg.contains("reset-templates"), "提示未列出新命令：{msg}");
    }
}
