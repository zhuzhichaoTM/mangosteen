//! 部署编排：「评估 → 选择 → 配置 → 执行」，统一部署到远端 GPU 节点。
//!
//! 归属：第三层 · 编排调度核心。
//! `Orchestrator` 的字段在 `super` 中定义，子模块可直接访问。

use super::*;
use mangosteen_core::plan::DeploymentPlan;

#[path = "provision.rs"]
mod cloud;
#[path = "evaluate.rs"]
mod evaluate;

/// 本地部署移除后的固定说明（评估与拒绝路径共用，避免文案漂移）。
const LOCAL_REMOVED: &str = "本地推理已交由 Ollama 承担，Mangosteen 只部署到远端节点";

impl Orchestrator {
    /// 执行部署。这是系统的主入口（模型已在本地时用）。
    ///
    /// 完整流程见模块文档。返回的 `DeploymentPlan` 已落台账。
    pub fn deploy(&mut self, req: &DeploymentRequest) -> MnResult<DeploymentPlan> {
        self.deploy_with_progress(req, &crate::progress::noop_sink)
    }
    /// 「准备」阶段：评估 → 选择设备 → 生成配置，**不启动**。
    ///
    /// `evaluated` 由调用方给出，因为网关无法自己决定它的来源：
    ///
    /// | 情形 | 获取方式 |
    /// |------|---------|
    /// | 本地已有文件 | [`Orchestrator::evaluate`]（读盘，精确） |
    /// | 远端未下载 | [`Orchestrator::evaluate_remote`]（HTTP `Range` 预取头部，精确） |
    /// | 预取失败 | [`Orchestrator::evaluate_estimated`]（按体积粗估，标注为估算） |
    ///
    /// ## 为什么必须拆成两段
    ///
    /// UI 的「部署」对未下载的模型走「评估 → 选设备 → 配置 → 下载 → 启动」：
    /// 先让用户看到「跑不跑得动、占多少内存」，再花时间下载。
    /// 下载若排在评估前，用户白等几 GB 才知道跑不动；启动若排在下载前，
    /// 引擎找不到文件必然失败。所以下载必须夹在 prepare 与 commit 之间，
    /// 且由上层在**不持网关锁**的线程里执行（见 [`the pull job manager`]）。
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
    pub(crate) fn deploy_inner(
        &mut self,
        req: &DeploymentRequest,
        sink: crate::progress::ProgressSink<'_>,
    ) -> MnResult<DeploymentPlan> {
        use crate::progress::{DeployStage, StageOutcome};

        let evaluated = self.evaluate(req)?;
        sink(
            DeployStage::Assessing,
            StageOutcome::Running,
            "正在解析模型元数据并评估所需算力资源".to_string(),
        );
        sink(
            DeployStage::Assessing,
            StageOutcome::Done,
            assessed_detail(&evaluated),
        );

        match self.prepare_with_progress(evaluated, req, sink)? {
            PreparedDeployment::Cloud(prepared) => self.commit_cloud(*prepared, sink),
        }
    }
    /// 「准备」阶段主体：唯一性检查 → 引擎检查 → 选择设备 → 落台账。
    pub(crate) fn prepare_inner(
        &mut self,
        evaluated: Evaluated,
        req: &DeploymentRequest,
        sink: crate::progress::ProgressSink<'_>,
    ) -> MnResult<PreparedDeployment> {
        use crate::progress::{DeployStage, StageOutcome};

        // ---- 云端部署必须有远端下载地址 ----
        //
        // UI 的「部署」对已下载到本地的模型走 `/api/deploy`，这条路径没有
        // 下载流程，`req.model_download` 是空的。云端部署要求「远端自己下」，
        // 所以这里从模型台账回填登记时保存的直连地址；台账里也没有时，
        // 保持为空并由 `deploy_cloud` 给出明确错误（不偷偷上传本地文件）。
        let mut req_owned;
        let req = if req.model_download.is_none() && req.target.forces_cloud() {
            let from_index = self
                .models
                .get(&req.model)
                .ok()
                .flatten()
                .and_then(|m| m.remote_url.map(|url| (url, m.spec.file_size_bytes)));
            if let Some((url, size)) = from_index {
                req_owned = req.clone();
                req_owned.model_download = Some(crate::assess::ModelDownloadSpec {
                    url,
                    expected_bytes: Some(size).filter(|s| *s > 0),
                });
                &req_owned
            } else {
                req
            }
        } else {
            req
        };

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
        // 部署前清理同模型的历史残留记录：UI 的 pull 和直接 /api/deploy
        // 都必须经过同一条清理路径。否则旧的 Ready/Provisioning 台账会被
        // 唯一性检查误判为活跃实例，直接返回“已有同 GPU 型号实例”，
        // 实际新部署根本没有开始。
        // 只停止该模型自己的进程、释放端口并移除台账；远端模型文件和实例
        // 保留，后续下载阶段会校验并复用资源。
        self.purge_model_deployments(&wanted_id)?;

        for rec in self.state.list()? {
            if !rec.status.is_active() {
                continue;
            }
            let running_id = mangosteen_core::model::ModelEntry::derive_id(
                &rec.plan.model.name,
                rec.plan.model.quant,
            );
            if running_id == wanted_id {
                // 只有「同一模型 + 同一 GPU 型号」才互相冲突。
                // 用户明确选择了另一种 GPU（例如当前活跃的是 P40、
                // 本次选择 V100S），必须允许创建新实例，不能把旧实例
                // 的存在错误地变成「请先停止」阻断。
                let same_gpu = match (
                    req.gpu_model.as_deref().filter(|s| !s.trim().is_empty()),
                    rec.plan.cloud.as_ref().map(|c| c.gpu_model.as_str()),
                ) {
                    (Some(want), Some(actual)) => actual.eq_ignore_ascii_case(want),
                    // 无法严格确认型号时不能假定匹配，避免 P40 被当成 V100S。
                    _ => false,
                };
                if !same_gpu {
                    continue;
                }
                return Err(MnError::bad_request(format!(
                    "模型 `{wanted_id}` 已有同 GPU 型号的活跃实例（{}，端口 {}）。\
                     若要重新启动，请先停止该实例",
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

        // ---- 2. 决策：远端节点（云端/已保存设备）----
        //
        // 产品层面仅部署到远端节点（本地推理交给 Ollama）。
        sink(
            DeployStage::Selecting,
            StageOutcome::Running,
            "正在选择部署节点（已保存设备优先）".to_string(),
        );

        let trail: Vec<String> = Vec::new();
        let reason = LOCAL_REMOVED.to_string();
        let mut cloud_plan = match self.scheduler.build_cloud_plan(&evaluated, req, &reason) {
            Ok(p) => p,
            Err(e) => {
                sink(DeployStage::Selecting, StageOutcome::Failed, e.to_string());
                return Err(e);
            }
        };
        let device = self.resolve_device(req);
        // 同一远端节点可承载多个模型；为每个部署分配不冲突的远端
        // llama-server 端口，不能所有模型都固定使用 8080。
        cloud_plan.engine_port = self.allocate_remote_engine_port();
        cloud_plan.engine.port = cloud_plan.engine_port;
        let why = self.scheduler.explain_local_failure(&evaluated);
        // 远端模型下载和节点配置由 prepare_cloud 按严格顺序执行：
        // 先等待实例创建并完成节点配置，再在节点侧下载模型。这里不能提前
        // 下载阶段由 prepare 内部在节点确认后上报，随后直接进入启动阶段。
        self.prepare_cloud(&evaluated, cloud_plan, trail, &why, device, sink)
    }

    /// 为同一远端节点上的多个模型分配互不冲突的远端端口。
    ///
    /// ## 为什么只看活跃部署
    ///
    /// 只有 `ready` / `provisioning` 的记录才意味着「远端进程此刻真的占着
    /// 这个端口」。把 `stopped` / `failed` 也算进来会让首台机器上的第一个
    /// 模型莫名其妙从 8081 开始——那些历史记录早已不再监听。
    ///
    /// 历史记录里 `stopped` 的实例再次启动时会复用原有端口分配逻辑
    /// （复用路径不经过这里），因此不会与这里的分配冲突。
    pub(crate) fn allocate_remote_engine_port(&self) -> u16 {
        let used: std::collections::BTreeSet<u16> = self
            .state
            .list()
            .unwrap_or_default()
            .into_iter()
            .filter(|r| r.status.is_active())
            .filter_map(|r| r.plan.cloud.map(|c| c.engine_port))
            .collect();
        (mangosteen_device::remote::LLAMA_CPP_PORT..=8191u16)
            .find(|port| !used.contains(port))
            .unwrap_or(mangosteen_device::remote::LLAMA_CPP_PORT)
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
}
