//! 模型领域模型：GGUF 规格、量化格式、模型台账条目。
//!
//! 归属：**基础能力层 · 模型管理模块**，同时被网关层·资源评估引擎消费。
//!
//! Mangosteen 只支持 GGUF 格式（推理引擎唯一性原则），因此本模块不定义
//! safetensors / PyTorch 等格式的抽象，避免无依据扩展。

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// GGUF 量化格式。
///
/// `bits_per_weight` 是**名义**位宽（含 scale/min 等辅助张量的摊销后近似值），
/// 用于资源估算；精确计算以 GGUF 头部真实张量为准（见 [`crate::gguf`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Quantization {
    // 非量化
    F32,
    F16,
    Bf16,
    // 传统 k-quant 之前的老格式
    Q4_0,
    Q4_1,
    Q5_0,
    Q5_1,
    Q8_0,
    // K-quant
    Q2K,
    Q3KS,
    Q3KM,
    Q3KL,
    Q4KS,
    Q4KM,
    Q5KS,
    Q5KM,
    Q6K,
    // I-quant（llama.cpp 的 importance-matrix 量化）
    Iq1S,
    Iq2Xxs,
    Iq2Xs,
    Iq2S,
    Iq3Xxs,
    Iq3Xs,
    Iq3S,
    Iq4Xs,
    Iq4Nl,
}

impl Quantization {
    /// 名义每权重比特数（含辅助张量摊销近似）。
    pub const fn bits_per_weight(self) -> f32 {
        match self {
            Quantization::F32 => 32.0,
            Quantization::F16 | Quantization::Bf16 => 16.0,
            Quantization::Q8_0 => 8.5,
            Quantization::Q6K => 6.5625,
            Quantization::Q5KM => 5.6875,
            Quantization::Q5KS => 5.625,
            Quantization::Q5_1 => 5.5,
            Quantization::Q5_0 => 5.5,
            Quantization::Q4KM => 4.8375,
            Quantization::Q4KS => 4.625,
            Quantization::Q4_1 => 4.5,
            Quantization::Q4_0 => 4.5,
            Quantization::Iq4Xs => 4.25,
            Quantization::Iq4Nl => 4.25,
            Quantization::Q3KL => 3.875,
            Quantization::Q3KM => 3.75,
            Quantization::Q3KS => 3.4375,
            Quantization::Iq3S => 3.4375,
            Quantization::Iq3Xs => 3.1875,
            Quantization::Iq3Xxs => 2.9375,
            Quantization::Q2K => 2.625,
            Quantization::Iq2S => 2.5,
            Quantization::Iq2Xs => 2.3125,
            Quantization::Iq2Xxs => 2.0625,
            Quantization::Iq1S => 1.5625,
        }
    }

    /// 是否近似无损（f16/bf16/f32），用于向用户提示质量取舍。
    pub const fn is_lossless_class(self) -> bool {
        matches!(
            self,
            Quantization::F32 | Quantization::F16 | Quantization::Bf16
        )
    }

    /// 单 token 权重读取放大系数。低比特量化在 CPU/GPU 上均需要反量化，
    /// 会额外消耗算力；该系数用于算力需求估算的保守修正。
    pub const fn dequant_penalty(self) -> f32 {
        if self.is_lossless_class() {
            1.0
        } else if self.bits_per_weight() >= 6.0 {
            1.05
        } else if self.bits_per_weight() >= 4.0 {
            1.10
        } else {
            1.25
        }
    }

    /// GGUF 文件内使用的规范标识符（`general.file_type` 及文件名后缀）。
    pub const fn as_str(self) -> &'static str {
        match self {
            Quantization::F32 => "F32",
            Quantization::F16 => "F16",
            Quantization::Bf16 => "BF16",
            Quantization::Q4_0 => "Q4_0",
            Quantization::Q4_1 => "Q4_1",
            Quantization::Q5_0 => "Q5_0",
            Quantization::Q5_1 => "Q5_1",
            Quantization::Q8_0 => "Q8_0",
            Quantization::Q2K => "Q2_K",
            Quantization::Q3KS => "Q3_K_S",
            Quantization::Q3KM => "Q3_K_M",
            Quantization::Q3KL => "Q3_K_L",
            Quantization::Q4KS => "Q4_K_S",
            Quantization::Q4KM => "Q4_K_M",
            Quantization::Q5KS => "Q5_K_S",
            Quantization::Q5KM => "Q5_K_M",
            Quantization::Q6K => "Q6_K",
            Quantization::Iq1S => "IQ1_S",
            Quantization::Iq2Xxs => "IQ2_XXS",
            Quantization::Iq2Xs => "IQ2_XS",
            Quantization::Iq2S => "IQ2_S",
            Quantization::Iq3Xxs => "IQ3_XXS",
            Quantization::Iq3Xs => "IQ3_XS",
            Quantization::Iq3S => "IQ3_S",
            Quantization::Iq4Xs => "IQ4_XS",
            Quantization::Iq4Nl => "IQ4_NL",
        }
    }

    /// 从 GGUF 的 `general.file_type` 枚举值反解量化格式。
    ///
    /// 依据 llama.cpp `llama_ftype` 的公开枚举顺序。无法识别时返回 `None`，
    /// 由调用方回退为**基于文件尺寸的实测位宽**推导，避免臆测。
    pub fn from_gguf_file_type(ftype: u32) -> Option<Self> {
        // llama_ftype: 0=ALL_F32,1=MOSTLY_F16,2=MOSTLY_Q4_0,3=MOSTLY_Q4_1,
        // 7=MOSTLY_Q8_0, 8=MOSTLY_Q5_0, 9=MOSTLY_Q5_1, 10=MOSTLY_Q2_K,
        // 11=MOSTLY_Q3_K_S,12=Q3_K_M,13=Q3_K_L,14=Q4_K_S,15=Q4_K_M,
        // 16=Q5_K_S,17=Q5_K_M,18=Q6_K,19=IQ2_XXS,20=IQ2_XS,21=Q2_K_S,
        // 22=IQ3_XS,23=IQ3_XXS,24=IQ1_S,25=IQ4_NL,26=IQ3_S,27=IQ3_M,
        // 28=IQ2_S,29=IQ2_M,30=IQ4_XS,31=IQ1_M,32=BF16
        Some(match ftype {
            0 => Quantization::F32,
            1 => Quantization::F16,
            32 => Quantization::Bf16,
            2 => Quantization::Q4_0,
            3 => Quantization::Q4_1,
            7 => Quantization::Q8_0,
            8 => Quantization::Q5_0,
            9 => Quantization::Q5_1,
            10 | 21 => Quantization::Q2K,
            11 => Quantization::Q3KS,
            12 => Quantization::Q3KM,
            13 => Quantization::Q3KL,
            14 => Quantization::Q4KS,
            15 => Quantization::Q4KM,
            16 => Quantization::Q5KS,
            17 => Quantization::Q5KM,
            18 => Quantization::Q6K,
            19 => Quantization::Iq2Xxs,
            20 => Quantization::Iq2Xs,
            22 => Quantization::Iq3Xs,
            23 => Quantization::Iq3Xxs,
            24 => Quantization::Iq1S,
            25 => Quantization::Iq4Nl,
            26 => Quantization::Iq3S,
            28 => Quantization::Iq2S,
            30 => Quantization::Iq4Xs,
            _ => return None,
        })
    }

    /// 大模型权重可用的全部量化格式，按体积从大到小排列。
    /// 供资源评估引擎在「给定预算内挑最优量化」时遍历。
    pub const ALL_DESCENDING: &'static [Quantization] = &[
        Quantization::F32,
        Quantization::F16,
        Quantization::Bf16,
        Quantization::Q8_0,
        Quantization::Q6K,
        Quantization::Q5KM,
        Quantization::Q5KS,
        Quantization::Q5_1,
        Quantization::Q5_0,
        Quantization::Q4KM,
        Quantization::Q4KS,
        Quantization::Q4_1,
        Quantization::Q4_0,
        Quantization::Iq4Xs,
        Quantization::Iq4Nl,
        Quantization::Q3KL,
        Quantization::Q3KM,
        Quantization::Q3KS,
        Quantization::Iq3S,
        Quantization::Iq3Xs,
        Quantization::Iq3Xxs,
        Quantization::Q2K,
        Quantization::Iq2S,
        Quantization::Iq2Xs,
        Quantization::Iq2Xxs,
        Quantization::Iq1S,
    ];
}

impl fmt::Display for Quantization {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Quantization {
    type Err = crate::MnError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let norm = s.trim().to_ascii_uppercase().replace('-', "_");
        for q in Quantization::ALL_DESCENDING {
            if q.as_str() == norm {
                return Ok(*q);
            }
        }
        Err(crate::MnError::bad_request(format!(
            "未知量化格式 `{s}`，可选：{}",
            Quantization::ALL_DESCENDING
                .iter()
                .map(|q| q.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )))
    }
}

/// 模型架构参数。**不含**任何部署决策，只描述模型自身。
///
/// 这些字段是资源评估引擎做显存/KV Cache 估算的最小充分集。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelSpec {
    /// 模型标识（用户可见名，如 `qwen2.5-7b-instruct`）
    pub name: String,
    /// 参数量（十亿为单位，如 7.6 表示 7.6B）
    pub params_b: f64,
    /// 量化格式
    pub quant: Quantization,
    /// Transformer 层数
    pub n_layers: u32,
    /// 注意力头数
    pub n_heads: u32,
    /// KV 头数（GQA/MQA 时小于 `n_heads`；等于 `n_heads` 即标准 MHA）
    pub n_kv_heads: u32,
    /// 隐藏维度（embedding dim）
    pub embedding_dim: u32,
    /// 词表大小
    pub vocab_size: u32,
    /// 训练时的原生上下文长度（模型能力上限）
    pub native_context: u32,
    /// GGUF 文件实际字节数（用于校核量化位宽）
    pub file_size_bytes: u64,
}

impl ModelSpec {
    /// 单个 attention head 的维度。
    pub fn head_dim(&self) -> u32 {
        if self.n_heads == 0 {
            return 0;
        }
        self.embedding_dim / self.n_heads
    }

    /// 是否使用 GQA / MQA。
    pub fn is_gqa(&self) -> bool {
        self.n_kv_heads < self.n_heads
    }

    /// 权重张量的名义字节数（不含 KV Cache、不含计算缓冲）。
    pub fn weight_bytes(&self) -> u64 {
        let bits = (self.params_b * 1e9) * self.quant.bits_per_weight() as f64;
        (bits / 8.0).round() as u64
    }

    /// 基于实测文件尺寸推导的真实位宽。
    ///
    /// 资源评估时若两者偏差超过 15%，说明用户填写的量化格式或参数量有误，
    /// 评估引擎会以实测值优先，并给出提示（见 `estimate::Assessment::warnings`）。
    pub fn effective_bits_per_weight(&self) -> f32 {
        if self.params_b <= 0.0 || self.file_size_bytes == 0 {
            return self.quant.bits_per_weight();
        }
        let bytes_per_param = self.file_size_bytes as f64 / (self.params_b * 1e9);
        (bytes_per_param * 8.0) as f32
    }

    /// 粗略估算每个 token 的 KV Cache 字节数（f16 存储）。
    ///
    /// 公式：`2(K 和 V) × n_layers × n_kv_heads × head_dim × 2字节`
    pub fn kv_bytes_per_token(&self) -> u64 {
        let per_layer = 2u64
            .saturating_mul(self.n_layers as u64)
            .saturating_mul(self.n_kv_heads as u64)
            .saturating_mul(self.head_dim() as u64)
            .saturating_mul(2); // f16
        per_layer
    }

    /// 给定上下文长度的 KV Cache 总字节数。
    pub fn kv_bytes(&self, context_len: u32) -> u64 {
        self.kv_bytes_per_token().saturating_mul(context_len as u64)
    }

    /// 校验字段自洽性。返回第一条不合法原因。
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("model.name 不能为空".into());
        }
        if self.params_b <= 0.0 || !self.params_b.is_finite() {
            return Err("params_b 必须为正数".into());
        }
        if self.n_layers == 0 {
            return Err("n_layers 必须大于 0".into());
        }
        if self.n_heads == 0 || self.n_kv_heads == 0 {
            return Err("n_heads / n_kv_heads 必须大于 0".into());
        }
        if self.n_kv_heads > self.n_heads {
            return Err(format!(
                "n_kv_heads({}) 不得大于 n_heads({})",
                self.n_kv_heads, self.n_heads
            ));
        }
        if self.embedding_dim == 0 || self.embedding_dim % self.n_heads != 0 {
            return Err(format!(
                "embedding_dim({}) 必须能被 n_heads({}) 整除",
                self.embedding_dim, self.n_heads
            ));
        }
        if self.native_context == 0 {
            return Err("native_context 必须大于 0".into());
        }
        Ok(())
    }
}

/// 模型所在地（本地路径 / 云端实例）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ModelLocation {
    /// 本机文件系统路径
    Local { path: PathBuf },
    /// 云端实例内路径，附实例 ID
    Cloud { instance_id: String, path: String },
}

/// 模型台账条目。基础能力层·模型管理模块的持久化实体。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelEntry {
    /// 稳定 ID（由 `name + quant` 派生，便于幂等注册）
    pub id: String,
    /// 模型规格
    pub spec: ModelSpec,
    /// 物理位置
    pub location: ModelLocation,
    /// SHA-256 摘要（十六进制小写）。`None` 表示尚未计算。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// 语义化版本标签（同家族不同迭代，如 `2024-11`）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// 最近一次校验通过的时间（Unix 秒）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified_at: Option<i64>,
}

impl ModelEntry {
    /// 由规格推导稳定 ID。
    pub fn derive_id(name: &str, quant: Quantization) -> String {
        let slug: String = name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                    c.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect();
        let slug = slug.trim_matches('-').to_string();
        format!("{slug}:{}", quant.as_str().to_ascii_lowercase())
    }

    /// 该条目是否部署在本地。
    pub fn is_local(&self) -> bool {
        matches!(self.location, ModelLocation::Local { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn qwen_7b() -> ModelSpec {
        // Qwen2.5-7B-Instruct 公开架构参数
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

    #[test]
    fn head_dim_and_gqa() {
        let s = qwen_7b();
        assert_eq!(s.head_dim(), 128);
        assert!(s.is_gqa());
    }

    #[test]
    fn kv_cache_scales_linearly_with_context() {
        let s = qwen_7b();
        let a = s.kv_bytes(4_096);
        let b = s.kv_bytes(8_192);
        assert_eq!(b, a * 2);
        // 28 层 × 4 KV头 × 128 dim × 2(KV) × 2B = 57344 B/token
        assert_eq!(s.kv_bytes_per_token(), 57_344);
    }

    #[test]
    fn weight_bytes_tracks_quantization() {
        let mut s = qwen_7b();
        let q4 = s.weight_bytes();
        s.quant = Quantization::F16;
        let f16 = s.weight_bytes();
        assert!(f16 > q4);
        // Q4_K_M 名义 4.8375 bpw → 约 4.6 GB
        assert!((4.0e9..5.2e9).contains(&(q4 as f64)));
    }

    #[test]
    fn effective_bits_detects_mismatch() {
        let mut s = qwen_7b();
        // 实际文件 4.68GB / 7.615B params ≈ 4.92 bpw
        let eff = s.effective_bits_per_weight();
        assert!((4.5..5.3).contains(&eff));
        // 故意声明为 F16 但文件仍是 4.6GB → 实测位宽暴露矛盾
        s.quant = Quantization::F16;
        assert!(s.effective_bits_per_weight() < 8.0);
    }

    #[test]
    fn validation_rejects_bad_geometry() {
        let mut s = qwen_7b();
        s.n_kv_heads = 64;
        assert!(s.validate().is_err());

        let mut s = qwen_7b();
        s.embedding_dim = 3585;
        assert!(s.validate().is_err());

        assert!(qwen_7b().validate().is_ok());
    }

    #[test]
    fn quant_roundtrip_str() {
        for q in Quantization::ALL_DESCENDING {
            let s = q.as_str();
            let parsed: Quantization = s.parse().unwrap();
            assert_eq!(parsed, *q, "roundtrip failed for {s}");
        }
        assert!("q4_k_m".parse::<Quantization>().is_ok());
        assert!("bogus".parse::<Quantization>().is_err());
    }

    #[test]
    fn quant_ordering_is_monotonic_by_size() {
        let bits: Vec<f32> = Quantization::ALL_DESCENDING
            .iter()
            .map(|q| q.bits_per_weight())
            .collect();
        for w in bits.windows(2) {
            assert!(w[0] >= w[1], "ALL_DESCENDING must be non-increasing: {w:?}");
        }
    }

    #[test]
    fn gguf_file_type_mapping() {
        assert_eq!(
            Quantization::from_gguf_file_type(15),
            Some(Quantization::Q4KM)
        );
        assert_eq!(
            Quantization::from_gguf_file_type(1),
            Some(Quantization::F16)
        );
        assert_eq!(Quantization::from_gguf_file_type(999), None);
    }

    #[test]
    fn stable_model_id() {
        assert_eq!(
            ModelEntry::derive_id("Qwen2.5 7B Instruct", Quantization::Q4KM),
            "qwen2.5-7b-instruct:q4_k_m"
        );
    }
}
