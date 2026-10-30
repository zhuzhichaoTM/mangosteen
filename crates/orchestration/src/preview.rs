//! 部署预览：不创建任何资源的「会发生什么、要花多少钱」。
//!
//! 归属：第三层 · 编排调度核心。
//! `Orchestrator` 的字段在 `super` 中定义，子模块可直接访问。
//!
//! ## 产品定位：聚焦云端节点
//!
//! 本地推理已由 Ollama 承担，Mangosteen 只负责把模型部署到远端 GPU 节点。
//! 因此这里**不再判断本地是否可行**：预览只回答两件事——
//!
//! 1. 有哪些**已保存的设备**可直接用（设置页「设备列表」）；
//! 2. 若要用云厂商新建节点，候选与费用是多少。
//!
//! 设备优先于云厂商：用户明确登记了机器，就不该再开一台计费实例。

use super::*;

impl Orchestrator {
    /// 部署预估：**不创建任何资源**，只回答「会在哪台机器上跑、要花多少钱」。
    ///
    /// 这是「云端花钱前必须让用户确认」的执行基础。设计要点：
    ///
    /// 1. **绝不产生副作用**：不建实例、不占端口、不写台账。UI 可以在用户
    ///    每次调整参数时反复调用它。
    /// 2. **设备列表优先**：`devices` 非空时前端默认选中第一台，
    ///    此时不产生任何费用。
    /// 3. **费用给出区间而非单点**：推理时长无法预知，给「按 1 小时」口径。
    pub fn preview(&self, req: &DeploymentRequest) -> MnResult<DeploymentPreview> {
        let evaluated = self.evaluate(req)?;

        // 已保存设备（不含密码），交给前端渲染选择列表
        let devices: Vec<DeviceBrief> = self
            .config
            .remote_devices
            .iter()
            .map(DeviceBrief::from_config)
            .collect();
        // 用户显式指定 → 用它；否则默认第一台（与 resolve_device 的规则一致）
        let default_device_id = req
            .device_id
            .clone()
            .filter(|id| devices.iter().any(|d| &d.id == id))
            .or_else(|| devices.first().map(|d| d.id.clone()));

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

        // 三档候选（最低成本 / 综合最优 / 性能最强）：UI 让用户显式选一个。
        // 没有配置云服务商时保持空列表 + 原因，前端据此只显示设备列表。
        let (tier_briefs, tier_error) = match self.cloud_tier_briefs(&constraints) {
            Ok(v) => (v, None),
            Err(e) => (Vec::new(), Some(e.to_string())),
        };

        let best = self.pick_cloud(&constraints).ok().and_then(|c| c.into_iter().next());

        let cloud = best.map(|c| {
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

        // 既没有设备也没有云候选 → 明确阻断，前端不得进入下一步
        let nothing_available = devices.is_empty() && cloud.is_none();
        if nothing_available && tier_error.is_none() {
            return Ok(DeploymentPreview {
                model: evaluated.spec.name.clone(),
                model_id: mangosteen_core::model::ModelEntry::derive_id(
                    &evaluated.spec.name,
                    evaluated.spec.quant,
                ),
                origin: ComputeOrigin::Cloud {
                    provider: String::new(),
                    instance_id: String::new(),
                },
                cloud: None,
                cloud_candidates: tier_briefs,
                cloud_error: Some(
                    "没有可用的部署节点：请在「设置 → 设备列表」添加 SSH 节点，\
                     或配置可用的云服务商凭证"
                        .to_string(),
                ),
                blocked: true,
                devices,
                default_device_id,
            });
        }

        Ok(DeploymentPreview {
            model: evaluated.spec.name.clone(),
            model_id: mangosteen_core::model::ModelEntry::derive_id(
                &evaluated.spec.name,
                evaluated.spec.quant,
            ),
            origin: ComputeOrigin::Cloud {
                provider: cloud
                    .as_ref()
                    .map(|b| b.provider.clone())
                    .unwrap_or_default(),
                // 预估阶段尚未创建实例；UI 必须用 `cloud.is_some()` 判断是否走厂商
                instance_id: String::new(),
            },
            cloud,
            cloud_candidates: tier_briefs,
            cloud_error: tier_error,
            blocked: false,
            devices,
            default_device_id,
        })
    }
}
