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
//! │  · EnginePlugin trait：启动 / 探活 / 自检    │
//! │  · EngineRegistry：引擎标识 → 插件           │
//! ├────────────────────────────────────────────┤
//! │ llama.cpp 插件      engine.rs               │
//! │  · 解析二进制位置 / 生成参数 / 拉起进程      │
//! ├────────────────────────────────────────────┤
//! │ Ollama 插件         engine.rs               │
//! │  · 复用常驻守护进程，不拥有进程              │
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

pub mod engine;
pub mod llama_args;
pub mod plugin;
pub mod remote;

pub use engine::{EngineHandle, EngineStatus, LlamaCppEngine, OllamaEngine};
pub use plugin::{EnginePlugin, EngineRegistry};
