//! 部署编排：「评估 → 选择 → 配置 → 执行」与本地失败后的云端降级。
//!
//! 归属：网关 crate 内部（第三层编排职责的第二层门面实现）。
//! `Gateway` 的字段在 `super`（gateway 模块）中定义，子模块可直接访问。

use super::*;

impl Gateway {
    /// 执行部署。这是系统的主入口（模型已在本地时用）。
    ///
    /// 完整流程见模块文档。返回的 `DeploymentPlan` 已落台账。
    ///
    /// 若模型尚未下载，用 [`Gateway::prepare_with_progress`] +
    /// [`Gateway::commit_local`] 把「下载」插在两段之间。
    pub fn deploy(&mut self, req: &DeploymentRequest) -> MnResult<DeploymentPlan> {
        self.deploy_with_progress(req, &mangosteen_orchestration::progress::noop_sink)
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
        sink: mangosteen_orchestration::progress::ProgressSink<'_>,
    ) -> MnResult<PreparedDeployment> {
        let result = self.prepare_inner(evaluated, req, sink);
        if let Err(e) = &result {
            sink(
                mangosteen_orchestration::progress::DeployStage::Failed,
                mangosteen_orchestration::progress::StageOutcome::Failed,
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
        sink: mangosteen_orchestration::progress::ProgressSink<'_>,
    ) -> MnResult<DeploymentPlan> {
        let result = self.commit_local_inner(prepared, sink);
        if let Err(e) = &result {
            sink(
                mangosteen_orchestration::progress::DeployStage::Failed,
                mangosteen_orchestration::progress::StageOutcome::Failed,
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
        sink: mangosteen_orchestration::progress::ProgressSink<'_>,
    ) -> MnResult<DeploymentPlan> {
        let result = self.deploy_inner(req, sink);
        // 失败统一在这里上报：主体里有十几处 `return Err`，逐个埋点既
        // 啰嗦又容易漏——漏掉的那条会表现为「进度条永远卡在某一步」。
        if let Err(e) = &result {
            sink(
                mangosteen_orchestration::progress::DeployStage::Failed,
                mangosteen_orchestration::progress::StageOutcome::Failed,
                format!("{e}"),
            );
        }
        result
    }
    /// 部署主体（一次跑完：模型已在本地时用）。
    ///
    /// 实现上就是「prepare + commit」背靠背，中间不插下载。
    pub(crate) fn deploy_inner(
        &mut self,
        req: &DeploymentRequest,
        sink: mangosteen_orchestration::progress::ProgressSink<'_>,
    ) -> MnResult<DeploymentPlan> {
        use mangosteen_orchestration::progress::{DeployStage, StageOutcome};

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
    pub(crate) fn prepare_inner(
        &mut self,
        evaluated: Evaluated,
        req: &DeploymentRequest,
        sink: mangosteen_orchestration::progress::ProgressSink<'_>,
    ) -> MnResult<PreparedDeployment> {
        use mangosteen_orchestration::progress::{DeployStage, StageOutcome};

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
    pub(crate) fn commit_local_inner(
        &mut self,
        prepared: PreparedLocal,
        sink: mangosteen_orchestration::progress::ProgressSink<'_>,
    ) -> MnResult<DeploymentPlan> {
        use mangosteen_orchestration::progress::{DeployStage, StageOutcome};

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
    pub(crate) fn launch_local(
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
    /// 收到的标准配置，不依赖 `mangosteen-device`，也不解释引擎 flags。
    pub(crate) fn cloud_launch_config(
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
    pub(crate) fn deploy_cloud(
        &mut self,
        evaluated: &Evaluated,
        cloud_plan: mangosteen_core::plan::CloudPlan,
        trail: Vec<String>,
        why_cloud: &str,
        sink: mangosteen_orchestration::progress::ProgressSink<'_>,
    ) -> MnResult<DeploymentPlan> {
        use mangosteen_orchestration::progress::{DeployStage, StageOutcome};

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
    /// 生成部署 ID。
    pub(crate) fn new_deployment_id(&self, evaluated: &Evaluated) -> String {
        let seq = self.next_seq();
        format!(
            "dep-{:x}{:03x}",
            mangosteen_core::time::now_unix_millis(),
            seq & 0xfff
        ) + &format!("-{}", sanitize(&evaluated.spec.name))
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
    /// 评估器引用（供上层在**不重入网关方法**时直接做一次评估）。
    ///
    /// 为什么需要：`pulljobs` 的部署钩子在 prepare 阶段要判断
    /// 「本地文件在不在，在就读盘精确评估」，而这个判断发生在闭包内、
    /// 已经短暂持有网关锁。走 [`Gateway::evaluate`] 会绕一圈（它按 ID
    /// 查台账），而这里手上的就是确切路径，直接用评估器更准。
    pub fn assessor_public(&self) -> &Assessor {
        &self.assessor
    }
}
