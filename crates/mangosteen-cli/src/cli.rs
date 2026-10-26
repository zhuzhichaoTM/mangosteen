//! 命令行解析与分发。
//!
//! 归属：**用户交互层**。
//!
//! ## 为什么手写解析器
//!
//! 按「禁止无依据扩展」原则，未引入 clap。本 CLI 的子命令与参数固定且有限，
//! 手写解析器约 400 行，带来两个额外好处：
//!
//! 1. **错误消息完全可控**：可以直接给出中文提示与下一步建议；
//! 2. **零编译期开销**：不引入 proc-macro 依赖链。
//!
//! ## 解析规则
//!
//! * `--key value` 与 `--key=value` 等价；
//! * 布尔开关只接受 `--flag`（不带值）；
//! * `-h` / `--help` 在每个层级都可用；
//! * 未知参数 → 明确报错并提示最接近的命令（不做静默忽略）。

use std::collections::HashMap;

use mangosteen_core::{MnError, MnResult};

use crate::output;

/// 解析后的参数集合。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Args {
    /// 位置参数
    pub positionals: Vec<String>,
    /// 选项（`--key value` 或 `--key=value`）
    pub options: HashMap<String, String>,
    /// 布尔开关
    pub flags: Vec<String>,
}

impl Args {
    /// 取选项。
    pub fn get(&self, key: &str) -> Option<&str> {
        self.options.get(key).map(|s| s.as_str())
    }

    /// 取选项并解析为数字。
    pub fn get_num<T: std::str::FromStr>(&self, key: &str) -> MnResult<Option<T>> {
        match self.options.get(key) {
            None => Ok(None),
            Some(v) => v
                .parse::<T>()
                .map(Some)
                .map_err(|_| MnError::bad_request(format!("选项 --{key} 的值 `{v}` 不是合法数字"))),
        }
    }

    /// 是否有某个开关。
    pub fn has(&self, flag: &str) -> bool {
        self.flags.iter().any(|f| f == flag)
    }

    /// 第 n 个位置参数。
    pub fn pos(&self, n: usize) -> Option<&str> {
        self.positionals.get(n).map(|s| s.as_str())
    }

    /// 要求第 n 个位置参数存在。
    pub fn require_pos(&self, n: usize, what: &str) -> MnResult<&str> {
        self.pos(n)
            .ok_or_else(|| MnError::bad_request(format!("缺少参数：{what}")))
    }
}

/// 已知的布尔开关名（其余 `--xxx` 一律视为带值选项）。
const BOOLEAN_FLAGS: &[&str] = &[
    "help",
    "version",
    "json",
    "stream",
    "all",
    "local-only",
    "no-cpu-fallback",
    "require-full-gpu",
    "force",
    "yes",
    "verbose",
    "quiet",
    "explain",
    "reset",
    "delete-file",
    "prune",
    "no-open",
    "no-verify",
    // `cloud cheapest --create`：开关语义，不能当成带值选项。
    // 不加进这里的话 `--create --yes` 会把 `--yes` 吃成 create 的值，
    // 表现是「明明加了 --create 却只查询不创建」，且不报错——很难查。
    "create",
];

/// 解析参数列表（不含程序名）。
pub fn parse_args(argv: &[String]) -> MnResult<Args> {
    let mut out = Args::default();
    let mut i = 0;
    while i < argv.len() {
        let a = &argv[i];
        if let Some(rest) = a.strip_prefix("--") {
            if rest.is_empty() {
                return Err(MnError::bad_request("发现空的 `--` 参数"));
            }
            // --key=value
            if let Some((k, v)) = rest.split_once('=') {
                out.options.insert(k.to_string(), v.to_string());
                i += 1;
                continue;
            }
            // 短别名归一
            let key = normalize_flag(rest);
            if BOOLEAN_FLAGS.contains(&key.as_str()) {
                out.flags.push(key);
                i += 1;
                continue;
            }
            // 带值选项
            if i + 1 >= argv.len() {
                return Err(MnError::bad_request(format!(
                    "选项 --{rest} 缺少值。用法：--{rest} <值>"
                )));
            }
            out.options.insert(key, argv[i + 1].clone());
            i += 2;
            continue;
        }
        if a == "-h" {
            out.flags.push("help".to_string());
            i += 1;
            continue;
        }
        if a == "-V" {
            out.flags.push("version".to_string());
            i += 1;
            continue;
        }
        if a.starts_with('-') && a.len() > 1 {
            return Err(MnError::bad_request(format!(
                "未知短选项 `{a}`。使用 --help 查看可用选项"
            )));
        }
        out.positionals.push(a.clone());
        i += 1;
    }
    Ok(out)
}

/// 短名/别名归一。
fn normalize_flag(s: &str) -> String {
    match s {
        "h" => "help".to_string(),
        "V" => "version".to_string(),
        "ctx" | "context" => "context".to_string(),
        other => other.to_string(),
    }
}

/// 顶层命令表。
fn commands() -> &'static [(&'static str, &'static str)] {
    &[
        ("deploy", "部署模型（本地优先，资源不足自动降级到云端）"),
        ("run", "部署并保持前台运行（Ctrl-C 优雅退出）"),
        ("stop", "停止部署并释放资源"),
        ("status", "查看网关与部署状态"),
        ("ps", "列出全部部署"),
        (
            "models",
            "模型管理：pull / search / add / list / show / verify / remove / sources",
        ),
        ("assess", "仅评估本地可行性，不部署"),
        (
            "cloud",
            "云端算力：providers / specs / pick / instances / destroy",
        ),
        ("config", "配置管理：show / init / path / validate"),
        ("serve", "仅启动 API 网关 HTTP 服务"),
        ("tui", "启动全屏交互界面"),
        ("doctor", "环境自检（引擎、硬件、配置、权限）"),
        ("engine", "本地推理引擎诊断（locate / env / probe）"),
        ("version", "显示版本信息"),
        ("help", "显示帮助"),
    ]
}

/// 入口。
pub fn run(argv: Vec<String>) -> MnResult<i32> {
    // argv[0] 是程序名
    let rest: Vec<String> = argv.into_iter().skip(1).collect();

    let args = parse_args(&rest)?;

    if args.has("version") || args.pos(0) == Some("version") {
        print_version();
        return Ok(0);
    }
    if args.has("help") || args.positionals.is_empty() {
        print_help();
        return Ok(0);
    }

    // 注意：不移除命令名。所有子命令统一约定「positionals[0] = 命令名，
    // 其后的位置参数从索引 1 开始」，这样每个子命令能自行识别 `models add`、
    // `cloud pick` 这类两级子命令，无需在分发层做特殊处理。
    let cmd = args.pos(0).unwrap_or("").to_string();
    if args.has("quiet") {
        // quiet 模式抑制 banner
    } else if cmd != "help" {
        output::print_banner();
    }

    match cmd.as_str() {
        "deploy" => crate::commands::deploy::run(args),
        "run" => crate::commands::deploy::run_foreground(args),
        "stop" => crate::commands::stop::run(args),
        "status" => crate::commands::status::run(args),
        "ps" | "list" => crate::commands::status::run_ps(args),
        "models" => crate::commands::models::run(args),
        "assess" => crate::commands::assess::run(args),
        "cloud" => crate::commands::cloud::run(args),
        "config" => crate::commands::config::run(args),
        "serve" => crate::commands::serve::run(args),
        "ui" => crate::commands::ui::run(args),
        "tui" => crate::tui::run(args),
        "doctor" => crate::commands::doctor::run(args),
        "engine" => crate::commands::engine::run(args),
        "help" => {
            print_help();
            Ok(0)
        }
        other => Err(MnError::bad_request(format!(
            "未知命令 `{other}`。可用命令：{}。执行 `mangosteen help` 查看详情",
            commands()
                .iter()
                .map(|(n, _)| *n)
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// 打印版本。
fn print_version() {
    println!(
        "{} {} ({} / {})",
        mangosteen_core::identity::PRODUCT_NAME,
        mangosteen_core::identity::VERSION,
        mangosteen_platform::os_name(),
        mangosteen_platform::arch_name()
    );
}

/// 打印帮助。
fn print_help() {
    println!(
        "{} {} —— 个人级大模型部署客户端",
        mangosteen_core::identity::PRODUCT_NAME,
        mangosteen_core::identity::VERSION
    );
    println!();
    println!("{}", output::bold("用法"));
    println!("  mangosteen <命令> [选项]");
    println!();
    println!("{}", output::bold("命令"));
    let width = commands().iter().map(|(n, _)| n.len()).max().unwrap_or(0);
    for (name, desc) in commands() {
        println!(
            "  {:<w$}  {}",
            output::paint(name, output::Color::Cyan),
            desc,
            w = width
        );
    }
    println!();
    println!("{}", output::bold("常用示例"));
    println!("  mangosteen models pull <owner>/<仓库>/<文件.gguf>  # 下载模型并登记");
    println!("  mangosteen models add ./qwen2.5-7b-q4_k_m.gguf   # 登记已有文件");
    println!("  mangosteen assess qwen2.5-7b --context 8192     # 评估能否本地跑");
    println!("  mangosteen deploy qwen2.5-7b --context 8192     # 部署（本地优先）");
    println!("  mangosteen deploy llama3.3-70b --allow-cloud    # 本地不足自动上云");
    println!("  mangosteen status                               # 看状态");
    println!("  mangosteen stop --all                           # 全部停止");
    println!();
    println!("{}", output::bold("全局选项"));
    println!("  --config <path>   指定配置文件");
    println!("  --json            以 JSON 输出（便于脚本处理）");
    println!("  --quiet           抑制 banner 与进度提示");
    println!("  --help, -h        显示帮助");
    println!("  --version, -V     显示版本");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(items: &[&str]) -> Args {
        parse_args(&items.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn parses_positionals_and_options() {
        let args = a(&["deploy", "qwen2.5-7b", "--context", "8192", "--json"]);
        assert_eq!(args.positionals, vec!["deploy", "qwen2.5-7b"]);
        assert_eq!(args.get("context"), Some("8192"));
        assert!(args.has("json"));
    }

    #[test]
    fn equals_syntax_works() {
        let args = a(&["deploy", "m", "--context=4096"]);
        assert_eq!(args.get("context"), Some("4096"));
    }

    #[test]
    fn short_flags_normalize() {
        let args = a(&["-h"]);
        assert!(args.has("help"));
        let args = a(&["-V"]);
        assert!(args.has("version"));
    }

    #[test]
    fn ctx_alias_maps_to_context() {
        let args = a(&["deploy", "m", "--ctx", "2048"]);
        assert_eq!(args.get("context"), Some("2048"));
    }

    #[test]
    fn missing_option_value_errors() {
        let e = parse_args(&["deploy".to_string(), "--context".to_string()]).unwrap_err();
        assert!(e.to_string().contains("缺少值"), "got {e}");
    }

    #[test]
    fn unknown_short_flag_errors() {
        let e = parse_args(&["-z".to_string()]).unwrap_err();
        assert!(e.to_string().contains("未知短选项"));
    }

    #[test]
    fn get_num_validates() {
        let args = a(&["assess", "m", "--context", "8192"]);
        let n: Option<u32> = args.get_num("context").unwrap();
        assert_eq!(n, Some(8192));
        let none: Option<u32> = args.get_num("missing").unwrap();
        assert_eq!(none, None);

        let bad = a(&["assess", "m", "--context", "abc"]);
        assert!(bad.get_num::<u32>("context").is_err());
    }

    #[test]
    fn require_pos_gives_clear_error() {
        let args = a(&["deploy"]);
        let e = args.require_pos(1, "<model>").unwrap_err();
        assert!(e.to_string().contains("<model>"));
    }

    #[test]
    fn boolean_flags_do_not_consume_next_arg() {
        let args = a(&["deploy", "m", "--local-only", "--context", "4096"]);
        assert!(args.has("local-only"));
        assert_eq!(args.get("context"), Some("4096"));
        assert_eq!(args.positionals.len(), 2);
    }

    #[test]
    fn double_dash_alone_errors() {
        let e = parse_args(&["--".to_string()]).unwrap_err();
        assert!(e.to_string().contains("空"));
    }

    #[test]
    fn help_and_version_exit_zero() {
        assert_eq!(run(vec!["mangosteen".into()]).unwrap(), 0);
        assert_eq!(run(vec!["mangosteen".into(), "--help".into()]).unwrap(), 0);
        assert_eq!(run(vec!["mangosteen".into(), "-V".into()]).unwrap(), 0);
        assert_eq!(run(vec!["mangosteen".into(), "version".into()]).unwrap(), 0);
    }

    #[test]
    fn unknown_command_is_actionable() {
        let e = run(vec!["mangosteen".into(), "frobnicate".into()]).unwrap_err();
        assert!(e.to_string().contains("未知命令"));
        assert!(e.to_string().contains("deploy"), "应列出可用命令：{e}");
    }

    #[test]
    fn command_table_covers_help_listing() {
        let names: Vec<&str> = commands().iter().map(|(n, _)| *n).collect();
        for expected in [
            "deploy", "stop", "status", "models", "cloud", "config", "doctor", "tui",
        ] {
            assert!(names.contains(&expected), "缺少命令 {expected}");
        }
    }
}
