//! 内置模型模板：QWEN38_27B_UNCENSORED（独立文件，模型通过文件隔离）。

/// 模板正文（唯一真源）。
pub const TEMPLATE: &str = r#"template: Uncensored
model_id: qwen3.8-27b
source: https://hf-mirror.com/
repository: JonathanColetti/Qwen3.8-27B-Uncensored-GGUF
model: Qwen3.8-27B-Uncensored-Q4_K_M.gguf
auxiliary_models:
  - mmproj-Qwen3.8-27B-Uncensored-F16.gguf (多模态投影)
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
  draft_n_max: 2
multimodal:
  mmproj: mmproj-Qwen3.8-27B-Uncensored-F16.gguf
alias: qwen3.8-27b
# 注：本模板**不使用外置草稿模型**。
# 主权重 GGUF 自带 Qwen3.8 原生内嵌 MTP 头，只传 --spec-type draft-mtp
# 即可获得投机解码加速，无需任何额外文件。
"#;


/// 模型标识（与部署页下拉框一致）。
pub const MODEL_ID: &str = "qwen3.8-27b";
/// 模板槽位。
pub const SLOT: &str = "uncensored";
/// 展示名。
pub const DISPLAY: &str = "Uncensored";
/// 该模型全部下载文件的 SHA256（(文件名小写, 哈希)），供 `builtin_file_sha256` 聚合。
pub const HASHES: &[(&str, &str)] = &[
    ("qwen3.8-27b-uncensored-q4_k_m.gguf", "4c5e2db039e9325ac7724c8846c71356a24ad1cdfa28002d73ecb6be645f9675"),
        ("mmproj-qwen3.8-27b-uncensored-f16.gguf", "5ac423f8a29059dc24e51bc6a43e9380dcd57a9347f28b62591e0b3f60b7081c")
];


