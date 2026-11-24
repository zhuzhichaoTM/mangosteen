//! 内置模型模板：QWEN36_35B_A3B_DEFAULT（独立文件，模型通过文件隔离）。

/// 模板正文（唯一真源）。
pub const TEMPLATE: &str = r#"template: default
model_id: qwen3.6-35b-a3b
engine: strata
source: https://hf-mirror.com/unsloth/Qwen3.6-35B-A3B-GGUF
model: Qwen3.6-35B-A3B-UD-Q4_K_XL.gguf
auxiliary_models:
  - mmproj-F16.gguf (视觉编码器，可选)
family: qwen36
model_quant: ud-q4_k_xl
context: 65536
kv: int8
mtp: on
vision: on
parallel: 1
alias: qwen3.6-35b-a3b
# Qwen3.6-35B-A3B 档位（~22GB）：
#   - 推荐 3090 24GB / V100S 32GB 节点（三级存储要求 32GB+ 内存、80GB+ SSD）
#   - 相比 Qwen3.8-Flash-Next 125B（83.6GB）更小，适合显存/磁盘受限的节点
#   - API Key 由网关任务级下发（Authorization: Bearer），不裸暴露端口
# 注：Strata 引擎自管模型存储（--data-dir）。
# Mangosteen 远端下载链路把 GGUF 下到 {workdir}/models/qwen36/ud-q4_k_xl/，
# 启动时 --data-dir 指向 workdir 根，Strata 发现同量化文件时直接复用，
# 未命中则由 strata launch 自行从 HF/ModelScope 拉取。
"#;


/// 模型标识（与部署页下拉框一致）。
pub const MODEL_ID: &str = "qwen3.6-35b-a3b";
/// 模板槽位。
pub const SLOT: &str = "default";
/// 展示名。
pub const DISPLAY: &str = "default";
/// 该模型全部下载文件的 SHA256（(文件名小写, 哈希)），供 `builtin_file_sha256` 聚合。
pub const HASHES: &[(&str, &str)] = &[
    ("qwen3.6-35b-a3b-ud-q4_k_xl.gguf", "707a55a8a4397ecde0c499d3e68c1ad1d240d1da65826b4949d1043f4450"),
        ("mmproj-f16.gguf", "8971ee4f331ff0a4c609374f32984b3d4e6dc086c0aa35f1d637fad1829e887f")
];


