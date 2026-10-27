//! # Mangosteen Gateway
//!
//! **API 网关层**：整个系统的唯一功能收口。
//!
//! ```text
//!  用户交互层 (CLI / TUI)
//!        │ 只与网关对话
//!        ▼
//! ┌──────────────────────────────────────────────────────┐
//! │ API 网关层                                            │
//! │                                                       │
//! │  ① 资源评估引擎  assess.rs                            │
//! │       硬件快照 + 模型规格 → 可行性 + 最优参数           │
//! │                                                       │
//! │  ② 调度决策器    scheduler.rs                         │
//! │       本地优先 → 不足/失败 → 云端降级 → 重试            │
//! │                                                       │
//! │  ③ 统一协议适配层 protocol.rs / server.rs             │
//! │       OpenAI 兼容接口，屏蔽本地/云端差异                │
//! └──────────────────────────────────────────────────────┘
//!        │                        │
//!        ▼                        ▼
//!   本地 llama.cpp          云端实例 + SSH 隧道
//! ```
//!
//! ## 不可违反的三条约束
//!
//! 1. **本地优先**：`Scheduler::decide` 必须先尝试本地，只有在本地资源明确
//!    不足或本地部署失败时才允许走云端；
//! 2. **网关唯一收口**：CLI 与 TUI 不持有 `LlamaCppEngine` / `CloudOrchestrator`
//!    的直接引用，一切经过 [`Gateway`]；
//! 3. **引擎差异插件化**：本层不按引擎写分支。新增推理框架只加
//!    `EnginePlugin`（见 `mangosteen_local::EngineRegistry`），
//!    网关里出现 `InferenceEngine::` 字面量即违规。
//!
//! 这三条由 `tests/layering.rs` 与 `scheduler::tests` 里的不变量测试共同守护。

pub mod cloudmon;
pub mod gateway;
pub mod http;
pub mod protocol;
pub mod pulljobs;
pub mod server;

pub use cloudmon::{spawn_cloud_monitor, CloudMonitor};
pub use gateway::{
    ActiveDeployment, CloudDeploymentRef, CloudMetricsSnapshot, Gateway, GatewayBuilder,
    GatewayStatus,
};
pub use mangosteen_orchestration::progress;
pub use mangosteen_orchestration::progress::{DeployStage, StageEvent, StageOutcome};
pub use mangosteen_orchestration::{assess, scheduler};
pub use mangosteen_orchestration::{Assessor, DeploymentRequest, Evaluated};

/// 在后台预热网关的慢速探测（引擎版本）。
///
/// `llama-server --version` 在部分安装上要 20~30 秒。若不做预热，
/// 用户打开控制台后的**第一次**状态查询就会卡住半分钟，
/// 表现为「页面白屏很久」。
///
/// 预热放在独立线程：启动立即返回，探测完成后状态自动补齐。
pub fn spawn_warm_up(gateway: std::sync::Arc<std::sync::Mutex<Gateway>>) {
    std::thread::spawn(move || {
        // 1) 短暂持锁：只取引擎副本与路径
        let snapshot = {
            let Ok(gw) = gateway.lock() else { return };
            gw.warm_up_snapshot()
        };
        let Some((engine, path)) = snapshot else {
            return;
        };

        // 2) **锁外**执行慢探测（`--version` 可能要 20~30 秒）。
        //    这一步绝不能持锁，否则期间所有 API 都会被阻塞。
        let probed = engine.probe_version();

        // 3) 再短暂持锁：写回结果。
        //    失败也要写——否则预估接口为了判断「引擎是否坏了」得自己
        //    再跑一次 27 秒的命令，而它是部署确认弹窗的必经接口。
        if let Ok(gw) = gateway.lock() {
            match probed {
                Ok(version) => gw.set_engine_version(path, version),
                Err(e) => gw.set_engine_failure(path, e.to_string()),
            }
        }
    });
}
pub use mangosteen_orchestration::{Decision, Scheduler, SchedulerOutcome};
pub use protocol::BackendTarget;
pub use server::GatewayServer;

/// 云端能力诊断（供 `mangosteen doctor` / `config validate` 使用）。
///
/// 包一层而不是让 CLI 直接调云端层：CLI 只与网关层交互是硬约束
/// （网关唯一收口原则）。这里纯转发，不含业务逻辑。
pub fn diagnose_cloud(config: &mangosteen_core::config::Config) -> Vec<String> {
    mangosteen_cloud::registry::diagnose(config)
}

/// 内置服务商驱动名列表。
pub fn builtin_providers() -> &'static [&'static str] {
    mangosteen_cloud::drivers::profiles::builtin_driver_names()
}

/// 依据配置构建驱动注册表，返回 (已注册名称, 跳过原因)。
///
/// 同样是对云端层的转发，让 CLI 不必直接依赖云端 crate。
pub fn probe_providers(
    config: &mangosteen_core::config::Config,
) -> (Vec<String>, Vec<(String, String)>) {
    let build = mangosteen_cloud::build_registry(config);
    let names = build
        .registry
        .names()
        .iter()
        .map(|s| s.to_string())
        .collect();
    (names, build.skipped)
}

/// 本地推理引擎诊断结果。
///
/// 供 `mangosteen doctor` / `mangosteen engine *` 使用。CLI 不直接持有
/// `LlamaCppEngine`（网关唯一收口），因此这里给出**只读快照**：
/// 定位结果、版本、以及启动时会注入的环境变量。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineDiagnosis {
    /// 定位到的可执行文件（`None` 表示未找到）
    pub binary: Option<std::path::PathBuf>,
    /// `--version` 输出（定位成功且能执行时才有）
    pub version: Option<String>,
    /// 定位/执行失败原因
    pub error: Option<String>,
    /// 启动时注入的动态库搜索变量名
    pub library_path_var: String,
    /// 启动时注入的动态库搜索路径值
    pub library_path: Option<String>,
    /// 本次探测实际搜索过的目录（按优先级）
    pub search_dirs: Vec<std::path::PathBuf>,
}

impl EngineDiagnosis {
    /// 引擎是否可用（能定位且能执行）。
    pub fn is_ready(&self) -> bool {
        self.binary.is_some() && self.version.is_some()
    }
}

/// 引擎诊断（**只读缓存版**，供 HTTP 接口使用）。
///
/// ## 与 [`diagnose_engine`] 的区别
///
/// | 函数 | 会执行 `--version` | 耗时 | 用途 |
/// |------|-----------------|------|------|
/// | `diagnose_engine` | 是 | 本机实测 27 秒 | CLI `doctor` / `engine probe` |
/// | `diagnose_engine_cached` | 否 | 微秒级 | `/api/doctor`、`/api/preview` |
///
/// 为什么必须分开：`llama-server --version` 会加载全部动态库与后端，
/// 在 Homebrew 安装上要 20~30 秒。HTTP 接口把它放在请求路径里就会超时，
/// 而这恰恰让「环境自检」和「部署费用确认」两个关键功能失效。
///
/// 缓存由启动时的预热线程（[`spawn_warm_up`]）后台填充。缓存为空时
/// 状态为 `version: None, error: None`——表示「还没探测完」，
/// 调用方应显示「探测中」而不是「失败」。
pub fn diagnose_engine_cached(
    config: Option<&mangosteen_core::config::Config>,
    cached_version: Option<String>,
) -> EngineDiagnosis {
    let local = config.map(|c| c.local.clone()).unwrap_or_default();
    let engine = mangosteen_local::LlamaCppEngine::from_config(local.engine_path.clone(), None)
        .with_engine_env(local.engine_env.clone())
        .with_auto_library_path(local.auto_library_path);

    let var = mangosteen_platform::dlpath::library_path_var().to_string();

    match engine.locate() {
        Ok(bin) => {
            let library_path = engine
                .launch_env(&bin)
                .into_iter()
                .find(|(k, _)| *k == var)
                .map(|(_, v)| v);
            EngineDiagnosis {
                binary: Some(bin),
                // 版本由调用方从预热缓存取来；这里不探测
                version: cached_version,
                error: None,
                library_path_var: var,
                library_path,
                search_dirs: mangosteen_platform::process::search_dirs(&[]),
            }
        }
        Err(_) => EngineDiagnosis {
            binary: None,
            version: None,
            error: None,
            library_path_var: var,
            library_path: None,
            search_dirs: mangosteen_platform::process::search_dirs(&[]),
        },
    }
}

/// 诊断本地推理引擎（含动态库路径探测）。
///
/// 这里刻意复用 `mangosteen_local::LlamaCppEngine` 的定位与环境注入逻辑，
/// 而不是自己拼一遍——两处逻辑一旦分叉，doctor 的结论就会与实际部署行为
/// 不一致（曾出现过：doctor 说 OK，部署却因库路径失败）。
pub fn diagnose_engine(config: Option<&mangosteen_core::config::Config>) -> EngineDiagnosis {
    let local = config.map(|c| c.local.clone()).unwrap_or_default();
    let engine = mangosteen_local::LlamaCppEngine::from_config(local.engine_path.clone(), None)
        .with_engine_env(local.engine_env.clone())
        .with_auto_library_path(local.auto_library_path);

    let var = mangosteen_platform::dlpath::library_path_var().to_string();
    let search_dirs = mangosteen_platform::process::search_dirs(&[]);

    match engine.locate() {
        Ok(bin) => {
            let library_path = engine
                .launch_env(&bin)
                .into_iter()
                .find(|(k, _)| *k == var)
                .map(|(_, v)| v);
            match engine.probe_version() {
                Ok(v) => EngineDiagnosis {
                    binary: Some(bin),
                    version: Some(v),
                    error: None,
                    library_path_var: var,
                    library_path,
                    search_dirs,
                },
                Err(e) => EngineDiagnosis {
                    binary: Some(bin),
                    version: None,
                    error: Some(e.to_string()),
                    library_path_var: var,
                    library_path,
                    search_dirs,
                },
            }
        }
        Err(e) => EngineDiagnosis {
            binary: None,
            version: None,
            error: Some(e.to_string()),
            library_path_var: var,
            library_path: None,
            search_dirs,
        },
    }
}

/// 引擎运行时布局说明（供 `mangosteen engine info` 展示）。
///
/// 参照 Ollama 的集成方式：引擎与它的动态库作为一个整体分发，
/// 由宿主进程显式注入库搜索路径，不依赖系统包管理器的安装状态。
pub fn engine_runtime_layout() -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            out.push(("可执行文件同级".to_string(), dir.display().to_string()));
            out.push((
                "随包运行时".to_string(),
                dir.join("runtime").display().to_string(),
            ));
        }
    }
    out.push((
        "系统 PATH".to_string(),
        // 分隔符由平台层给：网关层不得出现平台分支（跨平台兼容原则）
        mangosteen_platform::process::join_paths(&mangosteen_platform::process::search_dirs(&[])),
    ));
    out
}
