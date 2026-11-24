//! 内置模型模板：QWEN38_27B_DEFAULT（独立文件，模型通过文件隔离）。

/// 模板正文（唯一真源）。
pub const TEMPLATE: &str = r#"template: default
model_id: qwen3.8-27b
source: https://hf-mirror.com/
repository: unsloth/Qwen3.8-27B-GGUF
model: Qwen3.8-27B-UD-Q4_K_M.gguf
auxiliary_models:
  - mmproj-BF16.gguf (多模态投影)
  - MTP/mtp-Qwen3.8-27B-Q4_0.gguf (MTP draft)
context: 65536
gpu_layers: 99
parallel: 1
batch: 2048
ubatch: 512
cache_k: q8_0
cache_v: q8_0
flash_attn: on
image_max_tokens: 1024
mtmd_batch_max_tokens: 512
cache_reuse: 256
jinja: on
metrics: on
speculative:
  type: draft-mtp
  draft_model_path: MTP/mtp-Qwen3.8-27B-Q4_0.gguf
  draft_gpu_layers: 99
  draft_n_max: 2
  draft_cache_k: q8_0
  draft_cache_v: q8_0
multimodal:
  mmproj: mmproj-BF16.gguf
alias: qwen3.8-27b
"#;


/// 模型标识（与部署页下拉框一致）。
pub const MODEL_ID: &str = "qwen3.8-27b";
/// 模板槽位。
pub const SLOT: &str = "default";
/// 展示名。
pub const DISPLAY: &str = "default";
/// 该模型全部下载文件的 SHA256（(文件名小写, 哈希)），供 `builtin_file_sha256` 聚合。
pub const HASHES: &[(&str, &str)] = &[
    ("qwen3.8-27b-ud-q4_k_m.gguf", "1bce504dfecd84153666928a41d8a66706f0158d3a317bc5196b72819d7dc529"),
        ("mmproj-bf16.gguf", "83ee4f4f205fa514161778c41df1ea14144faa0f713510893b63c2395f5c2d53"),
        ("mtp-qwen3.8-27b-q4_0.gguf", "50d9ce5a6da381bbcfb31061cf73df94a90e6faf8efeddee379a9cb8f1501c6e")
];


