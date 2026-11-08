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
    CloudCandidateBrief, CloudInstanceBrief, CloudPreview, DeploymentPreview, PreparedDeployment,
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
    /// 配置持久化目标。`None` = 平台默认（真实用户配置路径）。
    ///
    /// 必须可覆盖：内置模板补齐与设置页保存都会写配置，若测试注入的
    /// 配置走平台默认路径，就会覆盖开发者本机的真实 `config.toml`。
    config_store_override: Option<mangosteen_base::config::ConfigStore>,
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
            config_store_override: None,
        }
    }

    /// 覆盖配置持久化目标。
    ///
    /// **测试必须调用**：否则内置模板补齐会把测试配置写进真实用户配置。
    pub fn config_store(mut self, store: mangosteen_base::config::ConfigStore) -> Self {
        self.config_store_override = Some(store);
        self
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
    pub fn build(mut self) -> MnResult<Gateway> {
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

        // 云端编排器：始终构造（即使当前无启用的服务商），以便后续
        // 通过 UI 配置密钥后能热重建驱动（reload_cloud_drivers 需要
        // 一个非 None 的 CloudOrchestrator 才能 replace_registry）。
        let build = mangosteen_provider::build_registry(&self.config);
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
        let cloud = Some(CloudOrchestrator::new(
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
                ssh_password: self
                    .config
                    .cloud
                    .resolved_password(mangosteen_platform::paths::real_env)
                    .unwrap_or(None),
                remote_workdir: self.config.cloud.remote_workdir.clone(),
                llama_server_url: self.config.cloud.llama_server_url.clone(),
                ..Default::default()
            },
        ));

        let guard = mangosteen_base::AccessGuard::from_config(&self.config.storage);
        let models = ModelStore::new(model_index_path, guard, self.config.storage.verify_hash);
        let state = StateStore::new(state_path);

        // 启动时清理终态垃圾记录（destroyed/failed）。这些记录不占资源、
        // 不参与复用，留着只会：① 让 UI 按模型匹配台账时误命中旧失败记录
        // （instance_id 为空却优先），导致实例 ID / 登陆方式显示错误；
        // ② 让台账随使用无限膨胀。stopped 记录必须保留——它是「下次部署
        // 复用同一台机器」的唯一依据。清理失败不影响启动。
        match state.prune_terminal() {
            Ok(n) if n > 0 => eprintln!("[state] ✓ 已清理 {n} 条终态历史记录（destroyed/failed）"),
            Err(e) => eprintln!("[state] 清理终态记录失败（忽略，不影响启动）：{e}"),
            _ => {}
        }

        // 首次启动把内置配置模板灌入用户配置（幂等：已存在的不覆盖）。
        //
        // 模板是部署的唯一真源，必须落在用户配置里才能被设置页编辑、
        // 被部署页下拉框索引。这里只补「缺失」的槽位，绝不覆盖用户已改过
        // 的内容——否则升级会把用户的自定义配置冲掉。
        //
        // ## 落盘路径必须跟随本实例的配置来源
        //
        // 早期实现无条件用 `ConfigStore::platform_default(None)`——那是
        // **真实用户配置路径**，与调用方给的 cfg 无关。e2e 测试注入的
        // 配置（临时 models_dir、已清空的 providers）因此被写进了开发者
        // 本机的 `~/Library/Application Support/mangosteen/config.toml`：
        // 跑一次测试，真实配置就被改成「模型库指向 /var/folders 临时目录、
        // 云服务商全部消失」。表现为「设置莫名丢失」「部署时找不到云端节点」。
        //
        // 现在改为优先沿用调用方传入的 store；仅在未指定时（生产默认路径）
        // 才回落到平台默认。测试侧只要传入自己的 store，就绝不会触碰真实文件。
        {
            let mut config = self.config.clone();
            if seed_builtin_templates(&mut config) {
                let store = self
                    .config_store_override
                    .clone()
                    .unwrap_or_else(|| mangosteen_base::config::ConfigStore::platform_default(None));
                match store.save(&config) {
                    Ok(_) => eprintln!("[config] ✓ 已写入内置模型配置模板（仅补充缺失项）"),
                    Err(e) => eprintln!("[config] 写入内置模板失败（忽略，使用内存配置）：{e}"),
                }
                self.config = config;
            }
        }

        let orch = mangosteen_orchestration::Orchestrator::new(
            self.config,
            assessor,
            local_engine,
            cloud,
            state,
            models,
        )?;
        // 把同一个 store 交给编排器：设置页/模板接口的保存走它，
        // 与上面的种子写入保持同一目标文件，避免「一处写真实路径、
        // 一处写测试路径」这种割裂。
        Ok(match self.config_store_override {
            Some(store) => orch.with_config_store(store),
            None => orch,
        })
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

/// 把内置模型配置模板补进配置（幂等）。
///
/// ## 为什么只补不覆盖
///
/// 模板是用户可编辑的部署真源。升级时若直接覆盖，用户辛苦调好的
/// 上下文/仓库会被冲掉。因此只在「该模型或该槽位还不存在」时写入，
/// 已存在的一律保持用户版本。
///
/// 返回是否发生了改动（true 表示需要落盘）。
fn seed_builtin_templates(cfg: &mut mangosteen_core::config::Config) -> bool {
    let mut changed = false;
    for (model, slot, display, text) in mangosteen_core::template::BUILTIN_TEMPLATES {
        let entry = cfg
            .model_whitelist
            .entry((*model).to_string())
            .or_insert_with(|| mangosteen_core::config::WhitelistedModel {
                display_name: (*display).to_string(),
                source_spec: String::new(),
                template: (*slot).to_string(),
                templates: Default::default(),
            });
        if entry.display_name.trim().is_empty() {
            entry.display_name = (*display).to_string();
            changed = true;
        }
        // 只在缺失时插入：用户已定制的版本必须保留
        if !entry.templates.contains_key(*slot) {
            entry.templates.insert((*slot).to_string(), (*text).to_string());
            changed = true;
        }
    }
    changed
}
