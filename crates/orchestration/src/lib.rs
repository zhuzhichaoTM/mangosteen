//! # Mangosteen Orchestration
//!
//! **第三层 · 编排调度核心**。承接 API Server 下发的部署请求，完成
//! 「模型评估 → 本地/云端模式决策 → 云端节点优选 → 节点配置 → 部署编排 →
//! 模型加载运行全生命周期管理」。
//!
//! ## 边界
//!
//! * 不实现 HTTP、路由或协议转换（第二层 gateway 的职责）；
//! * 不直接创建云厂商资源或拉起进程，而是通过 `cloud` / `local` 的公开 API；
//! * 引擎差异由 `mangosteen_local::EnginePlugin` 吸收，本层不写引擎分支。
//!
//! ## 模块
//!
//! | 模块 | 职责 |
//! |------|------|
//! | [`assess`] | 模型规格解析后的资源评估 |
//! | [`scheduler`] | 本地/云端模式决策与节点优选 |
//! | [`progress`] | 部署阶段定义与进度上报契约 |

pub mod assess;
pub mod progress;
pub mod scheduler;

pub use assess::{Assessor, ComputeTarget, DeploymentRequest, Evaluated, ModelDownloadSpec};
pub use scheduler::{Decision, LocalLaunchOutcome, Scheduler, SchedulerOutcome};
