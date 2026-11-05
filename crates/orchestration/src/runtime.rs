//! 部署编排核心运行时：承载 `Orchestrator` 状态与各职责模块装配。
//!
//! 这是公开模块 `orchestration::orchestrator` 的物理实现文件。具体职责拆分在
//! `deploy`、`lifecycle`、`query`、`preview`、`engine`、`cloud` 等同级短名文件；
//! 本文件定义运行状态及共享领域结构，不实现 HTTP 或协议转换。

use std::path::PathBuf;
use std::time::Duration;

use mangosteen_base::state::{DeploymentRecord, DeploymentStatus, StateStore};
use mangosteen_base::{ConfigStore, ModelStore};
use mangosteen_core::config::{Config, SchedulerConfig};
use mangosteen_core::plan::ComputeOrigin;
use mangosteen_core::policy::SelectionConstraints;
use mangosteen_core::{MnError, MnResult};
use mangosteen_device::engine::LlamaCppEngine;
use mangosteen_provider::orchestrator::CloudOrchestrator;
use mangosteen_provider::TunnelManager;

use crate::assess::{Assessor, DeploymentRequest, Evaluated};
use crate::backend::BackendTarget;
use crate::deployment::{
    assessed_detail, default_estimate_hours, sanitize, CloudPreview, DeploymentPreview,
    DeviceBrief, PreparedCloud, PreparedDeployment,
};
use crate::scheduler::{Decision, Scheduler};

#[path = "cloud.rs"]
mod cloud_ops;
pub use cloud_ops::InstanceSshInfo;
#[path = "deploy.rs"]
mod deploy;
#[path = "engine.rs"]
mod engine_admin;
#[path = "lifecycle.rs"]
mod lifecycle;
#[path = "preview.rs"]
mod preview;
#[path = "query.rs"]
mod query;

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
    /// 配置快照。
    ///
    /// 内部锁 + Arc：读侧 clone Arc（零成本共享），设置页写盘后由
    /// `replace_config` 原子换新——读侧借用永不跨锁。
    config: std::sync::RwLock<std::sync::Arc<Config>>,
    assessor: Assessor,
    /// 调度器。
    ///
    /// 内部锁：设置页保存时整体重建，读侧只短暂持读锁。
    scheduler: std::sync::RwLock<Scheduler>,
    local_engine: LlamaCppEngine,
    /// 引擎插件注册表。
    ///
    /// 编排核心层·引擎管理的落地形态：编排层只按 `config.local.engine`
    /// 取对应插件，**不写任何引擎专属分支**。新增推理框架只需在
    /// `mangosteen_device::EngineRegistry::with_builtin` 里登记，
    /// 本层一行不动（基线「推理引擎插件化原则」）。
    engines_registry: mangosteen_device::EngineRegistry,
    /// 云端编排器。
    ///
    /// 内部锁：与 `device_tunnels` 同理——部署长操作绝不持上层锁，
    /// 方法一律 `&self`，只有写入时短暂持锁。
    cloud: std::sync::Mutex<Option<CloudOrchestrator>>,
    /// 自建节点（设置页「设备列表」）的 SSH 隧道。
    ///
    /// 与云端实例分开管理：自建设备不经过厂商接口，即使一个云服务商都
    /// 没配置（`cloud == None`），设备部署也要能用。
    device_tunnels: std::sync::Mutex<TunnelManager>,
    /// 远端推理路由表：`模型ID → 本地端口`。
    ///
    /// ## 为什么改为内部锁（致命缺陷修复）
    ///
    /// 它与 `device_tunnels` 是编排器里仅有的**可变**状态。此前
    /// `Gateway = Arc<Mutex<Orchestrator>>`，HTTP 层每次请求都要拿这把
    /// 全局锁，而部署线程会持锁跑完整条 `prepare_with_progress`
    /// （创建实例 → 等开机 → SSH 免密 → 下载权重，1~3 分钟）。后果：
    /// 部署期间 `/api/status`、`/api/models/jobs` 等**全部**管理接口
    /// 排队阻塞，UI 一直显示「连接中」，对话转发也已受到牵连。
    ///
    /// 把可变状态下沉为细粒度内部锁后，编排器方法一律 `&self`，
    /// HTTP 层用 `Arc<Orchestrator>` 共享——部署长操作只在自己写入时
    /// 短暂持有这两把锁，管理接口与界面轮询不再被拖住。
    remote_inference_routes: std::sync::Mutex<std::collections::BTreeMap<String, u16>>,
    state: StateStore,
    models: ModelStore,
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
    /// SSH 登录信息缓存：`(provider:instance_id) → (时间, 信息)`。
    ///
    /// UI 的「登陆方式」会随部署页轮询查询，每次都打厂商 API 既慢又可能
    /// 触发限流。这里缓存 60 秒：端口变化（停机再开机重新映射）最迟
    /// 60 秒内反映，与实时性需求匹配。
    ssh_info_cache:
        std::sync::Mutex<std::collections::BTreeMap<String, (std::time::Instant, InstanceSshInfo)>>,
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
        let scheduler =
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

        let mut device_tunnels = TunnelManager::new(
            config.local.port_range_start,
            config.local.port_range_end,
            "ssh",
            None,
        )?;
        let mut remote_inference_routes = std::collections::BTreeMap::new();
        let mut remotes = config.remote_inferences.clone();
        if let Some(remote) = config.remote_inference.clone() {
            remotes.push(remote);
        }
        for remote in remotes {
            let password = remote
                .password
                .resolve(mangosteen_platform::paths::real_env)
                .ok();
            let target = mangosteen_device::remote::SshTarget {
                host: remote.host,
                port: remote.port,
                user: remote.user,
                password,
                identity_file: None,
            };
            let spec = mangosteen_provider::orchestrator::tunnel_spec_for(
                &target,
                remote.remote_port,
                false,
                Some(remote.remote_port),
            );
            let local = device_tunnels
                .establish(&spec, Duration::from_secs(30))?
                .local_port;
            remote_inference_routes.insert(remote.model_id, local);
        }

        Ok(Self {
            config: std::sync::RwLock::new(std::sync::Arc::new(config)),
            assessor,
            scheduler: std::sync::RwLock::new(scheduler),
            // 注册表在此构造：内置插件清单只在 device crate 里维护一份，
            // 编排层不关心有哪些引擎、各自怎么启动。
            engines_registry: mangosteen_device::EngineRegistry::with_builtin(
                local_engine.clone(),
                mangosteen_device::OllamaEngine::new(),
            ),
            local_engine,
            cloud: std::sync::Mutex::new(cloud),
            device_tunnels: std::sync::Mutex::new(device_tunnels),
            remote_inference_routes: std::sync::Mutex::new(remote_inference_routes),
            state,
            models,
            req_seq: std::sync::atomic::AtomicU64::new(0),
            engine_version_cache: std::sync::Mutex::new(None),
            config_store: None,
            ssh_info_cache: std::sync::Mutex::new(std::collections::BTreeMap::new()),
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

    /// 配置快照（Arc 共享，读侧零阻塞）。
    pub fn config(&self) -> std::sync::Arc<Config> {
        self.config.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// 取调度器读锁。
    pub(crate) fn scheduler(&self) -> std::sync::RwLockReadGuard<'_, Scheduler> {
        self.scheduler.read().unwrap_or_else(|e| e.into_inner())
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

    /// 取隧道表锁。
    ///
    /// 细粒度锁的**唯一入口**：部署长操作（创建实例、等开机、下载权重）
    /// 只在真正操作隧道时短暂持有，绝不在整个部署期间占着不放，
    /// 否则管理 API 会被拖成「一直转圈」。
    pub(crate) fn tunnels(
        &self,
    ) -> std::sync::MutexGuard<'_, TunnelManager> {
        // 锁中毒说明此前有线程在持锁期间 panic。隧道表本身只是一张
        // 「远端端口 → 本地转发」的映射，中毒后继续用它比直接崩掉整条
        // 部署链路代价小得多：`into_inner` 取出残余数据继续服务。
        self.device_tunnels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// 取云端编排器锁（细粒度内部锁，见字段注释）。
    pub(crate) fn cloud(
        &self,
    ) -> std::sync::MutexGuard<'_, Option<CloudOrchestrator>> {
        self.cloud.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 取远端路由表锁。
    pub(crate) fn routes(
        &self,
    ) -> std::sync::MutexGuard<'_, std::collections::BTreeMap<String, u16>> {
        self.remote_inference_routes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// 用新配置替换内存中的配置。
    ///
    /// 设置页写盘后必须调用它，否则用户改完要重启才生效——
    /// 这是「所见即所得」的最低要求。
    ///
    /// 只替换可热更新的部分：调度策略、本地参数、存储默认源、服务商开关。
    /// **不重建**引擎句柄与云端编排器（重建会中断正在服务的推理），
    /// 那些字段的变更需要重启，UI 会明确提示。
    pub fn replace_config(&self, cfg: Config) {
        {
            let mut sched = self.scheduler.write().unwrap_or_else(|e| e.into_inner());
            *sched =
                Scheduler::new(self.assessor.clone_for_scheduler(), cfg.scheduler.clone());
            // 端口区间可能被改过，重新设置（失败则保留旧值，不因设置页写错而崩溃）
            let _ = sched.set_port_range(cfg.local.port_range_start, cfg.local.port_range_end);
        }
        *self.config.write().unwrap_or_else(|e| e.into_inner()) = std::sync::Arc::new(cfg);
    }

    /// 用当前配置重建云端驱动注册表（设置页更新厂商密钥/参数后调用）。
    ///
    /// 只换驱动：驱动无状态、按调用独立发请求，替换不影响活跃部署与
    /// 隧道。返回 `(是否已生效, 跳过的服务商及原因)`：
    ///
    /// * 重建结果为空（如唯一服务商的密钥被清空）→ **保留旧注册表**，
    ///   避免活跃部署的生命周期操作因注册表突然变空而失效；
    /// * 本进程启动时没有云能力（`cloud == None`）→ 无法热建，需重启。
    pub fn reload_cloud_drivers(&self) -> (bool, Vec<(String, String)>) {
        let build = mangosteen_provider::build_registry(&self.config());
        if build.registry.is_empty() {
            return (false, build.skipped);
        }
        let mut cloud = self.cloud();
        match cloud.as_mut() {
            Some(o) => {
                o.replace_registry(build.registry);
                (true, build.skipped)
            }
            // 启动时无云能力（无云端 TunnelManager 等），无法安全热建
            None => (false, build.skipped),
        }
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
}
