//! 内置模型模板：QWEN38_FLASH_NEXT_IQ2XS_DEFAULT（独立文件，模型通过文件隔离）。

/// 模板正文（唯一真源）。
pub const TEMPLATE: &str = r#"template: default
model_id: qwen3.8-flash-next-125b-iq2xs
engine: strata
source: https://hf-mirror.com/ISTA-DASLab/Qwen3.8-Flash-Next-GSQ-RCO-GGUF
model: IQ2_XS/Qwen3.8-Flash-Next-GSQ-RCO-IQ2_XS-00001-of-00002.gguf, IQ2_XS/Qwen3.8-Flash-Next-GSQ-RCO-IQ2_XS-00002-of-00002.gguf
auxiliary_models:
  - mmproj-Qwen3.8-Flash-Next-BF16.gguf (视觉编码器，可选)
family: qwen
model_quant: iq2_xs
context: 65536
kv: int8
mtp: on
vision: on
parallel: 1
alias: qwen3.8-flash-next-125b-iq2xs
# IQ2_XS 档位（68GB）：
#   - 推荐 3090 24GB / V100S 32GB 节点（三级存储要求 32GB+ 内存、80GB+ SSD）
#   - 相比 IQ3_S（83.6GB）更小，适合显存/磁盘受限的节点
#   - API Key 由网关任务级下发（Authorization: Bearer），不裸暴露端口
# 注：Strata 引擎自管模型存储（--data-dir）。
# Mangosteen 远端下载链路把 GGUF 下到 {workdir}/models/IQ2_XS/，
# 启动时 --data-dir 指向 workdir 根，Strata 发现同量化文件时直接复用，
# 未命中则由 strata launch 自行从 HF/ModelScope 拉取。
"#;


/// 模型标识（与部署页下拉框一致）。
pub const MODEL_ID: &str = "qwen3.8-flash-next-125b-iq2xs";
/// 模板槽位。
pub const SLOT: &str = "default";
/// 展示名。
pub const DISPLAY: &str = "default";
/// 该模型全部下载文件的 SHA256（(文件名小写, 哈希)），供 `builtin_file_sha256` 聚合。
pub const HASHES: &[(&str, &str)] = &[
    ("qwen3.8-flash-next-gsq-rco-iq2_xs-00001-of-00002.gguf", "92cee27ae5bbadcd732416a0f7a7f0acc092399dbbe8f5a5efa707c2ec0a49d7"),
        ("qwen3.8-flash-next-gsq-rco-iq2_xs-00002-of-00002.gguf", "316b46f3a2dbd68c900f43136ab9449f9dcc3725dfd8c794847c204bc161e113"),
        ("mmproj-qwen3.8-flash-next-bf16.gguf", "b1a82259702816a5330d7bd7607cd9676b11780e79ff7348c21103ff3ce49bd0")
];


