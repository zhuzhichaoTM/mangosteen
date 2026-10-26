//! 调度决策器：本地优先策略的执行者。
//!
//! 归属：**API 网关层 · 调度决策器**（网关三大组件之一）。
//!
//! ## 决策流程（必须严格遵循）
//!
//! ```text
//!                 ┌──────────────────────┐
//!  部署请求 ──────▶│ ① 评估本地可行性     │
//!                 └──────────┬───────────┘
//!                            │
//!              ┌─────────────┴─────────────┐
//!         本地可行                     本地不可行
//!              │                           │
//!              ▼                           │
//!    ┌──────────────────┐                  │
//!    │ ② 下发本地部署    │                  │
//!    └────────┬─────────┘                  │
//!             │                            │
//!        ┌────┴────┐                       │
//!      成功      失败                       │
//!        │        │                        │
//!        ▼        └────────────┬───────────┘
//!   返回方案              ┌────▼─────────────┐
//!                        │ ③ 云端调度        │
//!                        │  节点选择器打分    │
//!                        └────┬─────────────┘
//!                             │
//!                        成功 / 重试耗尽
//! ```
//!
//! ## 核心不变量
//!
//! * **I1**：`allow_cloud = false` 时绝不产生云端方案；
//! * **I2**：云端方案必然带非空 `fallback_trail`（记录为何没能用本地）；
//! * **I3**：本地评估「可行」时，第一选择必然是本地（禁止默认优先云端）；
//! * **I4**：降级重试次数不超过配置上限。
//!
//! 这四条由本模块单测中的 `invariant_*` 用例逐条守护。

use mangosteen_core::cloud::InstanceState;
use mangosteen_core::estimate::Placement;
use mangosteen_core::plan::{CloudPlan, ComputeOrigin, DeploymentPlan, LocalPlan};
use mangosteen_core::policy::SelectionConstraints;
use mangosteen_core::config::SchedulerConfig;
use mangosteen_core::{MnError, MnResult};

use crate::assess::{Assessor, DeploymentRequest, Evaluated};

/// 决策结果。
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// 走本地算力
    Local {
        /// 本地部署参数
        plan: LocalPlan,
    },
    /// 走云端算力
    Cloud {
        /// 云端部署参数
        plan: Box<CloudPlan>,
        /// 为何没能用本地（必须非空，用于审计与展示）
        reason: String,
    },
    /// 拒绝部署
    Reject {
        /// 原因
        reason: String,
    },
}

impl Decision {
    /// 是否选择了本地。
    pub fn is_local(&self) -> bool {
        matches!(self, Decision::Local { .. })
    }

    /// 是否选择了云端。
    pub fn is_cloud(&self) -> bool {
        matches!(self, Decision::Cloud { .. })
    }

    /// 算力来源描述。
    pub fn origin_label(&self) -> String {
        match self {
            Decision::Local { .. } => "本地算力".to_string(),
            Decision::Cloud { plan, .. } => format!("云端算力 · {}", plan.provider),
            Decision::Reject { .. } => "已拒绝".to_string(),
        }
    }

    /// 降级原因（本地方案为 `None`）。
    pub fn fallback_reason(&self) -> Option<&str> {
        match self {
            Decision::Cloud { reason, .. } => Some(reason),
            _ => None,
        }
    }
}

/// 一次调度尝试的结果。
#[derive(Debug, Clone, PartialEq)]
pub struct SchedulerOutcome {
    /// 最终方案
    pub plan: DeploymentPlan,
    /// 决策明细
    pub decision: Decision,
    /// 完整降级轨迹（按发生顺序）
    pub trail: Vec<String>,
}

impl SchedulerOutcome {
    /// 是否走了降级路径。
    pub fn was_degraded(&self) -> bool {
        !self.trail.is_empty()
    }

    /// 人类可读的降级说明。
    pub fn trail_text(&self) -> String {
        if self.trail.is_empty() {
            "未发生降级".to_string()
        } else {
            self.trail.join(" → ")
        }
    }
}

/// 本地部署执行结果（由调用方在执行后回填）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalLaunchOutcome {
    /// 成功，携带监听端口
    Success {
        /// 端口
        port: u16,
    },
    /// 失败
    Failed {
        /// 失败原因
        reason: String,
    },
}

/// 调度决策器。
pub struct Scheduler {
    assessor: Assessor,
    config: SchedulerConfig,
    /// 端口分配起始偏移（每次部署递增，避免重复绑定）
    next_port_offset: std::sync::atomic::AtomicU32,
    /// 本地端口区间（含端点）
    local_ports: (u16, u16),
    /// 已被外部占用的端口（来自台账），分配时跳过
    reserved: std::sync::Mutex<Vec<u16>>,
}

impl Scheduler {
    /// 构造。
    pub fn new(assessor: Assessor, config: SchedulerConfig) -> Self {
        Self {
            assessor,
            config,
            next_port_offset: std::sync::atomic::AtomicU32::new(0),
            local_ports: (11_435, 11_999),
            reserved: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// 配置引用。
    pub fn config(&self) -> &SchedulerConfig {
        &self.config
    }

    /// 评估器引用。
    pub fn assessor(&self) -> &Assessor {
        &self.assessor
    }

    /// 分配一个本地端口（区间内轮转，跳过已预留与不可绑定的端口）。
    ///
    /// 区间耗尽时退回起点（此时部署会因端口占用失败并触发降级，
    /// 但仍返回一个合法端口，让错误发生在启动阶段而非分配阶段）。
    fn allocate_local_port(&self) -> u16 {
        use std::sync::atomic::Ordering;
        let reserved = self.reserved.lock().map(|v| v.clone()).unwrap_or_default();
        let span = self.config_port_span().max(1);
        let start_offset = self.next_port_offset.fetch_add(1, Ordering::Relaxed);

        for i in 0..span {
            let port = self.local_port_base()
                + ((start_offset.wrapping_add(i)) % span) as u16;
            if reserved.contains(&port) {
                continue;
            }
            if !mangosteen_platform::process::is_port_bindable(port) {
                continue;
            }
            // 记入预留，防止同一进程内连续分配撞车
            if let Ok(mut v) = self.reserved.lock() {
                if !v.contains(&port) {
                    v.push(port);
                }
            }
            return port;
        }
        self.local_port_base()
    }

    fn local_port_base(&self) -> u16 {
        // 端口区间由 local 配置给出；调度器只负责轮转
        self.local_ports.0
    }

    fn config_port_span(&self) -> u32 {
        (self.local_ports.1 - self.local_ports.0) as u32 + 1
    }

    /// 设置本地端口区间（链式，失败时返回错误）。
    pub fn with_port_range(mut self, start: u16, end: u16) -> MnResult<Self> {
        self.set_port_range(start, end)?;
        Ok(self)
    }

    /// 预留一个端口（已被其它部署占用）。
    ///
    /// 分配器本身只做区间轮转，不知道外部占用情况；网关在构造时把台账里
    /// 已有端口灌进来，避免新部署撞端口。
    pub fn reserve_port(&self, port: u16) {
        self.reserved.lock().map(|mut v| {
            if !v.contains(&port) {
                v.push(port);
            }
        }).ok();
    }

    /// 释放一个已预留的端口。
    ///
    /// 用于**回滚**：部署被用户中止后，prepare 阶段占用的端口要还回去，
    /// 否则反复「部署→停止」会一点点吃光端口区间（11516 起 400 多个），
    /// 最终表现为「端口耗尽，部署失败」。
    pub fn release_port(&self, port: u16) {
        if let Ok(mut v) = self.reserved.lock() {
            v.retain(|p| *p != port);
        }
    }

    /// 就地设置本地端口区间。
    pub fn set_port_range(&mut self, start: u16, end: u16) -> MnResult<()> {
        if start >= end {
            return Err(MnError::config(
                "local.port_range",
                "起始端口必须小于结束端口",
            ));
        }
        if start < 1024 {
            return Err(MnError::config(
                "local.port_range",
                "不得使用特权端口（<1024）",
            ));
        }
        self.local_ports = (start, end);
        Ok(())
    }

    /// 决策：只产出方案，不执行。
    ///
    /// 这是「本地优先」的核心实现。执行由网关负责，
    /// 执行失败后再调用 [`Scheduler::degrade_after_local_failure`]。
    pub fn decide(
        &self,
        evaluated: &Evaluated,
        req: &DeploymentRequest,
    ) -> MnResult<Decision> {
        // ---- 0. 用户显式指定的落位优先 ----
        //
        // 默认是「本地优先、不足降级云端」（产品基线）。但用户可以在部署页
        // 明确选「本地」或「云端」，此时必须尊重他的选择：
        // 选了本地就**绝不**静默上云（会产生意外费用），
        // 选了云端就跳过本地尝试（用户可能本机正忙）。
        if req.target.forces_local() {
            if !evaluated.is_local_viable() {
                return Ok(Decision::Reject {
                    reason: format!(
                        "{}。你选择了「本地算力」，不会自动改用云端。\
                         可减少上下文长度、换更小的量化，或改选「云端算力」",
                        self.explain_local_failure(evaluated)
                    ),
                });
            }
            let port = self.allocate_local_port();
            let plan = LocalPlan::from_assessment(
                &evaluated.spec,
                evaluated.model_path.to_string_lossy(),
                &evaluated.assessment,
                port,
            );
            return Ok(Decision::Local { plan });
        }

        if req.target.forces_cloud() {
            let why = "按你的选择使用「云端算力」（跳过本地）".to_string();
            let cloud_plan = self.build_cloud_plan(evaluated, req, &why)?;
            return Ok(Decision::Cloud {
                plan: Box::new(cloud_plan),
                reason: why,
            });
        }

        // ---- 本地可行性判定 ----
        if evaluated.is_local_viable() {
            // I3：本地可行 → 首选本地，不因「慢」而降级
            let port = self.allocate_local_port();
            let plan = LocalPlan::from_assessment(
                &evaluated.spec,
                evaluated.model_path.to_string_lossy(),
                &evaluated.assessment,
                port,
            );
            return Ok(Decision::Local { plan });
        }

        // ---- 本地不可行 → 云端 ----
        let local_reason = self.explain_local_failure(evaluated);

        if !req.allow_cloud {
            return Ok(Decision::Reject {
                reason: format!(
                    "{local_reason}。且本次请求禁止使用云端算力（--local-only），无法部署"
                ),
            });
        }

        // I2：云端方案必须带原因
        let cloud_plan = self.build_cloud_plan(evaluated, req, &local_reason)?;
        Ok(Decision::Cloud {
            plan: Box::new(cloud_plan),
            reason: local_reason,
        })
    }

    /// 本地部署失败后的降级决策。
    ///
    /// 与 [`Scheduler::decide`] 的区别：这里**已知**本地试过且失败，
    /// 因此降级轨迹里要记下具体的失败原因（而不是「资源不足」这种预判）。
    pub fn degrade_after_local_failure(
        &self,
        evaluated: &Evaluated,
        req: &DeploymentRequest,
        failure: &str,
        attempts_so_far: u32,
    ) -> MnResult<Decision> {
        // I4：重试上限
        if attempts_so_far >= self.config.max_fallbacks {
            return Ok(Decision::Reject {
                reason: format!(
                    "本地部署失败（{failure}），且降级重试已达上限 {} 次",
                    self.config.max_fallbacks
                ),
            });
        }
        if !req.allow_cloud {
            return Ok(Decision::Reject {
                reason: format!(
                    "本地部署失败（{failure}），且本次请求禁止使用云端算力（--local-only）"
                ),
            });
        }
        let reason = format!("本地部署失败：{failure}");
        let cloud_plan = self.build_cloud_plan(evaluated, req, &reason)?;
        Ok(Decision::Cloud {
            plan: Box::new(cloud_plan),
            reason,
        })
    }

    /// 生成云端方案。
    fn build_cloud_plan(
        &self,
        evaluated: &Evaluated,
        req: &DeploymentRequest,
        reason: &str,
    ) -> MnResult<CloudPlan> {
        let ctx = evaluated.assessment.context_len;
        let kv = evaluated.assessment.kv_cache;
        let batch = evaluated.assessment.batch_size;

        // 云端所需显存 = 同一套估算（推理引擎唯一性原则）
        let required_vram = self
            .assessor
            .cloud_vram_requirement(&evaluated.spec, ctx, kv, batch);

        let _constraints = SelectionConstraints {
            min_vram_bytes: required_vram,
            min_memory_bytes: evaluated.assessment.requirement.total_bytes(),
            min_disk_bytes: evaluated.spec.file_size_bytes.saturating_mul(2),
            max_price_per_hour: req.max_price_per_hour.or(self.config.max_price_per_hour),
            allowed_providers: Vec::new(),
            allowed_regions: Vec::new(),
            require_stock: self.config.require_stock,
        };

        // 引擎参数：与本地同一套语义，仅端口/绑定不同
        let engine = LocalPlan::from_assessment(
            &evaluated.spec,
            // 云端路径约定：模型放在实例的数据盘目录下
            format!("/root/mangosteen/models/{}", model_file_name(&evaluated.spec.name)),
            &evaluated.assessment,
            // 云端跑的是 llama.cpp，端口用 `LLAMA_CPP_PORT`（8080）。
            // 隧道必须转发到服务真正监听的端口，否则隧道建好了也连不上。
            mangosteen_cloud::remote::LLAMA_CPP_PORT,
        );

        // 云端跑 GPU：层数必须全量卸载，否则会用 CPU 算——那不是用户要的。
        // `from_assessment` 是按**本地**硬件算的（本机无 GPU 时 gpu_layers=0），
        // 直接搬过来会让云端推理慢几十倍。
        let mut engine = engine;
        engine.gpu_layers = evaluated.spec.n_layers;

        let _ = reason;
        Ok(CloudPlan {
            provider: String::new(), // 由编排器按打分结果回填
            gpu_model: String::new(),
            gpu_count: 1,
            vram_bytes_per_gpu: required_vram,
            cpu_cores: 8,
            memory_bytes: evaluated.assessment.requirement.total_bytes().max(8 << 30),
            disk_bytes: evaluated
                .spec
                .file_size_bytes
                .saturating_mul(2)
                .max(50 << 30),
            image: "ubuntu22.04-cuda12".to_string(),
            model_path: format!(
                "/root/mangosteen/models/{}",
                model_file_name(&evaluated.spec.name)
            ),
            engine_port: mangosteen_cloud::remote::LLAMA_CPP_PORT,
            local_port: self.allocate_local_port(),
            engine,
            max_price_per_hour: req.max_price_per_hour.or(self.config.max_price_per_hour),
            // 主链路：**远端自己下载**。设备选云端时，下载就该发生在云端——
            // 不要求本地先下一份再传上去。地址与本地下载同源同规则
            // （`ModelSource::download_url`），所以云端拿到的就是用户选的那个文件。
            download_url: req.model_download.as_ref().map(|d| d.url.clone()),
            download_bytes: req.model_download.as_ref().and_then(|d| d.expected_bytes),
            remote_model: None,
        })
    }

    /// 解释本地为何不可行（用于降级轨迹与用户提示）。
    pub fn explain_local_failure(&self, evaluated: &Evaluated) -> String {
        let a = &evaluated.assessment;
        match a.placement {
            Placement::Impossible => {
                let gpu = self.assessor.hardware().total_usable_accelerator_memory();
                let host = self.assessor.hardware().host.usable_memory();
                format!(
                    "本地资源不足：需要 {}，可用显存 {}、可用内存 {}",
                    a.requirement.describe(),
                    mangosteen_platform::fs::human_bytes(gpu),
                    mangosteen_platform::fs::human_bytes(host)
                )
            }
            _ => "本地部署不可行".to_string(),
        }
    }

    /// 组装最终部署方案（供网关在决策后落库）。
    pub fn assemble_plan(
        &self,
        id: impl Into<String>,
        evaluated: &Evaluated,
        decision: &Decision,
        trail: Vec<String>,
    ) -> MnResult<DeploymentPlan> {
        let id = id.into();
        match decision {
            Decision::Local { plan } => Ok(DeploymentPlan {
                id,
                model: evaluated.spec.clone(),
                origin: ComputeOrigin::Local,
                local: Some(plan.clone()),
                cloud: None,
                assessment: evaluated.assessment.clone(),
                created_at: mangosteen_core::time::now_unix(),
                fallback_trail: trail,
            }),
            Decision::Cloud { plan, .. } => Ok(DeploymentPlan {
                id,
                model: evaluated.spec.clone(),
                origin: ComputeOrigin::Cloud {
                    provider: plan.provider.clone(),
                    instance_id: String::new(), // 编排后回填
                },
                local: None,
                cloud: Some((**plan).clone()),
                assessment: evaluated.assessment.clone(),
                created_at: mangosteen_core::time::now_unix(),
                // I2：云端方案必须带非空轨迹
                fallback_trail: if trail.is_empty() {
                    vec!["cloud:unspecified".to_string()]
                } else {
                    trail
                },
            }),
            Decision::Reject { reason } => Err(MnError::InsufficientLocalResource {
                detail: reason.clone(),
            }),
        }
    }

    /// 把节点选择结果回填到云端方案。
    pub fn apply_selection(plan: &mut CloudPlan, provider: &str, spec_id: &str, gpu_model: &str, vram: u64) {
        plan.provider = provider.to_string();
        plan.gpu_model = gpu_model.to_string();
        plan.vram_bytes_per_gpu = vram;
        let _ = spec_id;
    }

    /// 实例是否已可用于推理。
    pub fn instance_ready(state: InstanceState) -> bool {
        state.is_serving()
    }
}

/// 从模型名推导云端文件名。
fn model_file_name(model: &str) -> String {
    let slug: String = model
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    format!("{}.gguf", slug.trim_matches('-'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assess::ComputeTarget;
    use mangosteen_core::estimate::{AssessRequest, Assessment, KvCacheType, ResourceRequirement};
    use mangosteen_core::hardware::{Accelerator, AcceleratorVendor, HardwareProfile, HostProfile};
    use mangosteen_core::model::{ModelSpec, Quantization};

    fn hw(vram_gib: u64, ram_gib: u64) -> HardwareProfile {
        let mut accs = Vec::new();
        if vram_gib > 0 {
            accs.push(Accelerator {
                name: "RTX 4090".into(),
                vendor: AcceleratorVendor::Nvidia,
                vram_bytes: vram_gib << 30,
                unified_memory_bytes: 0,
                compute_capability: Some("sm_89".into()),
                fp16_tflops: Some(165.0),
                memory_bandwidth_gbps: Some(1008.0),
            });
        }
        HardwareProfile {
            os: "linux".into(),
            arch: "x86_64".into(),
            host: HostProfile {
                total_memory_bytes: ram_gib << 30,
                available_memory_bytes: (ram_gib / 2) << 30,
                physical_cores: 16,
                logical_cores: 32,
                cpu_model: "test".into(),
            },
            accelerators: accs,
        }
    }

    fn spec_7b() -> ModelSpec {
        ModelSpec {
            name: "qwen2.5-7b-instruct".into(),
            params_b: 7.615,
            quant: Quantization::Q4KM,
            n_layers: 28,
            n_heads: 28,
            n_kv_heads: 4,
            embedding_dim: 3584,
            vocab_size: 152_064,
            native_context: 32_768,
            file_size_bytes: 4_683_073_024,
        }
    }

    fn spec_70b() -> ModelSpec {
        ModelSpec {
            name: "llama3.3-70b".into(),
            params_b: 70.6,
            quant: Quantization::Q4KM,
            n_layers: 80,
            n_heads: 64,
            n_kv_heads: 8,
            embedding_dim: 8192,
            vocab_size: 128_256,
            native_context: 131_072,
            file_size_bytes: 42_500_000_000,
        }
    }

    fn scheduler(vram_gib: u64, ram_gib: u64) -> Scheduler {
        let a = Assessor::new(hw(vram_gib, ram_gib));
        Scheduler::new(a, SchedulerConfig::default())
            .with_port_range(11500, 11600)
            .unwrap()
    }

    fn evaluated(spec: ModelSpec, s: &Scheduler) -> Evaluated {
        s.assessor()
            .evaluate_spec(&spec, &DeploymentRequest::default().with_context(4096))
    }

    // ---------------- I1: 禁止云端时绝不产生云端方案 ----------------

    #[test]
    fn invariant_i1_local_only_never_yields_cloud() {
        let s = scheduler(4, 8); // 放不下 70B
        let e = evaluated(spec_70b(), &s);
        let req = DeploymentRequest::for_model("m").with_context(4096).local_only();
        let d = s.decide(&e, &req).unwrap();
        assert!(!d.is_cloud(), "禁止云端时不得返回云端方案：{d:?}");
        assert!(matches!(d, Decision::Reject { .. }));
    }

    // ---------------- I2: 云端方案必带降级原因 ----------------

    #[test]
    fn invariant_i2_cloud_always_has_reason() {
        let s = scheduler(4, 8);
        let e = evaluated(spec_70b(), &s);
        let req = DeploymentRequest::for_model("m").with_context(4096);
        let d = s.decide(&e, &req).unwrap();
        assert!(d.is_cloud(), "本地放不下 70B，应降级云端");
        let reason = d.fallback_reason().expect("云端方案必须有降级原因");
        assert!(!reason.trim().is_empty());
        assert!(reason.contains("本地"), "原因应说明本地情况：{reason}");
    }

    #[test]
    fn assemble_plan_fills_trail_for_cloud() {
        let s = scheduler(4, 8);
        let e = evaluated(spec_70b(), &s);
        let d = s
            .decide(&e, &DeploymentRequest::for_model("m").with_context(4096))
            .unwrap();
        let plan = s.assemble_plan("d1", &e, &d, vec![]).unwrap();
        // 即使传入空轨迹，也必须被补成非空
        assert!(!plan.fallback_trail.is_empty());
        assert!(plan.respects_local_first());
    }

    // ---------------- I3: 本地可行时首选本地 ----------------

    #[test]
    fn invariant_i3_local_viable_prefers_local() {
        let s = scheduler(24, 64);
        let e = evaluated(spec_7b(), &s);
        assert!(e.is_local_viable());
        let d = s
            .decide(&e, &DeploymentRequest::for_model("m").with_context(8192))
            .unwrap();
        assert!(d.is_local(), "本地可行时必须选本地，实际：{d:?}");
    }

    #[test]
    fn invariant_i3_cpu_only_still_counts_as_local() {
        // 无 GPU 但内存够 → CPU 可跑 → 仍应选本地（不因慢而降级）
        let s = scheduler(0, 64);
        let e = evaluated(spec_7b(), &s);
        assert_eq!(e.assessment.placement, Placement::CpuOnly);
        assert!(e.is_local_viable());
        let d = s
            .decide(&e, &DeploymentRequest::for_model("m").with_context(4096))
            .unwrap();
        assert!(d.is_local(), "CPU 可跑也算本地可行，不应降级：{d:?}");
    }

    #[test]
    fn local_plan_has_valid_port_and_args() {
        let s = scheduler(24, 64);
        let e = evaluated(spec_7b(), &s);
        let d = s
            .decide(&e, &DeploymentRequest::for_model("m").with_context(4096))
            .unwrap();
        match d {
            Decision::Local { plan } => {
                assert!(plan.port >= 11500 && plan.port <= 11600);
                assert_eq!(plan.host, "127.0.0.1");
                assert!(plan.gpu_layers > 0);
                let args = plan.to_engine_args();
                assert!(args.contains(&"--n-gpu-layers".to_string()));
            }
            other => panic!("expected local, got {other:?}"),
        }
    }

    #[test]
    fn ports_rotate_across_decisions() {
        let s = scheduler(24, 64);
        let e = evaluated(spec_7b(), &s);
        let req = DeploymentRequest::for_model("m").with_context(4096);
        let mut ports = Vec::new();
        for _ in 0..5 {
            if let Decision::Local { plan } = s.decide(&e, &req).unwrap() {
                ports.push(plan.port);
            }
        }
        assert_eq!(ports.len(), 5);
        // 严格递增即可，**不要求连续**：区间内的端口若被本机其它进程
        // （或并行的其它测试）占住，分配器会跳过——那是正确行为。
        // 之前断言 `w[1] == w[0] + 1`，并行跑测试时偶发假失败。
        assert!(
            ports.windows(2).all(|w| w[1] > w[0]),
            "连续分配应严格递增：{ports:?}"
        );
    }

    /// 释放预留的端口，之后可被重新分配。
    ///
    /// 这条守护的是「部署被中止后端口能还回去」——若不还，
    /// 反复「部署→停止」会一点点吃光区间，最终表现为
    /// 「端口耗尽，部署失败」，而用户根本没跑过几个模型。
    #[test]
    fn released_port_becomes_allocatable_again() {
        let s = scheduler(24, 64);
        let span = (s.local_ports.1 - s.local_ports.0) as usize + 1;

        // 把区间里**每一个**端口都预留掉
        for p in s.local_ports.0..=s.local_ports.1 {
            s.reserve_port(p);
        }
        // 全被预留时，分配器只能回退到区间起点（此时部署会因端口占用失败）
        let starved = s.allocate_local_port();
        assert_eq!(starved, s.local_ports.0, "区间耗尽时应回退到起点");

        // 释放其中一个 → 必须能被分配出来
        let target = s.local_ports.0 + 7;
        s.release_port(target);
        let got = s.allocate_local_port();
        assert_eq!(
            got, target,
            "释放后的端口 {target} 应可被再次分配（实际给了 {got}），\
             区间跨度 {span}"
        );
    }

    #[test]
    fn release_port_is_idempotent() {
        let s = scheduler(24, 64);
        let p = s.local_ports.0 + 3;
        s.reserve_port(p);
        s.release_port(p);
        s.release_port(p); // 再释放一次不应 panic 或影响其它端口
        assert!(!s.reserved.lock().unwrap().contains(&p));
    }

    // ---------------- I4: 重试上限 ----------------

    #[test]
    fn invariant_i4_respects_fallback_limit() {
        let mut cfg = SchedulerConfig::default();
        cfg.max_fallbacks = 1;
        let a = Assessor::new(hw(4, 8));
        let s = Scheduler::new(a, cfg).with_port_range(11500, 11600).unwrap();
        let e = evaluated(spec_70b(), &s);
        let req = DeploymentRequest::for_model("m").with_context(4096);

        // 第 0 次 → 允许降级
        let d = s.degrade_after_local_failure(&e, &req, "OOM", 0).unwrap();
        assert!(d.is_cloud());

        // 第 1 次 → 已达上限
        let d = s.degrade_after_local_failure(&e, &req, "OOM", 1).unwrap();
        assert!(matches!(d, Decision::Reject { .. }), "{d:?}");
        if let Decision::Reject { reason } = d {
            assert!(reason.contains("上限"), "{reason}");
        }
    }

    #[test]
    fn degrade_after_local_failure_records_specific_reason() {
        let s = scheduler(24, 64);
        let e = evaluated(spec_7b(), &s);
        let req = DeploymentRequest::for_model("m").with_context(4096);
        let d = s
            .degrade_after_local_failure(&e, &req, "端口 11500 被占用", 0)
            .unwrap();
        assert!(d.is_cloud());
        let reason = d.fallback_reason().unwrap();
        assert!(reason.contains("端口 11500 被占用"), "{reason}");
    }

    #[test]
    fn degrade_respects_local_only() {
        let s = scheduler(24, 64);
        let e = evaluated(spec_7b(), &s);
        let req = DeploymentRequest::for_model("m").local_only();
        let d = s
            .degrade_after_local_failure(&e, &req, "OOM", 0)
            .unwrap();
        assert!(matches!(d, Decision::Reject { .. }));
    }

    // ---------------- 云端方案构造 ----------------

    #[test]
    fn cloud_plan_uses_same_engine_semantics() {
        let s = scheduler(4, 8);
        let e = evaluated(spec_70b(), &s);
        let d = s
            .decide(&e, &DeploymentRequest::for_model("m").with_context(4096))
            .unwrap();
        match d {
            Decision::Cloud { plan, .. } => {
                // 引擎参数语义与本地一致：同一套 flag
                let args = plan.engine_args();
                assert!(args.contains(&"--ctx-size".to_string()));
                assert!(args.contains(&"--n-gpu-layers".to_string()));
                // 云端绑 0.0.0.0
                assert!(args.contains(&"0.0.0.0".to_string()));
                // 云端路径
                assert!(plan.model_path.starts_with("/root/mangosteen/models/"));
                assert!(plan.model_path.ends_with(".gguf"));
                // 显存需求非零
                assert!(plan.vram_bytes_per_gpu > 0);
            }
            other => panic!("expected cloud, got {other:?}"),
        }
    }

    #[test]
    fn cloud_plan_respects_price_cap() {
        let s = scheduler(4, 8);
        let e = evaluated(spec_70b(), &s);
        let mut req = DeploymentRequest::for_model("m").with_context(4096);
        req.max_price_per_hour = Some(2.5);
        let d = s.decide(&e, &req).unwrap();
        match d {
            Decision::Cloud { plan, .. } => {
                assert_eq!(plan.max_price_per_hour, Some(2.5));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn scheduler_config_price_cap_is_fallback() {
        let mut cfg = SchedulerConfig::default();
        cfg.max_price_per_hour = Some(9.9);
        let a = Assessor::new(hw(4, 8));
        let s = Scheduler::new(a, cfg).with_port_range(11500, 11600).unwrap();
        let e = evaluated(spec_70b(), &s);
        // 请求未指定 → 用配置
        let d = s
            .decide(&e, &DeploymentRequest::for_model("m").with_context(4096))
            .unwrap();
        if let Decision::Cloud { plan, .. } = d {
            assert_eq!(plan.max_price_per_hour, Some(9.9));
        } else {
            panic!("expected cloud");
        }
    }

    // ---------------- 方案组装 ----------------

    #[test]
    fn assemble_local_plan_is_consistent() {
        let s = scheduler(24, 64);
        let e = evaluated(spec_7b(), &s);
        let d = s
            .decide(&e, &DeploymentRequest::for_model("m").with_context(4096))
            .unwrap();
        let plan = s.assemble_plan("dep-1", &e, &d, vec![]).unwrap();
        assert_eq!(plan.id, "dep-1");
        assert!(plan.origin.is_local());
        assert!(plan.local.is_some());
        assert!(plan.cloud.is_none());
        assert!(plan.respects_local_first());
        assert!(plan.local_access_port().is_some());
    }

    #[test]
    fn assemble_reject_is_an_error() {
        let s = scheduler(4, 8);
        let e = evaluated(spec_70b(), &s);
        let d = Decision::Reject {
            reason: "nope".into(),
        };
        let err = s.assemble_plan("d", &e, &d, vec![]).unwrap_err();
        assert!(matches!(err, MnError::InsufficientLocalResource { .. }));
    }

    #[test]
    fn explain_local_failure_mentions_numbers() {
        let s = scheduler(4, 8);
        let e = evaluated(spec_70b(), &s);
        let msg = s.explain_local_failure(&e);
        assert!(msg.contains("本地资源不足"), "{msg}");
        assert!(msg.contains("GiB"), "应给出具体数值：{msg}");
    }

    #[test]
    fn apply_selection_fills_provider() {
        let mut plan = CloudPlan {
            provider: String::new(),
            gpu_model: String::new(),
            gpu_count: 1,
            vram_bytes_per_gpu: 0,
            cpu_cores: 8,
            memory_bytes: 8 << 30,
            disk_bytes: 50 << 30,
            image: "i".into(),
            model_path: "/m".into(),
            engine_port: 8080,
            download_url: None,
            download_bytes: None,
            remote_model: None,
            local_port: 11500,
            engine: LocalPlan {
                model_path: "/m".into(),
                context_size: 4096,
                threads: 8,
                batch_size: 512,
                gpu_layers: 29,
                kv_cache_type_k: KvCacheType::F16,
                kv_cache_type_v: KvCacheType::F16,
                flash_attention: true,
                use_mmap: true,
                use_mlock: false,
                port: 8080,
                host: "127.0.0.1".into(),
                engine: Default::default(),
            },
            max_price_per_hour: None,
        };
        Scheduler::apply_selection(&mut plan, "autodl", "s1", "RTX 4090", 24 << 30);
        assert_eq!(plan.provider, "autodl");
        assert_eq!(plan.gpu_model, "RTX 4090");
        assert_eq!(plan.vram_bytes_per_gpu, 24 << 30);
    }

    #[test]
    fn port_range_validation() {
        let a = Assessor::new(hw(24, 64));
        let mut s = Scheduler::new(a, SchedulerConfig::default());
        assert!(s.set_port_range(11600, 11500).is_err());
        assert!(s.set_port_range(11500, 11500).is_err());
        assert!(s.set_port_range(80, 90).is_err(), "特权端口必须拒绝");
        assert!(s.set_port_range(11500, 11600).is_ok());
    }

    #[test]
    fn instance_ready_matches_serving_state() {
        assert!(Scheduler::instance_ready(InstanceState::Running));
        assert!(!Scheduler::instance_ready(InstanceState::Stopped));
        assert!(!Scheduler::instance_ready(InstanceState::Starting));
    }

    #[test]
    fn model_file_name_is_slugged() {
        assert_eq!(model_file_name("qwen2.5 7B/instruct"), "qwen2.5-7B-instruct.gguf");
        assert_eq!(model_file_name("simple"), "simple.gguf");
    }

    // ---------------- 兜底：评估结构完整性 ----------------

    #[test]
    fn assessment_is_self_consistent() {
        let a = Assessment {
            placement: Placement::FullGpu,
            requirement: ResourceRequirement {
                weights_bytes: 1,
                kv_cache_bytes: 1,
                compute_buffer_bytes: 1,
                runtime_overhead_bytes: 1,
            },
            context_len: 4096,
            kv_cache: KvCacheType::F16,
            quant: Quantization::Q4KM,
            threads: 8,
            batch_size: 512,
            accelerator_budget_bytes: 1 << 30,
            host_budget_bytes: 2 << 30,
            warnings: vec![],
        };
        assert_eq!(a.requirement.total_bytes(), 4);
        assert!(a.placement.is_local_satisfiable());
    }

    #[test]
    fn assess_request_defaults_match_request_defaults() {
        let r = DeploymentRequest::default();
        let ar = r.to_assess_request();
        assert_eq!(ar.allow_cpu_fallback, r.allow_cpu_fallback);
        assert_eq!(ar.batch_size, r.batch_size);
        // 与 core 的默认值保持一致（防止两处漂移）
        let core_default = AssessRequest::default();
        assert_eq!(ar.batch_size, core_default.batch_size);
        assert_eq!(ar.allow_cpu_fallback, core_default.allow_cpu_fallback);
    }
    // ---------------- 算力来源选择（本地 / 云端） ----------------

    /// 选了「本地」且资源不足 → **必须拒绝**，绝不能静默上云。
    ///
    /// 这是本功能最重要的一条：用户明确选择本地的动机通常就是
    /// 「我不想花钱」。若此时自动降级到云端，会在他不知情的情况下
    /// 开出计费实例——这是比「部署失败」严重得多的错误。
    #[test]
    fn target_local_insufficient_refuses_and_never_goes_cloud() {
        let s = scheduler(4, 8); // 放不下 70B
        let e = evaluated(spec_70b(), &s);
        let mut req = DeploymentRequest::for_model("m").with_context(4096);
        req.target = ComputeTarget::Local;

        let d = s.decide(&e, &req).unwrap();
        assert!(
            matches!(d, Decision::Reject { .. }),
            "选本地但资源不足必须拒绝：{d:?}"
        );
        assert!(!d.is_cloud(), "绝不能返回云端方案");

        // 拒绝原因要能指导用户怎么办
        if let Decision::Reject { reason } = d {
            assert!(reason.contains("本地算力"), "{reason}");
            assert!(reason.contains("不会自动改用云端"), "{reason}");
        }
    }

    /// 选了「本地」且资源充足 → 正常本地部署。
    #[test]
    fn target_local_sufficient_deploys_locally() {
        let s = scheduler(24, 64);
        let e = evaluated(spec_7b(), &s);
        let mut req = DeploymentRequest::for_model("m").with_context(4096);
        req.target = ComputeTarget::Local;

        let d = s.decide(&e, &req).unwrap();
        assert!(matches!(d, Decision::Local { .. }), "{d:?}");
    }

    /// 选了「云端」→ 跳过本地，即使本地完全够用。
    ///
    /// 场景：本机正跑着别的任务，用户主动要求用云端。
    /// 若仍走本地，用户会看到「部署完成」但拿不到预期的隔离环境。
    #[test]
    fn target_cloud_skips_local_even_when_local_viable() {
        let s = scheduler(24, 64);
        let e = evaluated(spec_7b(), &s);
        assert!(e.is_local_viable(), "前提：本地本来够用");

        let mut req = DeploymentRequest::for_model("m").with_context(4096);
        req.target = ComputeTarget::Cloud;

        let d = s.decide(&e, &req).unwrap();
        assert!(d.is_cloud(), "选云端必须走云端：{d:?}");
        if let Decision::Cloud { reason, .. } = d {
            assert!(reason.contains("云端算力"), "{reason}");
        }
    }

    /// 默认（Auto）行为不变：本地优先，不足才降级。
    #[test]
    fn target_auto_keeps_local_first() {
        let s = scheduler(24, 64);
        let e = evaluated(spec_7b(), &s);
        let req = DeploymentRequest::for_model("m").with_context(4096);
        assert_eq!(req.target, ComputeTarget::Auto);
        assert!(matches!(s.decide(&e, &req).unwrap(), Decision::Local { .. }));

        // 本地不足 → 降级云端
        let s2 = scheduler(4, 8);
        let e2 = evaluated(spec_70b(), &s2);
        assert!(s2.decide(&e2, &req).unwrap().is_cloud());
    }

    #[test]
    fn compute_target_parsing_is_strict() {
        assert_eq!(ComputeTarget::parse("local"), Some(ComputeTarget::Local));
        assert_eq!(ComputeTarget::parse("CLOUD"), Some(ComputeTarget::Cloud));
        assert_eq!(ComputeTarget::parse(" auto "), Some(ComputeTarget::Auto));
        // 未知值必须返回 None，由调用方报错——静默回退会让用户
        // 以为选择生效了，实际开出计费实例
        assert_eq!(ComputeTarget::parse("gpu"), None);
        assert_eq!(ComputeTarget::parse(""), None);
    }
}
