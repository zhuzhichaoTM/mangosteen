//! 模型下载任务管理。
//!
//! ## 为什么必须任务化
//!
//! 最初 `POST /api/models/pull` 是**同步阻塞**的：HTTP 请求一直挂着直到
//! 下载完成。这在 0.5B（469 MB）上还能忍，但换成 7B（4.5 GB）时：
//!
//! - 浏览器/代理会先超时断开，用户看到「下载失败」而服务端其实还在下
//! - 页面刷新就彻底丢失进度，无从知道下了多少
//! - 一个 worker 被长时间占用（固定线程池只有 8 个）
//!
//! 任务化的做法：请求立即返回 `job_id`，真正的下载在独立线程里跑，
//! 进度写进共享状态，前端轮询 `GET /api/models/jobs` 取进度。
//!
//! ## 断点续传靠什么
//!
//! 不靠这里的状态，靠磁盘上的 `.part` 文件——这是
//! [`mangosteen_base::download_model`] 的既有机制。因此任务即使被
//! 服务重启打断，重新发起同一模型下载时会自动从断点继续，
//! 这里的 `resumed` 字段只是把它「说给用户听」。

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use mangosteen_core::source::{ModelRef, ModelSource};
use mangosteen_core::MnResult;

/// 任务状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    /// 排队/准备中（解析元数据）
    Pending,
    /// 正在下载
    Running,
    /// 下载并校验完成
    Done,
    /// 失败（附原因）
    Failed,
    /// 用户取消
    Cancelled,
}

impl JobState {
    pub fn as_str(&self) -> &'static str {
        match self {
            JobState::Pending => "pending",
            JobState::Running => "running",
            JobState::Done => "done",
            JobState::Failed => "failed",
            JobState::Cancelled => "cancelled",
        }
    }

    /// 是否已终结（不再变化）。
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            JobState::Done | JobState::Failed | JobState::Cancelled
        )
    }
}

/// 一个下载任务的可观测状态。
#[derive(Debug, Clone)]
pub struct PullJob {
    /// 任务 ID
    pub id: String,
    /// 模型规格（`源:owner/repo/file.gguf`）
    pub spec: String,
    /// 文件名（展示用）
    pub file: String,
    /// 已在本地完整存在 → 未真正下载
    pub already_present: bool,
    /// 状态
    pub state: JobState,
    /// 已下载字节
    pub done_bytes: u64,
    /// 总字节（源未提供则为 `None`）
    pub total_bytes: Option<u64>,
    /// 开始时间（Unix 毫秒）
    pub started_ms: u64,
    /// 结束时间（Unix 毫秒）
    pub finished_ms: Option<u64>,
    /// 完成后登记的模型 ID
    pub model_id: Option<String>,
    /// 失败原因
    pub error: Option<String>,
    /// 收尾提示（如「已存在，跳过下载」）
    pub note: Option<String>,
    /// 下载成功但后续处理有问题的说明。
    ///
    /// 与 `error` 的区别是本字段**不代表失败**：文件已经完整落盘。
    /// 典型场景是下到了 imatrix 这类非模型 GGUF——传输成功，但它不能登记
    /// 成模型。把这种情况标成 failed 会让用户以为「白下了」，
    /// 而实际上文件就在磁盘上。
    pub warning: Option<String>,
    /// 本次任务是否「下载完自动部署」。
    ///
    /// UI 不再暴露独立的下载入口——用户点的是「部署」。若模型尚未下载，
    /// 系统会先下载再启动，下载只是部署的内部步骤。
    /// CLI 的 `models pull` 仍可单独下载（此字段为 false）。
    pub deploy_after: bool,
    /// 部署阶段描述（`准备中` / `部署中` / `已启动`），供 UI 展示进度。
    ///
    /// **兼容字段**：早期 UI 与 e2e 测试只认这个字符串。结构化进度见
    /// `deploy_stage` / `deploy_trail`，两者同时维护。
    pub deploy_state: Option<String>,
    /// 当前部署阶段（结构化，前端按它高亮步骤条）。
    pub deploy_stage: Option<mangosteen_orchestration::progress::DeployStage>,
    /// 当前阶段的细节说明（如「本地 · 4 线程 · 上下文 8192」）。
    pub deploy_detail: Option<String>,
    /// 部署轨迹：逐条追加的阶段事件。
    ///
    /// 为什么保留全部而非只留最新：用户想回头看「评估结论是什么」、
    /// 「为什么跳过了量化」，只留最新就把这些信息冲掉了。
    pub deploy_trail: Vec<mangosteen_orchestration::progress::StageEvent>,
    /// 自动部署成功后的部署 ID。
    pub deployment_id: Option<String>,
}

impl PullJob {
    /// 进度百分比（0~100）。总大小未知时为 `None`。
    pub fn percent(&self) -> Option<f64> {
        let total = self.total_bytes?;
        if total == 0 {
            return None;
        }
        Some((self.done_bytes as f64 / total as f64 * 100.0).clamp(0.0, 100.0))
    }

    /// 平均速度（字节/秒）。未开始或已完成时返回 `None`。
    pub fn bytes_per_sec(&self) -> Option<f64> {
        if !matches!(self.state, JobState::Running) {
            return None;
        }
        let now = mangosteen_core::time::now_unix_millis() as u64;
        let elapsed_ms = now.saturating_sub(self.started_ms);
        if elapsed_ms < 500 {
            return None;
        }
        Some(self.done_bytes as f64 / (elapsed_ms as f64 / 1000.0))
    }
}

/// 任务表。
///
/// 用 `Arc<Mutex<..>>` 而不是 `RwLock`：写入频率（每 250ms 每任务一次）
/// 与读取频率（前端每 1s 轮询一次）同量级，互斥锁足够且更简单。
#[derive(Clone, Default)]
pub struct PullJobs {
    inner: Arc<Mutex<BTreeMap<String, PullJob>>>,
    seq: Arc<Mutex<u64>>,
}

impl PullJobs {
    pub fn new() -> Self {
        Self::default()
    }

    /// 分配任务 ID。
    ///
    /// 格式 `pull-<序号>`：可读、可排序，且不泄漏时间戳之外的信息。
    fn next_id(&self) -> String {
        let mut g = match self.seq.lock() {
            Ok(g) => g,
            // 锁中毒说明有线程 panic；退回用时间戳保证仍唯一
            Err(_) => return format!("pull-{}", mangosteen_core::time::now_unix_millis()),
        };
        *g += 1;
        format!("pull-{}", *g)
    }

    /// 登记一个新任务并返回其 ID。
    pub fn create(&self, spec: &str, file: &str) -> String {
        self.create_with(spec, file, false)
    }

    /// 创建任务，并指定是否「下载完自动部署」。
    pub fn create_with(&self, spec: &str, file: &str, deploy_after: bool) -> String {
        let id = self.next_id();
        let job = PullJob {
            id: id.clone(),
            spec: spec.to_string(),
            file: file.to_string(),
            already_present: false,
            state: JobState::Pending,
            done_bytes: 0,
            total_bytes: None,
            started_ms: mangosteen_core::time::now_unix_millis() as u64,
            finished_ms: None,
            model_id: None,
            error: None,
            note: None,
            warning: None,
            deploy_after,
            deploy_state: if deploy_after {
                Some("准备中".to_string())
            } else {
                None
            },
            // 「部署中」的任务一创建就进入第一阶段，前端立刻能画出步骤条，
            // 不必等到第一次网络请求返回
            deploy_stage: if deploy_after {
                Some(mangosteen_orchestration::progress::DeployStage::Assessing)
            } else {
                None
            },
            deploy_detail: if deploy_after {
                Some("正在解析模型规格与元数据".to_string())
            } else {
                None
            },
            deploy_trail: Vec::new(),
            deployment_id: None,
        };
        if let Ok(mut g) = self.inner.lock() {
            g.insert(id.clone(), job);
        }
        id
    }

    /// 找一个「同规格且仍在跑」的任务。
    ///
    /// 用于防止用户连点两次「下载」而并发下同一个文件——
    /// 两个 curl 写同一个 `.part` 会把文件搞坏。
    pub fn find_active_by_spec(&self, spec: &str) -> Option<PullJob> {
        let g = self.inner.lock().ok()?;
        g.values()
            .find(|j| j.spec == spec && !j.state.is_terminal())
            .cloned()
    }

    /// 更新进度。
    pub fn set_progress(&self, id: &str, done: u64, total: Option<u64>) {
        if let Ok(mut g) = self.inner.lock() {
            if let Some(j) = g.get_mut(id) {
                j.done_bytes = done;
                if total.is_some() {
                    j.total_bytes = total;
                }
                // 一旦有真实字节流动，就认为进入下载中
                if j.state == JobState::Pending && done > 0 {
                    j.state = JobState::Running;
                }
            }
        }
    }

    /// 给任务补一条说明（如「从断点继续」）。
    ///
    /// 不暴露内部锁：早期版本让 server 直接 `inner_for_note()` 拿可变引用，
    /// 那是把内部状态泄漏到调用方，改一处结构就要连带改上层。
    pub fn set_note(&self, id: &str, note: impl Into<String>) {
        if let Ok(mut g) = self.inner.lock() {
            if let Some(j) = g.get_mut(id) {
                j.note = Some(note.into());
            }
        }
    }

    /// 标记开始（元数据已取到，已知总大小）。
    pub fn mark_running(&self, id: &str, total: Option<u64>) {
        if let Ok(mut g) = self.inner.lock() {
            if let Some(j) = g.get_mut(id) {
                j.state = JobState::Running;
                j.total_bytes = total;
            }
        }
    }

    /// 标记完成。
    pub fn mark_done(&self, id: &str, model_id: &str, done: u64, already_present: bool) {
        self.mark_done_with(id, Some(model_id), done, already_present, None);
    }

    /// 标记完成，并可选带一条警告。
    ///
    /// `model_id` 为 `None` 表示「文件下好了，但没能登记成模型」——
    /// 例如下到的是 imatrix 而非模型权重。这种情况**不是失败**：
    /// 传输已完成，文件在磁盘上，只是不能用于部署。
    pub fn mark_done_with(
        &self,
        id: &str,
        model_id: Option<&str>,
        done: u64,
        already_present: bool,
        warning: Option<String>,
    ) {
        if let Ok(mut g) = self.inner.lock() {
            if let Some(j) = g.get_mut(id) {
                j.state = JobState::Done;
                j.done_bytes = done;
                j.total_bytes = Some(j.total_bytes.unwrap_or(done));
                j.model_id = model_id.map(|s| s.to_string());
                j.finished_ms = Some(mangosteen_core::time::now_unix_millis() as u64);
                j.already_present = already_present;
                j.warning = warning;
                if already_present {
                    j.note = Some("本地已存在完整文件，未重复下载".to_string());
                }
            }
        }
    }

    /// 标记失败。
    pub fn mark_failed(&self, id: &str, error: impl Into<String>) {
        if let Ok(mut g) = self.inner.lock() {
            if let Some(j) = g.get_mut(id) {
                j.state = JobState::Failed;
                j.error = Some(error.into());
                j.finished_ms = Some(mangosteen_core::time::now_unix_millis() as u64);
            }
        }
    }

    /// 取消。
    pub fn cancel(&self, id: &str) -> bool {
        if let Ok(mut g) = self.inner.lock() {
            if let Some(j) = g.get_mut(id) {
                if j.state.is_terminal() {
                    return false;
                }
                j.state = JobState::Cancelled;
                j.finished_ms = Some(mangosteen_core::time::now_unix_millis() as u64);
                j.note = Some("已取消。已下载的部分保留，重新下载会续传".to_string());
                return true;
            }
        }
        false
    }

    /// 是否已被取消（供下载线程中断）。
    pub fn is_cancelled(&self, id: &str) -> bool {
        self.inner
            .lock()
            .ok()
            .and_then(|g| g.get(id).map(|j| j.state == JobState::Cancelled))
            .unwrap_or(false)
    }

    /// 列出全部任务（按 ID 排序，即创建顺序）。
    pub fn list(&self) -> Vec<PullJob> {
        self.inner
            .lock()
            .map(|g| g.values().cloned().collect())
            .unwrap_or_default()
    }

    /// 取单个任务。
    pub fn get(&self, id: &str) -> Option<PullJob> {
        self.inner.lock().ok()?.get(id).cloned()
    }

    /// 标记「进入部署阶段」。
    ///
    /// 保留旧字符串语义（`部署中`），同时推进结构化阶段。
    pub fn mark_deploying(&self, id: &str) {
        if let Ok(mut g) = self.inner.lock() {
            if let Some(j) = g.get_mut(id) {
                j.deploy_state = Some("部署中".to_string());
                if j.deploy_stage.is_none() {
                    j.deploy_stage =
                        Some(mangosteen_orchestration::progress::DeployStage::Assessing);
                }
            }
        }
    }

    /// 上报一个部署阶段事件。
    ///
    /// 同时做三件事：
    /// 1. 追加到轨迹（历史可回溯）
    /// 2. 更新「当前阶段」指针（前端据此高亮）
    /// 3. 同步旧字符串 `deploy_state`，让老调用方不失效
    ///
    /// **不覆盖** `deploy_stage` 的回退：阶段序号只允许前进。
    /// 曾考虑允许回退（如重试），但 UI 步骤条会因此来回跳，
    /// 用户会以为部署在反复失败。
    pub fn set_stage(
        &self,
        id: &str,
        stage: mangosteen_orchestration::progress::DeployStage,
        outcome: mangosteen_orchestration::progress::StageOutcome,
        detail: impl Into<String>,
    ) {
        use mangosteen_orchestration::progress::{DeployStage, StageOutcome};
        if let Ok(mut g) = self.inner.lock() {
            if let Some(j) = g.get_mut(id) {
                let detail = detail.into();
                j.deploy_trail
                    .push(mangosteen_orchestration::progress::StageEvent::now(
                        stage, outcome, &detail,
                    ));

                // 只在「前进或同级」时更新指针
                let cur_idx = j.deploy_stage.map(|s| s.index()).unwrap_or(0);
                if stage.index() >= cur_idx {
                    j.deploy_stage = Some(stage);
                }
                j.deploy_detail = Some(detail);

                j.deploy_state = Some(match (stage, outcome) {
                    (DeployStage::Ready, _) => "已启动".to_string(),
                    (DeployStage::Failed, _) => "未启动".to_string(),
                    (_, StageOutcome::Failed) => "未启动".to_string(),
                    _ => "部署中".to_string(),
                });
            }
        }
    }

    /// 记录自动部署的结果。
    pub fn set_deployment(&self, id: &str, deployment_id: Option<&str>, note: Option<String>) {
        use mangosteen_orchestration::progress::{DeployStage, StageOutcome};
        if let Ok(mut g) = self.inner.lock() {
            if let Some(j) = g.get_mut(id) {
                j.deployment_id = deployment_id.map(|s| s.to_string());
                j.deploy_state = Some(if deployment_id.is_some() {
                    "已启动".to_string()
                } else {
                    "未启动".to_string()
                });
                // 成功 → 终态 Ready；失败 → 失败事件（保留已有轨迹）
                if deployment_id.is_some() {
                    j.deploy_stage = Some(DeployStage::Ready);
                    j.deploy_detail = Some("部署完成，模型已就绪".to_string());
                    j.deploy_trail
                        .push(mangosteen_orchestration::progress::StageEvent::now(
                            DeployStage::Ready,
                            StageOutcome::Done,
                            "部署完成，模型已就绪",
                        ));
                } else {
                    j.deploy_stage = Some(DeployStage::Failed);
                    let why = note.clone().unwrap_or_else(|| "自动启动失败".to_string());
                    j.deploy_detail = Some(why.clone());
                    // 网关内部通常已经上报过一条 Failed 事件；
                    // 这里只在「还没报过」时补一条，避免轨迹里出现两条
                    // 内容几乎相同的失败记录。
                    let already_failed = j
                        .deploy_trail
                        .last()
                        .map(|e| e.outcome == StageOutcome::Failed)
                        .unwrap_or(false);
                    if !already_failed {
                        j.deploy_trail
                            .push(mangosteen_orchestration::progress::StageEvent::now(
                                DeployStage::Failed,
                                StageOutcome::Failed,
                                why,
                            ));
                    }
                }
                if let Some(n) = note {
                    j.warning = Some(n);
                }
            }
        }
    }

    /// 清空全部**已终结**任务（运行中的保留）。
    ///
    /// 为什么不一并删运行中的：那是「取消」的语义，需要真的中断
    /// 下载线程。用户说「删除后台任务」通常指清掉已完成/失败的记录，
    /// 把正在跑的一起删掉会让人以为下载被取消了却没真停。
    pub fn clear_finished(&self) -> usize {
        let mut n = 0;
        if let Ok(mut g) = self.inner.lock() {
            let before = g.len();
            g.retain(|_, j| !j.state.is_terminal());
            n = before - g.len();
        }
        n
    }

    /// 清理已终结且超过保留期的任务，避免任务表无限增长。
    ///
    /// 保留 10 分钟：够用户看清「刚才是成功还是失败」。
    pub fn prune(&self, keep_ms: u64) {
        let now = mangosteen_core::time::now_unix_millis() as u64;
        if let Ok(mut g) = self.inner.lock() {
            g.retain(|_, j| match j.finished_ms {
                Some(f) => now.saturating_sub(f) < keep_ms,
                None => true,
            });
        }
    }
}

/// 部署钩子：把「准备 → 下载 → 提交」三段串起来。
///
/// ## 为什么是两段而不是一个
///
/// 用户要求部署顺序为「评估 → 选设备 → 配置 → 下载 → 量化 → 启动」，
/// 即**下载前**就要把算力与参数定下来。而评估需要模型元数据：
///
/// - 远端模型 → `prepare` 里用 HTTP `Range` 取头部解析（精确）
/// - 取不到 → 按体积粗估（调用方需标注为估算）
///
/// 下载完成后 `register` 把文件写进台账，再 `commit` 启动引擎。
///
/// ## 为什么不直接传 Gateway 引用
///
/// 下载线程**不能持网关锁**：下载要几分钟，持锁会让整个 API 卡死
/// （这个坑在引擎版本预热上踩过）。所以两个钩子都由调用方构造，
/// 内部各自短暂加锁。
pub struct DeployHooks {
    /// 下载**前**调用：评估 → 选设备 → 配置，返回待启动的本地计划。
    ///
    /// 返回 `PreparedDeployment::Done` 表示无需下载（云端路径已在
    /// `prepare` 内完成全部工作），下载与提交都会被跳过。
    pub prepare: PrepareHook,
    /// 下载**后**调用：启动本地引擎，返回部署 ID。
    pub commit: CommitHook,
    /// **回滚**：撤销这次部署产生的全部痕迹（停引擎、删台账、释放端口、删文件）。
    ///
    /// 参数：`(部署 ID, 要删除的文件路径)`。
    ///
    /// 「是否删文件」由调用方判断——只有**本次下载的**文件才该删。
    /// 模型原本就在本地时（用户点的是「启动」），删掉它等于卸载已有资产。
    pub abort: AbortHook,
}

/// 模型登记回调：下载落地路径 → 模型 ID。
pub type RegisterHook = Arc<dyn Fn(&std::path::Path) -> MnResult<String> + Send + Sync>;

/// 回滚钩子：`(部署 ID, 要删除的文件路径)` → 结果。
pub type AbortHook =
    Arc<dyn Fn(Option<&str>, Option<&std::path::Path>) -> MnResult<()> + Send + Sync>;

/// `(prepare, commit, abort)` 之外的上层装配入口，供 handlers 构造钩子时复用。
pub type PrepareHook = Arc<dyn Fn() -> MnResult<crate::gateway::PreparedDeployment> + Send + Sync>;

/// 提交钩子：已准备的本地部署 → 部署 ID。
pub type CommitHook = Arc<dyn Fn(crate::gateway::PreparedLocal) -> MnResult<String> + Send + Sync>;

/// 启动一个下载任务（在独立线程执行）。
///
/// 返回 `job_id`。函数本身立即返回。
///
/// ## 参数为什么这么多
///
/// 下载需要的东西（源、模型引用、选项、登记回调）都已由调用方在
/// 持锁期间取好快照——**下载线程绝不持网关锁**，否则几 GB 的下载
/// 期间所有 API 都会被阻塞（这个坑在引擎版本预热上已经踩过一次）。
pub fn spawn(
    jobs: PullJobs,
    job_id: String,
    source: ModelSource,
    model: ModelRef,
    mut opts: mangosteen_base::DownloadOptions,
    register: RegisterHook,
    // 下载并登记成功后自动部署。`None` 表示只下载不部署（CLI 行为）。
    deploy: Option<DeployHooks>,
) -> std::thread::JoinHandle<()> {
    // 进度回调把字节数写回任务表
    let jobs_for_cb = jobs.clone();
    let id_for_cb = job_id.clone();
    opts.on_progress = Some(mangosteen_base::download::ProgressSink(Arc::new(
        move |done, total| {
            jobs_for_cb.set_progress(&id_for_cb, done, total);
        },
    )));

    // 取消检查：让 curl 在用户点「停止」后**真的停下**。
    //
    // 只标记任务状态是不够的——下载会继续跑、磁盘继续涨，几分钟后
    // 文件下完还会登记进台账，冒出个用户以为已经取消的「幽灵模型」。
    let jobs_for_cancel = jobs.clone();
    let id_for_cancel = job_id.clone();
    opts.should_cancel = Some(mangosteen_base::download::CancelCheck(Arc::new(
        move || jobs_for_cancel.is_cancelled(&id_for_cancel),
    )));

    // 为「线程起不来」的降级路径留一份副本：
    // 闭包一旦 move 走 `jobs`/`job_id`，外层就拿不到了。
    let jobs_fallback = jobs.clone();
    let id_fallback = job_id.clone();

    std::thread::Builder::new()
        .name(format!("mn-pull-{job_id}"))
        .spawn(move || {
            use mangosteen_orchestration::progress::{DeployStage, StageOutcome};
            jobs.mark_running(&job_id, opts.expected_bytes);

            // 只有「下载完自动部署」的任务才有阶段语义；
            // CLI 的纯下载不该在 UI 上冒出部署步骤。
            let tracked = deploy.is_some();

            // 本次任务会落到的文件路径（取消时删它）。
            //
            // 只有「文件原本不在本地」才会走到这个线程（已在本地的情况
            // 由 server 的 already 分支处理），所以这个文件一定是
            // 本次下载产生的——删它不算误删用户资产。
            let abort_file = opts.dest_dir.join(model.local_file_name());

            // ================= 第 1 段：准备（下载前）=================
            //
            // 评估 → 选设备 → 配置。这一段**必须在下载前**跑完，用户才能
            // 先看到「跑不跑得动、占多少内存」再决定要不要花时间下载。
            //
            // 评估所需的 GGUF 元数据由 `prepare` 内部用 HTTP Range 取文件
            // 头部拿到（见 `mangosteen_base::probe_remote_metadata`），
            // 失败则降级为按体积粗估。
            let mut prepared = None;
            if let Some(hooks) = deploy.as_ref() {
                // 准备阶段开始前先看一次：用户可能在请求返回后就点了停止
                if jobs.is_cancelled(&job_id) {
                    (hooks.abort)(None, None).ok();
                    jobs.set_note(&job_id, "已取消，未产生任何部署资源".to_string());
                    return;
                }
                match (hooks.prepare)() {
                    Ok(crate::gateway::PreparedDeployment::Done(_plan)) => {
                        // 云端路径：prepare 已在内部完成全部工作（含计费
                        // 实例创建）。不需要下载，也没有本地文件可登记。
                        jobs.set_deployment(&job_id, Some(&_plan.id), None);
                        return;
                    }
                    Ok(crate::gateway::PreparedDeployment::Local(p)) => {
                        prepared = Some(p);
                    }
                    Err(e) => {
                        // 准备失败（评估不可行 / 设备选择失败 / 引擎未适配）
                        // 就不该下载——这正是把评估前置的价值。
                        jobs.set_stage(
                            &job_id,
                            DeployStage::Failed,
                            StageOutcome::Failed,
                            format!("{e}"),
                        );
                        jobs.mark_failed(&job_id, format!("部署准备失败：{e}"));
                        return;
                    }
                }
                // 准备可能耗时几秒（远端预取），其间用户可能点了停止。
                // 此时台账里已经落了 provisioning 记录，必须回滚掉。
                if jobs.is_cancelled(&job_id) {
                    let dep_id = prepared.as_ref().map(|p| p.id.clone());
                    (hooks.abort)(dep_id.as_deref(), None).ok();
                    jobs.set_note(&job_id, "已取消，已回滚部署准备产生的记录".to_string());
                    return;
                }
            }

            // ================= 第 2 段：下载 =================
            if tracked {
                jobs.set_stage(
                    &job_id,
                    DeployStage::Downloading,
                    StageOutcome::Running,
                    match opts.expected_bytes {
                        Some(n) => {
                            format!("开始下载 {}", mangosteen_base::download::human_bytes(n))
                        }
                        None => "开始下载（源未提供总大小）".to_string(),
                    },
                );
            }

            match mangosteen_base::download_model(&source, &model, &opts, false) {
                Ok(dl) => {
                    if let Some(sha) = &dl.sha256 {
                        let _ = mangosteen_base::download::write_sha_sidecar(&dl.path, sha);
                    }
                    // 下载成功但用户已在最后一刻点了停止 → 回滚（含删文件）
                    if let Some(hooks) = deploy.as_ref() {
                        if jobs.is_cancelled(&job_id) {
                            let dep_id = prepared.as_ref().map(|p| p.id.clone());
                            // 这个文件是本次刚下完的，属于「本次部署产生的资源」
                            (hooks.abort)(dep_id.as_deref(), Some(abort_file.as_path())).ok();
                            jobs.set_note(
                                &job_id,
                                "已取消，已删除本次下载的文件与部署记录".to_string(),
                            );
                            return;
                        }
                    }
                    // 下载已经成功（文件完整落盘、哈希已校验）。
                    //
                    // 因此登记失败**不能**把任务判成 failed——那是把
                    // 「传输」和「能不能当模型用」两件事混在一起了。
                    // 典型场景：用户下到 imatrix（量化辅助数据），
                    // 传输完全正常，只是它本来就不是模型。
                    // 这里标记为 done + warning，如实告诉用户文件在哪、
                    // 为什么用不了。
                    match register(&dl.path) {
                        Ok(model_id) => {
                            let already = jobs
                                .get(&job_id)
                                .map(|j| j.done_bytes > 0 && j.state == JobState::Pending)
                                .unwrap_or(false);
                            jobs.mark_done(&job_id, &model_id, dl.bytes, already);
                            if tracked {
                                jobs.set_stage(
                                    &job_id,
                                    DeployStage::Downloading,
                                    StageOutcome::Done,
                                    format!(
                                        "已下载 {} 并登记为 {}",
                                        mangosteen_base::download::human_bytes(dl.bytes),
                                        model_id
                                    ),
                                );
                            }

                            // ============ 第 3 段：提交（启动）============
                            //
                            // 失败不回滚下载：文件已在磁盘上，用户可以直接
                            // 在「已下载模型」里再点「启动」。
                            // 所以这里用 warning 而不是 failed。
                            if let (Some(hooks), Some(p)) = (deploy.as_ref(), prepared) {
                                jobs.mark_deploying(&job_id);
                                match (hooks.commit)(*p) {
                                    Ok(dep_id) => {
                                        // 启动期间用户点了停止：引擎已经起来了，
                                        // 必须停掉它并回滚（否则「点了停止但模型还在跑」）。
                                        // 注意这里**不删文件**——下载已经完成并登记，
                                        // 删文件属于「卸载模型」，不是「停止部署」。
                                        if jobs.is_cancelled(&job_id) {
                                            (hooks.abort)(Some(&dep_id), None).ok();
                                            jobs.set_note(
                                                &job_id,
                                                "已停止部署，引擎已关闭、部署记录已清理\
                                                 （模型文件保留，可在「模型列表」中再次启动）"
                                                    .to_string(),
                                            );
                                            return;
                                        }
                                        jobs.set_deployment(&job_id, Some(&dep_id), None);
                                    }
                                    Err(e) => {
                                        jobs.set_deployment(
                                            &job_id,
                                            None,
                                            Some(format!(
                                                "模型已就绪，但自动启动失败：{e}。\
                                                 可在「已下载模型」里点「启动」重试"
                                            )),
                                        );
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            jobs.mark_done_with(
                                &job_id,
                                None,
                                dl.bytes,
                                false,
                                Some(format!(
                                    "文件已下载完成（{}），但无法登记为模型：{e}",
                                    mangosteen_base::download::human_bytes(dl.bytes)
                                )),
                            );
                            // 传输本身成功了，所以阶段是 Done；不能因为
                            // 「当不了模型」就把下载步骤标成失败。
                            if tracked {
                                jobs.set_stage(
                                    &job_id,
                                    DeployStage::Downloading,
                                    StageOutcome::Done,
                                    format!("已下载，但不是可推理的模型文件：{e}"),
                                );
                            }
                            // 登记失败 → 这次部署不可能成功，把 prepare 留下的
                            // `provisioning` 记录清掉，否则它会以「活跃实例」
                            // 身份永久挡住这个模型的后续部署。
                            // 不删文件：用户可能想自己处理（例如它是 imatrix）。
                            if let Some(hooks) = deploy.as_ref() {
                                let dep_id = prepared.as_ref().map(|p| p.id.clone());
                                (hooks.abort)(dep_id.as_deref(), None).ok();
                            }
                        }
                    }
                }
                Err(e) => {
                    // 用户主动取消：回滚并如实记录，不走「失败」路径。
                    // 失败会触发重试/降级，而取消的语义是「当我没做过」。
                    if e.is_cancelled() {
                        if let Some(hooks) = deploy.as_ref() {
                            let dep_id = prepared.as_ref().map(|p| p.id.clone());
                            (hooks.abort)(dep_id.as_deref(), Some(abort_file.as_path())).ok();
                        }
                        jobs.set_note(
                            &job_id,
                            "已停止部署，下载残留与部署记录均已清理".to_string(),
                        );
                        return;
                    }

                    // 下载失败也要清理 prepare 留下的 `provisioning` 记录。
                    //
                    // 不清的后果（实测踩到）：那条记录状态是「活跃」，
                    // 而唯一性检查按「同模型 + 活跃」拦人，于是这个模型
                    // **永远无法重新部署**，报「已有活跃实例」——但那个
                    // 实例根本不存在，只有一条僵尸台账。
                    //
                    // 注意**不删文件**：下载失败时文件没下完整，
                    // 保留 `.part` 是为了下次续传。
                    if let Some(hooks) = deploy.as_ref() {
                        let dep_id = prepared.as_ref().map(|p| p.id.clone());
                        (hooks.abort)(dep_id.as_deref(), None).ok();
                    }
                    if tracked {
                        jobs.set_stage(
                            &job_id,
                            DeployStage::Downloading,
                            StageOutcome::Failed,
                            format!("下载失败：{e}"),
                        );
                    }
                    jobs.mark_failed(&job_id, e.to_string());
                }
            }
        })
        .unwrap_or_else(|e| {
            // 线程起不来（极端资源耗尽）：同步标失败，不留悬挂任务
            jobs_fallback.mark_failed(&id_fallback, format!("无法启动下载线程：{e}"));
            std::thread::spawn(|| {})
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_gives_unique_increasing_ids() {
        let j = PullJobs::new();
        let a = j.create("modelscope:o/r/f.gguf", "f.gguf");
        let b = j.create("modelscope:o/r/g.gguf", "g.gguf");
        assert_ne!(a, b);
        assert_eq!(j.list().len(), 2);
    }

    #[test]
    fn progress_and_completion_are_recorded() {
        let j = PullJobs::new();
        let id = j.create("s:o/r/f.gguf", "f.gguf");
        j.mark_running(&id, Some(1000));
        j.set_progress(&id, 400, Some(1000));

        let job = j.get(&id).unwrap();
        assert_eq!(job.state, JobState::Running);
        assert_eq!(job.percent(), Some(40.0));

        j.mark_done(&id, "m:q4", 1000, false);
        let job = j.get(&id).unwrap();
        assert_eq!(job.state, JobState::Done);
        assert_eq!(job.percent(), Some(100.0));
        assert_eq!(job.model_id.as_deref(), Some("m:q4"));
        assert!(job.finished_ms.is_some());
    }

    #[test]
    fn percent_is_none_when_total_unknown() {
        let j = PullJobs::new();
        let id = j.create("s:o/r/f.gguf", "f.gguf");
        j.set_progress(&id, 123, None);
        // 总大小未知时不能编造百分比
        assert_eq!(j.get(&id).unwrap().percent(), None);
    }

    #[test]
    fn active_spec_lookup_prevents_double_download() {
        let j = PullJobs::new();
        let spec = "s:o/r/f.gguf";
        let id = j.create(spec, "f.gguf");
        // 未终结 → 能查到，用来拦重复下载
        assert!(j.find_active_by_spec(spec).is_some());
        j.mark_done(&id, "m", 1, false);
        // 已终结 → 查不到，允许重新发起
        assert!(j.find_active_by_spec(spec).is_none());
    }

    #[test]
    fn cancel_marks_terminal_and_is_idempotent() {
        let j = PullJobs::new();
        let id = j.create("s:o/r/f.gguf", "f.gguf");
        assert!(j.cancel(&id));
        assert!(j.is_cancelled(&id));
        // 对已终结的任务再取消返回 false，而不是覆盖原状态
        assert!(!j.cancel(&id));
    }

    #[test]
    fn failures_carry_reason() {
        let j = PullJobs::new();
        let id = j.create("s:o/r/f.gguf", "f.gguf");
        j.mark_failed(&id, "网络断了");
        let job = j.get(&id).unwrap();
        assert_eq!(job.state, JobState::Failed);
        assert!(job.error.unwrap().contains("网络"));
    }

    #[test]
    fn prune_removes_only_finished_old_jobs() {
        let j = PullJobs::new();
        let done = j.create("s:o/r/a.gguf", "a.gguf");
        let running = j.create("s:o/r/b.gguf", "b.gguf");
        j.mark_done(&done, "m", 1, false);

        // 保留 0ms → 已完成且立即过期
        j.prune(0);
        assert!(j.get(&done).is_none(), "已终结任务应被清理");
        assert!(j.get(&running).is_some(), "进行中任务不得被清理");
    }

    /// 下载成功但登记失败 → 必须是 done + warning，不是 failed。
    ///
    /// 这是用户实际遇到的问题：下到 imatrix_unsloth.gguf（13 MB，
    /// 完整传输成功），但它不是模型，登记失败。旧逻辑把整个任务标成
    /// failed，用户以为「白下了」，而文件其实好好躺在磁盘上。
    ///
    /// 传输成功与否、能不能当模型用，是两件事，状态必须能区分。
    #[test]
    fn download_success_with_register_failure_is_done_with_warning() {
        let j = PullJobs::new();
        let id = j.create("s:o/r/imatrix.gguf", "imatrix.gguf");
        j.mark_running(&id, Some(1000));
        j.set_progress(&id, 1000, Some(1000));

        j.mark_done_with(
            &id,
            None, // 没能登记成模型
            1000,
            false,
            Some("文件已下载完成，但无法登记为模型：这不是可推理的模型文件".into()),
        );

        let job = j.get(&id).unwrap();
        assert_eq!(job.state, JobState::Done, "下载成功就不该标 failed");
        assert!(job.error.is_none(), "不应写 error");
        assert!(job.model_id.is_none(), "没有登记成功，不应有 model_id");
        assert!(
            job.warning.as_deref().unwrap_or("").contains("无法登记"),
            "必须给出警告说明为什么用不了：{:?}",
            job.warning
        );
        assert_eq!(job.percent(), Some(100.0), "字节数应显示为已完成");
    }

    /// 清空只删已终结任务，运行中的必须保留。
    #[test]
    fn clear_finished_keeps_running_jobs() {
        let j = PullJobs::new();
        let done = j.create("s:o/r/a.gguf", "a.gguf");
        let failed = j.create("s:o/r/b.gguf", "b.gguf");
        let running = j.create("s:o/r/c.gguf", "c.gguf");

        j.mark_done(&done, "m:q4", 100, false);
        j.mark_failed(&failed, "网络断了");
        j.mark_running(&running, Some(100));

        let n = j.clear_finished();
        assert_eq!(n, 2, "应清掉 2 条已终结记录");
        assert!(j.get(&done).is_none());
        assert!(j.get(&failed).is_none());
        assert!(j.get(&running).is_some(), "运行中的任务不得被清除");
    }

    #[test]
    fn already_present_is_flagged_with_note() {
        let j = PullJobs::new();
        let id = j.create("s:o/r/f.gguf", "f.gguf");
        j.mark_done(&id, "m", 100, true);
        let job = j.get(&id).unwrap();
        assert!(job.already_present);
        assert!(job.note.unwrap().contains("未重复下载"));
    }

    /// 阶段指针只允许前进。
    ///
    /// 异步回调可能乱序到达（如「下载中」的进度事件晚于「配置完成」），
    /// 若允许回退，UI 步骤条会来回跳，用户会以为部署在反复失败。
    /// 但**轨迹要保留全部事件**——那是排查依据。
    #[test]
    fn deploy_stage_never_goes_backwards() {
        use mangosteen_orchestration::progress::{DeployStage, StageOutcome};
        let j = PullJobs::new();
        let id = j.create_with("s:o/r/f.gguf", "f.gguf", true);

        // 按新顺序：评估 → 选择 → 配置 → 下载 → 量化 → 启动
        j.set_stage(
            &id,
            DeployStage::Configuring,
            StageOutcome::Done,
            "配置完成",
        );
        j.set_stage(
            &id,
            DeployStage::Downloading,
            StageOutcome::Running,
            "开始下载",
        );
        // 迟到事件：不得把指针拉回「配置」
        j.set_stage(
            &id,
            DeployStage::Configuring,
            StageOutcome::Running,
            "迟到事件",
        );

        let job = j.get(&id).unwrap();
        assert_eq!(job.deploy_stage, Some(DeployStage::Downloading));
        assert_eq!(job.deploy_trail.len(), 3, "轨迹要保留全部事件");
    }

    /// 纯下载任务（CLI `models pull`）不该有部署阶段。
    #[test]
    fn plain_download_has_no_deploy_stage() {
        let j = PullJobs::new();
        let id = j.create("s:o/r/f.gguf", "f.gguf");
        let job = j.get(&id).unwrap();
        assert!(job.deploy_stage.is_none());
        assert!(job.deploy_trail.is_empty());
    }

    /// 部署成功 → 终态 Ready。
    #[test]
    fn set_deployment_marks_ready() {
        use mangosteen_orchestration::progress::{DeployStage, StageOutcome};
        let j = PullJobs::new();
        let id = j.create_with("s:o/r/f.gguf", "f.gguf", true);
        j.set_deployment(&id, Some("dep-1"), None);

        let job = j.get(&id).unwrap();
        assert_eq!(job.deploy_stage, Some(DeployStage::Ready));
        assert_eq!(job.deploy_state.as_deref(), Some("已启动"));
        assert_eq!(
            job.deploy_trail.last().map(|e| e.outcome),
            Some(StageOutcome::Done)
        );
    }

    /// 失败事件不能因为「网关报一次 + 任务层再报一次」而重复。
    #[test]
    fn failed_deploy_does_not_duplicate_event() {
        use mangosteen_orchestration::progress::{DeployStage, StageOutcome};
        let j = PullJobs::new();
        let id = j.create_with("s:o/r/f.gguf", "f.gguf", true);

        // 网关内部先报失败
        j.set_stage(&id, DeployStage::Failed, StageOutcome::Failed, "端口被占用");
        // 任务层收尾再报一次
        j.set_deployment(&id, None, Some("端口被占用".into()));

        let job = j.get(&id).unwrap();
        assert_eq!(job.deploy_stage, Some(DeployStage::Failed));
        let fails = job
            .deploy_trail
            .iter()
            .filter(|e| e.outcome == StageOutcome::Failed)
            .count();
        assert_eq!(fails, 1, "失败事件不应重复：{:?}", job.deploy_trail);
        assert_eq!(job.deploy_state.as_deref(), Some("未启动"));
    }
}
