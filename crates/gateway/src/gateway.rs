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
use mangosteen_cloud::orchestrator::CloudOrchestrator;
use mangosteen_core::config::{Config, LocalConfig, SchedulerConfig};
use mangosteen_core::plan::{ComputeOrigin, DeploymentPlan};
use mangosteen_core::policy::SelectionConstraints;
use mangosteen_core::{MnError, MnResult};
use mangosteen_local::engine::{EngineHandle, LlamaCppEngine};

use crate::assess::{Assessor, DeploymentRequest, Evaluated};
use crate::protocol::BackendTarget;
use crate::scheduler::{Decision, Scheduler};

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
    /// `mangosteen_local::EngineRegistry::with_builtin` 里登记，
    /// 本文件一行不动（基线「推理引擎插件化原则」）。
    engines_registry: mangosteen_local::EngineRegistry,
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
            engines_registry: mangosteen_local::EngineRegistry::with_builtin(
                local_engine.clone(),
                mangosteen_local::OllamaEngine::new(),
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

    /// 取**已缓存**的引擎版本。
    ///
    /// 只读缓存，绝不在请求路径上执行 `--version`。原因：该命令在部分安装
    /// 上要 20~30 秒，而 `status` 是 UI 高频轮询的接口。首次访问返回
    /// `None`（界面显示「探测中」），预热线程稍后填充。
    ///
    /// 这样把「慢」从**每个请求**转移到**启动时一次**，且不阻塞启动。
    fn cached_engine_version(&self, path: &std::path::Path) -> Option<String> {
        self.engine_version_cache.lock().ok().and_then(|g| {
            g.as_ref()
                .filter(|(p, _)| p == path)
                .and_then(|(_, r)| r.as_ref().ok().cloned())
        })
    }

    /// 取缓存的引擎版本（供 HTTP 接口展示，不触发探测）。
    pub fn engine_version(&self) -> Option<String> {
        let path = self.local_engine.locate().ok()?;
        self.cached_engine_version(&path)
    }

    /// 判断引擎是否有问题——**带超时**，兼顾准确与响应速度。
    ///
    /// ## 三种输入，三种结论
    ///
    /// | 情况 | 耗时 | 结论 |
    /// |------|------|------|
    /// | 缓存已有结果 | 0 | 直接给出（可用 / 不可用+原因）|
    /// | 无缓存，引擎响应快 | < 超时 | 现场探测并回填缓存，准确 |
    /// | 无缓存，引擎很慢 | = 超时 | 返回「未知」，由调用方保守处理 |
    ///
    /// ## 为什么必须带超时而不是「只读缓存」
    ///
    /// 只读缓存会带来**危险的错报**：服务刚启动、缓存还空时，一个坏掉的
    /// 引擎会被当成「本地可用」，UI 告诉用户「本地可跑、不花钱」；用户
    /// 确认后引擎启动失败、自动降级云端开始计费 —— 用户被意外收费。
    ///
    /// 而纯同步探测又会超时（`--version` 在本机要 27 秒）。
    ///
    /// 折中：给探测设一个短上限（[`ENGINE_PROBE_BUDGET`]）。假引擎、修好的
    /// 引擎通常毫秒级返回，能立刻给出准确结论；只有真正慢的引擎才会走到
    /// 超时分支，此时如实回答「未知」而不是编造结论。
    fn engine_problem_bounded(&self) -> Option<String> {
        let path = self.local_engine.locate().ok()?;

        // 1) 缓存命中 → 零成本
        if let Some(result) = self.cached_engine_result(&path) {
            return match result {
                Ok(_) => None,
                Err(msg) => Some(first_line(&msg)),
            };
        }

        // 2) 现场探测，最多等 ENGINE_PROBE_BUDGET
        match self.probe_with_budget(&path) {
            Some(Ok(_)) => None,
            Some(Err(msg)) => Some(first_line(&msg)),
            // 超时：结论未知。返回一句可操作的话，让 UI 能解释清楚。
            None => Some(format!(
                "引擎状态尚未确认（版本探测超过 {} 秒未返回）。                 为避免误报「本地免费」后实际降级云端产生费用，请稍后重试预估",
                ENGINE_PROBE_BUDGET.as_secs()
            )),
        }
    }

    /// 取缓存中的完整结果（成功或失败）。
    fn cached_engine_result(&self, path: &std::path::Path) -> Option<Result<String, String>> {
        self.engine_version_cache
            .lock()
            .ok()?
            .as_ref()
            .filter(|(p, _)| p == path)
            .map(|(_, r)| r.clone())
    }

    /// 在预算时间内探测引擎版本；超时返回 `None`。
    ///
    /// 超时后不杀线程：让它自然结束并回填缓存，这样下一次调用就命中缓存。
    /// 代价是一个后台线程短暂存活，比「反复探测」划算得多。
    fn probe_with_budget(&self, path: &std::path::Path) -> Option<Result<String, String>> {
        let engine = self.local_engine.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let r = engine.probe_version().map_err(|e| e.to_string());
            let _ = tx.send(r);
        });

        match rx.recv_timeout(ENGINE_PROBE_BUDGET) {
            Ok(result) => {
                // 回填缓存，后续请求零成本
                if let Ok(mut g) = self.engine_version_cache.lock() {
                    *g = Some((path.to_path_buf(), result.clone()));
                }
                Some(result)
            }
            Err(_) => None,
        }
    }

    /// 取出「用于预热」的引擎副本与路径。
    ///
    /// 预热必须分三步：**取快照 → 锁外探测 → 写回结果**。
    /// 因为 `probe_version` 在部分安装上要 20~30 秒，若在持锁状态下执行，
    /// 期间所有需要网关锁的 API（sysinfo / settings / status）都会被阻塞，
    /// 表现就是「页面能打开但所有接口超时」。
    pub fn warm_up_snapshot(&self) -> Option<(LlamaCppEngine, PathBuf)> {
        let path = self.local_engine.locate().ok()?;
        if self.cached_engine_version(&path).is_some() {
            return None;
        }
        Some((self.local_engine.clone(), path))
    }

    /// 写回预热得到的引擎版本（成功）。
    pub fn set_engine_version(&self, path: PathBuf, version: String) {
        if let Ok(mut g) = self.engine_version_cache.lock() {
            *g = Some((path, Ok(version)));
        }
    }

    /// 写回预热探测的失败结果。
    ///
    /// 记录失败同样重要：否则预估接口每次都要重跑 27 秒的 `--version`
    /// 才能知道引擎坏了，而它恰恰是用户点「部署」前的必经接口。
    pub fn set_engine_failure(&self, path: PathBuf, reason: String) {
        if let Ok(mut g) = self.engine_version_cache.lock() {
            *g = Some((path, Err(reason)));
        }
    }

    /// **同步**预热引擎版本缓存（等探测结果，不设上限）。
    ///
    /// ## 与 [`crate::spawn_warm_up`] 的分工
    ///
    /// | | `spawn_warm_up` | 本方法 |
    /// |---|---|---|
    /// | 时机 | 进程启动时 | 调用方显式要求 |
    /// | 阻塞 | 否（后台线程） | 是（等探测返回） |
    /// | 用途 | 生产环境首屏不卡 | 需要**确定性**结论的场合 |
    ///
    /// ## 为什么需要同步版本
    ///
    /// `preview` 的本地可行性依赖一次带 2 秒预算的探测
    /// （[`ENGINE_PROBE_BUDGET`]）。缓存为空时，若探测超预算，结论是
    /// 「引擎状态未知」→ 本地被判不可行。
    ///
    /// 这个保守方向在生产里是对的（宁可让用户重试，也不能谎报「免费」），
    /// 但在 e2e 里会变成**假失败**：几十个用例并行跑，CPU 被抢占，
    /// 一个假引擎脚本的进程启动也能超过 2 秒，于是「本地可行」被误判成
    /// 「未知」。测试里先调本方法把缓存填好，`preview` 就只读缓存、
    /// 结果确定。
    ///
    /// 返回 `None` 表示找不到引擎（此时无需预热，也无可预热）。
    ///
    /// 返回值是 `Result<String, String>` 而不是 `MnResult<_>`：与
    /// `engine_version_cache` 里存的形式一致（错误已压成一行文本），
    /// 调用方不需要再处理 `MnError` 的细节。
    pub fn warm_up_engine(&self) -> Option<Result<String, String>> {
        let (engine, path) = self.warm_up_snapshot()?;
        let result = match engine.probe_version() {
            Ok(v) => {
                self.set_engine_version(path, v.clone());
                Ok(v)
            }
            Err(e) => {
                let msg = e.to_string();
                self.set_engine_failure(path, msg.clone());
                Err(msg)
            }
        };
        Some(result)
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

    /// 执行部署。这是系统的主入口（模型已在本地时用）。
    ///
    /// 完整流程见模块文档。返回的 `DeploymentPlan` 已落台账。
    ///
    /// 若模型尚未下载，用 [`Gateway::prepare_with_progress`] +
    /// [`Gateway::commit_local`] 把「下载」插在两段之间。
    pub fn deploy(&mut self, req: &DeploymentRequest) -> MnResult<DeploymentPlan> {
        self.deploy_with_progress(req, &crate::progress::noop_sink)
    }

    /// 「准备」阶段：评估 → 选择设备 → 生成配置，**不启动**。
    ///
    /// `evaluated` 由调用方给出，因为网关无法自己决定它的来源：
    ///
    /// | 情形 | 获取方式 |
    /// |------|---------|
    /// | 本地已有文件 | [`Gateway::evaluate`]（读盘，精确） |
    /// | 远端未下载 | [`Gateway::evaluate_remote`]（HTTP `Range` 预取头部，精确） |
    /// | 预取失败 | [`Gateway::evaluate_estimated`]（按体积粗估，标注为估算） |
    ///
    /// ## 为什么必须拆成两段
    ///
    /// UI 的「部署」对未下载的模型走「评估 → 选设备 → 配置 → 下载 → 启动」：
    /// 先让用户看到「跑不跑得动、占多少内存」，再花时间下载。
    /// 下载若排在评估前，用户白等几 GB 才知道跑不动；启动若排在下载前，
    /// 引擎找不到文件必然失败。所以下载必须夹在 prepare 与 commit 之间，
    /// 且由上层在**不持网关锁**的线程里执行（见 [`crate::pulljobs`]）。
    ///
    /// 本方法返回时已落台账（`provisioning`），因此下载失败会留下一条
    /// 失败记录——便于用户查「刚才那次为什么没成」。
    pub fn prepare_with_progress(
        &mut self,
        evaluated: Evaluated,
        req: &DeploymentRequest,
        sink: crate::progress::ProgressSink<'_>,
    ) -> MnResult<PreparedDeployment> {
        let result = self.prepare_inner(evaluated, req, sink);
        if let Err(e) = &result {
            sink(
                crate::progress::DeployStage::Failed,
                crate::progress::StageOutcome::Failed,
                format!("{e}"),
            );
        }
        result
    }

    /// 「提交」阶段：启动本地引擎并等待健康检查。
    ///
    /// 调用方需保证**模型文件此刻已在磁盘上**（无论原本就有还是刚下载完）。
    /// 重复调用会因端口占用或引擎已启动而失败，不做幂等处理——
    /// 静默复用会让「上次启动到一半崩了」这类问题被掩盖。
    pub fn commit_local(
        &mut self,
        prepared: PreparedLocal,
        sink: crate::progress::ProgressSink<'_>,
    ) -> MnResult<DeploymentPlan> {
        let result = self.commit_local_inner(prepared, sink);
        if let Err(e) = &result {
            sink(
                crate::progress::DeployStage::Failed,
                crate::progress::StageOutcome::Failed,
                format!("{e}"),
            );
        }
        result
    }

    /// 执行部署，并在每个阶段上报进度。
    ///
    /// `sink` 是在**持有网关锁的状态下**被调用的（部署全程 `&mut self`），
    /// 因此实现必须极轻：只写内存里的任务表，不做网络/磁盘 IO。
    /// 否则几 GB 的下载期间会把整个 API 拖死。
    ///
    /// 阶段语义见 [`crate::progress`]。
    pub fn deploy_with_progress(
        &mut self,
        req: &DeploymentRequest,
        sink: crate::progress::ProgressSink<'_>,
    ) -> MnResult<DeploymentPlan> {
        let result = self.deploy_inner(req, sink);
        // 失败统一在这里上报：主体里有十几处 `return Err`，逐个埋点既
        // 啰嗦又容易漏——漏掉的那条会表现为「进度条永远卡在某一步」。
        if let Err(e) = &result {
            sink(
                crate::progress::DeployStage::Failed,
                crate::progress::StageOutcome::Failed,
                format!("{e}"),
            );
        }
        result
    }

    /// 部署主体（一次跑完：模型已在本地时用）。
    ///
    /// 实现上就是「prepare + commit」背靠背，中间不插下载。
    fn deploy_inner(
        &mut self,
        req: &DeploymentRequest,
        sink: crate::progress::ProgressSink<'_>,
    ) -> MnResult<DeploymentPlan> {
        use crate::progress::{DeployStage, StageOutcome};

        sink(
            DeployStage::Assessing,
            StageOutcome::Running,
            "正在读取模型元数据并评估资源需求".to_string(),
        );
        let evaluated = self.evaluate(req)?;
        sink(
            DeployStage::Assessing,
            StageOutcome::Done,
            assessed_detail(&evaluated),
        );

        match self.prepare_with_progress(evaluated, req, sink)? {
            PreparedDeployment::Done(plan) => Ok(*plan),
            PreparedDeployment::Local(prepared) => {
                let prepared = *prepared;
                // 模型已在本地（evaluate 走的是读盘路径或台账规格），
                // 因此下载阶段如实标「跳过」而不是假装下过。
                sink(
                    DeployStage::Downloading,
                    StageOutcome::Skipped,
                    "模型已在本地，无需下载".to_string(),
                );
                self.commit_local(prepared, sink)
            }
        }
    }

    /// 「准备」阶段主体：唯一性检查 → 引擎检查 → 选择设备 → 落台账。
    fn prepare_inner(
        &mut self,
        evaluated: Evaluated,
        req: &DeploymentRequest,
        sink: crate::progress::ProgressSink<'_>,
    ) -> MnResult<PreparedDeployment> {
        use crate::progress::{DeployStage, StageOutcome};

        // ---- 0. 同一模型只允许一个活跃实例 ----
        //
        // 为什么必须在这里拦：部署会占用端口、显存/内存与（云端）计费额度。
        // 同一模型起两个实例既浪费资源，又让「停止」语义变得含糊
        // （停哪个？）。因此按**模型 ID** 做唯一性约束。
        //
        // 判定用「模型 ID 相同 + 状态为活跃」，而不是「模型名相同」：
        // 同家族不同量化（q4_k_m / q4_0）是不同的模型，允许各跑一个。
        let wanted_id = mangosteen_core::model::ModelEntry::derive_id(
            &evaluated.spec.name,
            evaluated.spec.quant,
        );
        for rec in self.state.list()? {
            if !rec.status.is_active() {
                continue;
            }
            let running_id = mangosteen_core::model::ModelEntry::derive_id(
                &rec.plan.model.name,
                rec.plan.model.quant,
            );
            if running_id == wanted_id {
                return Err(MnError::bad_request(format!(
                    "模型 `{wanted_id}` 已有活跃实例（{}，端口 {}）。\
                     同一模型只允许一个实例，请先停止它再重新部署",
                    rec.plan.id,
                    rec.plan
                        .local_access_port()
                        .map(|p| p.to_string())
                        .unwrap_or_else(|| "—".into())
                )));
            }
        }

        // ---- 0.5 引擎可用性检查 ----
        //
        // 判断依据是「注册表里有没有这个引擎的插件」，而不是在网关里
        // 维护一份「哪些引擎能用」的名单——后者每接一个框架都要改本文件。
        //
        // 未适配的引擎必须**明确拒绝并说明**，绝不假装能跑：
        // 假装的结果是用户等到超时才发现根本没有服务在监听。
        let engine = self.config.local.engine;
        if !self.engines_registry.has(engine) {
            return Err(MnError::bad_request(format!(
                "当前推理引擎为 {}，但该引擎尚未适配（未注册插件；需要 {} 格式的模型，\
                 且启动方式与 llama.cpp 不同）。\n\
                 请在设置页改用已支持的引擎，或使用命令行工具自行启动 {}。",
                engine.display_name(),
                engine.required_format(),
                engine.display_name()
            )));
        }

        // ---- 2. 决策（本地优先）----
        sink(
            DeployStage::Selecting,
            StageOutcome::Running,
            "正在按本地优先策略选择算力设备".to_string(),
        );
        let mut trail: Vec<String> = Vec::new();
        let decision = self.scheduler.decide(&evaluated, req)?;

        match decision {
            Decision::Reject { reason } => {
                sink(DeployStage::Selecting, StageOutcome::Failed, reason.clone());
                Err(MnError::InsufficientLocalResource { detail: reason })
            }
            Decision::Local { plan } => {
                // ---- 3a. 准备本地（不启动）----
                sink(
                    DeployStage::Selecting,
                    StageOutcome::Done,
                    format!(
                        "已选择本地算力 · {} 线程 · 上下文 {}",
                        plan.threads, plan.context_size
                    ),
                );
                let id = self.new_deployment_id(&evaluated);
                // 节点配置的最终裁量权在这里：
                //  · engine：记录**部署时**的引擎，后续探活/停止按它分发插件
                //  · use_mlock：由配置 `local.mlock` 决定（默认关闭，
                //    因为锁定物理内存会挤压系统，个人机上反而更慢）
                let mut plan = plan;
                plan.engine = self.config.local.engine;
                plan.use_mlock = self.config.local.mlock;
                let mut full = self.scheduler.assemble_plan(
                    id.clone(),
                    &evaluated,
                    &Decision::Local { plan: plan.clone() },
                    trail.clone(),
                )?;
                if let Some(l) = full.local.as_mut() {
                    l.engine = plan.engine;
                    l.use_mlock = plan.use_mlock;
                }
                self.state
                    .upsert(DeploymentRecord::from_plan(full.clone()))?;
                self.state
                    .set_status(&id, DeploymentStatus::Provisioning, None)?;
                sink(
                    DeployStage::Configuring,
                    StageOutcome::Done,
                    configured_detail(&plan),
                );

                Ok(PreparedDeployment::Local(Box::new(PreparedLocal {
                    id,
                    evaluated,
                    plan,
                    req: req.clone(),
                })))
            }
            Decision::Cloud {
                plan: cloud_plan,
                reason,
            } => {
                trail.push(format!("cloud:{}", short(&reason)));
                let why = self.scheduler.explain_local_failure(&evaluated);
                sink(
                    DeployStage::Selecting,
                    StageOutcome::Done,
                    format!("已选择云端算力：{reason}"),
                );
                // 云端不需要本地文件，直接走完；下载/量化阶段如实标注，
                // 免得用户以为本地做了这些事。
                sink(
                    DeployStage::Downloading,
                    StageOutcome::Skipped,
                    "云端部署：模型由实例侧准备，本地不下载".to_string(),
                );
                sink(
                    DeployStage::Quantizing,
                    StageOutcome::Skipped,
                    "云端部署无需本地量化".to_string(),
                );
                let plan = self.deploy_cloud(&evaluated, *cloud_plan, trail, &why, sink)?;
                Ok(PreparedDeployment::Done(Box::new(plan)))
            }
        }
    }

    /// 「提交」阶段主体：启动引擎并等待健康检查。
    ///
    /// 本地启动失败后的降级也在这一层：`prepare` 里已经落过台账，
    /// 所以失败必须显式把状态改成 `Failed`，否则会留下一条永远
    /// `provisioning` 的记录，UI 上表现为「卡在部署中」。
    fn commit_local_inner(
        &mut self,
        prepared: PreparedLocal,
        sink: crate::progress::ProgressSink<'_>,
    ) -> MnResult<DeploymentPlan> {
        use crate::progress::{DeployStage, StageOutcome};

        let PreparedLocal {
            id,
            evaluated,
            plan,
            req,
        } = prepared;

        // 模型路径可能刚被 `with_model_path` 补上（下载场景：prepare 时
        // 文件还不存在）。台账在 prepare 阶段已按空路径落过盘，这里必须
        // 同步更新，否则后续「按台账重新启动 / 探活」会拿到空路径。
        if let Some(rec) = self.state.get(&id)? {
            if rec.plan.local.as_ref().map(|l| l.model_path.as_str())
                != Some(plan.model_path.as_str())
            {
                let mut updated = rec.clone();
                if let Some(l) = updated.plan.local.as_mut() {
                    l.model_path = plan.model_path.clone();
                }
                self.state.upsert(updated)?;
            }
        }

        // ---- 4. 量化策略（本链路不做自动量化）----
        //
        // 用户下的已经是量化版 GGUF，再量化一次只降精度。如实标「跳过」，
        // 不假装跑过——假装会让用户以为多花了几分钟。
        sink(
            DeployStage::Quantizing,
            StageOutcome::Skipped,
            format!(
                "无需量化：{} 已是 {} 量化版本",
                evaluated.spec.name,
                evaluated.spec.quant.as_str()
            ),
        );

        // ---- 5. 启动 ----
        sink(
            DeployStage::Launching,
            StageOutcome::Running,
            format!("正在启动 {} 并等待健康检查", plan.engine.display_name()),
        );
        match self.launch_local(&id, &plan) {
            Ok(handle) => {
                let pid = handle.pid();
                self.engines.insert(id.clone(), handle);
                self.state.set_pid(&id, pid)?;
                self.state.set_status(&id, DeploymentStatus::Ready, None)?;
                sink(
                    DeployStage::Launching,
                    StageOutcome::Done,
                    // pid 为 0 表示外部复用的服务（Ollama），
                    // 不是本进程拉起的子进程
                    if pid == 0 {
                        format!("外部引擎已就绪（端口 {}）", plan.port)
                    } else {
                        format!("引擎已就绪（PID {pid}，端口 {}）", plan.port)
                    },
                );
                self.state
                    .get(&id)?
                    .map(|r| r.plan)
                    .ok_or_else(|| MnError::bad_request("台账记录丢失"))
            }
            Err(local_err) => {
                let reason = format!("{local_err}");
                self.state
                    .set_status(&id, DeploymentStatus::Failed, Some(reason.clone()))?;

                // 用户选了「本地算力」→ 绝不降级上云。
                // 静默降级会直接产生计费，是最不能接受的意外。
                if req.target.forces_local() {
                    return Err(MnError::InsufficientLocalResource {
                        detail: format!(
                            "本地部署失败：{reason}。\
                             你选择了「本地算力」，不会自动改用云端（避免意外计费）"
                        ),
                    });
                }

                // 允许降级 → 重新决策。`prepare` 里已经算过一次本地方案，
                // 这里要的是「本地失败后的降级结论」，因此走 degrade 分支。
                let mut trail = vec![format!("local:{}", short(&reason))];
                let fallback = self
                    .scheduler
                    .degrade_after_local_failure(&evaluated, &req, &reason, 0)?;
                match fallback {
                    Decision::Cloud {
                        plan: cloud_plan,
                        reason: why,
                    } => {
                        trail.push(format!("cloud:{}", short(&why)));
                        sink(
                            DeployStage::Selecting,
                            StageOutcome::Done,
                            format!("本地失败，已降级到云端：{why}"),
                        );
                        self.deploy_cloud(
                            &evaluated,
                            *cloud_plan,
                            trail,
                            &format!("本地部署失败（{reason}）"),
                            sink,
                        )
                    }
                    Decision::Reject { reason } => Err(MnError::InsufficientLocalResource {
                        detail: format!(
                            "本地部署失败且无法降级：{reason}（原始错误：{local_err}）"
                        ),
                    }),
                    Decision::Local { .. } => {
                        // 理论上不会走到这里（degrade 只产出 Cloud/Reject）
                        Err(MnError::InsufficientLocalResource {
                            detail: format!("本地部署失败：{local_err}，降级决策异常"),
                        })
                    }
                }
            }
        }
    }

    /// 执行本地部署。
    fn launch_local(
        &self,
        deployment_id: &str,
        plan: &mangosteen_core::plan::LocalPlan,
    ) -> MnResult<EngineHandle> {
        // ---- 取插件，不写引擎分支 ----
        //
        // 这里**只问「当前引擎对应哪个插件」**。启动方式、是否拉进程、
        // 怎么探活全部由插件自己决定（llama.cpp 拉子进程，Ollama 复用
        // 常驻守护进程）。以前这里是 `if engine == Ollama { ... }`，
        // 每接一个新框架都要改本函数——正是基线禁止的「改动主链路」。
        let engine = self.config.local.engine;
        let Some(plugin) = self.engines_registry.get(engine) else {
            return Err(MnError::bad_request(format!(
                "推理引擎 {} 尚未适配（未注册插件）。\
                 请在设置页改用已支持的引擎，或使用命令行自行启动 {}。",
                engine.display_name(),
                engine.display_name()
            )));
        };
        plugin.precheck()?;

        let timeout = Duration::from_secs(self.config.scheduler.launch_timeout_secs);
        let mut attempt = 0;
        let max_attempts = 2; // 端口冲突时换一个端口重试一次
        let mut current = plan.clone();

        loop {
            match plugin.launch(&current, timeout) {
                Ok(h) => return Ok(h),
                Err(e) => {
                    attempt += 1;
                    // 端口被占用是最常见的瞬时失败，换端口重试即可。
                    // 复用型引擎（Ollama）不占这个端口，重试也无害。
                    if attempt < max_attempts && is_port_conflict(&e) {
                        let next = next_port(current.port, &self.config.local);
                        current.port = next;
                        let _ = deployment_id;
                        continue;
                    }
                    return Err(e);
                }
            }
        }
    }

    /// 为云端节点按部署时记录的引擎生成启动配置。
    ///
    /// Gateway 作为第三层编排入口负责调用引擎 SPI；CloudOrchestrator 只执行
    /// 收到的标准配置，不依赖 `mangosteen-local`，也不解释引擎 flags。
    fn cloud_launch_config(
        &self,
        plan: &mangosteen_core::plan::CloudPlan,
    ) -> MnResult<mangosteen_core::plan::EngineLaunchConfig> {
        let Some(plugin) = self.engines_registry.get(plan.engine.engine) else {
            return Err(MnError::bad_request(format!(
                "云端部署记录中的推理引擎 {} 尚未适配",
                plan.engine.engine.display_name()
            )));
        };
        Ok(plugin.launch_config(&plan.engine, "0.0.0.0", plan.engine_port))
    }

    /// 执行云端部署。
    ///
    /// `why_cloud` 说明「为什么必须走云端」，用于在无可用服务商时给出**准确**的
    /// 错误信息——是「本地资源不足」还是「本地部署失败」，用户排查方向完全不同。
    fn deploy_cloud(
        &mut self,
        evaluated: &Evaluated,
        cloud_plan: mangosteen_core::plan::CloudPlan,
        trail: Vec<String>,
        why_cloud: &str,
        sink: crate::progress::ProgressSink<'_>,
    ) -> MnResult<DeploymentPlan> {
        use crate::progress::{DeployStage, StageOutcome};

        if self.cloud.is_none() {
            return Err(MnError::NoViableCandidate {
                reason: format!(
                    "{why_cloud}；但未配置任何可用的云服务商，无法降级。\
                     请检查 config.providers 中的启用状态与凭证（`mangosteen cloud providers`）"
                ),
            });
        }

        // 云端所需显存由同一套估算给出（推理引擎唯一性原则）
        let required = self.assessor.cloud_vram_requirement(
            &evaluated.spec,
            evaluated.assessment.context_len,
            evaluated.assessment.kv_cache,
            evaluated.assessment.batch_size,
        );

        let constraints = SelectionConstraints {
            min_vram_bytes: required,
            min_memory_bytes: evaluated.assessment.requirement.total_bytes().max(8 << 30),
            min_disk_bytes: evaluated
                .spec
                .file_size_bytes
                .saturating_mul(2)
                .max(50 << 30),
            max_price_per_hour: cloud_plan.max_price_per_hour,
            allowed_providers: Vec::new(),
            allowed_regions: Vec::new(),
            require_stock: self.config.scheduler.require_stock,
        };

        // 先把本地状态准备好（这些操作需要 &self / &state，不能与 orchestrator 的
        // 可变借用重叠）
        let id = self.new_deployment_id(evaluated);
        let mut plan = self.scheduler.assemble_plan(
            id.clone(),
            evaluated,
            &Decision::Cloud {
                plan: Box::new(cloud_plan.clone()),
                reason: trail.last().cloned().unwrap_or_default(),
            },
            trail.clone(),
        )?;
        // 云端必须在远端把权重拿到，否则会建出一台「实例就绪但没有推理服务」
        // 的机器——用户看到部署成功、对话却 502，而且还在按小时计费。
        //
        // 云端**只有远端下载**这一条路（不做本地上传）：所以要么有下载地址，
        // 要么走 Ollama 自己按名字拉。两者都没有就明确拒绝，别建出空转的机器。
        if let Some(c) = plan.cloud.as_ref() {
            if c.download_url.is_none() && c.remote_model.is_none() {
                return Err(MnError::bad_request(format!(
                    "无法在云端获取模型 {}：没有可用的下载地址。\
                     云端部署要求模型来自可下载的源（ModelScope / HuggingFace）；\
                     若是本地自有文件，请把它打进镜像或改用本地部署。",
                    evaluated.spec.name
                )));
            }
        }

        self.state
            .upsert(DeploymentRecord::from_plan(plan.clone()))?;
        self.state
            .set_status(&id, DeploymentStatus::Provisioning, None)?;
        sink(
            DeployStage::Configuring,
            StageOutcome::Done,
            format!(
                "云端实例配置：显存 ≥ {} · 本地隧道端口 {}",
                mangosteen_platform::fs::human_bytes(required),
                plan.cloud
                    .as_ref()
                    .map(|c| c.local_port.to_string())
                    .unwrap_or_else(|| "—".to_string())
            ),
        );
        // 云端不需要在本地量化：镜像里已有推理环境
        sink(
            DeployStage::Quantizing,
            StageOutcome::Skipped,
            "云端部署无需本地量化".to_string(),
        );
        sink(
            DeployStage::Launching,
            StageOutcome::Running,
            "正在创建云端实例并建立 SSH 隧道".to_string(),
        );

        let launch_config = self.cloud_launch_config(&cloud_plan)?;
        let orchestrator = self.cloud.as_mut().expect("checked above");
        match orchestrator.provision(&constraints, &cloud_plan, &launch_config) {
            Ok(p) => {
                // 回填实际选中的服务商与实例 ID
                if let Some(c) = plan.cloud.as_mut() {
                    Scheduler::apply_selection(
                        c,
                        &p.instance.provider,
                        &p.instance.id,
                        &p.instance.spec.gpu.model,
                        p.instance.spec.gpu.vram_bytes,
                    );
                    c.local_port = p.local_port.unwrap_or(c.local_port);
                }
                plan.origin = ComputeOrigin::Cloud {
                    provider: p.instance.provider.clone(),
                    instance_id: p.instance.id.clone(),
                };
                self.state
                    .upsert(DeploymentRecord::from_plan(plan.clone()))?;
                self.state.set_status(&id, DeploymentStatus::Ready, None)?;
                sink(
                    DeployStage::Launching,
                    StageOutcome::Done,
                    format!(
                        "云端实例已就绪：{} · {}",
                        p.instance.provider, p.instance.id
                    ),
                );
                Ok(plan)
            }
            Err(e) => {
                self.state
                    .set_status(&id, DeploymentStatus::Failed, Some(e.to_string()))?;
                Err(e)
            }
        }
    }

    /// 评估器引用（供上层在**不重入网关方法**时直接做一次评估）。
    ///
    /// 为什么需要：`pulljobs` 的部署钩子在 prepare 阶段要判断
    /// 「本地文件在不在，在就读盘精确评估」，而这个判断发生在闭包内、
    /// 已经短暂持有网关锁。走 [`Gateway::evaluate`] 会绕一圈（它按 ID
    /// 查台账），而这里手上的就是确切路径，直接用评估器更准。
    pub fn assessor_public(&self) -> &Assessor {
        &self.assessor
    }

    /// 用远端预取的 GGUF 元数据评估（模型尚未下载到本地）。
    ///
    /// 供 [`crate::pulljobs`] 的「评估 → 选设备 → 配置 → 下载 → 启动」
    /// 链路在**下载前**调用。`info` 由 `mangosteen_base::probe_remote_metadata`
    /// 通过 HTTP `Range` 拿到文件头部后解析而来。
    pub fn evaluate_remote(
        &self,
        info: &mangosteen_core::gguf::GgufInfo,
        req: &DeploymentRequest,
    ) -> Evaluated {
        self.assessor.evaluate_remote(info, req)
    }

    /// 按「名字 + 量化 + 体积」粗估评估（远端元数据预取失败时的兜底）。
    ///
    /// 调用方必须把结论标注为估算——见 [`Assessor::evaluate_estimated`]。
    pub fn evaluate_estimated(
        &self,
        name: &str,
        quant: mangosteen_core::model::Quantization,
        file_size_bytes: u64,
        req: &DeploymentRequest,
    ) -> Evaluated {
        self.assessor
            .evaluate_estimated(name, quant, file_size_bytes, req)
    }

    /// 评估：按模型 ID 查台账或按路径直接解析。
    pub fn evaluate(&self, req: &DeploymentRequest) -> MnResult<Evaluated> {
        // 1. 先按台账 ID / 名称查
        if let Some(entry) = self.models.get(&req.model)? {
            if let mangosteen_core::model::ModelLocation::Local { path } = &entry.location {
                if path.is_file() {
                    return self.assessor.evaluate_file(path, req);
                }
            }
            // 云端模型或本地文件已丢失 → 用台账里的规格评估（不读盘）
            return Ok(self.assessor.evaluate_spec(&entry.spec, req));
        }

        // 2. 按模糊名查
        let hits = self.models.find_by_name(&req.model)?;
        if hits.len() == 1 {
            if let mangosteen_core::model::ModelLocation::Local { path } = &hits[0].location {
                if path.is_file() {
                    return self.assessor.evaluate_file(path, req);
                }
            }
            return Ok(self.assessor.evaluate_spec(&hits[0].spec, req));
        }
        if hits.len() > 1 {
            return Err(MnError::bad_request(format!(
                "模型名 `{}` 匹配到 {} 个模型：{}。请使用完整 ID",
                req.model,
                hits.len(),
                hits.iter()
                    .map(|h| h.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }

        // 3. 当作文件路径
        let p = PathBuf::from(&req.model);
        if p.is_file() {
            return self.assessor.evaluate_file(&p, req);
        }

        Err(MnError::ModelNotFound { path: p })
    }

    /// 生成部署 ID。
    fn new_deployment_id(&self, evaluated: &Evaluated) -> String {
        let seq = self.next_seq();
        format!(
            "dep-{:x}{:03x}",
            mangosteen_core::time::now_unix_millis(),
            seq & 0xfff
        ) + &format!("-{}", sanitize(&evaluated.spec.name))
    }

    // ---------------------------------------------------------------- 停止 / 查询

    /// 停止部署（释放本地进程 / 销毁云端实例）。
    pub fn stop(&mut self, id: &str) -> MnResult<()> {
        let record = self
            .state
            .get(id)?
            .ok_or_else(|| MnError::bad_request(format!("部署 `{id}` 不存在")))?;

        match &record.plan.origin {
            ComputeOrigin::Local => {
                if let Some(mut h) = self.engines.remove(id) {
                    h.shutdown()?;
                } else if let Some(pid) = record.pid {
                    // 进程不是本实例启动的（如 deploy 在另一个进程里），
                    // 按台账记录的 PID 终止，避免留下孤儿进程占用端口。
                    mangosteen_platform::process::terminate_pid(pid, Duration::from_secs(3))?;
                }
            }
            ComputeOrigin::Cloud {
                provider,
                instance_id,
            } => {
                if let Some(o) = self.cloud.as_mut() {
                    let errors = o.teardown(provider, instance_id);
                    if !errors.is_empty() {
                        return Err(MnError::Network {
                            stage: "销毁云端资源".to_string(),
                            reason: errors.join("；"),
                        });
                    }
                }
            }
        }
        self.state
            .set_status(id, DeploymentStatus::Destroyed, None)?;
        Ok(())
    }

    /// **停止服务**（云端：停推理进程 + 关机实例；本地：停引擎进程）。
    ///
    /// ## 与 [`Gateway::stop`] 的区别
    ///
    /// | | `stop_service` | `stop` |
    /// |---|---|---|
    /// | 云端实例 | **保留**（关机，停止 GPU 计费） | 销毁 |
    /// | 磁盘/权重 | 保留，再启动只需几秒 | 全部丢失，要重新上传 |
    /// | 台账状态 | `stopped` | `destroyed` |
    ///
    /// 这才是「停止」应有的语义：用户想暂停一下，不是想删机器。
    /// 真正释放资源（停止一切计费）请用 `DELETE /api/deployments/{id}`。
    pub fn stop_service(&mut self, id: &str) -> MnResult<()> {
        let record = self
            .state
            .get(id)?
            .ok_or_else(|| MnError::bad_request(format!("部署 `{id}` 不存在")))?;

        match &record.plan.origin {
            ComputeOrigin::Local => {
                if let Some(mut h) = self.engines.remove(id) {
                    h.shutdown()?;
                } else if let Some(pid) = record.pid {
                    mangosteen_platform::process::terminate_pid(pid, Duration::from_secs(3))?;
                }
            }
            ComputeOrigin::Cloud {
                provider,
                instance_id,
            } => {
                let o = self.cloud.as_mut().ok_or_else(|| {
                    MnError::bad_request("未配置云端能力，无法操作云端部署".to_string())
                })?;
                // 1) 先停推理进程。失败不致命——它可能本来就没跑（上次已停），
                //    但**要记下来**，因为「进程停不掉」意味着关机时可能丢数据。
                let mut warnings = Vec::new();
                if let Err(e) = o.stop_service(provider, instance_id) {
                    warnings.push(format!("停止推理进程：{e}"));
                }
                // 2) 再关机。这一步失败必须报错——机器还在跑就意味着继续计费。
                o.power_off(provider, instance_id)?;
                // 3) 关掉隧道：机器都关了，隧道只会反复重连
                if let Err(e) = o.tunnels_mut().close_all() {
                    warnings.push(format!("关闭隧道：{e}"));
                }
                let note = if warnings.is_empty() {
                    None
                } else {
                    Some(format!("实例已关机，但：{}", warnings.join("；")))
                };
                self.state.set_status(id, DeploymentStatus::Stopped, note)?;
                return Ok(());
            }
        }
        self.state.set_status(id, DeploymentStatus::Stopped, None)?;
        Ok(())
    }

    /// **启动服务**（云端：开机 + 起推理进程；本地：重新拉起引擎）。
    ///
    /// 与重新部署的区别：不重新建实例、不重新传权重，只把停掉的东西拉起来。
    /// 云端权重与二进制都还在盘上，所以通常十几秒就能用。
    pub fn start_service(&mut self, id: &str) -> MnResult<()> {
        let record = self
            .state
            .get(id)?
            .ok_or_else(|| MnError::bad_request(format!("部署 `{id}` 不存在")))?;
        let plan = record.plan.clone();

        match &plan.origin {
            ComputeOrigin::Local => Err(MnError::bad_request(
                "本地部署没有「保留实例再启动」的概念：请直接重新部署该模型".to_string(),
            )),
            ComputeOrigin::Cloud {
                provider,
                instance_id,
            } => {
                let cloud_plan = plan.cloud.clone().ok_or_else(|| {
                    MnError::bad_request("台账缺少云端方案，无法启动".to_string())
                })?;
                self.state
                    .set_status(id, DeploymentStatus::Provisioning, None)?;
                let launch_config = self.cloud_launch_config(&cloud_plan)?;
                let o = self.cloud.as_mut().ok_or_else(|| {
                    MnError::bad_request("未配置云端能力，无法操作云端部署".to_string())
                })?;
                // 1) 开机并等到 Running
                if let Err(e) = o.power_on(provider, instance_id, &cloud_plan) {
                    let msg = e.to_string();
                    self.state
                        .set_status(id, DeploymentStatus::Failed, Some(msg.clone()))?;
                    return Err(e);
                }
                // 2) 起推理进程
                if let Err(e) = o.start_service(provider, instance_id, &cloud_plan, &launch_config)
                {
                    let msg = e.to_string();
                    self.state
                        .set_status(id, DeploymentStatus::Failed, Some(msg.clone()))?;
                    return Err(e);
                }
                // 3) 重建隧道 + 等 /health
                let base = match o.establish_tunnel(provider, instance_id, &cloud_plan) {
                    Ok(b) => b,
                    Err(e) => {
                        let msg = e.to_string();
                        self.state
                            .set_status(id, DeploymentStatus::Failed, Some(msg.clone()))?;
                        return Err(e);
                    }
                };
                let health = format!("{base}/health");
                if let Err(e) = mangosteen_cloud::remote::wait_http_ready(
                    &health,
                    o.start_timeout(),
                    o.poll_interval(),
                ) {
                    let msg = e.to_string();
                    self.state
                        .set_status(id, DeploymentStatus::Failed, Some(msg.clone()))?;
                    return Err(e);
                }
                self.state.set_status(id, DeploymentStatus::Ready, None)?;
                Ok(())
            }
        }
    }

    /// 中止部署并**回滚**它产生的全部痕迹。
    ///
    /// ## 与 [`Gateway::stop`] 的区别
    ///
    /// | | `stop` | `abort_deployment` |
    /// |---|---|---|
    /// | 语义 | 停掉一个**已成功**的部署 | 撤销一次**进行中/刚完成**的部署 |
    /// | 台账 | 保留记录，状态改 `destroyed` | **删除记录** |
    /// | 端口 | 释放（进程退出后自然可用） | 显式从预留表移除 |
    /// | 文件 | 不动 | 按 `delete_model_file` 决定 |
    ///
    /// ## 为什么要显式释放端口
    ///
    /// `prepare` 阶段调 `scheduler.decide` 时就把端口记进了 `reserved`
    /// 列表。若回滚不还回去，反复「部署→停止」会一点点吃光端口区间，
    /// 最终表现是「端口耗尽，部署失败」——而用户根本没跑过几个模型。
    ///
    /// ## `delete_file` 的边界
    ///
    /// 传 `Some(path)` 才删，且**以传入路径为准**而不是台账里的
    /// `model_path`：远端部署的 prepare 发生在下载之前，台账里那条
    /// 记录写的是空路径（文件当时还不存在），照它删等于什么也没删。
    /// 调用方（[`crate::pulljobs`] 的 abort 钩子）知道文件真正落在哪。
    ///
    /// 只有**本次部署下载的**文件才该删。若模型原本就在本地
    /// （用户点的是「启动」而非「下载并启动」），删掉它等于卸载用户
    /// 已有的资产——那不是「停止部署」，是「删除模型」。
    pub fn abort_deployment(
        &mut self,
        id: &str,
        delete_file: Option<&std::path::Path>,
    ) -> MnResult<()> {
        let record = self.state.get(id)?;

        // 1) 停引擎 / 销毁云端实例（可能不存在：prepare 后立即被中止）
        if let Some(rec) = &record {
            match &rec.plan.origin {
                ComputeOrigin::Local => {
                    if let Some(mut h) = self.engines.remove(id) {
                        let _ = h.shutdown();
                    } else if let Some(pid) = rec.pid {
                        // 忽略失败：进程可能已经退了
                        let _ = mangosteen_platform::process::terminate_pid(
                            pid,
                            Duration::from_secs(3),
                        );
                    }
                }
                ComputeOrigin::Cloud {
                    provider,
                    instance_id,
                } => {
                    if let Some(o) = self.cloud.as_mut() {
                        // 云端实例不销毁会**持续计费**，失败必须暴露出来
                        let errors = o.teardown(provider, instance_id);
                        if !errors.is_empty() {
                            return Err(MnError::Network {
                                stage: "回滚云端资源".to_string(),
                                reason: format!(
                                    "实例 {} 可能仍在运行并计费：{}",
                                    instance_id,
                                    errors.join("；")
                                ),
                            });
                        }
                    }
                }
            }

            // 2) 释放端口预留
            if let Some(port) = rec.plan.local_access_port() {
                self.scheduler.release_port(port);
            }
        }

        // 3) 按需删文件（以调用方给的路径为准，见方法文档）
        if let Some(p) = delete_file {
            if p.is_file() {
                let _ = std::fs::remove_file(p);
            }
            // 顺手清掉续传残留（`<name>.gguf.part`）
            let part = p.with_extension("gguf.part");
            if part.is_file() {
                let _ = std::fs::remove_file(&part);
            }
            // 还有 `<name>.gguf.sha256` 之类的旁注文件
            let mut side = p.as_os_str().to_owned();
            side.push(".sha256");
            let side = PathBuf::from(side);
            if side.is_file() {
                let _ = std::fs::remove_file(&side);
            }
        }

        // 4) 删除台账记录（不是标记 destroyed——回滚要让这次尝试「没发生过」）
        self.state.remove(id)?;
        Ok(())
    }

    /// 停止全部部署。
    pub fn stop_all(&mut self) -> Vec<(String, String)> {
        let ids: Vec<String> = self
            .state
            .list_active()
            .unwrap_or_default()
            .into_iter()
            .map(|r| r.plan.id)
            .collect();
        let mut errors = Vec::new();
        for id in ids {
            if let Err(e) = self.stop(&id) {
                errors.push((id, e.to_string()));
            }
        }
        errors
    }

    /// 列出全部部署。
    pub fn list_deployments(&self) -> MnResult<Vec<DeploymentRecord>> {
        self.state.list()
    }

    /// 取单个部署。
    pub fn get_deployment(&self, id: &str) -> MnResult<Option<DeploymentRecord>> {
        self.state.get(id)
    }

    /// 探活：返回每个活跃部署的真实状态（并顺带修正台账）。
    ///
    /// 本地引擎可能已崩溃、云端实例可能被厂商回收，
    /// 因此「台账说 Ready」不等于「现在真的能服务」。
    pub fn refresh(&mut self) -> MnResult<Vec<ActiveDeployment>> {
        let records = self.state.list_active()?;
        let mut out = Vec::new();

        // ---- 先收敛历史遗留的重复实例 ----
        //
        // 「同一模型只允许一个实例」是产品约束。但台账里可能已有历史重复
        // （旧版本不拦、或多次 deploy 绕过）。若不收敛，UI 会同时显示两个
        // 同模型实例，「停止」按钮也只能指向其中一个，另一个变成孤儿。
        //
        // 策略：每个模型 ID 只保留最先创建的那个，其余真停掉（本地杀进程、
        // 云端销毁实例）。这条在刷新路径里做，所以每次探活都会自愈。
        let mut seen: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let mut keep: Vec<mangosteen_base::state::DeploymentRecord> = Vec::new();
        let mut dup_ids: Vec<String> = Vec::new();
        for r in records {
            let mid = mangosteen_core::model::ModelEntry::derive_id(
                &r.plan.model.name,
                r.plan.model.quant,
            );
            match seen.get(&mid) {
                // 已有一个同模型的活跃实例 → 这个算重复
                Some(_) => dup_ids.push(r.plan.id.clone()),
                None => {
                    seen.insert(mid, r.plan.id.clone());
                    keep.push(r);
                }
            }
        }
        for id in &dup_ids {
            // 尽力停止；失败也不阻断刷新（台账状态仍会被探活修正）
            let _ = self.stop(id);
        }

        for r in keep {
            let id = r.plan.id.clone();
            // 各分支都会赋值，这里只是给编译器一个初值
            #[allow(unused_assignments)]
            let mut ready = false;

            if r.plan.origin.is_local() {
                if let Some(h) = self.engines.get_mut(&id) {
                    let st = h.status(Duration::from_secs(0));
                    ready = st.is_serving();
                    if st.is_dead() {
                        // 引擎已死 → 台账改为 Failed，避免误报
                        self.state.set_status(
                            &id,
                            DeploymentStatus::Failed,
                            Some(format!("本地引擎已退出：{}", st.describe())),
                        )?;
                    }
                } else {
                    // 无内存句柄：该部署可能是**另一个进程**（如 `mangosteen deploy`
                    // 之后另起 `mangosteen serve`）启动的。此时不能直接判定为未就绪，
                    // 必须通过 HTTP 探活——这正是「网关唯一收口」在多进程下的必要补充。
                    ready = probe_local_health(&self.engines_registry, &r.plan);
                    if !ready && r.status == DeploymentStatus::Ready {
                        // 台账说 Ready 但探不通 → 进程确实没了，修正台账
                        self.state.set_status(
                            &id,
                            DeploymentStatus::Failed,
                            Some(format!(
                                "本地引擎在 {} 无响应（进程可能已退出）",
                                r.plan
                                    .local_access_port()
                                    .map(|p| format!("127.0.0.1:{p}"))
                                    .unwrap_or_else(|| "原端口".into())
                            )),
                        )?;
                    }
                }
            } else {
                // 云端：只信台账状态（真实探测需要网络往返，放到推理时兜底）
                ready = r.status == DeploymentStatus::Ready;
            }

            out.push(ActiveDeployment {
                id: id.clone(),
                // 用完整模型 ID（`name:quant`）而不是裸模型名：
                // `/v1/models`、推理请求、推理响应三处必须给出同一个标识，
                // 否则上层应用做一致性校验会失败。同一模型的不同量化版本
                // 也才能区分开。
                model: mangosteen_core::model::ModelEntry::derive_id(
                    &r.plan.model.name,
                    r.plan.model.quant,
                ),
                origin: r.plan.origin.clone(),
                status: r.status,
                port: r.plan.local_access_port(),
                ready,
            });
        }
        Ok(out)
    }

    /// 找出当前可用于推理的后端。
    ///
    /// 选择规则：优先本地（本地优先原则的延伸），否则取最近就绪的云端部署。
    pub fn active_backend(&mut self) -> MnResult<BackendTarget> {
        let active = self.refresh()?;

        // 1. 本地优先
        if let Some(a) = active
            .iter()
            .find(|a| a.origin.is_local() && a.ready && a.port.is_some())
        {
            return Ok(BackendTarget {
                host: "127.0.0.1".to_string(),
                port: a.port.unwrap(),
                model_id: a.model.clone(),
                origin_label: "本地算力".to_string(),
            });
        }
        // 2. 云端
        if let Some(a) = active
            .iter()
            .find(|a| !a.origin.is_local() && a.ready && a.port.is_some())
        {
            return Ok(BackendTarget {
                host: "127.0.0.1".to_string(),
                port: a.port.unwrap(),
                model_id: a.model.clone(),
                origin_label: a.origin.label(),
            });
        }
        Err(MnError::bad_request(
            "当前没有就绪的推理后端。请先执行 `mangosteen deploy <model>`",
        ))
    }

    /// 按模型 ID 选择后端；`None`/`"auto"` 表示自动（本地优先）。
    ///
    /// 为什么需要它：UI 的「模型信息」下拉让用户选具体模型，但如果网关
    /// 永远取「第一个就绪的」，那个选择就是摆设——用户选了 A，实际
    /// 路由到 B，回答风格对不上却看不出原因。
    ///
    /// 指定了具体模型却**没在运行**时必须明确报错，而不是悄悄退回自动：
    /// 静默换模型比报错更难排查（用户会以为「就是这个模型答的」）。
    pub fn active_backend_for(&mut self, want: Option<&str>) -> MnResult<BackendTarget> {
        let Some(id) = want
            .map(str::trim)
            .filter(|s| !s.is_empty() && *s != "auto")
        else {
            return self.active_backend();
        };

        let active = self.refresh()?;
        // 精确匹配模型 ID；也接受「不带量化后缀」的宽松匹配，
        // 因为 UI 显示与台账 ID 偶尔会在量化标记上不一致。
        let hit = active
            .iter()
            .find(|a| a.ready && a.port.is_some() && a.model == id)
            .or_else(|| {
                active
                    .iter()
                    .find(|a| a.ready && a.port.is_some() && a.model.split(':').next() == Some(id))
            });

        match hit {
            Some(a) => Ok(BackendTarget {
                host: "127.0.0.1".to_string(),
                port: a.port.unwrap(),
                model_id: a.model.clone(),
                origin_label: a.origin.label(),
            }),
            None => {
                let running: Vec<&str> = active
                    .iter()
                    .filter(|a| a.ready)
                    .map(|a| a.model.as_str())
                    .collect();
                Err(MnError::bad_request(format!(
                    "模型 `{id}` 未在运行，无法对话。\n当前运行中：{}",
                    if running.is_empty() {
                        "（无）".to_string()
                    } else {
                        running.join(", ")
                    }
                )))
            }
        }
    }

    /// 当前活跃的云端部署（**只读台账，不发网络请求**）。
    ///
    /// 设备页需要「现在用的是哪台云机器」才能去查监控。这里刻意只返回
    /// 元信息，把厂商接口调用留给调用方在**锁外**执行——云端往返是
    /// 百毫秒级，绝不能放在网关锁内（否则对话/状态等所有 API 都被拖慢）。
    ///
    /// 若有多个云端部署，取台账里**最近更新**的那个（与「当前在用」的直觉
    /// 一致），避免同时打多个厂商接口。
    pub fn active_cloud_deployment(&self) -> MnResult<Option<CloudDeploymentRef>> {
        let Some(rec) = self
            .state
            .list_active()?
            .into_iter()
            .filter(|r| !r.plan.origin.is_local())
            .max_by_key(|r| r.updated_at)
        else {
            return Ok(None);
        };

        let (provider, instance_id) = match &rec.plan.origin {
            ComputeOrigin::Cloud {
                provider,
                instance_id,
            } => (provider.clone(), instance_id.clone()),
            ComputeOrigin::Local => return Ok(None),
        };

        Ok(Some(CloudDeploymentRef {
            deployment_id: rec.plan.id.clone(),
            provider,
            instance_id,
            model: mangosteen_core::model::ModelEntry::derive_id(
                &rec.plan.model.name,
                rec.plan.model.quant,
            ),
            status: rec.status,
        }))
    }

    /// 查询某云端实例的实时监控（**会发起网络请求**）。
    ///
    /// 调用方必须确保**不在持有网关锁时调用**它——单次往返约 0.5 秒，
    /// 持锁会拖慢所有其它 API。后台采集线程（[`crate::cloudmon`]）
    /// 的做法是「持锁取元信息 → 放锁 → 再持锁调用本方法」，看似持锁，
    /// 但它运行在独立线程且频率被压到 5 秒一次，实际影响可接受；
    /// HTTP 请求路径**绝不**调用本方法，只读缓存。
    pub fn cloud_instance_metrics(
        &self,
        provider: &str,
        instance_id: &str,
    ) -> MnResult<Option<mangosteen_core::cloud::InstanceMetrics>> {
        let Some(orch) = self.cloud.as_ref() else {
            // 未配置云服务商：不是错误，设备页显示「未启用云端」即可
            return Ok(None);
        };
        orch.instance_metrics(Some(provider), instance_id)
    }

    /// 状态快照。
    pub fn status(&mut self) -> MnResult<GatewayStatus> {
        let engine_path = self.local_engine.locate().ok();
        // 只读缓存：`--version` 在部分安装上要 20~30 秒，绝不能在请求路径上跑
        let engine_version = engine_path
            .as_ref()
            .and_then(|p| self.cached_engine_version(p));

        Ok(GatewayStatus {
            version: mangosteen_core::identity::VERSION.to_string(),
            hardware: self.assessor.hardware().describe(),
            // 「已就绪」而非「已启用」：凭证缺失的服务商不能降级，
            // 展示成已就绪会误导用户以为本地不足时一定能上云。
            providers: mangosteen_cloud::build_registry(&self.config)
                .registry
                .names()
                .iter()
                .map(|s| s.to_string())
                .collect(),
            active: self.refresh()?,
            engine_path,
            engine_version,
        })
    }

    /// 云端选点：查询规格 → 打分排序 → 返回排序结果（**不创建资源**）。
    ///
    /// 供 `mangosteen cloud pick` 使用，让用户在花钱前看清决策依据。
    pub fn pick_cloud(
        &self,
        constraints: &SelectionConstraints,
    ) -> MnResult<Vec<mangosteen_core::policy::ScoredCandidate>> {
        let o = self
            .cloud
            .as_ref()
            .ok_or_else(|| MnError::NoViableCandidate {
                reason: "未配置任何可用的云服务商".to_string(),
            })?;
        let (candidates, latencies) = o.collect_candidates()?;
        Ok(o.rank(&candidates, &latencies, constraints))
    }

    /// 查询云端可创建规格（跨服务商）。
    ///
    /// 返回 `(服务商名, 规格)` 列表。
    pub fn list_cloud_specs(
        &self,
    ) -> MnResult<Vec<(String, mangosteen_core::cloud::AvailableSpec)>> {
        let o = self
            .cloud
            .as_ref()
            .ok_or_else(|| MnError::NoViableCandidate {
                reason: "未配置任何可用的云服务商".to_string(),
            })?;
        o.collect_candidates().map(|(c, _)| c)
    }

    /// 当前可用的**最低价**节点（按价格升序，**不创建任何资源**）。
    ///
    /// 与 [`Self::pick_cloud`] 的区别：后者走打分策略（默认 balanced，
    /// 综合价格/延迟/库存/硬件），「综合最优」未必最便宜。用户明确要
    /// 「最便宜」时用这个——它不受 `scheduler.strategy` 影响。
    ///
    /// 返回列表按价格升序，第一项即最便宜。**「可用」不等于库存字段为真**：
    /// 优云的库存接口是模拟调度（对能开的机器也返回 false），所以真正
    /// 「能不能开出来」要由 [`Self::create_cheapest_cloud`] 按顺序真开才知道。
    pub fn cheapest_cloud(
        &self,
        constraints: &SelectionConstraints,
    ) -> MnResult<Vec<(String, mangosteen_core::cloud::AvailableSpec)>> {
        let o = self
            .cloud
            .as_ref()
            .ok_or_else(|| MnError::NoViableCandidate {
                reason: "未配置任何可用的云服务商".to_string(),
            })?;
        o.cheapest(constraints)
    }

    /// 创建一台「当前可用的最低价」实例，返回 `(服务商名, 实例)`。
    ///
    /// ## 这是**裸创建**，不是部署
    ///
    /// 不建 SSH 隧道、不上传模型、不启动引擎。适合「先拿到一台机器」的场景
    /// （手工调试、跑别人的镜像）。要跑推理请走 [`Self::deploy`]——它会
    /// 完整编排，并且写部署台账。
    ///
    /// ## 裸实例不在部署台账里
    ///
    /// 因此**不会被 `stop`/`abort` 自动回收**，也不会出现在「模型实例」列表。
    /// 用 [`Self::destroy_cloud_instance`] 销毁。调用方（CLI）必须在输出里
    /// 明确告知用户这一点，否则就是「开了台机器没人记得」的持续计费。
    pub fn create_cheapest_cloud(
        &self,
        constraints: &SelectionConstraints,
        name: Option<&str>,
    ) -> MnResult<(String, mangosteen_core::cloud::Instance)> {
        let o = self
            .cloud
            .as_ref()
            .ok_or_else(|| MnError::NoViableCandidate {
                reason: "未配置任何可用的云服务商".to_string(),
            })?;
        o.create_cheapest(constraints, name)
    }

    /// 销毁任意云端实例（按 `服务商 + 实例 ID`，**不要求**它在台账里）。
    ///
    /// 存在的意义就是清理 [`Self::create_cheapest_cloud`] 这类裸实例：
    /// 没有台账记录，`stop(部署 ID)` 找不到它，用户就只能去控制台手工删——
    /// 那正是「忘关实例持续扣费」最常见的成因。
    ///
    /// 返回清理过程中的错误列表（空 = 全部成功）。
    pub fn destroy_cloud_instance(
        &mut self,
        provider: &str,
        instance_id: &str,
    ) -> MnResult<Vec<String>> {
        let o = self
            .cloud
            .as_mut()
            .ok_or_else(|| MnError::NoViableCandidate {
                reason: "未配置任何可用的云服务商".to_string(),
            })?;
        Ok(o.teardown(provider, instance_id))
    }

    /// 关闭网关：停止全部部署，释放资源。
    pub fn shutdown(&mut self) -> Vec<(String, String)> {
        self.stop_all()
    }

    /// 部署预估：**不创建任何资源**，只回答「会发生什么、要花多少钱」。
    ///
    /// 这是「云端花钱前必须让用户确认」的执行基础。设计要点：
    ///
    /// 1. **绝不产生副作用**：不建实例、不占端口、不写台账。UI 可以在用户
    ///    每次调整上下文/参数时反复调用它。
    /// 2. **明确回答「为什么上云」**：本地可行时说 `local`，不可行时给出
    ///    具体差额（差多少显存），避免用户误以为被强制上云。
    /// 3. **费用给出区间而非单点**：推理场景的实际使用时长无法预知，
    ///    给「按预估时长」与「按小时」两种口径，让用户自己判断。
    pub fn preview(&self, req: &DeploymentRequest) -> MnResult<DeploymentPreview> {
        let evaluated = self.evaluate(req)?;

        // 关键：本地可行性不能只看「资源是否够」。
        //
        // 实际 `deploy` 的本地分支包含两步：资源判定 + 引擎启动。二者任一
        // 失败都会降级云端。如果这里只判资源，会出现**危险的错报**：
        // UI 告诉用户「本地可跑、不花钱」，用户确认后引擎启动失败、
        // 自动降级到云端开始计费 —— 用户被意外收费。
        //
        // 因此这里需要一次引擎可用性判断，让预估结论与实际行为一致。
        //
        // ## 为什么只能读缓存，不能现场探测
        //
        // 这里曾经直接调 `probe_version()`，注释还写着「毫秒级」——那是错的。
        // `llama-server --version` 会加载全部动态库与后端，在 Homebrew 版上
        // 实测要 27 秒。结果是预估接口整体超时，而它是云端费用确认弹窗的
        // 唯一数据源：一超时，用户就点不了部署。
        //
        // 现在只读启动时预热填好的缓存（`spawn_warm_up`）。缓存为空视为
        // 「引擎状态未知」，按本地可行处理而不是让用户干等半分钟。
        // 宁可乐观（真失败时 deploy 会明确报错），也不能让确认流程不可用。
        let engine_problem = if evaluated.is_local_viable() {
            self.engine_problem_bounded()
        } else {
            None
        };

        // 用户选了「云端算力」→ 直接按云端出预估，不再宣称「本地可行」。
        // 否则弹窗会说「不花钱」，与用户的选择和实际部署行为都矛盾。
        let local_viable =
            !req.target.forces_cloud() && evaluated.is_local_viable() && engine_problem.is_none();

        // 本地可行 → 不涉及任何云端费用，直接返回
        if local_viable {
            return Ok(DeploymentPreview {
                model: evaluated.spec.name.clone(),
                model_id: mangosteen_core::model::ModelEntry::derive_id(
                    &evaluated.spec.name,
                    evaluated.spec.quant,
                ),
                local_viable: true,
                local_reason: None,
                origin: ComputeOrigin::Local,
                local: Some(LocalPreview::from_assessment(&evaluated)),
                cloud: None,
                cloud_candidates: Vec::new(),
                cloud_error: None,
                blocked: false,
            });
        }

        // 本地不可行：说明原因 + 给出云端方案与费用。
        // 原因必须区分「资源不足」与「引擎坏了」——用户的处理方式完全不同。
        let local_reason = if req.target.forces_cloud() {
            "按你的选择使用「云端算力」，全部资源在云端".to_string()
        } else {
            match (&engine_problem, evaluated.is_local_viable()) {
                (Some(engine_msg), _) => format!(
                    "本地资源足够，但推理引擎无法启动：{engine_msg}。\
                 修复后可避免云端费用（见 `mangosteen engine probe`）"
                ),
                (None, false) => self.scheduler.explain_local_failure(&evaluated),
                (None, true) => "本地可用".to_string(),
            }
        };

        // 云端显存需求与本地同一套公式（推理引擎唯一性 → 参数体系一致）
        let required_vram = self.assessor.cloud_vram_requirement(
            &evaluated.spec,
            evaluated.assessment.context_len,
            evaluated.assessment.kv_cache,
            evaluated.assessment.batch_size,
        );
        let constraints = SelectionConstraints {
            min_vram_bytes: required_vram,
            min_memory_bytes: evaluated.assessment.requirement.total_bytes().max(8 << 30),
            min_disk_bytes: evaluated
                .spec
                .file_size_bytes
                .saturating_mul(2)
                .max(50 << 30),
            max_price_per_hour: req.max_price_per_hour,
            allowed_providers: Vec::new(),
            allowed_regions: Vec::new(),
            require_stock: self.config.scheduler.require_stock,
        };

        let (candidates, candidates_error) = match self.pick_cloud(&constraints) {
            Ok(c) => (c, None),
            Err(e) => (Vec::new(), Some(e.to_string())),
        };

        let best = candidates.first().map(|c| {
            let hours = default_estimate_hours();
            CloudPreview {
                provider: c.provider.clone(),
                spec_id: c.spec_id.clone(),
                gpu_model: c.gpu_model.clone(),
                gpu_count: 1,
                vram_bytes: c.vram_bytes,
                region: c.region.clone(),
                latency_ms: c.latency_ms,
                price_per_hour: c.price_per_hour,
                estimate_hours: hours,
                estimate_total: c.price_per_hour * hours as f64,
                required_vram_bytes: required_vram,
            }
        });

        Ok(DeploymentPreview {
            model: evaluated.spec.name.clone(),
            model_id: mangosteen_core::model::ModelEntry::derive_id(
                &evaluated.spec.name,
                evaluated.spec.quant,
            ),
            local_viable: false,
            local_reason: Some(local_reason),
            origin: ComputeOrigin::Cloud {
                provider: best
                    .as_ref()
                    .map(|b| b.provider.clone())
                    .unwrap_or_default(),
                // 预估阶段尚未创建实例；这里是空占位。
                // UI 必须用 `cloud.is_some()` 判断是否真的会走云端，
                // 不能用 instance_id 是否为空来判断。
                instance_id: String::new(),
            },
            local: None,
            cloud: best,
            cloud_candidates: candidates
                .iter()
                .take(5)
                .map(|c| CloudCandidateBrief {
                    provider: c.provider.clone(),
                    gpu_model: c.gpu_model.clone(),
                    region: c.region.clone(),
                    price_per_hour: c.price_per_hour,
                    vram_bytes: c.vram_bytes,
                    latency_ms: c.latency_ms,
                    score_total: c.score_total,
                })
                .collect(),
            cloud_error: candidates_error,
            // 选了「本地」却没通过 → 部署会被拒绝，不会上云
            blocked: req.target.forces_local(),
        })
    }
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
/// 现在统一交给 [`mangosteen_local::EnginePlugin::probe`]。
fn probe_local_health(registry: &mangosteen_local::EngineRegistry, plan: &DeploymentPlan) -> bool {
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
            let build = mangosteen_cloud::build_registry(&self.config);
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
                let tunnels = mangosteen_cloud::TunnelManager::new(
                    self.config.local.port_range_start,
                    self.config.local.port_range_end,
                    "ssh",
                    Some(log_dir.clone()),
                )?;
                Some(CloudOrchestrator::new(
                    build.registry,
                    selector,
                    tunnels,
                    mangosteen_cloud::OrchestratorConfig {
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

impl Assessor {
    /// 供调度器使用（`Scheduler` 需要自己的评估器实例以计算云端显存）。
    fn clone_for_scheduler(&self) -> Assessor {
        Assessor::new(self.hardware().clone())
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
