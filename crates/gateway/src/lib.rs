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
//! │  统一协议与 HTTP API Server                          │
//! │       HTTP 解析 → 路由 → OpenAI 兼容 → 流式转发         │
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
//!    `EnginePlugin`（见 `mangosteen_device::EngineRegistry`），
//!    网关里出现 `InferenceEngine::` 字面量即违规。
//!
//! 这三条由 `tests/layering.rs` 与 `scheduler::tests` 里的不变量测试共同守护。

pub mod cloudmon;
pub mod gateway;
pub mod http;
pub mod sshmon;

pub mod protocol;
mod pull_worker;
pub mod pulljobs;
/// 部署运行态探测缓存（`running` 字段的数据源）。
pub mod runprobe;
pub mod server;

pub use cloudmon::{spawn_cloud_monitor, CloudMonitor};
pub use gateway::{
    ActiveDeployment, CloudDeploymentRef, CloudMetricsSnapshot, Gateway, GatewayBuilder,
    GatewayStatus,
};
pub use mangosteen_orchestration::progress;
pub use mangosteen_orchestration::progress::{noop_sink, DeployStage, StageEvent, StageOutcome};
pub use mangosteen_orchestration::{assess, scheduler};
pub use mangosteen_orchestration::{
    Assessor, DeploymentRequest, Evaluated, ModelDownloadSpec, PreparedDeployment,
};
pub use runprobe::{spawn_runtime_probe, RuntimeProbe};

pub use mangosteen_orchestration::{Decision, Scheduler, SchedulerOutcome};
pub use protocol::BackendTarget;
pub use server::GatewayServer;

/// 云端能力诊断（供 `mangosteen doctor` / `config validate` 使用）。
///
/// 包一层而不是让 CLI 直接调云端层：CLI 只与网关层交互是硬约束
/// （网关唯一收口原则）。这里纯转发，不含业务逻辑。
pub fn diagnose_cloud(config: &mangosteen_core::config::Config) -> Vec<String> {
    mangosteen_provider::registry::diagnose(config)
}

/// 内置服务商驱动名列表。
pub fn builtin_providers() -> &'static [&'static str] {
    mangosteen_provider::drivers::profiles::builtin_driver_names()
}

/// 依据配置构建驱动注册表，返回 (已注册名称, 跳过原因)。
///
/// 同样是对云端层的转发，让 CLI 不必直接依赖云端 crate。
pub fn probe_providers(
    config: &mangosteen_core::config::Config,
) -> (Vec<String>, Vec<(String, String)>) {
    let build = mangosteen_provider::build_registry(config);
    let names = build
        .registry
        .names()
        .iter()
        .map(|s| s.to_string())
        .collect();
    (names, build.skipped)
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
