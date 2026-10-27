//! # Mangosteen Cloud
//!
//! **算力执行层 · 分支 B：云端算力链路**。
//!
//! ```text
//! 网关层·调度决策器
//!      │  需要云端算力
//!      ▼
//! ┌──────────────────────────────────────────────┐
//! │ NodeSelector（core::policy）                  │  打分排序
//! └──────────────────────────────────────────────┘
//!      │  最优候选
//!      ▼
//! ┌──────────────────────────────────────────────┐
//! │ CloudOrchestrator                             │  统一 CRUD
//! │  · 驱动注册表（配置驱动动态加载）              │
//! │  · 实例生命周期（create→start→ready）          │
//! │  · 失败清理与重试                              │
//! └──────────────────────────────────────────────┘
//!      │
//!      ├── drivers::autodl / ucloud / aliyun
//!      └── net::TunnelManager（SSH 端口映射）
//! ```
//!
//! ## 多云抽象的核心承诺
//!
//! 上层只调用 [`CloudOrchestrator`] 的方法，拿 [`Instance`] 结构；
//! 新增服务商只需在 [`drivers`] 里加一个 `impl ProviderDriver` 并在
//! [`registry`] 注册，**不改动本文件与网关任何代码**。
//!
//! [`Instance`]: mangosteen_core::cloud::Instance

pub mod compshare;
pub mod drivers;
pub mod net;
pub mod orchestrator;
pub mod registry;

pub use compshare::CompshareDriver;
pub use net::TunnelManager;
pub use orchestrator::{CloudOrchestrator, OrchestratorConfig, Provisioned};
pub use registry::build_registry;
