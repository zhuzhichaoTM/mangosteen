//! `mangosteen doctor`
//!
//! 归属：**用户交互层 → 命令 `doctor`**。
//!
//! 环境自检。逐项检查并给出**可操作**的结论。这是「不要只报现象」原则
//! 在诊断场景的体现：每条 FAIL 都附修复建议。
//!
//! 检查项：
//!
//! | 项 | 说明 |
//! |----|------|
//! | 硬件 | CPU / 内存 / 加速卡采集结果 |
//! | 推理引擎 | llama-server 是否就位、版本 |
//! | 配置 | 能否加载、是否合法、密钥是否齐全 |
//! | 云服务商 | 驱动加载情况 |
//! | 目录权限 | 配置/模型/日志目录可写 |
//! | 端口 | 网关端口是否可绑定 |
//! | 磁盘 | 模型库所在分区剩余空间 |

use mangosteen_core::MnResult;

use crate::cli::Args;
use crate::commands::{load_config, reject_unknown_options};
use crate::output;

/// 允许的选项。
const ALLOWED: &[&str] = &["config", "json", "quiet"];

/// 单项检查结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    /// 检查项名
    pub name: String,
    /// 结论
    pub status: Status,
    /// 说明
    pub detail: String,
    /// 修复建议
    pub hint: Option<String>,
}

/// 检查结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// 正常
    Ok,
    /// 可继续但有影响
    Warn,
    /// 阻塞性问题
    Fail,
}

impl Status {
    /// 展示标签。
    pub fn label(self) -> &'static str {
        match self {
            Status::Ok => "OK",
            Status::Warn => "WARN",
            Status::Fail => "FAIL",
        }
    }

    /// 对应颜色。
    fn color(self) -> output::Color {
        match self {
            Status::Ok => output::Color::Green,
            Status::Warn => output::Color::Yellow,
            Status::Fail => output::Color::Red,
        }
    }
}

/// 入口。
pub fn run(args: Args) -> MnResult<i32> {
    reject_unknown_options(&args, ALLOWED)?;
    let checks = run_checks(&args);

    let fails = checks.iter().filter(|c| c.status == Status::Fail).count();
    let warns = checks.iter().filter(|c| c.status == Status::Warn).count();

    if args.has("json") {
        let arr: Vec<serde_json::Value> = checks
            .iter()
            .map(|c| {
                serde_json::json!({
                    "name": c.name,
                    "status": c.status.label(),
                    "detail": c.detail,
                    "hint": c.hint,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "checks": arr,
                "fail": fails,
                "warn": warns,
            }))
            .unwrap_or_default()
        );
        return Ok(if fails > 0 { 1 } else { 0 });
    }

    println!("{}", output::bold("环境自检"));
    println!();
    for c in &checks {
        let tag = output::paint(&format!("{:<4}", c.status.label()), c.status.color());
        println!("  [{}] {}", tag, output::bold(&c.name));
        println!("         {}", c.detail);
        if let Some(h) = &c.hint {
            println!(
                "         {} {}",
                output::dim("→"),
                output::paint(h, output::Color::Cyan)
            );
        }
    }

    println!();
    if fails == 0 && warns == 0 {
        output::print_success("全部检查通过，可以开始部署");
    } else if fails == 0 {
        output::print_warning(&format!("{warns} 项警告，不影响本地部署"));
    } else {
        output::print_warning(&format!("{fails} 项失败、{warns} 项警告"));
        println!(
            "  {}",
            output::dim("按上面的建议逐项处理后重跑 `mangosteen doctor`")
        );
    }

    Ok(if fails > 0 { 1 } else { 0 })
}

/// 执行全部检查。
pub fn run_checks(args: &Args) -> Vec<Check> {
    let mut out = Vec::new();

    // ---- 1. 硬件 ----
    let probe = mangosteen_platform::hardware::probe();
    if probe.warnings.is_empty() {
        out.push(Check {
            name: "硬件采集".into(),
            status: Status::Ok,
            detail: probe.profile.describe(),
            hint: None,
        });
    } else {
        out.push(Check {
            name: "硬件采集".into(),
            status: Status::Warn,
            detail: probe.warnings.join("；"),
            hint: Some("部分硬件信息未取到，资源评估会偏保守".into()),
        });
    }

    if probe.profile.has_accelerator() {
        out.push(Check {
            name: "加速卡".into(),
            status: Status::Ok,
            detail: format!(
                "{}，可全量/部分卸载",
                probe
                    .profile
                    .primary_accelerator()
                    .map(|a| a.name.clone())
                    .unwrap_or_default()
            ),
            hint: None,
        });
    } else {
        out.push(Check {
            name: "加速卡".into(),
            status: Status::Warn,
            detail: "未检测到可用加速卡，只能 CPU 推理".into(),
            hint: Some(
                "CPU 推理速度通常 <5 tok/s。如需 GPU 加速，检查驱动/显存；或配置云服务商以降级"
                    .into(),
            ),
        });
    }

    // ---- 2. 推理引擎 ----
    let (cfg, config_source) = match load_config(args) {
        Ok(v) => (Some(v.0), v.1),
        Err(e) => {
            out.push(Check {
                name: "配置加载".into(),
                status: Status::Fail,
                detail: e.to_string(),
                hint: Some("执行 `mangosteen config init` 生成模板".into()),
            });
            (None, "（加载失败）".to_string())
        }
    };

    if let Some(cfg) = &cfg {
        // 配置合法性
        match cfg.validate() {
            Ok(()) => out.push(Check {
                name: "配置合法性".into(),
                status: Status::Ok,
                detail: config_source.clone(),
                hint: None,
            }),
            Err(e) => out.push(Check {
                name: "配置合法性".into(),
                status: Status::Fail,
                detail: e.to_string(),
                hint: Some("修好配置项后重试，或删除配置让 Mangosteen 用默认值".into()),
            }),
        }

        // 本地优先开关
        if cfg.scheduler.local_first {
            out.push(Check {
                name: "本地优先策略".into(),
                status: Status::Ok,
                detail: "已启用（符合产品基线）".into(),
                hint: None,
            });
        } else {
            out.push(Check {
                name: "本地优先策略".into(),
                status: Status::Warn,
                detail: "被配置关闭，将不再优先尝试本地部署".into(),
                hint: Some("把 scheduler.local_first 改回 true；只在临时排障时才应关闭".into()),
            });
        }

        // 目录可写
        for (label, dir) in [
            ("模型库目录", cfg.storage.models_dir.clone()),
            ("下载缓存目录", cfg.storage.cache_dir.clone()),
            (
                "配置目录",
                mangosteen_platform::paths::config_dir(&mangosteen_platform::paths::real_env),
            ),
            (
                "日志目录",
                mangosteen_platform::paths::log_dir(&mangosteen_platform::paths::real_env),
            ),
        ] {
            let writable = ensure_writable(&dir);
            out.push(Check {
                name: label.into(),
                status: if writable.is_ok() {
                    Status::Ok
                } else {
                    Status::Fail
                },
                detail: format!(
                    "{} —— {}",
                    dir.display(),
                    writable
                        .as_ref()
                        .map(|_| "可写".to_string())
                        .unwrap_or_else(|e| e.clone())
                ),
                hint: writable
                    .err()
                    .map(|_| "检查该目录权限，或改用有写权限的路径（配置 storage.*）".to_string()),
            });
        }

        // 网关端口
        let port = cfg.gateway.port;
        if mangosteen_platform::process::is_port_bindable(port) {
            out.push(Check {
                name: "网关端口".into(),
                status: Status::Ok,
                detail: format!("{port} 可绑定"),
                hint: None,
            });
        } else {
            out.push(Check {
                name: "网关端口".into(),
                status: Status::Warn,
                detail: format!("{port} 已被占用"),
                hint: Some("已有 serve 在运行？否则改配置 gateway.port 或加 --port 启动".into()),
            });
        }

        // 云服务商
        let (registered, skipped) = mangosteen_gateway::probe_providers(cfg);
        if registered.is_empty() && cfg.providers.is_empty() {
            out.push(Check {
                name: "云服务商".into(),
                status: Status::Warn,
                detail: "未配置任何服务商".into(),
                hint: Some(
                    "本地资源不足时将无法降级。执行 `mangosteen config init` 后可填 AutoDL/优云智算 凭证"
                        .into(),
                ),
            });
        } else if registered.is_empty() {
            out.push(Check {
                name: "云服务商".into(),
                status: Status::Warn,
                detail: skipped
                    .iter()
                    .map(|(n, r)| format!("{n}：{r}"))
                    .collect::<Vec<_>>()
                    .join("；"),
                hint: Some("按上述原因修复凭证或配置".into()),
            });
        } else {
            out.push(Check {
                name: "云服务商".into(),
                status: Status::Ok,
                detail: format!("已就绪：{}", registered.join(", ")),
                hint: if skipped.is_empty() {
                    None
                } else {
                    Some(format!(
                        "另有 {} 个被跳过（用 `mangosteen cloud providers` 查看）",
                        skipped.len()
                    ))
                },
            });
        }
    }

    // 引擎检查需要配置里的 engine_path
    let engine = mangosteen_gateway::Assessor::probe();
    let _ = engine;
    // 引擎诊断经网关层转发（CLI 不直接持有 LlamaCppEngine）。
    let diag = mangosteen_gateway::diagnose_engine(cfg.as_ref());

    match (&diag.binary, &diag.version) {
        (Some(bin), Some(version)) => {
            out.push(Check {
                name: "本地推理引擎".into(),
                status: Status::Ok,
                detail: format!("{} —— {version}", bin.display()),
                hint: None,
            });
            // 库路径是「为什么它能跑」的关键信息：用户换到别的终端手工
            // 执行 llama-server 会得到相反结论，必须写清楚。
            if let Some(v) = &diag.library_path {
                out.push(Check {
                    name: "引擎运行库路径".into(),
                    status: Status::Ok,
                    detail: format!("启动时注入 {}={v}", diag.library_path_var),
                    hint: Some(format!(
                        "手工执行 llama-server 报找不到动态库是正常的；\
                         Mangosteen 启动时会自动注入 {}",
                        diag.library_path_var
                    )),
                });
            }
        }
        (Some(bin), None) => {
            let msg = diag.error.clone().unwrap_or_default();
            let looks_like_lib_issue = msg.contains("Library not loaded")
                || msg.contains("cannot open shared object")
                || msg.contains("image not found")
                || msg.contains("dyld");
            out.push(Check {
                name: "本地推理引擎".into(),
                status: Status::Fail,
                detail: format!("{} 存在但无法运行：{msg}", bin.display()),
                hint: Some(if looks_like_lib_issue {
                    format!(
                        "动态库找不到。已尝试注入 {}。可用 `mangosteen engine env` 查看探测结果，\
                         再用配置 local.engine_env 手工指定库目录",
                        diag.library_path_var
                    )
                } else {
                    "检查文件权限（chmod +x）与架构是否匹配（arm64 vs x86_64）".into()
                }),
            });
        }
        _ => {
            let tried = mangosteen_platform::process::join_paths(&diag.search_dirs);
            out.push(Check {
                name: "本地推理引擎".into(),
                status: Status::Fail,
                detail: format!(
                    "未找到 {}。已搜索：{}",
                    mangosteen_platform::engine_binary_name(),
                    if tried.is_empty() { "（PATH 为空）" } else { &tried }
                ),
                hint: Some(
                    "Mangosteen 以 llama.cpp 为唯一推理引擎。安装：brew install llama.cpp（macOS/Linux）；\
                     或把 llama-server 放到 mangosteen 同级的 runtime/ 目录；\
                     或用配置 local.engine_path 指定绝对路径"
                        .into(),
                ),
            });
        }
    }

    // ---- 磁盘空间 ----
    if let Some(cfg) = &cfg {
        if let Some(avail) = available_space(&cfg.storage.models_dir) {
            let gib = avail as f64 / (1u64 << 30) as f64;
            out.push(Check {
                name: "模型库磁盘".into(),
                status: if gib < 10.0 { Status::Warn } else { Status::Ok },
                detail: format!("{} 剩余约 {:.1} GiB", cfg.storage.models_dir.display(), gib),
                hint: if gib < 10.0 {
                    Some("7B Q4 模型约需 4.5 GiB，32B 约需 20 GiB。请清理或换盘".into())
                } else {
                    None
                },
            });
        }
    }

    out
}

/// 测试目录可写（实际创建目录并写一个探针文件）。
fn ensure_writable(dir: &std::path::Path) -> Result<(), String> {
    if let Err(e) = std::fs::create_dir_all(dir) {
        return Err(format!("无法创建：{e}"));
    }
    let probe = dir.join(".mangosteen-write-probe");
    match std::fs::write(&probe, b"probe") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            Ok(())
        }
        Err(e) => Err(format!("不可写：{e}")),
    }
}

/// 取分区剩余空间（Unix 用 statvfs；其它平台返回 `None`）。
/// 查询可用空间（转发到平台层实现，避免两处逻辑分叉）。
fn available_space(path: &std::path::Path) -> Option<u64> {
    mangosteen_platform::fs::available_space(path).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_labels_and_colors() {
        assert_eq!(Status::Ok.label(), "OK");
        assert_eq!(Status::Warn.label(), "WARN");
        assert_eq!(Status::Fail.label(), "FAIL");
        assert_eq!(Status::Ok.color(), output::Color::Green);
        assert_eq!(Status::Fail.color(), output::Color::Red);
    }

    #[test]
    fn checks_run_without_panicking() {
        let a = crate::cli::parse_args(&["doctor".to_string()]).unwrap();
        let checks = run_checks(&a);
        assert!(!checks.is_empty(), "至少应有硬件检查项");
        // 硬件与加速卡检查必须存在
        assert!(checks.iter().any(|c| c.name == "硬件采集"));
        assert!(checks.iter().any(|c| c.name == "加速卡"));
    }

    #[test]
    fn writable_probe_detects_good_dir() {
        let d = std::env::temp_dir().join(format!("mn-doctor-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        assert!(ensure_writable(&d).is_ok());
        // 探针文件不应残留
        assert!(!d.join(".mangosteen-write-probe").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn writable_probe_detects_readonly_dir() {
        use std::os::unix::fs::PermissionsExt;
        let d = std::env::temp_dir().join(format!("mn-doctor-ro-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o500)).unwrap();

        // root 用户可以无视权限，因此仅在非 root 时断言失败
        let is_root = unsafe { libc::geteuid() } == 0;
        if !is_root {
            assert!(ensure_writable(&d).is_err(), "只读目录应被判定为不可写");
        }
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700)).ok();
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn available_space_is_reported() {
        let v = available_space(&std::env::temp_dir());
        assert!(v.is_some());
        assert!(v.unwrap() > 0);
    }

    #[test]
    fn doctor_rejects_unknown_option() {
        let a = crate::cli::parse_args(&[
            "doctor".to_string(),
            "--whatever".to_string(),
            "1".to_string(),
        ])
        .unwrap();
        let e = run(a).unwrap_err();
        assert!(e.to_string().contains("--whatever"));
    }
}
