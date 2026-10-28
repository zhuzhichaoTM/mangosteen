//! 网关门面：系统唯一功能收口。
//!
//! 归属：**API 网关层**。
//!
//! ## 为什么需要门面
//!
//! 「网关唯一收口原则」要求上层（CLI / TUI / 任何客户端）不得直连本地引擎或
//! 云厂商 API。`Gateway` 就是这条约束的物理实现：它持有全部下层能力，
//! 对外只暴露**语义化方法**（部署、停止、查询、推理转发）。
//!
//! ```text
//! CLI/TUI ──▶ Gateway ──┬── Assessor（资源评估引擎）
//!                       ├── Scheduler（调度决策器）
//!                       ├── LlamaCppEngine（本地链路）
//!                       ├── CloudOrchestrator（云端链路）
//!                       └── StateStore / ModelStore（基础能力层）
//! ```
//!
//! ## 部署执行顺序
//!
//! ```text
//! 1. 评估 → 2. 节点决策(基于用户配置决定是本地还是云端部署) → 3. 执行
//! 4. 落台账（每步状态变更都立即持久化）
//! ```

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use mangosteen_base::state::{DeploymentRecord, DeploymentStatus, StateStore};
use mangosteen_base::ModelStore;
use mangosteen_core::config::{Config, LocalConfig, SchedulerConfig};
use mangosteen_core::plan::{ComputeOrigin, DeploymentPlan};
use mangosteen_core::policy::SelectionConstraints;
use mangosteen_core::{MnError, MnResult};
use mangosteen_device::engine::{EngineHandle, LlamaCppEngine};
use mangosteen_provider::orchestrator::CloudOrchestrator;

use crate::protocol::BackendTarget;

mod cloud_ops;
mod deploy;
mod engine_admin;
mod lifecycle;
mod preview;
mod query;
use mangosteen_orchestration::assess::{Assessor, DeploymentRequest, Evaluated};
use mangosteen_orchestration::scheduler::{Decision, Scheduler};

/// 引擎版本探测的时间预算。
///
/// 为什么是 2 秒：正常引擎（假引擎、修好的引擎）毫秒级返回；2 秒足够
/// 覆盖冷启动抖动，又不会让 HTTP 请求明显卡顿。真正慢的引擎（加载全部
/// 动态库要 20~30 秒）会走「未知」分支，由调用方保守处理。
const ENGINE_PROBE_BUDGET: Duration = Duration::from_secs(2);

/// 「准备」阶段的产物。
///
/// 把「评估 + 选择 + 配置」与「启动」分开，是为了让**下载夹在中间**：
/// UI 对未下载的模型要走「评估 → 选设备 → 配置 → 下载 → 启动」，
/// 先让用户看到跑不跑得动，再花时间下几 GB。
#[derive(Debug)]
pub enum PreparedDeployment {
    /// 决策为本地：参数已定、台账已落，等待下载（若需要）后启动。
    ///
    /// `Box` 与 [`PreparedDeployment::Done`] 同理：两个变体体积相差一个数量级。
    Local(Box<PreparedLocal>),
    /// 决策为云端：云端链路不需要本地文件，已在 prepare 内完成。
    ///
    /// `Box` 是因为 `DeploymentPlan` 远大于 `PreparedLocal`（含云端规格、
    /// 引擎参数等整块配置）；不装箱会让每次 `prepare` 的返回值都搬几百字节。
    Done(Box<mangosteen_core::plan::DeploymentPlan>),
}

/// 本地部署的「已准备」状态，等待 [`Gateway::commit_local`] 启动。
///
/// 为什么带 `evaluated`：启动后要按它回填/展示模型规格，而重新评估一次
/// 既浪费（远端预取要发 HTTP）又可能与用户刚看到的结果不一致。
///
/// 为什么带 `req`：启动失败时要判断「是否允许降级云端」，而
/// `req.target.forces_local()` 是唯一的判据——静默降级会直接产生计费。
#[derive(Debug)]
pub struct PreparedLocal {
    /// 部署 ID（台账里已有 `provisioning` 记录）
    pub id: String,
    /// 评估结论（含资源需求分解，供 UI 展示）
    pub evaluated: Evaluated,
    /// 本地启动参数
    pub plan: mangosteen_core::plan::LocalPlan,
    /// 原始部署诉求（降级判据需要）
    pub req: DeploymentRequest,
}

impl PreparedLocal {
    /// 补上模型文件的**实际落盘路径**。
    ///
    /// ## 为什么必须有这一步
    ///
    /// `prepare` 发生在下载**之前**，此时文件还不存在：远端评估
    /// （`evaluate_remote`）和粗估（`evaluate_estimated`）构造的
    /// `Evaluated` 里 `model_path` 是空的，`LocalPlan::from_assessment`
    /// 照抄这个空值。若不在启动前补上，引擎会收到空路径并报
    /// 「模型文件不存在：」——冒号后面什么都没有，极难排查。
    ///
    /// 调用方（[`crate::pulljobs`] 的 commit 钩子）知道下载落地位置，
    /// 在启动前调本方法写回。
    ///
    /// 模型已在本地时（`evaluate_file`）路径本来就是对的，
    /// 重复设置成同一路径无害。
    pub fn with_model_path(mut self, path: impl Into<String>) -> Self {
        let p = path.into();
        if !p.is_empty() {
            self.plan.model_path = p.clone();
            self.evaluated.model_path = std::path::PathBuf::from(p);
        }
        self
    }
}

/// 取错误信息的第一行（弹窗里只需要一句能读懂的话）。
fn first_line(msg: &str) -> String {
    msg.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("引擎无法启动")
        .trim()
        .to_string()
}

/// 「模型评估」阶段的结果文字。
///
/// 必须包含**资源占用明细**：这是用户决定要不要继续部署的唯一依据。
/// 只写「评估完成」等于没说——用户想问的正是「要占多少内存 / 跑不跑得动」。
///
/// 用 [`ResourceRequirement::describe`] 而不是自己拼：那个函数已被 CLI
/// 复用，两处格式一旦分叉，用户会看到同一份数据两种说法。
pub fn assessed_detail(evaluated: &Evaluated) -> String {
    let a = &evaluated.assessment;
    format!(
        "{} · {} · ctx={} · {}　| 本地{}",
        evaluated.spec.name,
        a.placement.describe(evaluated.spec.n_layers),
        a.context_len,
        a.requirement.describe(),
        if a.placement.is_local_satisfiable() {
            "可行"
        } else {
            "不可行"
        }
    )
}

/// 「设备配置」阶段的结果文字。
///
/// 把真正会传给引擎的参数摊开写：用户排障时要对照这些值，
/// 含糊的「配置完成」没法用来核对。
fn configured_detail(plan: &mangosteen_core::plan::LocalPlan) -> String {
    format!(
        "本地 · {} 线程 · 批 {} · 上下文 {} · 端口 {} · 引擎 {} · {} · KV {}/{}",
        plan.threads,
        plan.batch_size,
        plan.context_size,
        plan.port,
        plan.engine.display_name(),
        if plan.gpu_layers == 0 {
            "纯 CPU".to_string()
        } else {
            format!("{} 层到 GPU", plan.gpu_layers)
        },
        plan.kv_cache_type_k.as_cli_value(),
        plan.kv_cache_type_v.as_cli_value(),
    )
}

/// 网关整体状态快照（供 CLI / TUI 展示）。
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
    pub status: DeploymentStatus,
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
    pub status: DeploymentStatus,
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
    pub origin: ComputeOrigin,
    /// 状态
    pub status: DeploymentStatus,
    /// 访问端口
    pub port: Option<u16>,
    /// 是否就绪
    pub ready: bool,
}

/// 网关门面。
pub struct Gateway {
    config: Config,
    assessor: Assessor,
    scheduler: Scheduler,
    local_engine: LlamaCppEngine,
    /// 引擎插件注册表。
    ///
    /// 编排核心层·引擎管理的落地形态：网关只按 `config.local.engine`
    /// 取对应插件，**不写任何引擎专属分支**。新增推理框架只需在
    /// `mangosteen_device::EngineRegistry::with_builtin` 里登记，
    /// 本文件一行不动（基线「推理引擎插件化原则」）。
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
    /// 为什么必须缓存：`llama-server --version` 会加载全部动态库与后端，
    /// 在部分安装（如 Homebrew 版）上要 **20~30 秒**。`status` 是高频调用
    /// （UI 每几秒轮询一次），每次都探测会让界面像卡死一样。
    ///
    /// 缓存键含路径：用户改了 `local.engine_path` 后会重新探测，
    /// 不会一直返回旧引擎的版本。
    ///
    /// 值区分成功与失败：`Err(原因)` 记录「探测过且失败」，这样预估接口
    /// 不必再跑一次 27 秒的命令就能说出「引擎坏了」。
    engine_version_cache: std::sync::Mutex<Option<(PathBuf, Result<String, String>)>>,
    /// 配置文件存储（设置页写盘用）。
    ///
    /// `None` 表示由测试或外部构造时未提供——此时设置页只能读不能写，
    /// 而不是让整个网关起不来。
    config_store: Option<mangosteen_base::ConfigStore>,
}

impl Gateway {
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
            // 注册表在此构造：内置插件清单只在 local crate 里维护一份，
            // 网关不关心有哪些引擎、各自怎么启动。
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
    pub fn with_config_store(mut self, store: mangosteen_base::ConfigStore) -> Self {
        self.config_store = Some(store);
        self
    }

    /// 取配置存储。未挂载时返回一个平台默认实例，保证设置页可用。
    pub fn config_store(&self) -> mangosteen_base::ConfigStore {
        self.config_store
            .clone()
            .unwrap_or_else(|| mangosteen_base::ConfigStore::platform_default(None))
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

    /// 下一个请求序号。
    pub fn next_seq(&self) -> u64 {
        self.req_seq
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    // ---------------------------------------------------------------- 部署

    // ---------------------------------------------------------------- 停止 / 查询
}

/// 云端费用的默认预估时长（小时）。
///
/// 为什么是 1 小时：个人用户跑一次推理调试通常在这个量级。给「按小时」
/// 口径是为了让用户能自己乘——不假装知道他要跑多久。
fn default_estimate_hours() -> u32 {
    1
}

/// 部署预估结果。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct DeploymentPreview {
    /// 模型名
    pub model: String,
    /// 完整模型 ID
    pub model_id: String,
    /// 本地是否可行
    pub local_viable: bool,
    /// 本地不可行的具体原因（本地可行时为 `None`）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_reason: Option<String>,
    /// 将会使用的算力来源
    pub origin: ComputeOrigin,
    /// 本地方案（本地可行时）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local: Option<LocalPreview>,
    /// 云端方案与费用（需要上云且已选到节点时）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cloud: Option<CloudPreview>,
    /// 备选节点（按打分排序，最多 5 个）
    pub cloud_candidates: Vec<CloudCandidateBrief>,
    /// 选点失败原因（无可用服务商/无满足约束的规格）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cloud_error: Option<String>,
    /// 用户选择了「本地算力」但本地不可行 → 部署会**停止**，不会上云。
    ///
    /// 为什么要单独一个字段：此时 `local_viable=false` 且 `cloud` 可能有值
    /// （因为我们仍会算一次云端报价供用户参考「改选云端要花多少钱」），
    /// 前端若按「cloud 有值 ⇒ 会创建实例」渲染，就会让用户误以为点确认
    /// 会上云。实际行为是拒绝部署。
    pub blocked: bool,
}

/// 本地方案预览。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct LocalPreview {
    /// 落位形态
    pub placement: String,
    /// 上下文
    pub context_len: u32,
    /// KV 精度
    pub kv_cache: String,
    /// 量化
    pub quant: String,
    /// 权重字节
    pub weights_bytes: u64,
    /// KV 字节
    pub kv_cache_bytes: u64,
    /// 计算缓冲字节
    pub compute_bytes: u64,
    /// 运行时开销字节
    pub overhead_bytes: u64,
    /// 合计
    pub total_bytes: u64,
    /// 线程数
    pub threads: u32,
    /// 加速卡预算
    pub accelerator_budget_bytes: u64,
    /// 主机内存预算
    pub host_budget_bytes: u64,
    /// 提示
    pub warnings: Vec<String>,
}

impl LocalPreview {
    fn from_assessment(e: &Evaluated) -> Self {
        let a = &e.assessment;
        Self {
            placement: a.placement.describe(e.spec.n_layers),
            context_len: a.context_len,
            kv_cache: a.kv_cache.as_cli_value().to_string(),
            quant: a.quant.to_string(),
            weights_bytes: a.requirement.weights_bytes,
            kv_cache_bytes: a.requirement.kv_cache_bytes,
            compute_bytes: a.requirement.compute_buffer_bytes,
            overhead_bytes: a.requirement.runtime_overhead_bytes,
            total_bytes: a.requirement.total_bytes(),
            threads: a.threads,
            accelerator_budget_bytes: a.accelerator_budget_bytes,
            host_budget_bytes: a.host_budget_bytes,
            warnings: a.warnings.iter().map(|w| w.message()).collect(),
        }
    }
}

/// 云端方案预览（含费用）。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct CloudPreview {
    /// 服务商
    pub provider: String,
    /// 规格 ID
    pub spec_id: String,
    /// GPU 型号
    pub gpu_model: String,
    /// 卡数
    pub gpu_count: u32,
    /// 显存字节
    pub vram_bytes: u64,
    /// 地域
    pub region: String,
    /// 延迟
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u32>,
    /// 时价（元/小时）
    pub price_per_hour: f64,
    /// 预估时长（小时）
    pub estimate_hours: u32,
    /// 预估总额（元）
    pub estimate_total: f64,
    /// 该模型在云上需要的显存
    pub required_vram_bytes: u64,
}

/// 备选节点摘要。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct CloudCandidateBrief {
    /// 服务商
    pub provider: String,
    /// GPU 型号
    pub gpu_model: String,
    /// 地域
    pub region: String,
    /// 时价
    pub price_per_hour: f64,
    /// 显存
    pub vram_bytes: u64,
    /// 延迟
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u32>,
    /// 总分
    pub score_total: f64,
}

/// 通过 HTTP 探活一个本地部署（**经引擎插件分发**）。
///
/// 为什么不在这里判断引擎类型：探活端点与端口选择都是引擎专属知识
/// （llama.cpp 用 `plan` 端口 + `/health`；Ollama 用固定 11434 +
/// `/api/version`）。把这些写进网关，每接一个框架就要改本函数。
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

/// 判断错误是否为端口冲突。
fn is_port_conflict(e: &MnError) -> bool {
    match e {
        MnError::EngineLaunch { reason, .. } => reason.contains("已被占用"),
        _ => false,
    }
}

/// 端口区间内取下一个端口（回绕）。
fn next_port(current: u16, cfg: &LocalConfig) -> u16 {
    if current >= cfg.port_range_end {
        cfg.port_range_start
    } else {
        current + 1
    }
}

/// 截断原因文本（避免轨迹过长）。
fn short(s: &str) -> String {
    s.chars().take(80).collect()
}

/// 把模型名转成 ID 安全片段。
fn sanitize(s: &str) -> String {
    let out: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "model".to_string()
    } else {
        trimmed.chars().take(32).collect()
    }
}

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

        Gateway::new(self.config, assessor, local_engine, cloud, state, models)
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

    #[test]
    fn sanitize_produces_safe_ids() {
        assert_eq!(sanitize("qwen2.5-7b"), "qwen2.5-7b");
        assert_eq!(sanitize("Qwen 2.5 / 7B"), "Qwen-2.5---7B");
        assert_eq!(sanitize("!!!"), "model");
        assert!(sanitize(&"x".repeat(100)).len() <= 32);
    }

    #[test]
    fn short_truncates_long_reason() {
        let long = "x".repeat(200);
        assert_eq!(short(&long).chars().count(), 80);
        assert_eq!(short("ok"), "ok");
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
    fn scheduler_config_override() {
        let base = SchedulerConfig::default();
        let c = scheduler_config_with_overrides(&base, Some(3.5));
        assert_eq!(c.max_price_per_hour, Some(3.5));
        // 未指定时保持原值
        let c2 = scheduler_config_with_overrides(&base, None);
        assert_eq!(c2.max_price_per_hour, base.max_price_per_hour);
    }
}
