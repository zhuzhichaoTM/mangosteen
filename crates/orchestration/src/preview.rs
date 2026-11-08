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
use mangosteen_core::model::Quantization;

/// 硬编码的模型参数（离线阶段提前读取）
///
/// 当前只支持 qwen2.5-0.5b 和 qwen3.8-27b 两个模型
fn get_hardcoded_model_spec(model_id: &str) -> Option<mangosteen_core::model::ModelSpec> {
    // 支持匹配完整 ID（含量化后缀）和基础 ID
    // 处理多种变体：qwen2.5-0.5b, qwen2.5-0.5b:q4_k_m, qwen2.5-0.5b-instruct:q4_k_m 等
    let base_id = model_id
        .split(':')
        .next()
        .unwrap_or(model_id)
        .trim()
        .to_lowercase()
        .replace("-instruct", "")
        .replace("instruct", "")
        .replace("gguf", "");
    
    match base_id.as_str() {
        id if id.contains("qwen2.5") && id.contains("0.5b") => Some(mangosteen_core::model::ModelSpec {
            name: "qwen2.5-0.5b".to_string(),
            params_b: 0.5, // 0.5B 参数
            quant: Quantization::Q4KM,
            n_layers: 24,
            n_heads: 14,
            n_kv_heads: 2,
            embedding_dim: 896,
            vocab_size: 151936,
            native_context: 32768,
            file_size_bytes: 16_464_440_224, // qwen2.5-0.5b-instruct-q4_k_m.gguf 约 15.3GB
        }),
        id if id.contains("qwen3.8") && id.contains("27b") => Some(mangosteen_core::model::ModelSpec {
            name: "qwen3.8-27b".to_string(),
            params_b: 27.0, // 27B 参数
            quant: Quantization::Q4KM,
            n_layers: 62,
            n_heads: 40,
            n_kv_heads: 8,
            embedding_dim: 5120,
            vocab_size: 152064,
            native_context: 32768,
            file_size_bytes: 16_464_440_224, // Qwen3.8-27B-UD-Q4_K_M.gguf 约 15.3GB
        }),
        _ => None,
    }
}

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
        // 使用硬编码的模型参数（离线阶段提前读取）
        // 当前只支持 qwen2.5-0.5b 和 qwen3.8-27b 两个模型
        let spec = get_hardcoded_model_spec(&req.model)
            .ok_or_else(|| MnError::bad_request(format!("不支持的模型: {}，当前只支持 qwen2.5-0.5b 和 qwen3.8-27b", req.model)))?;

        let assessment = self.assessor.evaluate_spec(&spec, req);

        // 已保存设备（不含密码），交给前端渲染选择列表
        let devices: Vec<DeviceBrief> = self
            .config()
            .remote_devices
            .iter()
            .map(DeviceBrief::from_config)
            .collect();
        // 默认设备**必须为空**：部署页的候选列表是云端候选（云规格 + 运行中
        // 实例），历史上默认取第一台自定义节点会误导前端，且某些路径会让
        // 「选了 GPU 规格」的请求被劫持到自定义节点。只有用户显式指定
        // device_id 时才回传，否则保持 None。
        let default_device_id = req
            .device_id
            .clone()
            .filter(|id| devices.iter().any(|d| &d.id == id));

        // 云端显存需求与本地同一套公式（推理引擎唯一性 → 参数体系一致）
        let required_vram = self.assessor.cloud_vram_requirement(
            &spec,
            assessment.assessment.context_len,
            assessment.assessment.kv_cache,
            assessment.assessment.batch_size,
        );
        let constraints = SelectionConstraints {
            min_vram_bytes: required_vram,
            min_memory_bytes: assessment.assessment.requirement.total_bytes().max(8 << 30),
            min_disk_bytes: spec
                .file_size_bytes
                .saturating_mul(2)
                .max(50 << 30),
            max_price_per_hour: req.max_price_per_hour,
            allowed_providers: Vec::new(),
            allowed_regions: Vec::new(),
            require_stock: self.config().scheduler.require_stock,
        };

        // 三档候选（最低成本 / 综合最优 / 性能最强）：UI 让用户显式选一个。
        // 没有配置云服务商时保持空列表 + 原因，前端据此只显示设备列表。
        let (tier_briefs, tier_error) = match self.cloud_tier_briefs(&constraints) {
            Ok(v) => (v, None),
            Err(e) => (Vec::new(), Some(e.to_string())),
        };

        let best = self
            .pick_cloud(&constraints)
            .ok()
            .and_then(|c| c.into_iter().next());

        let cloud = best.map(|c| {
            let hours = default_estimate_hours();
            let sane = crate::deployment::sane_hourly_price(c.price_per_hour);
            CloudPreview {
                provider: c.provider.clone(),
                spec_id: c.spec_id.clone(),
                gpu_model: c.gpu_model.clone(),
                gpu_count: 1,
                vram_bytes: c.vram_bytes,
                region: c.region.clone(),
                latency_ms: c.latency_ms,
                price_per_hour: sane,
                estimate_hours: hours,
                estimate_total: sane.map(|p| p * hours as f64),
                required_vram_bytes: required_vram,
            }
        });

        // 云端**已有**实例（运行中/已停机）：节点选择弹窗把它们作为
        // 「可复用节点」与新购规格并列展示。查询失败降级为空列表，
        // 绝不阻断整个预览——复用是可选项，创建链路不受影响。
        let cloud_instances = self.cloud_instance_briefs();

        // 既没有设备也没有云候选 → 明确阻断，前端不得进入下一步
        let nothing_available = devices.is_empty() && cloud.is_none();
        if nothing_available && tier_error.is_none() {
            return Ok(DeploymentPreview {
                model: spec.name.clone(),
                model_id: mangosteen_core::model::ModelEntry::derive_id(
                    &spec.name,
                    spec.quant,
                ),
                origin: ComputeOrigin::Cloud {
                    provider: String::new(),
                    instance_id: String::new(),
                },
                cloud: None,
                cloud_candidates: tier_briefs,
                cloud_instances,
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
            model: spec.name.clone(),
            model_id: mangosteen_core::model::ModelEntry::derive_id(
                &spec.name,
                spec.quant,
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
            cloud_instances,
            cloud_error: tier_error,
            blocked: false,
            devices,
            default_device_id,
        })
    }
}
