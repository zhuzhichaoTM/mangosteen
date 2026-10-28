//! 网关门面：系统唯一功能收口。
//!
//! 归属：**API 网关层**。
//!
//! ## 为什么需要门面
//!
//! 「网关唯一收口原则」要求上层（CLI / TUI / 任何客户端）不得直连本地引擎或
//! 云厂商 API。本模块提供这条约束的**唯一入口**：
//!
//! * [`Gateway`] 是 [`Orchestrator`] 的类型别名——部署编排与运行生命周期
//!   的实体位于第三层 `mangosteen-orchestration`，网关不再持有执行状态；
//! * [`GatewayBuilder`] 负责「读配置 → 采集硬件 → 建各组件」的装配，
//!   CLI 不必知道下层有哪些 crate。
//!
//! ```text
//! CLI/TUI/HTTP ──▶ gateway::Gateway ──▶ orchestration::Orchestrator
//!                                          ├── Assessor（资源评估）
//!                                          ├── Scheduler（决策与选点）
//!                                          ├── device::EngineRegistry（引擎）
//!                                          └── provider::CloudOrchestrator（节点）
//! ```
//!
//! ## 部署执行顺序
//!
//! ```text
//! 1. 评估 → 2. 节点决策(基于用户配置决定是本地还是云端部署) → 3. 执行
//! 4. 落台账（每步状态变更都立即持久化）
//! ```
//!
//! 顺序与阶段语义由 `mangosteen_orchestration::orchestrator` 实现，
//! 本模块只负责装配与转发。

use std::path::PathBuf;
use std::time::Duration;

use mangosteen_base::state::StateStore;
use mangosteen_base::ModelStore;
use mangosteen_core::config::{Config, SchedulerConfig};
use mangosteen_core::MnResult;
use mangosteen_device::LlamaCppEngine;
pub use mangosteen_orchestration::deployment::{
    CloudCandidateBrief, CloudPreview, DeploymentPreview, LocalPreview, PreparedDeployment,
    PreparedLocal,
};
use mangosteen_orchestration::Assessor;
pub use mangosteen_orchestration::{
    ActiveDeployment, CloudDeploymentRef, CloudMetricsSnapshot, GatewayStatus, Orchestrator,
};
use mangosteen_provider::orchestrator::CloudOrchestrator;

/// 网关门面：第二层只保留编排核心的类型别名，HTTP 层通过它统一收口。
pub type Gateway = Orchestrator;

/// 从配置与平台能力组装一个网关。
///
/// 这是 CLI 唯一需要的构造入口——它把「读配置 → 采集硬件 → 建各组件」
/// 这些装配细节收在网关层内部，CLI 不必知道下层有哪些 crate。
pub struct GatewayBuilder {
    config: Config,
    engine_path: Option<PathBuf>,
    log_dir: Option<PathBuf>,
    state_path: Option<PathBuf>,
    model_index_path: Option<PathBuf>,
}

impl GatewayBuilder {
    /// 用配置构造 builder。
    pub fn new(config: Config) -> Self {
        Self {
            config,
            engine_path: None,
            log_dir: None,
            state_path: None,
            model_index_path: None,
        }
    }

    /// 覆盖引擎路径。
    pub fn engine_path(mut self, p: Option<PathBuf>) -> Self {
        self.engine_path = p;
        self
    }

    /// 覆盖日志目录。
    pub fn log_dir(mut self, p: Option<PathBuf>) -> Self {
        self.log_dir = p;
        self
    }

    /// 覆盖台账路径。
    pub fn state_path(mut self, p: PathBuf) -> Self {
        self.state_path = Some(p);
        self
    }

    /// 覆盖模型台账路径。
    pub fn model_index_path(mut self, p: PathBuf) -> Self {
        self.model_index_path = Some(p);
        self
    }

    /// 构建。
    pub fn build(self) -> MnResult<Gateway> {
        let env = &mangosteen_platform::paths::real_env as &dyn Fn(&str) -> Option<String>;

        let log_dir = self
            .log_dir
            .unwrap_or_else(|| mangosteen_platform::paths::log_dir(env));
        mangosteen_platform::fs::ensure_dir(&log_dir)?;

        let state_path = self
            .state_path
            .unwrap_or_else(|| mangosteen_platform::paths::state_file(env));
        let model_index_path = self
            .model_index_path
            .unwrap_or_else(|| mangosteen_platform::paths::model_index_file(env));

        // 硬件采集（失败也继续，用保守兜底）
        let probe = mangosteen_platform::hardware::probe();
        let assessor = Assessor::new(probe.profile);

        let engine_path = self
            .engine_path
            .or_else(|| self.config.local.engine_path.clone());
        let local_engine = LlamaCppEngine::from_config(engine_path, Some(log_dir.clone()))
            .with_engine_env(self.config.local.engine_env.clone())
            .with_auto_library_path(self.config.local.auto_library_path);

        // 云端编排器：只有存在启用的服务商时才构造
        let cloud = if self.config.has_cloud_capability() {
            let build = mangosteen_provider::build_registry(&self.config);
            if build.registry.is_empty() {
                None
            } else {
                let mut pref = mangosteen_core::policy::default_gpu_preference();
                for (k, v) in &self.config.scheduler.gpu_preference {
                    pref.insert(k.clone(), *v);
                }
                let selector = mangosteen_core::policy::NodeSelector::new(
                    self.config.scheduler.weights,
                    pref,
                )?
                .with_strategy(self.config.scheduler.strategy);
                let tunnels = mangosteen_provider::TunnelManager::new(
                    self.config.local.port_range_start,
                    self.config.local.port_range_end,
                    "ssh",
                    Some(log_dir.clone()),
                )?;
                Some(CloudOrchestrator::new(
                    build.registry,
                    selector,
                    tunnels,
                    mangosteen_provider::OrchestratorConfig {
                        provision_timeout: Duration::from_secs(
                            self.config.scheduler.provision_timeout_secs,
                        ),
                        start_timeout: Duration::from_secs(
                            self.config.scheduler.provision_timeout_secs,
                        ),
                        // 密码供隧道认证用（优云这类只支持密码登录的厂商必需）
                        ssh_password: self
                            .config
                            .cloud
                            .resolved_password(mangosteen_platform::paths::real_env)
                            .unwrap_or(None),
                        remote_workdir: self.config.cloud.remote_workdir.clone(),
                        llama_server_url: self.config.cloud.llama_server_url.clone(),
                        ..Default::default()
                    },
                ))
            }
        } else {
            None
        };

        let guard = mangosteen_base::AccessGuard::from_config(&self.config.storage);
        let models = ModelStore::new(model_index_path, guard, self.config.storage.verify_hash);
        let state = StateStore::new(state_path);

        mangosteen_orchestration::Orchestrator::new(
            self.config,
            assessor,
            local_engine,
            cloud,
            state,
            models,
        )
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
