//! GGUF 文件解析：从文件头提取模型架构参数。
//!
//! 归属：**基础能力层 · 模型管理模块**。
//!
//! ## 为什么必须解析真实文件而不是让用户填参数
//!
//! 资源评估的准确性完全取决于 `n_layers` / `n_kv_heads` / `embedding_dim` 这几个
//! 字段。手填极易出错，而 KV Cache 估算对这些参数是**线性敏感**的——
//! 把 `n_kv_heads` 从 4 填成 28 会让 32K 上下文的 KV 估算差 7 倍。
//!
//! ## GGUF v3 文件布局
//!
//! ```text
//! [magic: u32 = 0x46554747 "GGUF"]
//! [version: u32]
//! [tensor_count: u64]
//! [metadata_kv_count: u64]
//! [metadata KV pairs ...]   ← 只读这一段
//! [tensor infos ...]
//! [padding]
//! [tensor data ...]         ← 不读，只 stat 文件大小
//! ```
//!
//! 本模块**只读取元数据段**并按需早停，因此对 20GB 模型也是毫秒级。
//! 未识别的小端/大端由 `version` 前的 magic 判定（GGUF 规范要求小端，
//! 这里仍做校验以防文件损坏）。

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{MnError, MnResult};
use crate::model::{ModelSpec, Quantization};

/// GGUF magic：小端读入时等于 `0x46554747`（"GGUF" 四字节按小端解释）。
pub const GGUF_MAGIC: u32 = 0x4655_4747;
/// 支持的最高版本。
pub const MAX_SUPPORTED_VERSION: u32 = 3;

/// GGUF 元数据值。只覆盖解析所需类型，未用到的类型解析后丢弃。
#[derive(Debug, Clone, PartialEq)]
pub enum MetaValue {
    /// 无符号整数
    Uint(u64),
    /// 有符号整数
    Int(i64),
    /// 浮点
    Float(f32),
    /// 布尔
    Bool(bool),
    /// 字符串
    Str(String),
    /// 数组（元素类型已归一化）
    Array(Vec<MetaValue>),
}

impl MetaValue {
    /// 取整数（含无损转换）。
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            MetaValue::Uint(v) => Some(*v),
            MetaValue::Int(v) if *v >= 0 => Some(*v as u64),
            _ => None,
        }
    }

    /// 取字符串。
    pub fn as_str(&self) -> Option<&str> {
        match self {
            MetaValue::Str(s) => Some(s),
            _ => None,
        }
    }

    /// 取浮点（整数自动提升）。
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            MetaValue::Float(v) => Some(*v as f64),
            MetaValue::Uint(v) => Some(*v as f64),
            MetaValue::Int(v) => Some(*v as f64),
            _ => None,
        }
    }
}

/// 解析结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GgufInfo {
    /// 文件格式版本
    pub version: u32,
    /// 张量数量
    pub tensor_count: u64,
    /// 架构标识（`general.architecture`），如 `llama`、`qwen2`
    pub architecture: String,
    /// 模型展示名（`general.name`）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// 层数
    pub n_layers: u32,
    /// 注意力头数
    pub n_heads: u32,
    /// KV 头数
    pub n_kv_heads: u32,
    /// 隐藏维度
    pub embedding_dim: u32,
    /// 词表大小（取 tokenizer 词表长度）
    pub vocab_size: u32,
    /// 训练上下文长度
    pub native_context: u32,
    /// GGUF 内容类型（`general.type`），如 `model` / `imatrix` / `adapter`。
    ///
    /// 为什么需要它：`.gguf` 扩展名不只用于模型。量化用的 imatrix 数据、
    /// LoRA adapter 也都是 GGUF 容器，但它们没有 `block_count` 等架构字段，
    /// 无法做资源评估。区分出来才能给出「这不是模型文件」的准确提示，
    /// 而不是含糊地报「元数据缺少必需字段」。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gguf_type: Option<String>,
    /// 声明的文件类型枚举（`general.file_type`）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_type: Option<u32>,
    /// 由 `file_type` 反解出的量化格式
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quant: Option<Quantization>,
    /// 文件字节数
    pub file_size_bytes: u64,
    /// 全部元数据（供排障与未来扩展；`--verbose` 时输出）
    #[serde(skip)]
    pub metadata: BTreeMap<String, MetaValue>,
}

impl GgufInfo {
    /// 转换为资源评估所需的 [`ModelSpec`]。
    ///
    /// 参数量由文件大小与量化位宽反推（比让用户填更准），
    /// 若 `file_type` 缺失则用 4.5 bpw 的中位经验值。
    pub fn to_model_spec(&self) -> ModelSpec {
        let bpw = self
            .quant
            .map(|q| q.bits_per_weight())
            .unwrap_or(DEFAULT_BPW);
        let params_b = if self.file_size_bytes > 0 {
            // 扣掉元数据/词表对齐的近似开销（约 1%）
            let params = (self.file_size_bytes as f64 * 8.0 / bpw as f64) / 1e9;
            params * 0.99
        } else {
            0.0
        };
        ModelSpec {
            name: self
                .name
                .clone()
                .unwrap_or_else(|| format!("{}-{}L", self.architecture, self.n_layers)),
            params_b,
            quant: self.quant.unwrap_or(Quantization::Q4KM),
            n_layers: self.n_layers,
            n_heads: self.n_heads,
            n_kv_heads: self.n_kv_heads,
            embedding_dim: self.embedding_dim,
            vocab_size: self.vocab_size,
            native_context: self.native_context,
            file_size_bytes: self.file_size_bytes,
        }
    }
}

/// `file_type` 缺失时的经验位宽。
const DEFAULT_BPW: f32 = 4.5;

/// 读取器：带边界检查的 GGUF 元数据解析。
struct Reader<R: Read> {
    inner: R,
    /// 已读取字节数，用于防止恶意文件导致无限读取
    consumed: u64,
    /// 总字节数上限（元数据段合理上限）
    limit: u64,
}

impl<R: Read> Reader<R> {
    fn new(inner: R, limit: u64) -> Self {
        Self {
            inner,
            consumed: 0,
            limit,
        }
    }

    fn take(&mut self, n: u64) -> MnResult<()> {
        self.consumed = self.consumed.saturating_add(n);
        if self.consumed > self.limit {
            return Err(MnError::GgufFormat {
                path: std::path::PathBuf::from("<stream>"),
                reason: format!("元数据段超过 {} 字节上限，疑似文件损坏", self.limit),
            });
        }
        Ok(())
    }

    fn u8(&mut self) -> MnResult<u8> {
        let mut b = [0u8; 1];
        self.inner.read_exact(&mut b).map_err(map_read_err)?;
        self.take(1)?;
        Ok(b[0])
    }

    fn u16(&mut self) -> MnResult<u16> {
        let mut b = [0u8; 2];
        self.inner.read_exact(&mut b).map_err(map_read_err)?;
        self.take(2)?;
        Ok(u16::from_le_bytes(b))
    }

    fn u32(&mut self) -> MnResult<u32> {
        let mut b = [0u8; 4];
        self.inner.read_exact(&mut b).map_err(map_read_err)?;
        self.take(4)?;
        Ok(u32::from_le_bytes(b))
    }

    fn u64(&mut self) -> MnResult<u64> {
        let mut b = [0u8; 8];
        self.inner.read_exact(&mut b).map_err(map_read_err)?;
        self.take(8)?;
        Ok(u64::from_le_bytes(b))
    }

    fn i8v(&mut self) -> MnResult<i8> {
        Ok(self.u8()? as i8)
    }
    fn i16v(&mut self) -> MnResult<i16> {
        Ok(self.u16()? as i16)
    }
    fn i32v(&mut self) -> MnResult<i32> {
        Ok(self.u32()? as i32)
    }
    fn i64v(&mut self) -> MnResult<i64> {
        Ok(self.u64()? as i64)
    }
    fn f32v(&mut self) -> MnResult<f32> {
        Ok(f32::from_bits(self.u32()?))
    }
    fn f64v(&mut self) -> MnResult<f64> {
        Ok(f64::from_bits(self.u64()?))
    }

    fn string(&mut self) -> MnResult<String> {
        let len = self.u64()?;
        // 单个字符串上限 64 MiB，超出视为损坏
        if len > 64 * 1024 * 1024 {
            return Err(MnError::GgufFormat {
                path: std::path::PathBuf::from("<stream>"),
                reason: format!("字符串长度 {len} 异常"),
            });
        }
        let mut buf = vec![0u8; len as usize];
        self.inner.read_exact(&mut buf).map_err(map_read_err)?;
        self.take(len)?;
        String::from_utf8(buf).map_err(|e| MnError::GgufFormat {
            path: std::path::PathBuf::from("<stream>"),
            reason: format!("字符串非 UTF-8：{e}"),
        })
    }

    /// 读取指定类型的元数据值。
    fn value(&mut self, ty: u32, depth: u8) -> MnResult<MetaValue> {
        if depth > 4 {
            return Err(MnError::GgufFormat {
                path: std::path::PathBuf::from("<stream>"),
                reason: "元数据数组嵌套过深（>4 层）".into(),
            });
        }
        Ok(match ty {
            0 => MetaValue::Uint(self.u8()? as u64),
            1 => MetaValue::Int(self.i8v()? as i64),
            2 => MetaValue::Uint(self.u16()? as u64),
            3 => MetaValue::Int(self.i16v()? as i64),
            4 => MetaValue::Uint(self.u32()? as u64),
            5 => MetaValue::Int(self.i32v()? as i64),
            6 => MetaValue::Float(self.f32v()?),
            7 => MetaValue::Bool(self.u8()? != 0),
            8 => MetaValue::Str(self.string()?),
            9 => {
                let elem_ty = self.u32()?;
                let n = self.u64()?;
                // 数组元素数上限：防止 `n = u64::MAX` 导致分配爆炸
                if n > 1_000_000 {
                    return Err(MnError::GgufFormat {
                        path: std::path::PathBuf::from("<stream>"),
                        reason: format!("数组元素数 {n} 异常"),
                    });
                }
                // 词表数组可达 15 万元素，全量保存代价大；只保留前 1 个用于类型判断
                let keep = if n <= 4096 { n as usize } else { 0 };
                let mut items = Vec::with_capacity(keep);
                for i in 0..n {
                    let v = self.value(elem_ty, depth + 1)?;
                    if (i as usize) < keep {
                        items.push(v);
                    }
                }
                MetaValue::Array(items)
            }
            10 => MetaValue::Uint(self.u64()?),
            11 => MetaValue::Int(self.i64v()?),
            12 => MetaValue::Float(self.f64v()? as f32),
            other => {
                return Err(MnError::GgufFormat {
                    path: std::path::PathBuf::from("<stream>"),
                    reason: format!("未知元数据类型 {other}"),
                })
            }
        })
    }
}

fn map_read_err(e: std::io::Error) -> MnError {
    MnError::GgufFormat {
        path: std::path::PathBuf::from("<stream>"),
        reason: format!("读取提前结束（文件可能被截断）：{e}"),
    }
}

/// 元数据段读取上限：256 MiB。正常 GGUF 元数据 < 10 MiB。
const META_LIMIT: u64 = 256 * 1024 * 1024;

/// 从实现了 `Read` 的数据流解析 GGUF 元数据。
///
/// `file_size_bytes` 由调用方提供（数据流本身可能不知道总长）。
pub fn parse_reader<R: Read>(
    inner: R,
    file_size_bytes: u64,
) -> MnResult<(u32, u64, BTreeMap<String, MetaValue>)> {
    let mut r = Reader::new(BufReader::with_capacity(64 * 1024, inner), META_LIMIT);

    let magic = r.u32()?;
    if magic != GGUF_MAGIC {
        return Err(MnError::GgufFormat {
            path: std::path::PathBuf::from("<stream>"),
            reason: format!(
                "magic 不匹配：期望 0x{GGUF_MAGIC:08X}（\"GGUF\"），实际 0x{magic:08X}"
            ),
        });
    }

    let version = r.u32()?;
    if version == 0 || version > MAX_SUPPORTED_VERSION {
        return Err(MnError::GgufFormat {
            path: std::path::PathBuf::from("<stream>"),
            reason: format!("不支持的 GGUF 版本 {version}（支持 1..={MAX_SUPPORTED_VERSION}）"),
        });
    }

    let tensor_count = r.u64()?;
    // 可信上限：LLM 张量数通常 < 1000，量化后不会超 1e6
    if tensor_count > 10_000_000 {
        return Err(MnError::GgufFormat {
            path: std::path::PathBuf::from("<stream>"),
            reason: format!("张量数 {tensor_count} 异常"),
        });
    }

    let kv_count = r.u64()?;
    if kv_count > 1_000_000 {
        return Err(MnError::GgufFormat {
            path: std::path::PathBuf::from("<stream>"),
            reason: format!("元数据项数 {kv_count} 异常"),
        });
    }

    let mut meta = BTreeMap::new();
    for _ in 0..kv_count {
        let key = r.string()?;
        let ty = r.u32()?;
        let val = r.value(ty, 0)?;
        meta.insert(key, val);
    }

    let _ = file_size_bytes;
    Ok((version, tensor_count, meta))
}

/// 解析 GGUF 文件。
///
/// 只读取元数据段，不读取张量数据，对 20GB 文件也是毫秒级。
pub fn parse_file(path: &Path) -> MnResult<GgufInfo> {
    let meta = std::fs::metadata(path).map_err(|_| MnError::ModelNotFound {
        path: path.to_path_buf(),
    })?;
    if !meta.is_file() {
        return Err(MnError::GgufFormat {
            path: path.to_path_buf(),
            reason: "不是普通文件".into(),
        });
    }
    let file_size = meta.len();
    // 最小合法 GGUF：magic(4)+ver(4)+tensors(8)+kvcount(8) = 24 字节
    if file_size < 24 {
        return Err(MnError::GgufFormat {
            path: path.to_path_buf(),
            reason: format!("文件仅 {file_size} 字节，不足最小 GGUF 长度"),
        });
    }

    let file = File::open(path).map_err(|e| MnError::io(format!("打开 {}", path.display()), &e))?;
    let (version, tensor_count, metadata) =
        parse_reader(BufReader::new(file), file_size).map_err(|e| enrich(e, path))?;

    let info = extract_info(path, version, tensor_count, metadata, file_size)?;
    Ok(info)
}

/// 从**数据流**解析 GGUF 元数据段。
///
/// ## 用途
///
/// 「下载前评估」需要真实的层数 / 注意力头数 / 隐藏维度，这些只在 GGUF
/// 元数据段里。模型尚未落到本地时，可以用 HTTP `Range` 只取文件头部
/// （元数据段通常远小于张量数据）走这里解析，不必等下完几个 GB。
///
/// ## `file_size_bytes` 为什么必须由调用方给
///
/// 元数据里**没有**文件总大小，而参数量是「文件大小 ÷ 量化位宽」反推的。
/// 流式解析拿到的是前几 MB，不能据此当总大小——必须由调用方从
/// 源的元数据（`Content-Range` / 搜索结果里的 size）传进来。
pub fn parse_stream<R: Read>(inner: R, file_size_bytes: u64) -> MnResult<GgufInfo> {
    let (version, tensor_count, metadata) =
        parse_reader(inner, file_size_bytes).map_err(|e| enrich(e, Path::new("<stream>")))?;
    extract_info(
        Path::new("<stream>"),
        version,
        tensor_count,
        metadata,
        file_size_bytes,
    )
}

/// 给底层解析错误补上文件路径信息。
fn enrich(e: MnError, path: &Path) -> MnError {
    match e {
        MnError::GgufFormat { reason, .. } => MnError::GgufFormat {
            path: path.to_path_buf(),
            reason,
        },
        other => other,
    }
}

/// 从元数据映射中抽取架构参数。
fn extract_info(
    path: &Path,
    version: u32,
    tensor_count: u64,
    metadata: BTreeMap<String, MetaValue>,
    file_size_bytes: u64,
) -> MnResult<GgufInfo> {
    let architecture = metadata
        .get("general.architecture")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    // 先看 `general.type`：imatrix / adapter 等也是 .gguf 容器，
    // 但没有架构字段。若不先识别，用户会看到「缺少 block_count」这种
    // 让人无从下手的报错，而真正的原因是「下错文件了」。
    let gguf_type = metadata
        .get("general.type")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    if let Some(t) = &gguf_type {
        if !t.eq_ignore_ascii_case("model") && !t.eq_ignore_ascii_case("default") {
            let what = match t.as_str() {
                "imatrix" => "量化用的重要性矩阵（imatrix）数据",
                "adapter" => "LoRA adapter",
                _ => "非模型数据",
            };
            return Err(MnError::GgufFormat {
                path: path.to_path_buf(),
                reason: format!(
                    "这不是可推理的模型文件，而是{what}（general.type = `{t}`）。\
                     它不能用于部署，也无法做资源评估。\
                     请回到「部署」页搜索，选择文件名以 .gguf 结尾且不带 imatrix 的模型文件"
                ),
            });
        }
    }

    // 架构专属键名形如 `qwen2.block_count`，键前缀即 architecture
    let key = |suffix: &str| format!("{architecture}.{suffix}");

    let get_u32 =
        |k: &str| -> Option<u32> { metadata.get(k).and_then(|v| v.as_u64()).map(|v| v as u32) };

    let n_layers = get_u32(&key("block_count"));
    let n_heads = get_u32(&key("attention.head_count"));
    let embedding_dim = get_u32(&key("embedding_length"));
    let native_context = get_u32(&key("context_length"));

    // 缺失核心字段无法做资源评估 → 明确报错，不猜测
    let missing: Vec<&str> = [
        ("block_count", n_layers.is_some()),
        ("attention.head_count", n_heads.is_some()),
        ("embedding_length", embedding_dim.is_some()),
    ]
    .iter()
    .filter_map(|(name, present)| if *present { None } else { Some(*name) })
    .collect();
    if !missing.is_empty() {
        return Err(MnError::GgufFormat {
            path: path.to_path_buf(),
            reason: format!(
                "元数据缺少必需字段 `{}`（架构 `{architecture}`），无法评估资源需求",
                missing.join("`, `")
            ),
        });
    }
    let n_layers = n_layers.unwrap();
    let n_heads = n_heads.unwrap();
    let embedding_dim = embedding_dim.unwrap();

    // KV 头缺失 → 视为 MHA（最保守，KV 估算偏大不会导致 OOM）
    let n_kv_heads = get_u32(&key("attention.head_count_kv")).unwrap_or(n_heads);

    // 词表大小：优先取 tokenizer 词表长度
    let vocab_size = metadata
        .get("tokenizer.ggml.tokens")
        .and_then(|v| match v {
            MetaValue::Array(items) => Some(items.len() as u32),
            _ => None,
        })
        .filter(|n| *n > 0)
        // 数组被截断时回退到元数据里的显式字段
        .or_else(|| {
            metadata
                .get(&key("vocab_size"))
                .and_then(|v| v.as_u64())
                .map(|v| v as u32)
        })
        .unwrap_or(0);

    // 上下文：部分老模型无 context_length，退化为 2048
    let native_context = native_context.unwrap_or(2048).max(512);

    let file_type = metadata
        .get("general.file_type")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);
    let quant = file_type.and_then(Quantization::from_gguf_file_type);

    let name = metadata
        .get("general.name")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let info = GgufInfo {
        version,
        tensor_count,
        architecture,
        gguf_type,
        name,
        n_layers,
        n_heads,
        n_kv_heads,
        embedding_dim,
        vocab_size,
        native_context,
        file_type,
        quant,
        file_size_bytes,
        metadata,
    };

    // 自洽性校验：head_dim 必须为整数
    if info.n_heads > 0 && info.embedding_dim % info.n_heads != 0 {
        return Err(MnError::GgufFormat {
            path: path.to_path_buf(),
            reason: format!(
                "embedding_length({}) 不能被 head_count({}) 整除",
                info.embedding_dim, info.n_heads
            ),
        });
    }
    if info.n_kv_heads > info.n_heads {
        return Err(MnError::GgufFormat {
            path: path.to_path_buf(),
            reason: format!(
                "attention.head_count_kv({}) 大于 head_count({})",
                info.n_kv_heads, info.n_heads
            ),
        });
    }

    Ok(info)
}

/// 计算文件 SHA-256。
///
/// 大文件按 8 MiB 分块流式计算，内存占用恒定。
pub fn sha256_file(path: &Path) -> MnResult<String> {
    use sha2::{Digest, Sha256};

    let mut f =
        File::open(path).map_err(|e| MnError::io(format!("打开 {}", path.display()), &e))?;
    f.seek(SeekFrom::Start(0))
        .map_err(|e| MnError::io("定位文件头", &e))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 8 * 1024 * 1024];
    loop {
        let n = f
            .read(&mut buf)
            .map_err(|e| MnError::io(format!("读取 {}", path.display()), &e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex_lower(&hasher.finalize()))
}

/// 字节数组转小写十六进制。
pub fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

/// 构造最小合法 GGUF 字节序列（测试与自检用，`mangosteen doctor` 会用它验证解析器）。
pub fn build_test_gguf(
    architecture: &str,
    n_layers: u32,
    n_heads: u32,
    n_kv_heads: u32,
    embedding: u32,
    ctx: u32,
) -> Vec<u8> {
    fn push_str(out: &mut Vec<u8>, s: &str) {
        out.extend_from_slice(&(s.len() as u64).to_le_bytes());
        out.extend_from_slice(s.as_bytes());
    }
    fn push_kv_u32(out: &mut Vec<u8>, k: &str, v: u32) {
        push_str(out, k);
        out.extend_from_slice(&4u32.to_le_bytes()); // type = UINT32
        out.extend_from_slice(&v.to_le_bytes());
    }

    let mut out = Vec::new();
    out.extend_from_slice(&GGUF_MAGIC.to_le_bytes());
    out.extend_from_slice(&3u32.to_le_bytes()); // version
    out.extend_from_slice(&0u64.to_le_bytes()); // tensor_count

    let keys: Vec<String> = vec![
        "general.architecture".into(),
        format!("{architecture}.block_count"),
        format!("{architecture}.attention.head_count"),
        format!("{architecture}.attention.head_count_kv"),
        format!("{architecture}.embedding_length"),
        format!("{architecture}.context_length"),
    ];
    // 6 项元数据（architecture 为字符串，其余为 u32）
    out.extend_from_slice(&(6u64).to_le_bytes());

    push_str(&mut out, &keys[0]);
    out.extend_from_slice(&8u32.to_le_bytes()); // type = STRING
    push_str(&mut out, architecture);

    push_kv_u32(&mut out, &keys[1], n_layers);
    push_kv_u32(&mut out, &keys[2], n_heads);
    push_kv_u32(&mut out, &keys[3], n_kv_heads);
    push_kv_u32(&mut out, &keys[4], embedding);
    push_kv_u32(&mut out, &keys[5], ctx);

    out
}

/// 构造一个 `general.type = imatrix` 的最小 GGUF（**仅测试用**）。
///
/// 复现真实场景：imatrix 是量化辅助数据，用 GGUF 容器但只有
/// `general.type` 与少量键，没有 block_count 等架构字段。
pub fn build_test_imatrix_gguf() -> Vec<u8> {
    fn push_str(out: &mut Vec<u8>, s: &str) {
        out.extend_from_slice(&(s.len() as u64).to_le_bytes());
        out.extend_from_slice(s.as_bytes());
    }
    let mut out = Vec::new();
    out.extend_from_slice(&GGUF_MAGIC.to_le_bytes());
    out.extend_from_slice(&3u32.to_le_bytes());
    out.extend_from_slice(&0u64.to_le_bytes()); // tensor_count
    out.extend_from_slice(&2u64.to_le_bytes()); // 2 项元数据

    push_str(&mut out, "general.type");
    out.extend_from_slice(&8u32.to_le_bytes()); // STRING
    push_str(&mut out, "imatrix");

    push_str(&mut out, "general.name");
    out.extend_from_slice(&8u32.to_le_bytes());
    push_str(&mut out, "imatrix_unsloth");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn qwen_like() -> Vec<u8> {
        build_test_gguf("qwen2", 28, 28, 4, 3584, 32768)
    }

    #[test]
    fn parses_minimal_gguf() {
        let bytes = qwen_like();
        let (_v, _t, meta) = parse_reader(Cursor::new(&bytes), bytes.len() as u64).unwrap();
        assert_eq!(meta.len(), 6);
        assert_eq!(
            meta.get("general.architecture").unwrap().as_str(),
            Some("qwen2")
        );
        assert_eq!(meta.get("qwen2.block_count").unwrap().as_u64(), Some(28));
    }

    #[test]
    fn rejects_bad_magic() {
        let mut bytes = qwen_like();
        bytes[0] = 0x00;
        let e = parse_reader(Cursor::new(&bytes), bytes.len() as u64).unwrap_err();
        assert!(matches!(e, MnError::GgufFormat { .. }));
        assert!(e.to_string().contains("magic"));
    }

    #[test]
    fn rejects_future_version() {
        let mut bytes = qwen_like();
        bytes[4..8].copy_from_slice(&99u32.to_le_bytes());
        let e = parse_reader(Cursor::new(&bytes), bytes.len() as u64).unwrap_err();
        assert!(e.to_string().contains("版本"));
    }

    #[test]
    fn rejects_truncated_file() {
        let bytes = qwen_like();
        let cut = &bytes[..20];
        let e = parse_reader(Cursor::new(cut), cut.len() as u64).unwrap_err();
        assert!(matches!(e, MnError::GgufFormat { .. }));
    }

    #[test]
    fn rejects_absurd_kv_count() {
        let mut bytes = qwen_like();
        // 覆盖 kv_count 字段（偏移 16..24）
        bytes[16..24].copy_from_slice(&u64::MAX.to_le_bytes());
        let e = parse_reader(Cursor::new(&bytes), bytes.len() as u64).unwrap_err();
        assert!(matches!(e, MnError::GgufFormat { .. }));
    }

    #[test]
    fn extract_info_validates_divisibility() {
        // embedding 3585 不能被 28 整除
        let bytes = build_test_gguf("llama", 28, 28, 4, 3585, 4096);
        let (_v, _t, meta) = parse_reader(Cursor::new(&bytes), bytes.len() as u64).unwrap();
        let e = extract_info(Path::new("/t.gguf"), 3, 0, meta, bytes.len() as u64).unwrap_err();
        assert!(e.to_string().contains("整除"));
    }

    #[test]
    fn extract_info_rejects_kv_heads_gt_heads() {
        let bytes = build_test_gguf("llama", 32, 32, 64, 4096, 8192);
        let (_v, _t, meta) = parse_reader(Cursor::new(&bytes), bytes.len() as u64).unwrap();
        let e = extract_info(Path::new("/t.gguf"), 3, 0, meta, bytes.len() as u64).unwrap_err();
        assert!(e.to_string().contains("head_count_kv"));
    }

    #[test]
    fn missing_required_fields_reports_them() {
        // 只写 architecture，缺少 block_count 等
        let mut out = Vec::new();
        out.extend_from_slice(&GGUF_MAGIC.to_le_bytes());
        out.extend_from_slice(&3u32.to_le_bytes());
        out.extend_from_slice(&0u64.to_le_bytes());
        out.extend_from_slice(&1u64.to_le_bytes());
        let k = "general.architecture";
        out.extend_from_slice(&(k.len() as u64).to_le_bytes());
        out.extend_from_slice(k.as_bytes());
        out.extend_from_slice(&8u32.to_le_bytes());
        let a = "llama";
        out.extend_from_slice(&(a.len() as u64).to_le_bytes());
        out.extend_from_slice(a.as_bytes());

        let (_v, _t, meta) = parse_reader(Cursor::new(&out), out.len() as u64).unwrap();
        let e = extract_info(Path::new("/t.gguf"), 3, 0, meta, out.len() as u64).unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("block_count"), "got {msg}");
        assert!(msg.contains("attention.head_count"), "got {msg}");
    }

    #[test]
    fn to_model_spec_derives_params_from_size() {
        let bytes = build_test_gguf("qwen2", 28, 28, 4, 3584, 32768);
        let (_v, _t, meta) = parse_reader(Cursor::new(&bytes), bytes.len() as u64).unwrap();
        // 假设真实文件 4.68GB
        let info = extract_info(Path::new("/t.gguf"), 3, 0, meta, 4_683_073_024).unwrap();
        let spec = info.to_model_spec();
        assert_eq!(spec.n_layers, 28);
        assert_eq!(spec.n_kv_heads, 4);
        assert!(spec.is_gqa());
        assert_eq!(spec.native_context, 32768);
        // 无 file_type → 默认 Q4_K_M
        assert_eq!(spec.quant, Quantization::Q4KM);
        // 参数量应在 7~9B 量级
        assert!(
            (7.0..9.0).contains(&spec.params_b),
            "params={}",
            spec.params_b
        );
        assert!(spec.validate().is_ok());
    }

    #[test]
    fn string_limit_prevents_absurd_allocation() {
        let mut out = Vec::new();
        out.extend_from_slice(&GGUF_MAGIC.to_le_bytes());
        out.extend_from_slice(&3u32.to_le_bytes());
        out.extend_from_slice(&0u64.to_le_bytes());
        out.extend_from_slice(&1u64.to_le_bytes());
        out.extend_from_slice(&(u64::MAX / 2).to_le_bytes()); // 恶意超长 key
        let e = parse_reader(Cursor::new(&out), out.len() as u64).unwrap_err();
        assert!(e.to_string().contains("字符串长度"));
    }

    #[test]
    fn array_depth_guard() {
        // 构造 depth=5 的嵌套数组：type=9 表示数组
        let mut out = Vec::new();
        out.extend_from_slice(&GGUF_MAGIC.to_le_bytes());
        out.extend_from_slice(&3u32.to_le_bytes());
        out.extend_from_slice(&0u64.to_le_bytes());
        out.extend_from_slice(&1u64.to_le_bytes());
        out.extend_from_slice(&1u64.to_le_bytes()); // key 长度
        out.extend_from_slice(b"k");
        out.extend_from_slice(&9u32.to_le_bytes()); // 值是数组
                                                    // 逐层嵌套：每层 elem_type=9, n=1
        for _ in 0..6 {
            out.extend_from_slice(&9u32.to_le_bytes());
            out.extend_from_slice(&1u64.to_le_bytes());
        }
        let e = parse_reader(Cursor::new(&out), 1 << 20).unwrap_err();
        assert!(e.to_string().contains("嵌套"), "got {e}");
    }

    #[test]
    fn sha256_matches_known_value() {
        let dir = std::env::temp_dir().join(format!("mn-sha-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("abc.bin");
        std::fs::write(&p, b"abc").unwrap();
        // SHA-256("abc") 的标准值
        assert_eq!(
            sha256_file(&p).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn hex_lower_encoding() {
        assert_eq!(hex_lower(&[0x00, 0x0f, 0xa5, 0xff]), "000fa5ff");
        assert_eq!(hex_lower(&[]), "");
    }

    #[test]
    fn nonexistent_file_maps_to_model_not_found() {
        let e = parse_file(Path::new("/definitely/not/here.gguf")).unwrap_err();
        assert!(matches!(e, MnError::ModelNotFound { .. }));
    }

    #[test]
    fn tiny_file_rejected() {
        let dir = std::env::temp_dir().join(format!("mn-tiny-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("tiny.gguf");
        std::fs::write(&p, b"GGUF").unwrap();
        let e = parse_file(&p).unwrap_err();
        assert!(e.to_string().contains("不足最小"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn full_file_roundtrip_on_disk() {
        let dir = std::env::temp_dir().join(format!("mn-gguf-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("q.gguf");
        let mut bytes = build_test_gguf("qwen2", 28, 28, 4, 3584, 32768);
        // 补足文件体积，模拟真实权重区
        bytes.extend(std::iter::repeat(0u8).take(4096));
        std::fs::write(&p, &bytes).unwrap();

        let info = parse_file(&p).unwrap();
        assert_eq!(info.architecture, "qwen2");
        assert_eq!(info.n_layers, 28);
        assert_eq!(info.n_kv_heads, 4);
        assert_eq!(info.embedding_dim, 3584);
        assert_eq!(info.version, 3);
        assert_eq!(info.file_size_bytes, bytes.len() as u64);
        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod gguf_type_tests {
    use super::*;

    /// imatrix 文件必须被明确识别为「不是模型」，而不是报「缺字段」。
    ///
    /// 用户实际踩过：下到 `imatrix_unsloth.gguf`，登记时报
    /// 「元数据缺少必需字段 block_count / attention.head_count /
    /// embedding_length（架构 unknown）」——这话技术上没错，
    /// 但完全没告诉用户「你下错文件了」。
    #[test]
    fn imatrix_is_reported_as_not_a_model() {
        let dir = std::env::temp_dir().join(format!("mn-gguf-imatrix-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("imatrix_unsloth.gguf");
        std::fs::write(&path, build_test_imatrix_gguf()).unwrap();

        let err = parse_file(&path).expect_err("imatrix 不应被当作模型解析成功");
        let msg = err.to_string();
        assert!(
            msg.contains("不是可推理的模型文件"),
            "必须直说「这不是模型」，实际：{msg}"
        );
        assert!(msg.contains("imatrix"), "应指出具体类型，实际：{msg}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 正常模型的 gguf_type 应为 None 或 "model"，且能解析成功。
    #[test]
    fn normal_model_still_parses() {
        let dir = std::env::temp_dir().join(format!("mn-gguf-model-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("qwen.gguf");
        std::fs::write(&path, build_test_gguf("qwen2", 24, 14, 2, 896, 32768)).unwrap();

        let info = parse_file(&path).expect("正常模型应解析成功");
        assert_eq!(info.architecture, "qwen2");
        assert_eq!(info.n_layers, 24);
        // 没有 general.type 时不应误判为非模型
        assert!(
            info.gguf_type.is_none() || info.gguf_type.as_deref() == Some("model"),
            "正常模型不应被标成非模型类型"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
