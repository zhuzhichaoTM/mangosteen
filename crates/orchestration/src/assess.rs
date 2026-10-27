//! 资源评估引擎。
//!
//! 归属：**API 网关层 · 资源评估引擎**（网关三大组件之一）。
//!
//! ## 输入输出
//!
//! ```text
//! 输入：部署请求（模型 + 上下文诉求 + 是否允许 CPU 回退）
//!      硬件快照（由平台适配层采集，可缓存）
//! 输出：Assessment（可行性 + 落位形态 + 最优参数 + 告警）
//! ```
//!
//! ## 与 `mangosteen_core::estimate` 的分工
//!
//! * `core::estimate` 是**纯函数算法**（不读文件、不碰硬件）；
//! * 本模块是**编排层**：读 GGUF 拿真实架构参数 → 调纯函数算法 →
//!   把结果包装成带上下文的决策依据。
//!
//! 这样算法可以 100% 单测，编排层只需测「参数是否正确传递」。

use std::path::{Path, PathBuf};

use mangosteen_core::estimate::{self, AssessRequest, Assessment, KvCacheType, Placement};
use mangosteen_core::gguf;
use mangosteen_core::hardware::HardwareProfile;
use mangosteen_core::model::ModelSpec;
use mangosteen_core::{MnError, MnResult};
use serde::{Deserialize, Serialize};
/// 用户指定的算力落位。
///
/// 为什么需要它：默认策略是「本地优先、不足才降级云端」，这是产品基线。
/// 但用户有时明确知道自己要什么——例如手边机器在跑别的任务、想全部放到
/// 云端；或反过来，想把数据留在本地、绝不上云。
///
/// 因此提供一个**显式覆盖**，同时保留默认的自动策略：
/// - `Auto`（默认）：本地优先，不足才降级
/// - `Local`：只用本地，不足就报错，绝不静默上云（避免意外计费）
/// - `Cloud`：只用云端，跳过本地尝试
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ComputeTarget {
    /// 自动：本地优先，不足降级云端（产品默认）
    #[default]
    Auto,
    /// 强制本地：资源全部在本地，不足则失败
    Local,
    /// 强制云端：资源全部在云端，跳过本地
    Cloud,
}

impl ComputeTarget {
    /// 从字符串解析（HTTP 入参）。
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "local" => Some(Self::Local),
            "cloud" => Some(Self::Cloud),
            _ => None,
        }
    }

    /// 是否强制本地。
    pub fn forces_local(self) -> bool {
        matches!(self, Self::Local)
    }

    /// 是否强制云端。
    pub fn forces_cloud(self) -> bool {
        matches!(self, Self::Cloud)
    }
}

/// 部署诉求。
///
/// 需要 `Clone`：`prepare` 与 `commit` 分成两段后，降级判据（
/// `target.forces_local()`）要在 commit 阶段再用一次，而两段之间可能
/// 已跨越下载（几十分钟）。传引用会让上层难以持有（下载线程需要
/// `'static`），克隆一份成本极低（几个字段）。
#[derive(Debug, Clone)]
pub struct DeploymentRequest {
    /// 模型 ID（台账中的 ID，或本地 GGUF 路径）
    pub model: String,
    /// 期望上下文长度。`None` 表示「尽量大」
    pub context_len: Option<u32>,
    /// 期望 KV 精度
    pub kv_cache: Option<KvCacheType>,
    /// 是否接受纯 CPU 回退
    pub allow_cpu_fallback: bool,
    /// 是否接受部分卸载
    pub allow_partial_offload: bool,
    /// 是否允许云端降级
    pub allow_cloud: bool,
    /// 最高可接受云端时价（元/小时）
    pub max_price_per_hour: Option<f64>,
    /// 批大小
    pub batch_size: u32,
    /// 用户指定的算力落位（默认 `Auto`：本地优先）
    pub target: ComputeTarget,
    /// 模型文件的**远端下载信息**（云端直连下载用）。
    ///
    /// 设备选云端时，模型的下载应该发生在云端。这里带上「从哪下、多大」，
    /// 编排层才能把它写进云端方案，让远端自己下——而不是要求本地先下一份
    /// 再传上去。
    ///
    /// `None` 的情形：本地文件路径直接部署（`models add` 的产物）、或调用方
    /// 是 CLI/e2e 且没走下载流程。此时云端只能走「上传本地文件」的备选链路。
    pub model_download: Option<ModelDownloadSpec>,
}

/// 模型文件的远端下载信息。
#[derive(Debug, Clone, PartialEq)]
pub struct ModelDownloadSpec {
    /// 直连下载地址（由 `ModelSource::download_url` 拼出，与本地下载同规则）
    pub url: String,
    /// 期望字节数。`None` 表示未知，此时不做大小校验（不假装校验过）
    pub expected_bytes: Option<u64>,
}

impl Default for DeploymentRequest {
    fn default() -> Self {
        Self {
            model: String::new(),
            context_len: None,
            kv_cache: None,
            allow_cpu_fallback: true,
            allow_partial_offload: true,
            allow_cloud: true,
            max_price_per_hour: None,
            batch_size: 512,
            target: ComputeTarget::Auto,
            model_download: None,
        }
    }
}

impl DeploymentRequest {
    /// 指定模型。
    pub fn for_model(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            ..Default::default()
        }
    }

    /// 指定上下文。
    pub fn with_context(mut self, ctx: u32) -> Self {
        self.context_len = Some(ctx);
        self
    }

    /// 禁止云端降级（用于「只想知道本地能不能跑」的查询场景）。
    pub fn local_only(mut self) -> Self {
        self.allow_cloud = false;
        self
    }

    /// 转成纯算法的请求参数。
    pub fn to_assess_request(&self) -> AssessRequest {
        AssessRequest {
            context_len: self.context_len,
            kv_cache: self.kv_cache,
            allow_cpu_fallback: self.allow_cpu_fallback,
            allow_partial_offload: self.allow_partial_offload,
            batch_size: self.batch_size,
        }
    }
}

/// 评估结果（带解析出的模型信息）。
#[derive(Debug, Clone, PartialEq)]
pub struct Evaluated {
    /// 解析出的模型规格
    pub spec: ModelSpec,
    /// 模型文件绝对路径（本地部署用）
    pub model_path: PathBuf,
    /// 评估结论
    pub assessment: Assessment,
}

impl Evaluated {
    /// 本地是否可承载。
    pub fn is_local_viable(&self) -> bool {
        self.assessment.placement.is_local_satisfiable()
    }

    /// 是否为「全量 GPU 卸载」这一最优形态。
    pub fn is_full_gpu(&self) -> bool {
        matches!(self.assessment.placement, Placement::FullGpu)
    }

    /// 一行摘要。
    pub fn summary(&self) -> String {
        format!(
            "{} · {} · ctx={} · {}",
            self.spec.name,
            self.assessment.placement.describe(self.spec.n_layers),
            self.assessment.context_len,
            self.assessment.requirement.describe()
        )
    }
}

/// 粗估兜底用的典型架构参数（7B 级稠密模型）。
///
/// 只在远端元数据预取失败时使用，见 [`Assessor::evaluate_estimated`]。
const ESTIMATE_LAYERS: u32 = 32;
const ESTIMATE_HEADS: u32 = 32;
const ESTIMATE_KV_HEADS: u32 = 8;
const ESTIMATE_EMBEDDING: u32 = 4096;
const ESTIMATE_VOCAB: u32 = 151_936;
const ESTIMATE_NATIVE_CTX: u32 = 32_768;

/// 资源评估引擎。
///
/// 缓存硬件快照：硬件不会在秒级内变化，而采集要跑外部命令（`nvidia-smi`），
/// 频繁调用会拖慢交互。
pub struct Assessor {
    hardware: HardwareProfile,
}

impl Assessor {
    /// 使用给定硬件快照。
    pub fn new(hardware: HardwareProfile) -> Self {
        Self { hardware }
    }

    /// 现场采集硬件。
    pub fn probe() -> Self {
        let probe = mangosteen_platform::hardware::probe();
        Self::new(probe.profile)
    }

    /// 硬件快照引用。
    pub fn hardware(&self) -> &HardwareProfile {
        &self.hardware
    }

    /// 评估一个**本地 GGUF 文件**。
    ///
    /// 解析失败（文件损坏 / 非 GGUF）直接返回错误——不做猜测。
    pub fn evaluate_file(&self, path: &Path, req: &DeploymentRequest) -> MnResult<Evaluated> {
        let info = gguf::parse_file(path)?;
        let spec = info.to_model_spec();
        spec.validate().map_err(|reason| MnError::GgufFormat {
            path: path.to_path_buf(),
            reason,
        })?;
        let assessment = estimate::assess(&spec, &self.hardware, &req.to_assess_request());
        Ok(Evaluated {
            spec,
            model_path: path.to_path_buf(),
            assessment,
        })
    }

    /// 评估一个**已知规格**（云端模型、或台账里已有记录的模型）。
    ///
    /// 不读文件，因此不校验磁盘存在性。
    pub fn evaluate_spec(&self, spec: &ModelSpec, req: &DeploymentRequest) -> Evaluated {
        let assessment = estimate::assess(spec, &self.hardware, &req.to_assess_request());
        Evaluated {
            spec: spec.clone(),
            model_path: PathBuf::new(),
            assessment,
        }
    }

    /// 用**远端预取的 GGUF 元数据**评估（模型尚未下载到本地）。
    ///
    /// 这是「下载前评估」的正路：部署流程用 HTTP `Range` 只取文件头部，
    /// 解析出真实架构参数后走这里。`GgufInfo::to_model_spec` 会按
    /// 文件大小与量化位宽反推参数量，因此结论与本地评估**等价**。
    pub fn evaluate_remote(
        &self,
        info: &mangosteen_core::gguf::GgufInfo,
        req: &DeploymentRequest,
    ) -> Evaluated {
        self.evaluate_spec(&info.to_model_spec(), req)
    }

    /// 仅凭「名字 + 量化 + 字节数」的**粗估**评估。
    ///
    /// ## 什么时候才用它
    ///
    /// 只在远端元数据预取失败（源不支持 Range、网络抖动）时兜底，
    /// 目的是**不让一次网络问题阻断整个部署**。调用方必须把结论标注为
    /// 「估算」，不能与精确评估混为一谈。
    ///
    /// ## 为什么架构参数取保守中位值
    ///
    /// 层数与 KV 头数决定 KV Cache 大小，而这两项从文件名与体积推不出来。
    /// 取「32 层 / 32 头 / 8 KV 头 / 4096 维」这组主流 7B 级稠密模型的
    /// 典型值：对多数模型误差在 2 倍以内，够用来判断「大概能不能跑」，
    /// 不足以据此精调参数。因此下载完成后会用**真实文件重估**，
    /// 实际启动参数以那次为准。
    pub fn evaluate_estimated(
        &self,
        name: &str,
        quant: mangosteen_core::model::Quantization,
        file_size_bytes: u64,
        req: &DeploymentRequest,
    ) -> Evaluated {
        let bpw = quant.bits_per_weight();
        let params_b = if file_size_bytes > 0 {
            (file_size_bytes as f64 * 8.0 / bpw as f64) / 1e9 * 0.99
        } else {
            0.0
        };
        let spec = ModelSpec {
            name: name.to_string(),
            params_b,
            quant,
            n_layers: ESTIMATE_LAYERS,
            n_heads: ESTIMATE_HEADS,
            n_kv_heads: ESTIMATE_KV_HEADS,
            embedding_dim: ESTIMATE_EMBEDDING,
            vocab_size: ESTIMATE_VOCAB,
            native_context: ESTIMATE_NATIVE_CTX,
            file_size_bytes,
        };
        self.evaluate_spec(&spec, req)
    }

    /// 在给定预算内挑选能跑的最高质量量化档。
    ///
    /// 用于「用户说想要 32B，但只有 24GB 卡」这类场景：
    /// 与其直接报「跑不了」，不如告诉用户「Q3_K_M 可以跑」。
    pub fn suggest_quantization(
        &self,
        spec: &ModelSpec,
        req: &DeploymentRequest,
        budget_bytes: u64,
    ) -> Option<(mangosteen_core::model::Quantization, Assessment)> {
        estimate::best_quant_within(spec, &self.hardware, &req.to_assess_request(), budget_bytes)
    }

    /// 云端实例需要多少显存才能承载。
    pub fn cloud_vram_requirement(
        &self,
        spec: &ModelSpec,
        context_len: u32,
        kv: KvCacheType,
        batch: u32,
    ) -> u64 {
        estimate::cloud_requirement_bytes(spec, context_len, kv, batch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mangosteen_core::hardware::{Accelerator, AcceleratorVendor, HostProfile};
    use mangosteen_core::model::Quantization;

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

    fn write_gguf(dir: &Path) -> PathBuf {
        let mut bytes = gguf::build_test_gguf("qwen2", 28, 28, 4, 3584, 32768);
        bytes.extend(std::iter::repeat(0u8).take(4096));
        let p = dir.join("q.gguf");
        std::fs::write(&p, &bytes).unwrap();
        p
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mn-assess-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn evaluates_real_gguf_file() {
        let d = tmpdir("file");
        let f = write_gguf(&d);
        let a = Assessor::new(hw(24, 64));
        let e = a.evaluate_file(&f, &DeploymentRequest::default()).unwrap();
        assert_eq!(e.spec.n_layers, 28);
        assert_eq!(e.spec.n_kv_heads, 4);
        assert!(e.is_local_viable());
        assert!(e.summary().contains("qwen2"));
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn corrupt_file_returns_gguf_error() {
        let d = tmpdir("corrupt");
        let f = d.join("bad.gguf");
        std::fs::write(&f, b"not a gguf at all, just text padding here").unwrap();
        let a = Assessor::new(hw(24, 64));
        let err = a
            .evaluate_file(&f, &DeploymentRequest::default())
            .unwrap_err();
        assert!(matches!(err, MnError::GgufFormat { .. }));
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn missing_file_returns_model_not_found() {
        let a = Assessor::new(hw(24, 64));
        let err = a
            .evaluate_file(
                Path::new("/no/such/model.gguf"),
                &DeploymentRequest::default(),
            )
            .unwrap_err();
        assert!(matches!(err, MnError::ModelNotFound { .. }));
    }

    #[test]
    fn spec_evaluation_skips_disk() {
        let spec = ModelSpec {
            name: "qwen2.5-32b".into(),
            params_b: 32.5,
            quant: Quantization::Q4KM,
            n_layers: 64,
            n_heads: 40,
            n_kv_heads: 8,
            embedding_dim: 5120,
            vocab_size: 152_064,
            native_context: 32_768,
            file_size_bytes: 19_800_000_000,
        };
        let a = Assessor::new(hw(8, 64));
        let e = a.evaluate_spec(&spec, &DeploymentRequest::default().with_context(4096));
        // 8GB 卡放 18.4GiB 权重 → 只能部分卸载
        assert!(matches!(
            e.assessment.placement,
            Placement::PartialOffload { .. }
        ));
        assert!(e.is_local_viable());
        assert!(!e.is_full_gpu());
    }

    #[test]
    fn small_card_insufficient_marks_not_viable() {
        let spec = ModelSpec {
            name: "huge".into(),
            params_b: 70.0,
            quant: Quantization::Q4KM,
            n_layers: 80,
            n_heads: 64,
            n_kv_heads: 8,
            embedding_dim: 8192,
            vocab_size: 152_064,
            native_context: 32_768,
            file_size_bytes: 40_000_000_000,
        };
        let a = Assessor::new(hw(4, 8));
        let mut req = DeploymentRequest::default().with_context(4096);
        req.allow_cpu_fallback = false;
        req.allow_partial_offload = false;
        let e = a.evaluate_spec(&spec, &req);
        assert!(!e.is_local_viable());
        assert_eq!(e.assessment.placement, Placement::Impossible);
    }

    #[test]
    fn request_translates_to_assess_request() {
        let r = DeploymentRequest {
            model: "m".into(),
            context_len: Some(16384),
            kv_cache: Some(KvCacheType::Q8_0),
            allow_cpu_fallback: false,
            allow_partial_offload: false,
            allow_cloud: false,
            max_price_per_hour: Some(3.0),
            batch_size: 1024,
            target: ComputeTarget::Auto,
            // 测试只验证「请求翻译到纯算法层」，不涉及远端下载信息
            model_download: None,
        };
        let ar = r.to_assess_request();
        assert_eq!(ar.context_len, Some(16384));
        assert_eq!(ar.kv_cache, Some(KvCacheType::Q8_0));
        assert!(!ar.allow_cpu_fallback);
        assert!(!ar.allow_partial_offload);
        assert_eq!(ar.batch_size, 1024);
        // allow_cloud / max_price 不属于纯算法层
        assert!(!r.allow_cloud);
    }

    #[test]
    fn local_only_builder() {
        let r = DeploymentRequest::for_model("m").local_only();
        assert!(!r.allow_cloud);
        assert_eq!(r.model, "m");
    }

    #[test]
    fn suggest_quantization_finds_workable_option() {
        let spec = ModelSpec {
            name: "qwen2.5-32b".into(),
            params_b: 32.5,
            quant: Quantization::Q4KM,
            n_layers: 64,
            n_heads: 40,
            n_kv_heads: 8,
            embedding_dim: 5120,
            vocab_size: 152_064,
            native_context: 32_768,
            file_size_bytes: 0,
        };
        let a = Assessor::new(hw(24, 64));
        let req = DeploymentRequest::default().with_context(4096);
        let (q, _) = a
            .suggest_quantization(&spec, &req, 24 << 30)
            .expect("24GB 应能跑 32B 的低比特量化");
        assert!(q.bits_per_weight() < 6.0, "picked {q}");
    }

    #[test]
    fn cloud_vram_requirement_is_monotonic_in_context() {
        let spec = ModelSpec {
            name: "m".into(),
            params_b: 7.615,
            quant: Quantization::Q4KM,
            n_layers: 28,
            n_heads: 28,
            n_kv_heads: 4,
            embedding_dim: 3584,
            vocab_size: 152_064,
            native_context: 32_768,
            file_size_bytes: 4_683_073_024,
        };
        let a = Assessor::new(hw(0, 64));
        let small = a.cloud_vram_requirement(&spec, 4096, KvCacheType::F16, 512);
        let large = a.cloud_vram_requirement(&spec, 32768, KvCacheType::F16, 512);
        assert!(large > small);
    }

    #[test]
    fn hardware_accessor_returns_snapshot() {
        let a = Assessor::new(hw(24, 64));
        assert_eq!(a.hardware().accelerators.len(), 1);
        assert!(a.hardware().has_accelerator());
    }
}
