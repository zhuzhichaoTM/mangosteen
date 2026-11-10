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
    "foreground",
    "no-verify",
    // `cloud cheapest --create`：开关语义，不能当成带值选项。
    // 不加进这里的话 `--create --yes` 会把 `--yes` 吃成 create 的值，
    // 表现是「明明加了 --create 却只查询不创建」，且不报错——很难查。
    "create",
    // 节点选择相关的布尔开关
    "interactive",
    "running",
    "stopped",
    // deploy 异步/轮询开关
    "watch",
    "async",
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
        ("deploy", "部署模型到云端（租赁 GPU 算力）"),
        ("stop", "停止部署并释放云端资源"),
        ("status", "查看网关与部署状态"),
        (
            "cloud",
            "云端算力：providers / specs / pick / instances / destroy / key",
        ),
        ("device", "自定义节点：list / add / remove / test"),
        (
            "config",
            "配置管理：show / init / path / validate / reset-templates",
        ),
        ("ui", "启动 Web UI 控制台"),
        ("doctor", "环境自检（云服务商、配置、权限）"),
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
        "stop" => crate::commands::stop::run(args),
        "status" => crate::commands::status::run(args),
        "cloud" => crate::commands::cloud::run(args),
        "device" => crate::commands::device::run(args),
        "config" => crate::commands::config::run(args),
        "ui" => {
            if args.pos(1).is_some() {
                crate::commands::ui::run_lifecycle(args)
            } else {
                crate::commands::ui::run(args)
            }
        }
        "doctor" => crate::commands::doctor::run(args),
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
        "Mangosteen—— 云端租赁部署大模型部署客户端 v{}",
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
    println!("  mangosteen deploy qwen2.5-7b --gpu-model 3080TI --watch    # 部署到云端，轮询进度");
    println!("  mangosteen deploy qwen2.5-7b --device dev-001               # 指定自定义节点部署");
    println!("  mangosteen deploy qwen2.5-7b --template default               # 指定配置模板");
    println!("  mangosteen status                                             # 看部署状态");
    println!("  mangosteen stop <部署ID> | --all --yes                        # 停止部署并释放资源");
    println!("  mangosteen cloud instances --running                          # 查看运行中云端实例");
    println!("  mangosteen device list                                        # 查看自定义节点");
    println!("  mangosteen ui start                                           # 后台启动 UI/API 网关");
    println!("  mangosteen cloud key compshare --password <AK/SK>             # 配置优云智算密钥");
    println!("  mangosteen doctor                                             # 环境自检");
    println!();
    println!("{}", output::bold("各命令用法"));
    println!("  deploy <模型> [--gpu-model <型号>] [--device <节点ID>] [--template <槽位>] [--watch] [--async]");
    println!("    部署模型到云端。--gpu-model 指定 GPU 型号（如 3080TI、P40）；");
    println!("    --device 指定自定义节点；--template 选择配置模板；--watch 轮询进度；--async 异步返回");
    println!();
    println!("  stop <部署ID> | --all [--yes]");
    println!("    停止指定部署并释放资源。--all 停止全部部署；--yes 免确认直接执行");
    println!();
    println!("  status [--json]");
    println!("    查看网关与部署状态。--json 以 JSON 输出");
    println!();
    println!("  cloud providers                          # 列出已配置/已加载的云服务商");
    println!("  cloud specs [--min-vram 24] [--max-price 3]  # 查询可用规格与报价");
    println!("  cloud pick [--min-vram 24] [--explain]       # 按约束打分排序推荐节点");
    println!("  cloud instances [--running|--stopped]        # 列出云端实例（含已关机）");
    println!("  cloud destroy <部署ID> | --instance <厂商>/<ID> --yes  # 销毁云端实例");
    println!("  cloud key <厂商> [--password <值> | --env <ENV>] [--access-key <值>] [--secret-key <值>] [--set <k=v>] [--unset <key>] [--enable|--disable]  # 管理云厂商密钥（仅保留 access_key_id/access_key_secret）");
    println!();
    println!("  device list                                   # 列出自定义节点");
    println!("  device add <主机> <端口> --user <用户> [--workdir <目录>] [--password <密码>] [--name <名称>]  # 新增自定义节点");
    println!("  device remove <ID>                            # 删除自定义节点");
    println!("  device test <ID>                              # 测试节点 SSH 连通性");
    println!();
    println!("  config show | init | path | validate | reset-templates  # 配置管理");
    println!("  ui [start|stop|restart|status] [--port <端口>] [--no-open]   # Web UI 控制台生命周期");
    println!("  doctor                                        # 环境自检（云服务商、配置、权限）");
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
        let args = a(&["deploy", "m", "--context", "8192"]);
        let n: Option<u32> = args.get_num("context").unwrap();
        assert_eq!(n, Some(8192));
        let none: Option<u32> = args.get_num("missing").unwrap();
        assert_eq!(none, None);

        let bad = a(&["deploy", "m", "--context", "abc"]);
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
            "deploy", "stop", "status", "cloud", "config", "doctor",
        ] {
            assert!(names.contains(&expected), "缺少命令 {expected}");
        }
    }
}
