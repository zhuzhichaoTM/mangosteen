//! 部署编排与运行生命周期：第三层编排调度核心的执行面。
//!
//! 归属：**第三层 · 编排调度核心**。
//!
//! ## 职责
//!
//! 「评估 → 本地/云端模式决策 → 节点选择 → 节点配置 → 部署编排 →
//! 模型加载运行全生命周期管理」这一整条链路落在这里。它持有执行所需的
//! 下层能力（评估器、调度器、引擎注册表、云端编排器、台账、模型库），
//! 但**不实现 HTTP、路由或协议转换**——那是第二层 `mangosteen-gateway`
//! 的职责。网关只把请求收口到本结构的方法上。
//!
//! ## 与 device / provider 的边界
//!
//! * 本地与云端的「模型下载、引擎准备、启动、探活、停止」都属于
//!   `mangosteen-device`，本层只做决策与编排；
//! * `mangosteen-provider` 只负责创建与连接 GPU 节点，本层调用它拿实例，
//!   不解释任何厂商协议细节。
//!
//! ## 状态归属
//!
//! 运行中的本地引擎句柄（`engines`）、请求序号、引擎版本缓存都是**进程内
//! 运行状态**，不是领域数据；它们归编排层持有，网关不再直接管理。
//!
//! ## 子模块
//!
//! | 模块 | 职责 |
//! |------|------|
//! | [`deploy`](self) | 评估 → 决策 → 配置 → 启动 / 云端降级 |
//! | `lifecycle` | 停止服务、恢复服务、回滚中止、整体关停 |
//! | `query` | 台账列表、活跃后端、云端监控、状态快照 |
//! | `cloud_ops` | 云端选点、规格列举、裸实例创建与销毁 |
//! | `preview` | 无副作用的部署预估与费用 |
//! | `engine_admin` | 引擎版本探测与缓存 |

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use mangosteen_base::state::{DeploymentRecord, DeploymentStatus, StateStore};
use mangosteen_base::{ConfigStore, ModelStore};
use mangosteen_core::config::{Config, LocalConfig, SchedulerConfig};
use mangosteen_core::plan::{ComputeOrigin, DeploymentPlan};
use mangosteen_core::policy::SelectionConstraints;
use mangosteen_core::{MnError, MnResult};
use mangosteen_device::engine::{EngineHandle, LlamaCppEngine};
use mangosteen_provider::orchestrator::CloudOrchestrator;

use crate::assess::{Assessor, DeploymentRequest, Evaluated};
use crate::backend::BackendTarget;
use crate::deployment::{
    assessed_detail, configured_detail, default_estimate_hours, first_line, sanitize, short,
    CloudCandidateBrief, CloudPreview, DeploymentPreview, LocalPreview, PreparedDeployment,
    PreparedLocal,
};
use crate::scheduler::{Decision, Scheduler};

#[path = "orchestrator_cloud_ops.rs"]
mod cloud_ops;
#[path = "orchestrator_deploy.rs"]
mod deploy;
#[path = "orchestrator_engine_admin.rs"]
mod engine_admin;
#[path = "orchestrator_lifecycle.rs"]
mod lifecycle;
#[path = "orchestrator_preview.rs"]
mod preview;
#[path = "orchestrator_query.rs"]
mod query;

/// 引擎版本探测的时间预算。
///
/// 为什么是 2 秒：正常引擎（假引擎、修好的引擎）毫秒级返回；2 秒足够
/// 覆盖冷启动抖动，又不会让 HTTP 请求明显卡顿。真正慢的引擎（加载全部
/// 动态库要 20~30 秒）会走「未知」分支，由调用方保守处理。
const ENGINE_PROBE_BUDGET: Duration = Duration::from_secs(2);

/// 网关整体状态快照（供 CLI / TUI / HTTP 展示）。
#[derive(Debug, Clone, PartialEq)]
pub struct GatewayStatus {
    /// 网关版本
    pub version: String,
    /// 硬件描述
    pub hardware: String,
    /// 已启用的云服务商
    pub providers: Vec<String>,
    /// 活跃部署
    pub active: Vec<ActiveDeployment>,
    /// 本地引擎二进制路径（`None` 表示未找到）
    pub engine_path: Option<PathBuf>,
    /// 引擎版本
    pub engine_version: Option<String>,
}

/// 活跃云端部署的定位信息（不含任何厂商调用）。
#[derive(Debug, Clone, PartialEq)]
pub struct CloudDeploymentRef {
    /// 部署 ID
    pub deployment_id: String,
    /// 服务商驱动名
    pub provider: String,
    /// 厂商侧实例 ID
    pub instance_id: String,
    /// 模型 ID
    pub model: String,
    /// 部署状态
    pub status: mangosteen_base::state::DeploymentStatus,
}

/// 云端部署的监控快照（设备页签数据源）。
#[derive(Debug, Clone, PartialEq)]
pub struct CloudMetricsSnapshot {
    /// 部署 ID
    pub deployment_id: String,
    /// 服务商驱动名
    pub provider: String,
    /// 厂商侧实例 ID
    pub instance_id: String,
    /// 模型 ID
    pub model: String,
    /// 部署状态
    pub status: mangosteen_base::state::DeploymentStatus,
    /// 监控指标（`None` 表示服务商未提供监控能力）
    pub metrics: Option<mangosteen_core::cloud::InstanceMetrics>,
    /// 查询失败原因（与 `metrics` 互斥，用于就地提示）
    pub error: Option<String>,
}

/// 一条活跃部署的概要。
#[derive(Debug, Clone, PartialEq)]
pub struct ActiveDeployment {
    /// 部署 ID
    pub id: String,
    /// 模型 ID
    pub model: String,
    /// 算力来源
    pub origin: mangosteen_core::plan::ComputeOrigin,
    /// 状态
    pub status: mangosteen_base::state::DeploymentStatus,
    /// 访问端口
    pub port: Option<u16>,
    /// 是否就绪
    pub ready: bool,
}

/// 部署编排核心。
///
/// 持有执行所需的下层能力；生命周期与状态都收敛在这里，网关只做转发。
pub struct Orchestrator {
    config: Config,
    assessor: Assessor,
    scheduler: Scheduler,
    local_engine: LlamaCppEngine,
    /// 引擎插件注册表。
    ///
    /// 编排核心层·引擎管理的落地形态：编排层只按 `config.local.engine`
    /// 取对应插件，**不写任何引擎专属分支**。新增推理框架只需在
    /// `mangosteen_device::EngineRegistry::with_builtin` 里登记，
    /// 本层一行不动（基线「推理引擎插件化原则」）。
    engines_registry: mangosteen_device::EngineRegistry,
    cloud: Option<CloudOrchestrator>,
    state: StateStore,
    models: ModelStore,
    /// 本地运行中的引擎：部署 ID → 句柄
    engines: HashMap<String, EngineHandle>,
    /// 请求 ID 序号
    req_seq: std::sync::atomic::AtomicU64,
    /// 引擎版本缓存：`(路径, 版本)`。
    ///
    /// ## 为什么必须缓存
    ///
    /// `llama-server --version` 会加载全部动态库与后端，在部分安装
    /// （如 Homebrew 版）上要 **20~30 秒**。状态查询是高频调用
    /// （UI 每几秒轮询一次），每次都探测会让界面像卡死一样。
    ///
    /// 缓存键含路径：用户改了 `local.engine_path` 后会重新探测，
    /// 不会一直返回旧引擎的版本。
    ///
    /// 值区分成功与失败：`Err(原因)` 记录「探测过且失败」，这样预估接口
    /// 不必再跑一次 27 秒的命令就能说出「引擎坏了」。
    engine_version_cache: std::sync::Mutex<Option<(PathBuf, Result<String, String>)>>,
    /// 配置文件存储（设置页写盘用）。
    config_store: Option<ConfigStore>,
}

impl Orchestrator {
    /// 构造。
    pub fn new(
        config: Config,
        assessor: Assessor,
        local_engine: LlamaCppEngine,
        cloud: Option<CloudOrchestrator>,
        state: StateStore,
        models: ModelStore,
    ) -> MnResult<Self> {
        let mut scheduler =
            Scheduler::new(assessor.clone_for_scheduler(), config.scheduler.clone());
        scheduler.set_port_range(config.local.port_range_start, config.local.port_range_end)?;
        // 网关自身端口也要预留：默认 11516 落在引擎区间 11435-11999 内，
        // 分配器虽然有 `is_port_bindable` 兜底，但显式预留能避免依赖
        // 「网关已开始监听」这个时序前提。
        scheduler.reserve_port(config.gateway.port);
        // 避开台账里已占用端口的部署：进程重启后分配器从零开始，
        // 若不跳过会反复撞上同一个端口（表现为「端口已被占用」）。
        if let Ok(existing) = state.list_active() {
            for rec in existing {
                if let Some(p) = rec.plan.local_access_port() {
                    scheduler.reserve_port(p);
                }
            }
        }

        Ok(Self {
            config,
            assessor,
            scheduler,
            // 注册表在此构造：内置插件清单只在 device crate 里维护一份，
            // 编排层不关心有哪些引擎、各自怎么启动。
            engines_registry: mangosteen_device::EngineRegistry::with_builtin(
                local_engine.clone(),
                mangosteen_device::OllamaEngine::new(),
            ),
            local_engine,
            cloud,
            state,
            models,
            engines: HashMap::new(),
            req_seq: std::sync::atomic::AtomicU64::new(0),
            engine_version_cache: std::sync::Mutex::new(None),
            config_store: None,
        })
    }

    /// 挂载配置存储（供设置页写盘）。
    pub fn with_config_store(mut self, store: ConfigStore) -> Self {
        self.config_store = Some(store);
        self
    }

    /// 取配置存储。未挂载时返回一个平台默认实例，保证设置页可用。
    pub fn config_store(&self) -> ConfigStore {
        self.config_store
            .clone()
            .unwrap_or_else(|| ConfigStore::platform_default(None))
    }

    /// 配置引用。
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// 台账引用。
    pub fn state(&self) -> &StateStore {
        &self.state
    }

    /// 模型库引用。
    pub fn models(&self) -> &ModelStore {
        &self.models
    }

    /// 评估器引用。
    pub fn assessor(&self) -> &Assessor {
        &self.assessor
    }

    /// 本地引擎引用（供诊断/版本探测）。
    pub fn local_engine(&self) -> &LlamaCppEngine {
        &self.local_engine
    }

    /// 下一个请求序号。
    pub fn next_seq(&self) -> u64 {
        self.req_seq
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    /// 用新配置替换内存中的配置。
    ///
    /// 设置页写盘后必须调用它，否则用户改完要重启才生效——
    /// 这是「所见即所得」的最低要求。
    ///
    /// 只替换可热更新的部分：调度策略、本地参数、存储默认源、服务商开关。
    /// **不重建**引擎句柄与云端编排器（重建会中断正在服务的推理），
    /// 那些字段的变更需要重启，UI 会明确提示。
    pub fn replace_config(&mut self, cfg: Config) {
        self.scheduler = Scheduler::new(self.assessor.clone_for_scheduler(), cfg.scheduler.clone());
        // 端口区间可能被改过，重新设置（失败则保留旧值，不因设置页写错而崩溃）
        let _ = self
            .scheduler
            .set_port_range(cfg.local.port_range_start, cfg.local.port_range_end);
        self.config = cfg;
    }
}

/// 把 `SchedulerConfig` 转成调度器可用的形式（便于 CLI 覆盖）。
pub fn scheduler_config_with_overrides(
    base: &SchedulerConfig,
    max_price: Option<f64>,
) -> SchedulerConfig {
    let mut c = base.clone();
    if let Some(p) = max_price {
        c.max_price_per_hour = Some(p);
    }
    c
}

/// 通过 HTTP 探活一个本地部署（**经引擎插件分发**）。
///
/// 为什么不在这里判断引擎类型：探活端点与端口选择都是引擎专属知识
/// （llama.cpp 用 `plan` 端口 + `/health`；Ollama 用固定 11434 +
/// `/api/version`）。把这些写进编排层，每接一个框架就要改本函数。
/// 现在统一交给 [`mangosteen_device::EnginePlugin::probe`]。
fn probe_local_health(registry: &mangosteen_device::EngineRegistry, plan: &DeploymentPlan) -> bool {
    let Some(local) = plan.local.as_ref() else {
        return false;
    };
    // 用部署时记录的引擎，而不是当前配置：用户中途切换引擎时，
    // 已在跑的实例仍应按它**启动时**的引擎探活，否则会瞬间全判失败。
    let engine = local.engine;
    match registry.get(engine) {
        Some(plugin) => plugin.probe(local),
        // 插件不存在（引擎已被移除）→ 无法探活，如实返回 false
        None => false,
    }
}

/// 引擎类型相关的小工具：端口冲突判定与端口回绕。
///
/// 保留在此是因为「端口被占用就换一个再试一次」是编排策略，不是 HTTP 细节。
fn next_port(current: u16, cfg: &LocalConfig) -> u16 {
    if current >= cfg.port_range_end {
        cfg.port_range_start
    } else {
        current + 1
    }
}

fn is_port_conflict(e: &MnError) -> bool {
    match e {
        MnError::EngineLaunch { reason, .. } => reason.contains("已被占用"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mangosteen_core::config::SchedulerConfig;

    #[test]
    fn scheduler_config_override() {
        let base = SchedulerConfig::default();
        let c = scheduler_config_with_overrides(&base, Some(3.5));
        assert_eq!(c.max_price_per_hour, Some(3.5));
        // 未指定时保持原值
        let c2 = scheduler_config_with_overrides(&base, None);
        assert_eq!(c2.max_price_per_hour, base.max_price_per_hour);
    }

    #[test]
    fn next_port_wraps_within_range() {
        let cfg = LocalConfig {
            port_range_start: 11500,
            port_range_end: 11502,
            ..Default::default()
        };
        assert_eq!(next_port(11500, &cfg), 11501);
        assert_eq!(next_port(11502, &cfg), 11500);
    }

    #[test]
    fn port_conflict_detection() {
        let e = MnError::EngineLaunch {
            binary: PathBuf::from("llama-server"),
            reason: "端口 11435 已被占用。若之前有残留...".into(),
        };
        assert!(is_port_conflict(&e));

        let other = MnError::EngineLaunch {
            binary: PathBuf::from("llama-server"),
            reason: "未找到 llama-server".into(),
        };
        assert!(!is_port_conflict(&other));
    }
}
