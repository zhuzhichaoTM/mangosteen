//! 架构分层约束的自动化校验。
//!
//! 本测试把提示词里的**强制设计约束**变成可执行的断言。任何违反都会让
//! `cargo test` 失败，从而在 CI 阶段拦住架构腐化。
//!
//! ## 被守护的约束
//!
//! | 编号 | 约束 | 校验方式 |
//! |------|------|----------|
//! | C1 | 网关唯一收口：CLI 不直连本地引擎 / 云厂商抽象 | 检查 CLI 的 Cargo.toml 依赖与源码 `use` |
//! | C2 | 平台无关：核心层不出现 `cfg(target_os)` | 扫描 core/gateway/base/session 源码 |
//! | C3 | **推理引擎插件化**：编排主链路不出现引擎专属分支 | 扫描 gateway/scheduler/assess，要求经注册表分发 |
//! | C4 | 本地优先：`Scheduler::decide` 对本地可行输入必然返回本地 | 已在 scheduler 单测中覆盖，这里再做源码级检查 |
//! | C5 | 分层方向：只允许依赖更底层 | 检查各 crate 的依赖声明 |
//! | C6 | **五层架构完整性**：基线定义的模块都必须存在 | 检查关键文件与类型是否齐备 |
//!
//! ## 演进记录
//!
//! 早先有两条约束是「单引擎 / 单格式」时代的产物，与新基线冲突，已改写：
//!
//! | 旧约束 | 旧语义 | 现约束 |
//! |--------|--------|--------|
//! | C3 | 全仓不出现 vLLM / TensorRT 等引擎名 | 引擎差异必须插件化（可多引擎） |
//! | 格式 | 只支持 GGUF | 下载不限格式，但必须标识类型 |
//!
//! 测试通过读取仓库内的源文件实现——这不是「测试实现细节」，
//! 而是「测试架构契约」，正是这类约束最合适的校验手段。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// 仓库根目录。
fn repo_root() -> PathBuf {
    // 测试运行时 cwd 是 crate 目录（crates/cli）
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("repo root")
        .to_path_buf()
}

/// 包名 → 目录名。
///
/// 目录统一去掉了 `mangosteen-` 前缀（`crates/core` → `crates/core`），
/// 但 Cargo 包名保留前缀（避免与 std 的 `core` 命名空间混淆）。
/// 本文件的断言一律用**包名**书写，这里做唯一一次换算。
fn crate_dir(crate_name: &str) -> String {
    crate_name
        .strip_prefix("mangosteen-")
        .unwrap_or(crate_name)
        .to_string()
}

/// 读取某 crate 的全部 `.rs` 源码（拼接）。
fn crate_sources(crate_name: &str) -> Vec<(PathBuf, String)> {
    let dir = repo_root()
        .join("crates")
        .join(crate_dir(crate_name))
        .join("src");
    let mut out = Vec::new();
    collect_rs(&dir, &mut out);
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn collect_rs(dir: &Path, out: &mut Vec<(PathBuf, String)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_rs(&p, out);
        } else if p.extension().map(|x| x == "rs").unwrap_or(false) {
            if let Ok(text) = std::fs::read_to_string(&p) {
                out.push((p, text));
            }
        }
    }
}

/// 读取 crate 的 Cargo.toml。
fn crate_manifest(crate_name: &str) -> String {
    let p = repo_root()
        .join("crates")
        .join(crate_dir(crate_name))
        .join("Cargo.toml");
    std::fs::read_to_string(&p).unwrap_or_default()
}

/// 去掉注释与字符串字面量后的源码，避免注释里的反例被误判。
///
/// 这是必要的：本仓库大量注释在**解释**为什么不用某个技术栈
/// （例如「禁止引入 vLLM」），若直接扫原文会把自己写死。
fn strip_comments_and_strings(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let bytes: Vec<char> = src.chars().collect();
    let mut i = 0;
    let mut state = 0u8; // 0=code 1=line comment 2=block comment(depth) 3=string 4=raw?

    while i < bytes.len() {
        let c = bytes[i];
        let next = bytes.get(i + 1).copied().unwrap_or('\0');
        match state {
            0 => {
                if c == '/' && next == '/' {
                    state = 1;
                    i += 2;
                    continue;
                }
                if c == '/' && next == '*' {
                    state = 2;
                    i += 2;
                    continue;
                }
                if c == '"' {
                    state = 3;
                    out.push(' ');
                    i += 1;
                    continue;
                }
                out.push(c);
                i += 1;
            }
            1 => {
                if c == '\n' {
                    state = 0;
                    out.push('\n');
                }
                i += 1;
            }
            2 => {
                if c == '*' && next == '/' {
                    state = 0;
                    i += 2;
                    continue;
                }
                i += 1;
            }
            3 => {
                if c == '\\' {
                    i += 2;
                    continue;
                }
                if c == '"' {
                    state = 0;
                    out.push(' ');
                }
                i += 1;
            }
            _ => {
                i += 1;
            }
        }
    }
    out
}

// ============================================================ C1

#[test]
fn c1_cli_does_not_depend_on_execution_crates() {
    let manifest = crate_manifest("mangosteen-cli");

    // CLI 只允许依赖：core（领域类型）、platform（路径/终端能力）、
    // base（台账只读）、gateway（唯一后端入口）
    for forbidden in ["mangosteen-device", "mangosteen-provider"] {
        assert!(
            !manifest.contains(forbidden),
            "C1 违反：CLI 不得依赖 `{forbidden}`（网关唯一收口原则）。\
             所有算力操作必须经 `mangosteen-gateway`。"
        );
    }
    // 正向断言：必须确实依赖网关
    assert!(
        manifest.contains("mangosteen-gateway"),
        "CLI 必须依赖网关层作为唯一后端入口"
    );
}

#[test]
fn c1_cli_sources_do_not_import_execution_crates() {
    for (path, src) in crate_sources("mangosteen-cli") {
        let code = strip_comments_and_strings(&src);
        for forbidden in ["mangosteen_device::", "mangosteen_provider::"] {
            assert!(
                !code.contains(forbidden),
                "C1 违反：{} 中出现 `{forbidden}`。\
                 CLI 只能通过网关层访问算力能力。",
                path.display()
            );
        }
    }
}

#[test]
fn c1_cli_does_not_talk_to_vendors_directly() {
    // CLI 不应自己发 HTTP 给云厂商，也不应自己拉起引擎
    for (path, src) in crate_sources("mangosteen-cli") {
        let code = strip_comments_and_strings(&src);
        for pattern in ["api.autodl.com", "api.ucloudai.com", "aliyuncs.com"] {
            assert!(
                !code.contains(pattern),
                "C1 违反：{} 中出现厂商 API 地址 `{pattern}`",
                path.display()
            );
        }
        // 只允许 gateway::GatewayServer 提供的 HTTP 服务，
        // 不允许 CLI 自建 TcpListener 去请求后端（serve 命令绑定监听是合法的）
        if path.file_name().map(|f| f == "serve.rs").unwrap_or(false) {
            continue;
        }
        assert!(
            !code.contains("TcpStream::connect"),
            "C1 违反：{} 自行发起 TCP 连接。转发必须由网关完成。",
            path.display()
        );
    }
}

// ============================================================ C2

#[test]
fn c2_core_has_no_platform_branches() {
    for (path, src) in crate_sources("mangosteen-core") {
        let code = strip_comments_and_strings(&src);
        for pat in [
            "cfg(target_os",
            "cfg!(target_os",
            "cfg(unix)",
            "cfg(windows)",
        ] {
            assert!(
                !code.contains(pat),
                "C2 违反：{} 出现平台分支 `{pat}`。\
                 平台差异必须收敛到 mangosteen-platform。",
                path.display()
            );
        }
    }
}

#[test]
fn c2_gateway_has_no_platform_branches() {
    for (path, src) in crate_sources("mangosteen-gateway") {
        let code = strip_comments_and_strings(&src);
        for pat in [
            "cfg(target_os",
            "cfg!(target_os",
            "cfg(unix)",
            "cfg(windows)",
        ] {
            assert!(
                !code.contains(pat),
                "C2 违反：{} 出现平台分支 `{pat}`",
                path.display()
            );
        }
    }
}

#[test]
fn c2_base_has_no_platform_branches() {
    for (path, src) in crate_sources("mangosteen-base") {
        let code = strip_comments_and_strings(&src);
        for pat in ["cfg(target_os", "cfg!(target_os"] {
            assert!(
                !code.contains(pat),
                "C2 违反：{} 出现平台分支 `{pat}`",
                path.display()
            );
        }
    }
}

#[test]
fn c2_core_does_not_depend_on_platform() {
    let manifest = crate_manifest("mangosteen-core");
    assert!(
        !manifest.contains("mangosteen-platform"),
        "C2 违反：core 不得依赖平台适配层（会造成反向依赖）"
    );
    // core 也不应直接依赖 libc / windows-sys
    assert!(!manifest.contains("libc"), "core 不得依赖 libc");
    assert!(
        !manifest.contains("windows-sys"),
        "core 不得依赖 windows-sys"
    );
}

/// 平台专属代码必须只出现在平台适配层（或 CLI 的终端控制）。
#[test]
fn c2_platform_code_is_centralized() {
    // 允许出现平台分支的 crate
    let allowed: BTreeSet<&str> = ["mangosteen-platform", "mangosteen-cli"]
        .into_iter()
        .collect();

    for crate_name in [
        "mangosteen-core",
        "mangosteen-base",
        "mangosteen-session",
        "mangosteen-device",
        "mangosteen-provider",
        "mangosteen-gateway",
    ] {
        assert!(!allowed.contains(crate_name));
        for (path, src) in crate_sources(crate_name) {
            let code = strip_comments_and_strings(&src);
            assert!(
                !code.contains("libc::") && !code.contains("windows_sys::"),
                "C2 违反：{} 直接调用平台 API。应改为调用 mangosteen-platform。",
                path.display()
            );
        }
    }
}

// ============================================================ C3

/// C3 推理引擎插件化：编排主链路不得出现引擎专属分支。
///
/// 基线原文：「推理引擎插件化原则：新增推理框架仅需要在引擎管理模块
/// 新增插件，不改动调度主链路。」
///
/// 反例（曾经的写法）：
/// ```ignore
/// if engine == InferenceEngine::Ollama { /* Ollama 专用 */ }
/// ```
/// 这种代码每接一个框架就要改网关，正是约束要禁止的。
///
/// 现在的判定：编排主链路（gateway / scheduler / assess）里
/// **不得出现任何 `InferenceEngine::` 字面量**——出现了就说明
/// 某个引擎被特殊对待了。
#[test]
fn c3_engine_differences_are_pluginized() {
    // 编排主链路已按基线归位到 `mangosteen-orchestration`（第三层），
    // 因此引擎字面量禁令扫**编排层**；网关只做装配与协议转换。
    for crate_name in ["mangosteen-orchestration", "mangosteen-gateway"] {
        for (path, src) in crate_sources(crate_name) {
            // 设置页需要把可选引擎列成下拉项：那是**配置序列化**，不是
            // 按引擎分支的执行逻辑，允许出现枚举名。判据是文件归属：
            // 只有 `handlers/settings.rs` 这一处是配置读写。
            if path
                .file_name()
                .map(|f| f == "settings.rs")
                .unwrap_or(false)
            {
                continue;
            }
            let code = strip_comments_and_strings(&src);
            assert!(
                !code.contains("InferenceEngine::"),
                "C3 违反：{} 出现引擎字面量。引擎差异必须收敛到插件，\
                 主链路只允许通过 EngineRegistry 分发。",
                path.display()
            );
        }
    }

    // 编排主链路必须确实经注册表取插件（防止「删掉了分支但也没接上插件」）。
    // 编排逻辑分布在多个 `orchestrator*.rs` 平铺模块（deploy / lifecycle /
    // engine_admin 等），因此按整个 crate 汇总扫描，而不是只看单个文件。
    let orch: String = crate_sources("mangosteen-orchestration")
        .into_iter()
        .map(|(_, s)| strip_comments_and_strings(&s))
        .collect();
    assert!(
        orch.contains("engines_registry.get("),
        "C3 违反：编排层未通过注册表取引擎插件"
    );
}

/// 引擎插件抽象必须存在，且内置清单只有一处。
///
/// 「内置清单只有一处」是防止漂移的关键：如果网关、配置、UI 各维护
/// 一份「支持哪些引擎」的列表，新增引擎时漏改一处就会表现为
/// 「设置页能选、部署说未适配」这类自相矛盾。
#[test]
fn c3_engine_registry_is_single_source() {
    let local: String = crate_sources("mangosteen-device")
        .into_iter()
        .map(|(_, s)| s)
        .collect();
    let code = strip_comments_and_strings(&local);

    for need in [
        "pub trait EnginePlugin",
        "pub struct EngineRegistry",
        "pub fn with_builtin",
    ] {
        assert!(
            code.contains(need),
            "C3 违反：引擎管理模块缺少 `{need}`（插件化基础设施）"
        );
    }

    // 内置清单的**定义**只能有一处。
    //
    // 注意：网关调用 `EngineRegistry::with_builtin(...)` 是允许的
    // （那是「实例化」）；被禁止的是在别处**再定义**一份清单。
    for other in [
        "mangosteen-gateway",
        "mangosteen-base",
        "mangosteen-provider",
        "mangosteen-cli",
    ] {
        for (path, src) in crate_sources(other) {
            let c = strip_comments_and_strings(&src);
            assert!(
                !c.contains("pub fn with_builtin"),
                "C3 违反：{} 自定义了引擎插件清单。\
                 内置清单必须唯一（在 mangosteen-device）。",
                path.display()
            );
        }
    }
}

/// 未适配的引擎必须**不在**注册表里——这样网关会明确拒绝，
/// 而不是拿到一个默认实现然后启动超时。
#[test]
fn c3_unimplemented_engines_are_absent_from_registry() {
    // 这条是行为契约，实现在 local crate 的单测里（plugin::tests），
    // 这里做源码级确认：内置注册只登记两个引擎。
    let src = crate_sources("mangosteen-device")
        .into_iter()
        .find(|(p, _)| p.file_name().map(|f| f == "plugin.rs").unwrap_or(false))
        .map(|(_, s)| s)
        .expect("plugin.rs 必须存在");
    let code = strip_comments_and_strings(&src);
    let i = code.find("pub fn with_builtin").expect("with_builtin");
    let seg = &code[i..];
    let end = seg.find("\n    }").unwrap_or(seg.len());
    let body = &seg[..end];
    // 只允许 register 两次（llama.cpp + ollama）
    let registers = body.matches(".register(").count();
    assert_eq!(
        registers, 2,
        "内置注册应只登记已实现的引擎（当前 llama.cpp + Ollama），实际 {registers} 次"
    );
}

/// C7：云供应商层只负责节点供应，不得持有模型下载/引擎启停逻辑。
///
/// 云端部署同样要下载模型、准备并拉起推理引擎；这些执行语义必须复用
/// `mangosteen-device`，否则同一套「模型 → 引擎」行为会出现两份实现，
/// 改一处漏一处。
#[test]
fn c7_provider_does_not_run_models() {
    let forbidden = [
        "fn ensure_llama_server",
        "fn start_llama_server",
        "fn stop_llama_server",
        "fn download_model_remote",
    ];
    for (path, src) in crate_sources("mangosteen-provider") {
        let code = strip_comments_and_strings(&src);
        for pat in forbidden {
            assert!(
                !code.contains(pat),
                "C7 违反：{} 定义了 `{pat}`。\
                 模型/引擎执行属于 device，provider 只负责创建与连接节点。",
                path.display()
            );
        }
    }
}

/// C7b：`DeviceRuntime` 必须同时覆盖本地与远端目标。
#[test]
fn c7_device_runtime_covers_local_and_remote() {
    let src: String = crate_sources("mangosteen-device")
        .into_iter()
        .map(|(_, s)| s)
        .collect();
    let code = strip_comments_and_strings(&src);
    assert!(
        code.contains("pub trait DeviceRuntime"),
        "C7 违反：device 缺少统一设备运行时契约（DeviceRuntime）"
    );
    assert!(
        code.contains("enum DeviceTarget") && code.contains("Remote"),
        "C7 违反：DeviceTarget 必须覆盖本地与远端目标"
    );
}

// ============================================================ C4

#[test]
fn c4_scheduler_checks_local_before_cloud() {
    let src = crate_sources("mangosteen-orchestration")
        .into_iter()
        .find(|(p, _)| p.file_name().map(|f| f == "scheduler.rs").unwrap_or(false))
        .map(|(_, s)| s)
        .expect("orchestration/scheduler.rs must exist");
    let code = strip_comments_and_strings(&src);

    // decide() 中必须先判断本地可行性
    let decide_pos = code.find("pub fn decide").expect("decide() must exist");
    let local_check = code[decide_pos..]
        .find("is_local_viable")
        .expect("decide() 必须检查本地可行性");
    // 云端方案构造必须出现在本地判定之后
    let cloud_build = code[decide_pos..]
        .find("build_cloud_plan")
        .expect("decide() 必须能构造云端方案");
    assert!(
        local_check < cloud_build,
        "C4 违反：decide() 中本地可行性判断必须早于云端方案构造"
    );
}

#[test]
fn c4_default_config_enables_local_first() {
    let src = crate_sources("mangosteen-core")
        .into_iter()
        .find(|(p, _)| p.file_name().map(|f| f == "config.rs").unwrap_or(false))
        .map(|(_, s)| s)
        .expect("config.rs must exist");
    // 默认值构造里 local_first 必须为 true
    assert!(
        src.contains("local_first: true"),
        "C4 违反：SchedulerConfig 的默认必须 local_first = true"
    );
}

// ============================================================ C5

#[test]
fn c5_dependency_direction_is_top_down() {
    // 每层只允许依赖更底层
    let rules: &[(&str, &[&str])] = &[
        // core 是最底层领域层，不依赖任何自家 crate
        ("mangosteen-core", &[]),
        // platform 需要 core 的类型
        ("mangosteen-platform", &["mangosteen-core"]),
        // base 需要 core + platform
        (
            "mangosteen-base",
            &["mangosteen-core", "mangosteen-platform"],
        ),
        // session（第一层·用户会话管理）只需要领域类型与平台文件能力。
        // 它**不得**依赖 gateway/local/cloud——会话层不感知算力来源，
        // 只记录「谁在什么时候做了什么」。
        (
            "mangosteen-session",
            &["mangosteen-core", "mangosteen-platform"],
        ),
        (
            "mangosteen-device",
            &["mangosteen-core", "mangosteen-platform"],
        ),
        (
            // provider 只做节点供应与连接；模型下载/引擎启停复用 device 的
            // 远端运行时，因此允许依赖 device（同层组合，不是反向依赖）。
            "mangosteen-provider",
            &[
                "mangosteen-core",
                "mangosteen-platform",
                "mangosteen-device",
            ],
        ),
        (
            // 第三层编排调度核心：持有评估、决策、部署编排与运行生命周期，
            // 通过 device 与 provider 完成执行，通过 base 落台账。
            "mangosteen-orchestration",
            &[
                "mangosteen-core",
                "mangosteen-platform",
                "mangosteen-base",
                "mangosteen-session",
                "mangosteen-device",
                "mangosteen-provider",
            ],
        ),
        (
            "mangosteen-gateway",
            &[
                "mangosteen-core",
                "mangosteen-platform",
                "mangosteen-base",
                // 网关要为上层暴露会话接口 → 依赖会话层
                "mangosteen-session",
                // 部署编排与运行生命周期在第三层，网关只做收口
                "mangosteen-orchestration",
                "mangosteen-device",
                "mangosteen-provider",
            ],
        ),
        (
            "mangosteen-cli",
            &[
                "mangosteen-core",
                "mangosteen-platform",
                "mangosteen-base",
                // CLI 是第一层交互入口，与会话层同层，可直接使用
                "mangosteen-session",
                "mangosteen-gateway",
            ],
        ),
    ];

    let all = [
        "mangosteen-core",
        "mangosteen-platform",
        "mangosteen-base",
        "mangosteen-session",
        "mangosteen-device",
        "mangosteen-provider",
        "mangosteen-orchestration",
        "mangosteen-gateway",
        "mangosteen-cli",
    ];

    for (crate_name, allowed) in rules {
        let manifest = crate_manifest(crate_name);
        for other in all {
            if other == *crate_name {
                continue;
            }
            let declared = manifest.contains(&format!("{other} = "));
            if allowed.contains(&other) {
                continue;
            }
            assert!(
                !declared,
                "C5 违反：`{crate_name}` 不得依赖 `{other}`（依赖方向必须自上而下）"
            );
        }
    }
}

#[test]
fn c5_base_does_not_depend_on_execution_or_gateway() {
    let manifest = crate_manifest("mangosteen-base");
    for forbidden in [
        "mangosteen-gateway",
        "mangosteen-device",
        "mangosteen-provider",
        "mangosteen-cli",
    ] {
        assert!(
            !manifest.contains(forbidden),
            "C5 违反：基础能力层不得依赖 `{forbidden}`"
        );
    }
}

// ============================================================ C6

/// 仓库内某文件是否存在（相对仓库根）。
fn file_exists(rel: &str) -> bool {
    repo_root().join(rel).is_file()
}

/// 某 crate 的源码里是否包含给定片段（**不去注释**，用于查类型/函数名）。
fn crate_has(crate_name: &str, needle: &str) -> bool {
    crate_sources(crate_name)
        .iter()
        .any(|(_, src)| src.contains(needle))
}

/// C6 基线五层架构完整性。
///
/// 逐层核对基线定义的模块是否**真的存在于代码里**。
///
/// 这条测试的意义：架构不能只写在文档里。文档说「有模型评估模块」，
/// 但代码里找不到对应类型，那就是空话。反过来，这条测试也防止
/// 「重构时不小心删掉了某一层的入口」。
#[test]
fn c6_five_layer_architecture_is_complete() {
    // ---------- 第一层：用户会话与交互层 ----------
    assert!(
        file_exists("crates/session/src/lib.rs"),
        "第一层缺少「用户会话管理」模块（mangosteen-session）"
    );
    assert!(
        crate_has("mangosteen-session", "pub struct SessionStore"),
        "第一层缺少会话存储（SessionStore）"
    );
    assert!(
        crate_has("mangosteen-session", "pub enum SessionKind"),
        "第一层应区分推理会话与部署任务会话（SessionKind）"
    );
    // 交互入口：CLI + 内嵌 Web UI
    assert!(file_exists("crates/cli/src/main.rs"), "缺少 CLI 入口");
    assert!(
        file_exists("crates/webui/src/lib.rs"),
        "第一层缺少独立 Web UI crate"
    );

    // ---------- 第二层：API 网关层（唯一收口） ----------
    assert!(
        file_exists("crates/gateway/src/server.rs"),
        "第二层缺少 API Server"
    );
    for module in [
        "crates/gateway/src/http/server.rs",
        "crates/gateway/src/http/request.rs",
        "crates/gateway/src/http/response.rs",
        "crates/gateway/src/http/router.rs",
        "crates/gateway/src/http/upstream.rs",
        "crates/gateway/src/http/handlers/mod.rs",
        "crates/gateway/src/http/handlers/status.rs",
        "crates/gateway/src/http/handlers/models.rs",
        "crates/gateway/src/http/handlers/pull.rs",
        "crates/gateway/src/http/handlers/deployment.rs",
        "crates/gateway/src/http/handlers/sessions.rs",
        "crates/gateway/src/http/handlers/inference.rs",
        "crates/gateway/src/http/handlers/attachments.rs",
        "crates/gateway/src/http/handlers/settings.rs",
    ] {
        assert!(file_exists(module), "第二层缺少 HTTP 模块边界：{module}");
    }
    let server = crate_sources("mangosteen-gateway")
        .into_iter()
        .find(|(p, _)| p.ends_with("http/server.rs"))
        .map(|(_, src)| src)
        .expect("HTTP server implementation must exist");
    assert!(
        !server.contains("pub struct ParsedRequest")
            && !server.contains("pub struct Response")
            && !server.contains("pub fn parse_request"),
        "HTTP request parsing and response DTOs must live in dedicated modules"
    );
    assert!(
        file_exists("crates/gateway/src/http/router.rs"),
        "第二层缺少独立 HTTP 路由模块"
    );

    for route in ["\"/v1/chat/completions\"", "\"/api/deploy\""] {
        assert!(
            crate_has("mangosteen-gateway", route),
            "API Server 缺少路由 {route}"
        );
    }

    // ---------- 第三层：编排调度核心层 ----------
    // 模块 1：引擎管理（插件式多推理框架）
    assert!(
        file_exists("crates/device/src/plugin.rs"),
        "第三层缺少「引擎管理」插件模块"
    );
    assert!(
        crate_has("mangosteen-device", "pub trait EnginePlugin"),
        "引擎管理缺少插件抽象（EnginePlugin）"
    );
    // 模块 2：模型评估
    assert!(
        file_exists("crates/orchestration/src/assess.rs"),
        "第三层缺少「模型评估」模块"
    );
    assert!(
        crate_has("mangosteen-orchestration", "pub struct Assessor"),
        "模型评估缺少 Assessor"
    );
    // 模块 3：节点选择（多云调度决策器）
    assert!(
        file_exists("crates/orchestration/src/scheduler.rs"),
        "第三层缺少「节点选择」模块"
    );
    assert!(
        crate_has("mangosteen-orchestration", "pub fn decide"),
        "节点选择缺少决策入口（decide）"
    );
    // 模块 3.5：部署编排与运行生命周期（第三层的执行面）
    for f in [
        "crates/orchestration/src/runtime.rs",
        "crates/orchestration/src/deploy.rs",
        "crates/orchestration/src/evaluate.rs",
        "crates/orchestration/src/launch.rs",
        "crates/orchestration/src/provision.rs",
        "crates/orchestration/src/lifecycle.rs",
        "crates/orchestration/src/query.rs",
    ] {
        assert!(file_exists(f), "第三层缺少「部署编排与生命周期」模块：{f}");
    }
    // 模块目录只用于真实子命名空间：同名单模块必须平铺成 `<name>.rs`，
    // 不得出现 `orchestration/src/orchestrator/` 这类重复层级。
    assert!(
        !file_exists("crates/orchestration/src/orchestrator/mod.rs")
            && !file_exists("crates/gateway/src/gateway/mod.rs")
            && !file_exists("crates/core/src/transport/mod.rs"),
        "同名单模块必须使用平铺文件，不得再引入同层目录"
    );
    assert!(
        crate_has("mangosteen-orchestration", "pub struct Orchestrator"),
        "部署编排核心缺少 Orchestrator"
    );
    // 模块 4：节点配置（本地 / 云端两套配置生成）
    assert!(
        crate_has("mangosteen-core", "pub struct LocalPlan"),
        "节点配置缺少本地方案（LocalPlan）"
    );
    assert!(
        crate_has("mangosteen-core", "pub struct CloudPlan"),
        "节点配置缺少云端方案（CloudPlan）"
    );
    // 模块 5：多云厂商插件适配
    assert!(
        file_exists("crates/provider/src/registry.rs"),
        "第三层缺少多云驱动注册表"
    );
    assert!(
        crate_has("mangosteen-core", "pub trait ProviderDriver"),
        "多云抽象缺少驱动接口（ProviderDriver）"
    );

    // ---------- 第四层：资产与执行层 ----------
    // 模块 1：模型下载管理
    assert!(
        crate_has("mangosteen-base", "pub fn download_model"),
        "第四层缺少「模型下载管理」"
    );
    // 模块 2：模型量化管理（可选能力）
    assert!(
        file_exists("crates/base/src/quantize.rs"),
        "第四层缺少「模型量化管理」模块（基线的可选工具能力）"
    );
    assert!(
        crate_has("mangosteen-base", "pub struct QuantizeRequest"),
        "量化模块缺少任务描述（QuantizeRequest）"
    );
    // 模块 3：加载执行
    assert!(
        crate_has("mangosteen-device", "pub struct EngineHandle"),
        "第四层缺少「加载执行」句柄（EngineHandle）"
    );

    // ---------- 第五层：基础支撑层 ----------
    assert!(
        crate_has("mangosteen-core", "pub struct Config"),
        "第五层缺少配置持久化（Config）"
    );
    assert!(
        crate_has("mangosteen-base", "pub struct StateStore"),
        "第五层缺少状态持久化（StateStore）"
    );
    assert!(
        crate_has("mangosteen-base", "pub struct AccessGuard"),
        "第五层缺少文件访问权限控制（AccessGuard）"
    );
    assert!(
        file_exists("crates/platform/src/process.rs"),
        "第五层缺少进程/平台基础能力"
    );
}

/// C6 会话层不得感知算力来源。
///
/// 基线把会话管理放在**第一层**，它的职责是「维护任务状态、上下文、
/// 操作记录」，不判断能不能部署、不启动引擎。若它引用了引擎或云厂商，
/// 就说明分层被打穿了。
#[test]
fn c6_session_layer_does_not_know_about_compute() {
    for (path, src) in crate_sources("mangosteen-session") {
        let code = strip_comments_and_strings(&src);
        for forbidden in [
            "LlamaCppEngine",
            "EngineHandle",
            "CloudOrchestrator",
            "ProviderDriver",
            "deploy(",
        ] {
            assert!(
                !code.contains(forbidden),
                "C6 违反：{} 出现 `{forbidden}`。\
                 会话层只记录状态，不得参与部署或引擎管理。",
                path.display()
            );
        }
    }
}

// ============================================================ 其它结构约束

#[test]
fn workspace_contains_all_layers() {
    let manifest = std::fs::read_to_string(repo_root().join("Cargo.toml")).unwrap();
    for c in [
        "mangosteen-core",
        "mangosteen-platform",
        "mangosteen-base",
        "mangosteen-session",
        "mangosteen-device",
        "mangosteen-provider",
        "mangosteen-gateway",
        "mangosteen-cli",
    ] {
        // 目录名去掉了 `mangosteen-` 前缀，包名保留。
        let dir = c.strip_prefix("mangosteen-").unwrap_or(c);
        assert!(
            manifest.contains(&format!("crates/{dir}")),
            "工作区缺少 crate `{c}`（目录 crates/{dir}）"
        );
        // `mangosteen-cli` 是二进制目标，不出现在 [workspace.dependencies]，
        // 只校验目录存在（上面那条）与它自己的 Cargo.toml 存在。
        if c != "mangosteen-cli" {
            assert!(
                manifest.contains(&format!("{c} = ")),
                "工作区缺少 crate `{c}` 的包名声明"
            );
        }
    }
}

#[test]
fn no_unwrap_in_production_paths() {
    // 生产代码中 `unwrap()` 会在异常输入下 panic，对客户端是致命的。
    // 允许在测试模块与非关键常量解析中使用，因此只做**统计性**检查：
    // 每千行 unwrap 数必须低于阈值，防止成片蔓延。
    let mut total_lines = 0usize;
    let mut total_unwraps = 0usize;

    for crate_name in [
        "mangosteen-core",
        "mangosteen-platform",
        "mangosteen-base",
        "mangosteen-device",
        "mangosteen-provider",
        "mangosteen-gateway",
    ] {
        for (_, src) in crate_sources(crate_name) {
            // 只看测试模块之前的部分
            let prod = match src.find("#[cfg(test)]") {
                Some(pos) => &src[..pos],
                None => &src[..],
            };
            let code = strip_comments_and_strings(prod);
            total_lines += code.lines().count();
            total_unwraps += code.matches(".unwrap()").count();
        }
    }

    let per_kloc = total_unwraps as f64 * 1000.0 / total_lines.max(1) as f64;
    assert!(
        per_kloc < 3.0,
        "生产代码中 unwrap 密度过高：{total_unwraps} 次 / {total_lines} 行 = {per_kloc:.2}/kloc。\
         请改用 ? 或显式 match。"
    );
}

/// 多模型格式：下载**不限格式**，但必须标识类型。
///
/// 基线原文：「支持多种模型格式（GGUF、Safetensors 等）」。
///
/// 这条**取代**了旧约束「只支持 GGUF」。旧语义下下载会按扩展名过滤，
/// 挡住了用 vLLM / SGLang / TensorRT 的用户。现在改为：任何格式都能下，
/// 但必须如实标注是什么格式，让用户按自己用的引擎判断。
#[test]
fn multi_format_is_identified_not_filtered() {
    let src = crate_sources("mangosteen-base")
        .into_iter()
        .find(|(p, _)| p.file_name().map(|f| f == "download.rs").unwrap_or(false))
        .map(|(_, s)| s)
        .expect("download.rs 必须存在");
    let code = strip_comments_and_strings(&src);

    // 必须有多格式识别能力
    assert!(
        code.contains("fn detect_format"),
        "下载层必须能识别模型格式（否则用户不知道自己下的是什么）"
    );

    // 格式名本身是**字符串字面量**，会被剥离器抹掉，所以这里查原文。
    // 只在 detect_format 函数体内查，避免被注释或无关代码误命中。
    let i = src.find("fn detect_format").expect("detect_format");
    let seg = &src[i..];
    let end = seg.find("\n}\n").unwrap_or(seg.len().min(2000));
    let body = &seg[..end];
    for fmt in ["gguf", "safetensors", "onnx", "pytorch"] {
        assert!(
            body.contains(fmt),
            "格式识别必须覆盖 `{fmt}`（基线要求支持多格式）"
        );
    }
}

/// 模型登记仍必须读 GGUF 元数据——非 GGUF 只能作文件管理。
///
/// 这是当前阶段的**真实边界**，不能假装支持：资源评估依赖
/// `block_count` / `embedding_length` 等字段，safetensors 没有这些。
/// 如实标注比「假装能部署」重要。
#[test]
fn model_registration_still_requires_gguf_metadata() {
    let src = crate_sources("mangosteen-base")
        .into_iter()
        .find(|(p, _)| p.file_name().map(|f| f == "models.rs").unwrap_or(false))
        .map(|(_, s)| s)
        .expect("models.rs 必须存在");
    let code = strip_comments_and_strings(&src);
    assert!(
        code.contains("gguf::") || code.contains("extract_info"),
        "模型登记必须解析 GGUF 元数据（资源评估依赖它）"
    );
}

#[test]
fn comment_stripper_works() {
    // 元测试：确保 C2/C3 的判定基础可靠
    let src = r#"
// 这里提到 vllm 只是注释，不应被判定
let a = "字符串里的 vllm 也不应命中";
/* 块注释 vllm */
let b = 1;
"#;
    let stripped = strip_comments_and_strings(src);
    assert!(
        !stripped.contains("vllm"),
        "剥离器失效，会把注释内容当代码：{stripped}"
    );
    assert!(stripped.contains("let b = 1;"));
}
