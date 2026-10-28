//! 部署预览：不创建任何资源的「会发生什么、要花多少钱」。
//!
//! 归属：第三层 · 编排调度核心。
//! `Orchestrator` 的字段在 `super` 中定义，子模块可直接访问。

use super::*;

impl Orchestrator {
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
