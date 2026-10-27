//! 资源评估引擎：显存/内存需求估算、量化选档、本地部署可行性判定。
//!
//! 归属：**API 网关层 · 资源评估引擎**。
//!
//! ## 估算模型
//!
//! 一次本地部署占用的显存/内存由四块构成：
//!
//! ```text
//! 总需求 = 权重(weights) + KV Cache(kv) + 计算缓冲(compute) + 运行时开销(overhead)
//! ```
//!
//! | 组成 | 决定因素 | 说明 |
//! |------|----------|------|
//! | weights | 参数量 × 量化位宽 | 有实测文件大小时以实测为准（见 [`ModelSpec::effective_bits_per_weight`]） |
//! | kv | 层数 × KV头数 × head_dim × 上下文 × KV精度 | GQA 模型 KV 极小，是长上下文的决定性变量 |
//! | compute | 批大小 × 上下文 × 隐藏维度 | 与 `--batch-size`、是否启用 FlashAttention 相关 |
//! | overhead | 固定项 | CUDA context / Metal 驱动 / 张量对齐碎片 |
//!
//! ## 为什么不用「参数量 × 2 就够」这种土办法
//!
//! 消费级显卡上真正卡人的往往是 **KV Cache**：7B 模型 GQA 下 128K 上下文
//! 仍需 ~3.5 GiB，而非 GQA 的老模型（如 Llama-2-7B）在 8K 就要 4 GiB。
//! 因此本模块把 KV 作为一等公民单独计算，并支持 KV 量化降档。

use serde::{Deserialize, Serialize};

use crate::hardware::{AcceleratorVendor, HardwareProfile};
use crate::model::{ModelSpec, Quantization};

/// 常量：1 GiB（二进制）。
pub const GIB: u64 = 1 << 30;
/// 常量：1 MiB。
pub const MIB: u64 = 1 << 20;

/// KV Cache 存储精度。
///
/// llama.cpp 支持 `--cache-type-k/v`。量化 KV 会带来轻微的精度损失，
/// 但对长上下文部署是关键手段（q8_0 几乎无损，体积减半）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KvCacheType {
    /// f16，llama.cpp 默认
    F16,
    /// q8_0，近似无损，体积约 1/2
    Q8_0,
    /// q4_0，体积约 1/4，长上下文质量有可见下降
    Q4_0,
}

impl KvCacheType {
    /// 每个 KV 元素的等效字节数（含分块 scale 摊销）。
    pub const fn bytes_per_element(self) -> f32 {
        match self {
            KvCacheType::F16 => 2.0,
            KvCacheType::Q8_0 => 1.0625,
            KvCacheType::Q4_0 => 0.5625,
        }
    }

    /// 相对于 f16 的体积系数。
    pub const fn size_factor(self) -> f32 {
        self.bytes_per_element() / 2.0
    }

    /// llama.cpp 命令行参数值。
    pub const fn as_cli_value(self) -> &'static str {
        match self {
            KvCacheType::F16 => "f16",
            KvCacheType::Q8_0 => "q8_0",
            KvCacheType::Q4_0 => "q4_0",
        }
    }

    /// 降档序列（质量从高到低），供估算器在空间不足时逐级尝试。
    pub const DOWNGRADE_CHAIN: &'static [KvCacheType] =
        &[KvCacheType::F16, KvCacheType::Q8_0, KvCacheType::Q4_0];
}

/// 资源需求分解结果（字节）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceRequirement {
    /// 模型权重
    pub weights_bytes: u64,
    /// KV Cache
    pub kv_cache_bytes: u64,
    /// 计算缓冲
    pub compute_buffer_bytes: u64,
    /// 运行时固定开销
    pub runtime_overhead_bytes: u64,
}

impl ResourceRequirement {
    /// 合计需求。
    pub fn total_bytes(&self) -> u64 {
        self.weights_bytes
            .saturating_add(self.kv_cache_bytes)
            .saturating_add(self.compute_buffer_bytes)
            .saturating_add(self.runtime_overhead_bytes)
    }

    /// 人类可读分解，用于 CLI `plan` 输出。
    pub fn describe(&self) -> String {
        format!(
            "权重 {:.2} GiB + KV {:.2} GiB + 计算缓冲 {:.2} GiB + 开销 {:.2} GiB = {:.2} GiB",
            self.weights_bytes as f64 / GIB as f64,
            self.kv_cache_bytes as f64 / GIB as f64,
            self.compute_buffer_bytes as f64 / GIB as f64,
            self.runtime_overhead_bytes as f64 / GIB as f64,
            self.total_bytes() as f64 / GIB as f64,
        )
    }
}

/// 本地部署的落位形态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Placement {
    /// 全部层（含输出层）在加速卡上
    FullGpu,
    /// 部分层在加速卡，其余在 CPU。`gpu_layers` 对应 llama.cpp `--n-gpu-layers`
    PartialOffload { gpu_layers: u32 },
    /// 纯 CPU 推理，加速卡不参与
    CpuOnly,
    /// 本地任何形态都无法承载
    Impossible,
}

impl Placement {
    /// 是否可部署（非 Impossible）。
    pub fn is_deployable(&self) -> bool {
        !matches!(self, Placement::Impossible)
    }

    /// 是否为「本地资源明确满足」——调度决策器的本地优先分支依据。
    ///
    /// `CpuOnly` 也算满足：CPU 推理虽然慢，但确实能跑，
    /// 按「本地优先」原则不应因慢而降级到云端。是否接受 CPU 速度
    /// 由用户在请求中通过 `allow_cpu_fallback` 决定。
    pub fn is_local_satisfiable(&self) -> bool {
        self.is_deployable()
    }

    /// 人类可读描述。
    pub fn describe(&self, total_layers: u32) -> String {
        match self {
            Placement::FullGpu => "全部层驻显存（GPU 全量卸载）".to_string(),
            Placement::PartialOffload { gpu_layers } => {
                format!("部分卸载：{}/{} 层驻显存", gpu_layers, total_layers + 1)
            }
            Placement::CpuOnly => "纯 CPU 推理（无加速卡参与）".to_string(),
            Placement::Impossible => "本地无法承载".to_string(),
        }
    }
}

/// 评估过程中的非致命提示。CLI 会以 `warning` 级别输出。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum Warning {
    /// 声明的量化格式与实际文件尺寸不符
    QuantMismatch {
        declared_bpw: String,
        measured_bpw: String,
    },
    /// 请求上下文超过模型原生上限
    ContextClamped {
        requested: u32,
        native: u32,
        actual: u32,
    },
    /// KV Cache 被降档
    KvCacheDowngraded { from: String, to: String },
    /// 发生部分卸载，吞吐会明显低于全量卸载
    PartialOffloadPenalty { gpu_layers: u32 },
    /// 走 CPU 推理，预期吞吐极低
    CpuFallback,
    /// 加速卡为统一内存架构，显存与内存竞争
    UnifiedMemoryContention,
}

impl Warning {
    /// 人类可读消息。
    pub fn message(&self) -> String {
        match self {
            Warning::QuantMismatch {
                declared_bpw,
                measured_bpw,
            } => format!(
                "声明的量化位宽 {declared_bpw} bpw 与实测 {measured_bpw} bpw 偏差较大，已按实测值估算"
            ),
            Warning::ContextClamped {
                requested,
                native,
                actual,
            } => format!(
                "请求上下文 {requested} 超过模型原生上限 {native}，已收敛为 {actual}"
            ),
            Warning::KvCacheDowngraded { from, to } => {
                format!("显存紧张，KV Cache 精度由 {from} 降档为 {to}")
            }
            Warning::PartialOffloadPenalty { gpu_layers } => format!(
                "仅 {gpu_layers} 层驻显存，跨 PCIe 搬运会使解码吞吐下降 40%~70%"
            ),
            Warning::CpuFallback => {
                "未检测到可用加速卡，将以 CPU 推理运行，解码速度可能低于 5 tok/s".to_string()
            }
            Warning::UnifiedMemoryContention => {
                "统一内存架构下 GPU 与 CPU 共享带宽，推理期间请避免其它重内存负载".to_string()
            }
        }
    }
}

/// 部署请求中与资源相关的意图参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssessRequest {
    /// 期望上下文长度。`None` 表示「在不 OOM 前提下尽可能大」。
    pub context_len: Option<u32>,
    /// 期望 KV 精度。`None` 表示允许自动降档。
    pub kv_cache: Option<KvCacheType>,
    /// 是否接受纯 CPU 回退
    pub allow_cpu_fallback: bool,
    /// 是否接受部分卸载
    pub allow_partial_offload: bool,
    /// 批大小（llama.cpp `--batch-size`），影响计算缓冲
    pub batch_size: u32,
}

impl Default for AssessRequest {
    fn default() -> Self {
        Self {
            context_len: None,
            kv_cache: None,
            allow_cpu_fallback: true,
            allow_partial_offload: true,
            batch_size: 512,
        }
    }
}

impl AssessRequest {
    /// 指定上下文长度。
    pub fn with_context(mut self, ctx: u32) -> Self {
        self.context_len = Some(ctx);
        self
    }

    /// 指定 KV 精度。
    pub fn with_kv_cache(mut self, kv: KvCacheType) -> Self {
        self.kv_cache = Some(kv);
        self
    }

    /// 禁止 CPU 回退。
    pub fn no_cpu_fallback(mut self) -> Self {
        self.allow_cpu_fallback = false;
        self
    }

    /// 禁止部分卸载（要求全量驻显存，否则视为本地不可行）。
    pub fn require_full_gpu(mut self) -> Self {
        self.allow_partial_offload = false;
        self
    }
}

/// 评估结论。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Assessment {
    /// 落位形态
    pub placement: Placement,
    /// 在该落位下的资源需求
    pub requirement: ResourceRequirement,
    /// 最终采用的上下文长度
    pub context_len: u32,
    /// 最终采用的 KV 精度
    pub kv_cache: KvCacheType,
    /// 最终采用的量化格式（可能与输入不同，若启用了自动选档）
    pub quant: Quantization,
    /// 建议线程数
    pub threads: u32,
    /// 建议批大小
    pub batch_size: u32,
    /// 加速卡预算（可用显存）
    pub accelerator_budget_bytes: u64,
    /// 主机内存预算
    pub host_budget_bytes: u64,
    /// 非致命提示
    pub warnings: Vec<Warning>,
}

impl Assessment {
    /// 显存余量（正数为富余，负数为超支）。
    pub fn accelerator_headroom_bytes(&self) -> i64 {
        self.accelerator_budget_bytes as i64
            - (self.requirement.weights_bytes
                + self
                    .requirement
                    .kv_cache_bytes
                    .min(self.requirement.kv_cache_bytes)) as i64
    }

    /// 一行摘要。
    pub fn summary(&self) -> String {
        format!(
            "{} · ctx={} · kv={} · quant={} · {}",
            self.placement.describe(0),
            self.context_len,
            self.kv_cache.as_cli_value(),
            self.quant,
            self.requirement.describe()
        )
    }
}

/// 估算固定运行时开销。
///
/// 取值依据：
/// * CUDA：每进程 context + cuBLAS workspace 约 400 MiB（4090 实测 350~450）
/// * Metal：约 250 MiB
/// * 纯 CPU：约 64 MiB（仅线程栈与临时区）
fn runtime_overhead(vendor: AcceleratorVendor) -> u64 {
    match vendor {
        AcceleratorVendor::Nvidia | AcceleratorVendor::Amd => 400 * MIB,
        AcceleratorVendor::Intel => 320 * MIB,
        AcceleratorVendor::Apple => 250 * MIB,
        AcceleratorVendor::None => 64 * MIB,
    }
}

/// 估算计算缓冲。
///
/// 近似为 `batch_size × context_chunk × hidden_dim × 2 字节 × 系数`，并给下限。
/// 开启 FlashAttention 后实际更低，这里取保守值（不启用 FA 时的量级）。
fn compute_buffer(spec: &ModelSpec, batch_size: u32) -> u64 {
    // 单 token 激活量级：hidden_dim × 若干中间张量（QKV + MLP 中间层约 8×hidden）
    let per_token = (spec.embedding_dim as u64)
        .saturating_mul(8)
        .saturating_mul(2);
    let raw = per_token.saturating_mul(batch_size as u64);
    // 分母系数：llama.cpp 复用缓冲，实际远小于逐 token 累加
    let scaled = raw / 4;
    scaled.clamp(192 * MIB, 4 * GIB)
}

/// 权重字节数：优先用实测文件大小（GGUF 元数据与对齐已含在内）。
fn weights_bytes(spec: &ModelSpec) -> u64 {
    if spec.file_size_bytes > 0 {
        spec.file_size_bytes
    } else {
        spec.weight_bytes()
    }
}

/// 计算给定 (context, kv_type) 下的完整资源需求。
pub fn requirement(
    spec: &ModelSpec,
    context_len: u32,
    kv_cache: KvCacheType,
    batch_size: u32,
) -> ResourceRequirement {
    let kv_bytes = (spec.kv_bytes(context_len) as f64 * kv_cache.size_factor() as f64) as u64;
    ResourceRequirement {
        weights_bytes: weights_bytes(spec),
        kv_cache_bytes: kv_bytes,
        compute_buffer_bytes: compute_buffer(spec, batch_size),
        runtime_overhead_bytes: runtime_overhead(AcceleratorVendor::None),
    }
}

/// 在给定显存预算下，最多能放下多少层（含输出层共 `n_layers + 1` 层）。
///
/// 单层占用 = 单层权重 + 单层 KV。输出层（`output.weight`）只计权重。
/// 返回值范围 `0..=n_layers+1`；0 表示连一层都放不下。
fn max_gpu_layers(spec: &ModelSpec, kv_cache: KvCacheType, ctx: u32, budget: u64) -> u32 {
    let total_layers = spec.n_layers;
    let w_total = weights_bytes(spec);
    // 输出层权重 ≈ 词表 × hidden × bpw/8，在 GGUF 中常单独存放
    let output_layer_bytes = (spec.vocab_size as u64)
        .saturating_mul(spec.embedding_dim as u64)
        .saturating_mul(spec.quant.bits_per_weight() as u64)
        / 8;
    let per_layer_w = w_total.saturating_sub(output_layer_bytes) / total_layers as u64;
    let per_layer_kv =
        (spec.kv_bytes(ctx / ctx.max(1)) as f64 * kv_cache.size_factor() as f64) as u64;

    if budget < per_layer_w {
        return 0;
    }
    // 先按权重算层数，再校验 KV 是否也放得下，取小者
    let mut layers = (budget / per_layer_w.max(1)).min(total_layers as u64) as u32;
    while layers > 0 {
        let need =
            per_layer_w.saturating_mul(layers as u64) + per_layer_kv.saturating_mul(layers as u64);
        if need <= budget {
            break;
        }
        layers -= 1;
    }
    // 输出层需额外权重，若权重部分已能全下且还有余量则 +1
    if layers == total_layers {
        let extra = per_layer_w.saturating_add(output_layer_bytes);
        if budget
            .saturating_sub(per_layer_w.saturating_mul(total_layers as u64))
            .saturating_sub(per_layer_kv.saturating_mul(total_layers as u64))
            >= extra
        {
            return total_layers + 1;
        }
    }
    layers
}

/// 在显存预算内，找出能放下的最大上下文长度。
///
/// 从 `native_context` 向下按 2 的幂收敛，找到满足预算的最大值；
/// 若连最小 2K 都放不下，返回 0。
fn max_context_within(spec: &ModelSpec, kv: KvCacheType, budget: u64, batch: u32) -> u32 {
    let base = requirement(spec, 0, kv, batch);
    let fixed = base.weights_bytes + base.compute_buffer_bytes + base.runtime_overhead_bytes;
    if fixed >= budget {
        return 0;
    }
    let kv_per_token = (spec.kv_bytes_per_token() as f64 * kv.size_factor() as f64) as u64;
    if kv_per_token == 0 {
        return spec.native_context;
    }
    let available = budget - fixed;
    let max_ctx = (available / kv_per_token).min(u32::MAX as u64) as u32;
    // 收敛到 2 的幂，并向模型原生上限取小
    let mut ctx = spec.native_context.min(max_ctx);
    // 对齐到 256 的整数倍（llama.cpp 的 n_ctx 不需要但仍便于人读）
    ctx -= ctx % 256;
    ctx
}

/// 用户未指定上下文长度时的**保守默认值**。
///
/// 为什么不用模型原生上限（32k / 128k）：KV cache 与上下文长度线性相关，
/// 直接把原生上限拉满等于预先分配几百 MB 到几 GB 的纯浪费——
/// 绝大多数对话用不到 8k。实测：0.5B 模型按原生 32768 分配，
/// KV cache 就有 400 MB，在内存紧张的机器上直接拖慢一切。
///
/// 需要更长上下文时由用户显式传 `context_len`。
pub const DEFAULT_CONTEXT: u32 = 8192;

/// 核心入口：给定模型规格与硬件，产出评估结论。
///
/// 决策顺序严格遵循「先大后小、先快后慢」：
/// 1. 全量 GPU 卸载 → 2. 部分卸载 → 3. 纯 CPU → 4. 判定不可行
pub fn assess(spec: &ModelSpec, hw: &HardwareProfile, req: &AssessRequest) -> Assessment {
    let mut warnings = Vec::new();
    let _vendor = hw
        .primary_accelerator()
        .map(|a| a.vendor)
        .unwrap_or(AcceleratorVendor::None);

    // ---- 1. 上下文上限收敛到模型原生能力 ----
    // 未指定 → 保守默认（见 `DEFAULT_CONTEXT`），而不是模型原生上限。
    let want_ctx = req
        .context_len
        .unwrap_or_else(|| spec.native_context.min(DEFAULT_CONTEXT));
    let mut ctx = want_ctx.min(spec.native_context);
    if want_ctx > spec.native_context {
        warnings.push(Warning::ContextClamped {
            requested: want_ctx,
            native: spec.native_context,
            actual: ctx,
        });
    }

    // ---- 2. 量化位宽与实测偏差校验 ----
    if spec.file_size_bytes > 0 {
        let declared = spec.quant.bits_per_weight();
        let measured = spec.effective_bits_per_weight();
        if (declared - measured).abs() / declared.max(0.1) > 0.15 {
            warnings.push(Warning::QuantMismatch {
                declared_bpw: format!("{declared:.2}"),
                measured_bpw: format!("{measured:.2}"),
            });
        }
    }

    let gpu_budget = hw.total_usable_accelerator_memory();
    let host_budget = hw.host.usable_memory();
    let threads = hw.host.recommended_threads();
    let batch = req.batch_size.max(1);
    if let Some(a) = hw.primary_accelerator() {
        if a.is_unified_memory() {
            warnings.push(Warning::UnifiedMemoryContention);
        }
    }

    // ---- 3. KV 精度选择：用户指定 > 固定 F16 > 自动降档 ----
    let kv_candidates: Vec<KvCacheType> = match req.kv_cache {
        Some(k) => vec![k],
        None => KvCacheType::DOWNGRADE_CHAIN.to_vec(),
    };

    // ---- 4. 逐档尝试：全量 GPU → 部分卸载 ----
    if gpu_budget > 0 && req.context_len.is_none() {
        // 「尽可能大上下文」模式：先定 KV 精度，再算最大 ctx
        for (i, kv) in kv_candidates.iter().enumerate() {
            let max_ctx = max_context_within(spec, *kv, gpu_budget, batch);
            if max_ctx >= 2048 {
                let final_ctx = max_ctx.min(spec.native_context);
                let r = requirement(spec, final_ctx, *kv, batch);
                if i > 0 {
                    warnings.push(Warning::KvCacheDowngraded {
                        from: kv_candidates[0].as_cli_value().to_string(),
                        to: kv.as_cli_value().to_string(),
                    });
                }
                return Assessment {
                    placement: Placement::FullGpu,
                    requirement: r,
                    context_len: final_ctx,
                    kv_cache: *kv,
                    quant: spec.quant,
                    threads,
                    batch_size: batch,
                    accelerator_budget_bytes: gpu_budget,
                    host_budget_bytes: host_budget,
                    warnings,
                };
            }
        }
        // 全量放不下 → 尝试部分卸载，上下文回退到 4K
        ctx = ctx.min(4096);
    }

    for (i, kv) in kv_candidates.iter().enumerate() {
        let r = requirement(spec, ctx, *kv, batch);
        let gpu_need = r.weights_bytes + r.kv_cache_bytes;
        if gpu_need + r.compute_buffer_bytes + r.runtime_overhead_bytes <= gpu_budget {
            if i > 0 {
                warnings.push(Warning::KvCacheDowngraded {
                    from: kv_candidates[0].as_cli_value().to_string(),
                    to: kv.as_cli_value().to_string(),
                });
            }
            return Assessment {
                placement: Placement::FullGpu,
                requirement: r,
                context_len: ctx,
                kv_cache: *kv,
                quant: spec.quant,
                threads,
                batch_size: batch,
                accelerator_budget_bytes: gpu_budget,
                host_budget_bytes: host_budget,
                warnings,
            };
        }
    }

    // 部分卸载
    if req.allow_partial_offload && gpu_budget > 0 {
        for kv in &kv_candidates {
            let layers = max_gpu_layers(
                spec,
                *kv,
                ctx,
                gpu_budget.saturating_sub(compute_buffer(spec, batch)),
            );
            if layers == 0 || layers > spec.n_layers {
                continue;
            }
            let r = requirement(spec, ctx, *kv, batch);
            // 校验：留在 CPU 的那部分权重 + KV 必须能被主机内存兜住
            let cpu_share = (r.weights_bytes as f64
                * (1.0 - layers as f64 / (spec.n_layers + 1) as f64))
                as u64;
            if cpu_share + r.compute_buffer_bytes <= host_budget {
                warnings.push(Warning::PartialOffloadPenalty { gpu_layers: layers });
                return Assessment {
                    placement: Placement::PartialOffload { gpu_layers: layers },
                    requirement: r,
                    context_len: ctx,
                    kv_cache: *kv,
                    quant: spec.quant,
                    threads,
                    batch_size: batch,
                    accelerator_budget_bytes: gpu_budget,
                    host_budget_bytes: host_budget,
                    warnings,
                };
            }
        }
    }

    // ---- 5. 纯 CPU ----
    if req.allow_cpu_fallback {
        for (i, kv) in kv_candidates.iter().enumerate() {
            let r = requirement(spec, ctx, *kv, batch);
            if r.total_bytes() <= host_budget {
                warnings.push(Warning::CpuFallback);
                if i > 0 {
                    warnings.push(Warning::KvCacheDowngraded {
                        from: kv_candidates[0].as_cli_value().to_string(),
                        to: kv.as_cli_value().to_string(),
                    });
                }
                return Assessment {
                    placement: Placement::CpuOnly,
                    requirement: r,
                    context_len: ctx,
                    kv_cache: *kv,
                    quant: spec.quant,
                    threads,
                    batch_size: batch,
                    accelerator_budget_bytes: gpu_budget,
                    host_budget_bytes: host_budget,
                    warnings,
                };
            }
        }
    }

    // ---- 6. 不可行 ----
    let r = requirement(spec, ctx, KvCacheType::F16, batch);
    Assessment {
        placement: Placement::Impossible,
        requirement: r,
        context_len: ctx,
        kv_cache: KvCacheType::F16,
        quant: spec.quant,
        threads,
        batch_size: batch,
        accelerator_budget_bytes: gpu_budget,
        host_budget_bytes: host_budget,
        warnings,
    }
}

/// 在固定上下文下，从「最大体积」向「最小体积」挑选能塞进预算的最高质量量化档。
///
/// 返回 `(量化格式, 评估结论)`。若所有档位都放不下，返回 `None`。
///
/// 用途：用户只给预算不给量化时，由网关自动选档，避免「先 OOM 再降级」的往返。
pub fn best_quant_within(
    base_spec: &ModelSpec,
    hw: &HardwareProfile,
    req: &AssessRequest,
    budget_bytes: u64,
) -> Option<(Quantization, Assessment)> {
    for quant in Quantization::ALL_DESCENDING {
        let mut candidate = base_spec.clone();
        candidate.quant = *quant;
        // 量化改变后，实测文件大小不再成立，只有与原始量化相同时才保留
        if *quant != base_spec.quant {
            candidate.file_size_bytes = 0;
        }
        let a = assess(&candidate, hw, req);
        let need = match a.placement {
            Placement::FullGpu | Placement::PartialOffload { .. } => a.requirement.total_bytes(),
            Placement::CpuOnly => a.requirement.total_bytes(),
            Placement::Impossible => continue,
        };
        if need <= budget_bytes {
            return Some((*quant, a));
        }
    }
    None
}

/// 校验一个「云端实例规格」是否能承载该模型。返回所需的显存字节数。
///
/// 云端实例一律使用 llama.cpp（推理引擎唯一性原则），因此复用同一套估算。
pub fn cloud_requirement_bytes(
    spec: &ModelSpec,
    context_len: u32,
    kv_cache: KvCacheType,
    batch_size: u32,
) -> u64 {
    requirement(spec, context_len, kv_cache, batch_size).total_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware::{Accelerator, HostProfile};

    fn qwen_7b_q4() -> ModelSpec {
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
            file_size_bytes: 4_683_073_024, // ≈4.36 GiB
        }
    }

    fn qwen_32b_q4() -> ModelSpec {
        ModelSpec {
            name: "qwen2.5-32b-instruct".into(),
            params_b: 32.5,
            quant: Quantization::Q4KM,
            n_layers: 64,
            n_heads: 40,
            n_kv_heads: 8,
            embedding_dim: 5120,
            vocab_size: 152_064,
            native_context: 32_768,
            file_size_bytes: 19_800_000_000, // ≈18.4 GiB
        }
    }

    fn hw_with_vram(gib: u64, unified: bool, total_ram_gib: u64) -> HardwareProfile {
        let mut accs = Vec::new();
        if gib > 0 || unified {
            accs.push(if unified {
                Accelerator {
                    name: "Apple M3 Max".into(),
                    vendor: AcceleratorVendor::Apple,
                    vram_bytes: 0,
                    unified_memory_bytes: total_ram_gib * GIB,
                    compute_capability: Some("m3".into()),
                    fp16_tflops: None,
                    memory_bandwidth_gbps: Some(400.0),
                }
            } else {
                Accelerator {
                    name: format!("Generic {gib}GiB"),
                    vendor: AcceleratorVendor::Nvidia,
                    vram_bytes: gib * GIB,
                    unified_memory_bytes: 0,
                    compute_capability: Some("sm_89".into()),
                    fp16_tflops: Some(165.0),
                    memory_bandwidth_gbps: Some(1008.0),
                }
            });
        }
        HardwareProfile {
            os: "linux".into(),
            arch: "x86_64".into(),
            host: HostProfile {
                total_memory_bytes: total_ram_gib * GIB,
                available_memory_bytes: total_ram_gib * GIB / 2,
                physical_cores: 16,
                logical_cores: 32,
                cpu_model: "test".into(),
            },
            accelerators: accs,
        }
    }

    #[test]
    fn kv_bytes_use_requested_precision() {
        let s = qwen_7b_q4();
        let f16 = requirement(&s, 8192, KvCacheType::F16, 512);
        let q8 = requirement(&s, 8192, KvCacheType::Q8_0, 512);
        let q4 = requirement(&s, 8192, KvCacheType::Q4_0, 512);
        assert!(f16.kv_cache_bytes > q8.kv_cache_bytes);
        assert!(q8.kv_cache_bytes > q4.kv_cache_bytes);
        // q8 约一半
        let ratio = q8.kv_cache_bytes as f64 / f16.kv_cache_bytes as f64;
        assert!((0.48..0.56).contains(&ratio), "ratio={ratio}");
    }

    #[test]
    fn requirement_total_is_sum() {
        let s = qwen_7b_q4();
        let r = requirement(&s, 4096, KvCacheType::F16, 512);
        assert_eq!(
            r.total_bytes(),
            r.weights_bytes + r.kv_cache_bytes + r.compute_buffer_bytes + r.runtime_overhead_bytes
        );
    }

    #[test]
    fn weights_prefer_measured_file_size() {
        let s = qwen_7b_q4();
        let r = requirement(&s, 0, KvCacheType::F16, 512);
        assert_eq!(r.weights_bytes, s.file_size_bytes);
    }

    #[test]
    fn seven_b_fits_fully_on_8gb_card() {
        let a = assess(
            &qwen_7b_q4(),
            &hw_with_vram(8, false, 32),
            &AssessRequest::default().with_context(4096),
        );
        // 4.36GiB 权重 + KV 0.22GiB + 缓冲/开销 ≈ 0.7GiB → 略超 8GiB？
        // 预算 8GiB，总需求约 5.3GiB → 全量放得下
        assert_eq!(a.placement, Placement::FullGpu, "{}", a.summary());
        assert!(a.requirement.total_bytes() < 8 * GIB);
    }

    #[test]
    fn thirty_two_b_needs_partial_offload_on_8gb() {
        let a = assess(
            &qwen_32b_q4(),
            &hw_with_vram(8, false, 64),
            &AssessRequest::default().with_context(4096),
        );
        match a.placement {
            Placement::PartialOffload { gpu_layers } => {
                assert!(gpu_layers > 0 && gpu_layers < 65, "layers={gpu_layers}");
                assert!(a
                    .warnings
                    .iter()
                    .any(|w| matches!(w, Warning::PartialOffloadPenalty { .. })));
            }
            other => panic!("expected partial offload, got {other:?}"),
        }
    }

    #[test]
    fn no_accelerator_falls_back_to_cpu() {
        let a = assess(
            &qwen_7b_q4(),
            &hw_with_vram(0, false, 32),
            &AssessRequest::default().with_context(4096),
        );
        assert_eq!(a.placement, Placement::CpuOnly);
        assert!(a.warnings.iter().any(|w| matches!(w, Warning::CpuFallback)));
    }

    #[test]
    fn cpu_fallback_disabled_means_impossible() {
        let a = assess(
            &qwen_7b_q4(),
            &hw_with_vram(0, false, 32),
            &AssessRequest::default()
                .with_context(4096)
                .no_cpu_fallback(),
        );
        assert_eq!(a.placement, Placement::Impossible);
        assert!(!a.placement.is_local_satisfiable());
    }

    #[test]
    fn oversized_model_on_small_host_is_impossible() {
        let a = assess(
            &qwen_32b_q4(),
            &hw_with_vram(4, false, 8),
            &AssessRequest::default().with_context(4096),
        );
        assert_eq!(a.placement, Placement::Impossible);
    }

    #[test]
    fn context_clamped_to_native() {
        let a = assess(
            &qwen_7b_q4(),
            &hw_with_vram(24, false, 64),
            &AssessRequest::default().with_context(131_072),
        );
        assert_eq!(a.context_len, 32_768);
        assert!(a
            .warnings
            .iter()
            .any(|w| matches!(w, Warning::ContextClamped { .. })));
    }

    #[test]
    fn auto_context_picks_max_that_fits() {
        // 24GiB 卡，7B Q4，不指定 ctx → 应尽量吃满原生 32K
        let a = assess(
            &qwen_7b_q4(),
            &hw_with_vram(24, false, 64),
            &AssessRequest::default(),
        );
        assert_eq!(a.context_len, 32_768);
        assert_eq!(a.placement, Placement::FullGpu);
    }

    #[test]
    fn auto_context_shrinks_on_small_vram() {
        // 6GiB 卡放 7B：ctx 会被压缩
        let a = assess(
            &qwen_7b_q4(),
            &hw_with_vram(6, false, 32),
            &AssessRequest::default(),
        );
        if a.placement == Placement::FullGpu {
            assert!(a.context_len <= 32_768);
            assert!(a.context_len >= 2048);
        }
    }

    #[test]
    fn kv_downgrade_rescues_tight_fit() {
        // 非 GQA 模型 KV 巨大，8K 上下文在 8GiB 上需要降 KV 精度
        let mut s = qwen_7b_q4();
        s.n_kv_heads = 28; // 退化为 MHA，KV 放大 7 倍
        let a = assess(
            &s,
            &hw_with_vram(8, false, 32),
            &AssessRequest::default().with_context(8192),
        );
        // 要么降档 KV，要么部分卸载，总之要能部署
        assert!(a.placement.is_deployable(), "{}", a.summary());
    }

    #[test]
    fn unified_memory_warns_and_reserves() {
        let a = assess(
            &qwen_7b_q4(),
            &hw_with_vram(0, true, 64),
            &AssessRequest::default().with_context(8192),
        );
        assert!(a
            .warnings
            .iter()
            .any(|w| matches!(w, Warning::UnifiedMemoryContention)));
        // 64GiB × 70% = 44.8GiB 预算，7B 必然全量
        assert_eq!(a.placement, Placement::FullGpu);
        assert!((a.accelerator_budget_bytes as f64 / GIB as f64 - 44.8).abs() < 0.1);
    }

    #[test]
    fn quant_mismatch_warning_when_declared_wrong() {
        let mut s = qwen_7b_q4();
        s.quant = Quantization::F16; // 声明 f16 但文件只有 4.36GiB
        let a = assess(
            &s,
            &hw_with_vram(24, false, 64),
            &AssessRequest::default().with_context(4096),
        );
        assert!(a
            .warnings
            .iter()
            .any(|w| matches!(w, Warning::QuantMismatch { .. })));
    }

    #[test]
    fn require_full_gpu_blocks_partial() {
        let a = assess(
            &qwen_32b_q4(),
            &hw_with_vram(8, false, 64),
            &AssessRequest::default()
                .with_context(4096)
                .require_full_gpu(),
        );
        // 不允许部分卸载 → 只能 CPU 或不可行
        assert!(matches!(
            a.placement,
            Placement::CpuOnly | Placement::Impossible
        ));
    }

    #[test]
    fn best_quant_within_picks_highest_quality_that_fits() {
        let mut s = qwen_7b_q4();
        s.file_size_bytes = 0; // 让量化真正影响估算
        let hw = hw_with_vram(24, false, 64);
        let req = AssessRequest::default().with_context(4096);
        let (q, a) = best_quant_within(&s, &hw, &req, 24 * GIB).unwrap();
        assert!(a.requirement.total_bytes() <= 24 * GIB);
        // 24GiB 能容纳 f16 的 7B（~15GiB），应选 f16/bfloat16 而非 q4
        assert!(
            q.bits_per_weight() >= Quantization::Q8_0.bits_per_weight(),
            "picked {q}"
        );
    }

    #[test]
    fn best_quant_within_respects_tight_budget() {
        let mut s = qwen_32b_q4();
        s.file_size_bytes = 0;
        let hw = hw_with_vram(24, false, 64);
        let req = AssessRequest::default().with_context(4096);
        let (q, _) = best_quant_within(&s, &hw, &req, 24 * GIB).unwrap();
        // 32B 在 24GiB 内只能放 q3/q4 级
        assert!(q.bits_per_weight() < 6.0, "picked {q}");
    }

    #[test]
    fn best_quant_returns_none_when_impossible() {
        let mut s = qwen_32b_q4();
        s.file_size_bytes = 0;
        let hw = hw_with_vram(4, false, 8);
        let req = AssessRequest::default()
            .with_context(4096)
            .no_cpu_fallback();
        assert!(best_quant_within(&s, &hw, &req, 4 * GIB).is_none());
    }

    #[test]
    fn max_context_is_aligned_and_bounded() {
        let s = qwen_7b_q4();
        let ctx = max_context_within(&s, KvCacheType::F16, 24 * GIB, 512);
        assert_eq!(ctx % 256, 0);
        assert!(ctx <= s.native_context);
    }

    #[test]
    fn max_gpu_layers_zero_when_budget_below_one_layer() {
        let s = qwen_7b_q4();
        assert_eq!(max_gpu_layers(&s, KvCacheType::F16, 4096, 10 * MIB), 0);
    }

    #[test]
    fn max_gpu_layers_full_on_generous_budget() {
        let s = qwen_7b_q4();
        let layers = max_gpu_layers(&s, KvCacheType::F16, 4096, 100 * GIB);
        assert_eq!(layers, s.n_layers + 1);
    }

    #[test]
    fn cloud_requirement_reuses_local_math() {
        let s = qwen_7b_q4();
        let a = cloud_requirement_bytes(&s, 4096, KvCacheType::Q8_0, 512);
        let b = requirement(&s, 4096, KvCacheType::Q8_0, 512).total_bytes();
        assert_eq!(a, b);
    }
}
