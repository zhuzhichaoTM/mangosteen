//! 内置模型模板：QWEN25_05B_DEFAULT（独立文件，模型通过文件隔离）。

/// 模板正文（唯一真源）。
pub const TEMPLATE: &str = r#"template: default
model_id: qwen2.5-0.5b
source: https://hf-mirror.com/Qwen/Qwen2.5-0.5B-Instruct-GGUF
model: qwen2.5-0.5b-instruct-q4_k_m.gguf
context: 8192
gpu_layers: 99
batch: 512
ubatch: 128
cache_k: q4_0
cache_v: q4_0
alias: qwen2.5-0.5b
"#;


/// 模型标识（与部署页下拉框一致）。
pub const MODEL_ID: &str = "qwen2.5-0.5b";
/// 模板槽位。
pub const SLOT: &str = "default";
/// 展示名。
pub const DISPLAY: &str = "default";
/// 该模型全部下载文件的 SHA256（(文件名小写, 哈希)），供 `builtin_file_sha256` 聚合。
pub const HASHES: &[(&str, &str)] = &[
    ("qwen2.5-0.5b-instruct-q4_k_m.gguf", "74a4da8c9fdbcd15bd1f6d01d621410d31c6fc00986f5eb687824e7b93d7a9db")
];


