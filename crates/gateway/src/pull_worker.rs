//! 后台模型下载与自动部署 worker。
//!
//! 归属：第二层 API Server 的异步执行面。HTTP handler 只创建任务，
//! 本模块负责在无网关锁的线程中执行「准备 → 下载 → 登记 → 启动」。

use std::sync::Arc;

use mangosteen_core::source::{ModelRef, ModelSource};
use mangosteen_core::MnResult;

use crate::pulljobs::{JobState, PullJobs};

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
/// 候选下载地址的单次测速超时（秒）。
///
/// 只发 HEAD，2 秒足够区分「可达」与「不可达」；拖太久会让下载任务
/// 在真正开始前就卡住，得不偿失。
const PROBE_TIMEOUT_SECS: u64 = 2;

/// 把实际选中的地址主机归一化为 UI/台账可读来源。
fn source_name_for_url(url: &str) -> String {
    if url.contains("hf-mirror.com") {
        "hf-mirror".to_string()
    } else if url.contains("huggingface.co") {
        "huggingface".to_string()
    } else {
        url.split("/").nth(2).unwrap_or("custom").to_string()
    }
}

pub struct DeployHooks {
    /// 下载**前**调用：评估 → 选设备 → 配置，返回待启动的云端计划。
    pub prepare: PrepareHook,
    /// 云端：设备配置完成后调用，启动引擎并探活，返回部署 ID。
    pub commit_cloud: CommitCloudHook,
    /// 复用实例钩子：实例 ID 非空时跳过创建，直接开机重启。
    pub resume: ResumeHook,
    /// 复用实例不存在时将历史记录标记为 destroyed，随后回退创建流程。
    pub invalidate: InvalidateHook,
    /// 部署成功后刷新网关业务转发快照。
    pub refresh_routes: RefreshRoutesHook,
    /// **回滚**：撤销这次部署产生的全部痕迹（停引擎、释放端口、删文件）。
    ///
    /// 实例**只停不删**：取消/卸载都保留实例与磁盘，供下次复用。
    /// 参数：`(部署 ID, 要删除的文件路径)`。
    ///
    /// 「是否删文件」由调用方判断——只有**本次下载的**文件才该删。
    /// 模型原本就在本地时（用户点的是「启动」），删掉它等于卸载已有资产。
    pub abort: AbortHook,
}

/// 模型登记回调：下载落地路径 → 模型 ID。
pub type RegisterHook =
    Arc<dyn Fn(&std::path::Path, Option<&str>) -> MnResult<String> + Send + Sync>;

/// 回滚钩子：`(部署 ID, 要删除的文件路径)` → 结果。
pub type AbortHook =
    Arc<dyn Fn(Option<&str>, Option<&std::path::Path>) -> MnResult<()> + Send + Sync>;

/// `(prepare, commit_cloud, abort)` 之外的上层装配入口，供 handlers 构造钩子时复用。
pub type CancelHook = Arc<dyn Fn() -> bool + Send + Sync>;
pub type PrepareHook =
    Arc<dyn Fn(CancelHook) -> MnResult<mangosteen_orchestration::PreparedDeployment> + Send + Sync>;

/// 云端提交钩子：已准备好的节点 → 部署 ID（启动引擎并探活）。
pub type CommitCloudHook =
    Arc<dyn Fn(mangosteen_orchestration::PreparedCloud) -> MnResult<String> + Send + Sync>;

/// 复用实例钩子：已有部署 ID → 开机重启并探活。
///
/// 这是「实例 ID 非空时复用」的落点：不创建实例、不重下权重，
/// 只把停掉的东西拉起来。
pub type ResumeHook = Arc<dyn Fn(&str) -> MnResult<String> + Send + Sync>;

/// 将失效复用记录标记为不可复用，随后由本次请求走完整创建流程。
pub type InvalidateHook = Arc<dyn Fn(&str, &str) -> MnResult<()> + Send + Sync>;
/// 刷新业务路由快照钩子。
pub type RefreshRoutesHook = Arc<dyn Fn() + Send + Sync>;

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
    // 要复用的已有部署 ID。`Some` 表示跳过创建与下载，直接开机重启。
    reuse_id: Option<String>,
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

            // ================= 第 0 段：复用已有实例 =================
            //
            // 部署页点「部署」时，如果这个模型已经有实例（之前部署过、
            // 现在处于停止状态），就不该重新创建实例、重新装依赖、
            // 重新下载几 GB 权重——那些都还在机器上。
            //
            // 判定由网关在受理请求时完成（见 `reuse_deployment_id`），
            // 这里只负责执行「开机 → 起进程 → 建隧道 → 探活」。
            if let (Some(hooks), Some(dep_id)) = (deploy.as_ref(), reuse_id.clone()) {
                if jobs.is_cancelled(&job_id) {
                    jobs.set_note(&job_id, "已取消，未产生任何变更".to_string());
                    return;
                }
                jobs.mark_deploying(&job_id);
                match (hooks.resume)(&dep_id) {
                    Ok(id) => {
                        jobs.set_deployment(&job_id, Some(&id), None);
                        jobs.mark_done_external(&job_id, "已复用已有实例并启动推理服务");
                    }
                    Err(e) => {
                        let why = format!("复用实例启动失败：{e}");
                        jobs.mark_failed(&job_id, why.clone());
                        jobs.set_deployment(&job_id, None, Some(why));
                    }
                }
                return;
            }

            // ================= 第 1 段：准备（下载前）=================
            //
            // 评估 → 选设备 → 配置。这一段**必须在下载前**跑完，用户才能
            // 先看到「跑不跑得动、占多少内存」再决定要不要花时间下载。
            //
            // 评估所需的 GGUF 元数据由 `prepare` 内部用 HTTP Range 取文件
            // 头部拿到（见 `mangosteen_base::probe_remote_metadata`），
            // 失败则降级为按体积粗估。
            if let Some(hooks) = deploy.as_ref() {
                // 准备阶段开始前先看一次：用户可能在请求返回后就点了停止
                if jobs.is_cancelled(&job_id) {
                    let rollback = (hooks.abort)(None, None);
                    jobs.set_note(
                        &job_id,
                        match rollback {
                            Ok(()) => "已取消，未产生任何部署资源".to_string(),
                            Err(e) => format!("已取消，但资源回滚失败，请检查实例与隧道：{e}"),
                        },
                    );
                    return;
                }
                let cancel_check: CancelHook = {
                    let jobs = jobs.clone();
                    let id = job_id.clone();
                    Arc::new(move || jobs.is_cancelled(&id))
                };
                match (hooks.prepare)(cancel_check) {
                    Ok(mangosteen_orchestration::PreparedDeployment::Cloud(p)) => {
                        // prepare 可能包含节点创建、SSH 配置和远端下载；
                        // 用户可在这段时间取消。必须在提交启动前再次检查，
                        // 否则取消后仍会拉起引擎。
                        if jobs.is_cancelled(&job_id) {
                            // 取消只停实例，不删机器：已装好的环境与已下载的
                            // 权重下次部署可复用。这里必须把台账 id 传下去，
                            // 否则钩子不知道要停哪台实例。
                            let rollback = (hooks.abort)(Some(p.id.as_str()), None);
                            jobs.set_note(
                                &job_id,
                                match rollback {
                                    Ok(()) => "已取消：实例已停止保存，隧道已关闭，可再次部署复用"
                                        .to_string(),
                                    Err(e) => {
                                        format!("已取消，但实例停止失败，请检查是否仍在计费：{e}")
                                    }
                                },
                            );
                            return;
                        }
                        // 云端路径：设备配置已完成（实例/隧道/权重）。
                        // 不需要本地下载，也没有本地文件可登记；
                        // 直接进入「启动模型」阶段。
                        match (hooks.commit_cloud)(*p) {
                            Ok(dep_id) => {
                                jobs.set_deployment(&job_id, Some(&dep_id), None);
                                // 成功也必须终结任务状态：只写 deployment 会让
                                // job.state 永远停在 Running（非终态），后续同
                                // 模型的部署请求会被 find_active_by_spec 复用到
                                // 这个僵尸任务上，部署看似受理、实际永不启动。
                                jobs.mark_done_external(&job_id, "云端模型已启动推理服务");
                                (hooks.refresh_routes)();
                            }
                            Err(e) => {
                                // 启动失败必须终结任务状态：只改 deploy_state
                                // 会让 job.state 永远停在 running，UI 轮询不停、
                                // 「停止」按钮也一直亮着。阶段指针保持停留在
                                // 故障发生的那一步，不自动跳转。
                                let why = format!("节点已就绪，但启动推理引擎失败：{e}");
                                jobs.mark_failed(&job_id, why.clone());
                                jobs.set_deployment(&job_id, None, Some(why));
                            }
                        }
                        return;
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

            // ---- 自动选择 HuggingFace 下载站点 ----
            //
            // UI 不再让用户在 hf-mirror / huggingface.co 之间选。这里在
            // 下载线程内测两者的可达性与往返耗时，选更快的那个；全部失败
            // 则保留原地址，让正式下载给出真实错误。
            //
            // 为什么放在线程里：测速是网络操作，绝不能在 HTTP handler 或
            // 网关锁内执行。
            // 只对内置的两个 HuggingFace 站点做自动择优；用户自定义的
            // HF 形态源（自建镜像）必须原样使用，不能被替换掉。
            let is_builtin_hf = matches!(source.name.as_str(), "hf-mirror" | "hf");
            if is_builtin_hf && source.kind == mangosteen_core::source::SourceKind::HuggingFace {
                let candidates = mangosteen_base::download::huggingface_candidates(&model);
                let ranked = mangosteen_base::download::ranked_candidates(
                    &candidates,
                    &opts.curl_bin,
                    PROBE_TIMEOUT_SECS,
                );
                if let Some((_, first)) = ranked.first() {
                    opts.url_override = Some(first.clone());
                    opts.fallback_urls = ranked.into_iter().skip(1).map(|(_, url)| url).collect();
                }
            }

            match mangosteen_base::download_model(&source, &model, &opts, false) {
                Ok(dl) => {
                    if let Some(sha) = &dl.sha256 {
                        let _ = mangosteen_base::download::write_sha_sidecar(&dl.path, sha);
                    }
                    // 下载成功但用户已在最后一刻点了停止 → 回滚（含删文件）
                    if let Some(hooks) = deploy.as_ref() {
                        if jobs.is_cancelled(&job_id) {
                            // 云端路径在 prepare 阶段已提交并 return，走到这里的
                            // 只可能是纯下载任务（无本地部署计划可回滚）。
                            let dep_id: Option<String> = None;
                            // 这个文件是本次刚下完的，属于「本次部署产生的资源」
                            let rollback =
                                (hooks.abort)(dep_id.as_deref(), Some(abort_file.as_path()));
                            jobs.set_note(
                                &job_id,
                                match rollback {
                                    Ok(()) => "已取消，已删除本次下载的文件".to_string(),
                                    Err(e) => {
                                        format!("已取消，但清理失败，请检查实例与隧道：{e}")
                                    }
                                },
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
                    let actual_source = source_name_for_url(&dl.url);
                    match register(&dl.path, Some(actual_source.as_str())) {
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
                            // 纯云端部署在 prepare 阶段已提交并 return，走到这里的
                            // 只可能是纯下载任务（CLI `models pull`），无需启动引擎。
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
                                let dep_id: Option<String> = None;
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
                            let dep_id: Option<String> = None;
                            let rollback =
                                (hooks.abort)(dep_id.as_deref(), Some(abort_file.as_path()));
                            jobs.set_note(
                                &job_id,
                                match rollback {
                                    Ok(()) => "已停止部署，下载残留与部署记录均已清理".to_string(),
                                    Err(e) => {
                                        format!("已停止部署，但资源回滚失败，请检查实例与隧道：{e}")
                                    }
                                },
                            );
                        } else {
                            jobs.set_note(
                                &job_id,
                                "已停止下载任务，下载残留保留以便续传".to_string(),
                            );
                        }
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
                        let dep_id: Option<String> = None;
                        let _ = (hooks.abort)(dep_id.as_deref(), None);
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
