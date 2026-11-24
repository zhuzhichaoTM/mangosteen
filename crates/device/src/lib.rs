//! # Mangosteen Local
//!
//! **资产与执行层 · 模块 3：加载执行**（对应基线五层架构的第四层）。
//!
//! ```text
//! 网关调度指令（节点配置 + 模型路径）
//!      │
//!      ▼
//! ┌────────────────────────────────────────────┐
//! │ 引擎插件注册表  plugin.rs                    │
//! │  · EnginePlugin trait：统一部署 API          │
//! │    （TLS 协商 / 健康检查 URL / 预下载目录 /  │
//! │      启动参数适配 / 启动前检查 / 启停探活）   │
//! │  · EngineRegistry：引擎标识 → 插件           │
//! ├────────────────────────────────────────────┤
//! │ llama.cpp 插件   llama_cpp.rs                │
//! │  · qwen2.5-0.5b / qwen3.8-27b 等 GGUF 部署  │
//! │  · 二进制定位 / CLI 参数 / 远端进程管理      │
//! ├────────────────────────────────────────────┤
//! │ Strata 插件      strata.rs                   │
//! │  · qwen3.6-35b-a3b 等 MoE 三级存储推理       │
//! │  · --family/--model 启动 / --model-path 复用 │
//! └────────────────────────────────────────────┘
//!      │
//!      ▼  平台适配层（process / paths / fs）
//! ```
//!
//! ## 与架构的对应关系
//!
//! * 本 crate 只做「把 [`LocalPlan`] 变成运行中的推理服务」；
//! * 它**不决定**是否该用本地算力——那是编排核心层·节点选择的职责；
//! * 它**不暴露** HTTP 接口给上层——那是 API 网关层的职责；
//! * 引擎差异全部收敛在 [`plugin::EnginePlugin`]，**新增框架不改上层**。
//!
//! [`LocalPlan`]: mangosteen_core::plan::LocalPlan

pub mod llama_cpp;
pub mod plugin;
pub mod remote;
pub mod strata;

pub use llama_cpp::{llama_cpp_args, EngineStatus, LlamaCppEngine};
pub use plugin::{plugin_for, EnginePlugin, EngineRegistry};
pub use remote::DeviceRuntime;
pub use strata::StrataEngine;

// ============================================================ Ollama
// OllamaEngine 已删除（2026-11-23 用户裁决：暂时不需要）。
// InferenceEngine::Ollama 枚举保留（台账/配置兼容），插件注册表不再登记。

/// 取内置引擎插件（无状态的一次性实例，用于无注册表上下文的调用方）。
///
/// 编排层持有 `EngineRegistry`（长跑实例）；provider 等无状态执行层
/// 用本函数按引擎取一次性插件即可——两者行为一致（插件本身无状态）。
///
/// 2026-11-23 用户裁决：
/// - 每次调用返回新 Box 实例（插件构造是廉价的值语义），避免 static 的 const 构造限制
/// - Ollama 引擎移除（不需要常驻守护进程的复用形态）
pub fn builtin_plugin(
    engine: mangosteen_core::config::InferenceEngine,
) -> Option<Box<dyn EnginePlugin>> {
    use mangosteen_core::config::InferenceEngine;
    match engine {
        InferenceEngine::Strata => Some(Box::new(StrataEngine::new())),
        InferenceEngine::LlamaCpp => Some(Box::new(LlamaCppEngine::default())),
        _ => None,
    }
}