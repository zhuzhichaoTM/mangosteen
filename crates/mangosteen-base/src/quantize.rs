//! **第四层 · 资产与执行层，模块 2：模型量化管理**。
//!
//! ## 定位
//!
//! 基线明确：这是**可选工具能力**，「按需提供模型量化处理能力，
//! 非部署主链路必经流程」。
//!
//! 因此本模块的边界很硬：
//!
//! | 做 | 不做 |
//! |----|------|
//! | 探测 `llama-quantize` 是否可用 | 不参与部署决策 |
//! | 生成量化参数、产出新 GGUF | 不自动量化（用户显式发起） |
//! | 校验输入/输出路径合法性 | 不删除原文件（量化是派生，不是替换） |
//!
//! ## 为什么独立成模块
//!
//! 量化会**改变模型文件本身**（体积、精度、可部署性）。把它塞进
//! 下载或加载流程里，会让「为什么我的模型变小了」变成无法回答的问题。
//! 单独一条显式路径，用户不做就完全不发生。
//!
//! ## 与主链路的关系
//!
//! ```text
//! 模型下载（必走） → 模型登记（必走） → 加载执行（必走）
//!                                    ↘ 量化（可选，产出新文件后重新登记）
//! ```
//!
//! 量化产物是一个**新的模型条目**，走的是和下载完成相同的登记路径，
//! 所以不需要在主链路上加任何分支。

use std::path::{Path, PathBuf};

use mangosteen_core::{MnError, MnResult};

/// 目标量化类型。
///
/// 只列 llama.cpp 常用且稳定的几档。不做「支持全部 20 种」的表面功夫——
/// 每多一档就要多一份体积/精度说明，而用户真正会选的通常就是这几个。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuantType {
    /// Q4_0：最省空间，质量损失明显（老格式，兼容性最好）
    Q4_0,
    /// Q4_K_M：空间与质量的最佳平衡（推荐默认）
    Q4KM,
    /// Q5_K_M：质量优先，体积比 Q4 大约 20%
    Q5KM,
    /// Q8_0：接近原始精度，体积约等于 FP16 的一半
    Q8_0,
    /// F16：不量化，只做格式转换（用于从 BF16/F32 转半精度）
    F16,
}

impl QuantType {
    /// 全部可选档位。
    pub fn all() -> &'static [QuantType] {
        &[
            Self::Q4_0,
            Self::Q4KM,
            Self::Q5KM,
            Self::Q8_0,
            Self::F16,
        ]
    }

    /// llama-quantize 的参数值（第二个位置参数）。
    pub fn cli_value(self) -> &'static str {
        match self {
            Self::Q4_0 => "Q4_0",
            Self::Q4KM => "Q4_K_M",
            Self::Q5KM => "Q5_K_M",
            Self::Q8_0 => "Q8_0",
            Self::F16 => "F16",
        }
    }

    /// 文件名里的量化标记（小写，用于生成输出文件名）。
    pub fn slug(self) -> &'static str {
        match self {
            Self::Q4_0 => "q4_0",
            Self::Q4KM => "q4_k_m",
            Self::Q5KM => "q5_k_m",
            Self::Q8_0 => "q8_0",
            Self::F16 => "f16",
        }
    }

    /// 从字符串解析（大小写不敏感，接受 `q4_k_m` / `Q4_K_M` / `q4km`）。
    pub fn parse(s: &str) -> Option<Self> {
        let n = s.trim().to_ascii_lowercase().replace(['-', '.'], "_");
        match n.as_str() {
            "q4_0" => Some(Self::Q4_0),
            "q4_k_m" | "q4km" => Some(Self::Q4KM),
            "q5_k_m" | "q5km" => Some(Self::Q5KM),
            "q8_0" => Some(Self::Q8_0),
            "f16" | "fp16" => Some(Self::F16),
            _ => None,
        }
    }

    /// 相对原始 F16 的体积比例（用于给用户一个量级预期）。
    ///
    /// 这些是经验值，不是精确公式——量化后的实际体积还取决于
    /// 张量分布与元数据开销。用于「选档时的直觉判断」而非计费。
    pub fn size_ratio(self) -> f64 {
        match self {
            Self::Q4_0 => 0.30,
            Self::Q4KM => 0.33,
            Self::Q5KM => 0.40,
            Self::Q8_0 => 0.55,
            Self::F16 => 1.0,
        }
    }

    /// 质量提示（如实说明取舍，不吹「无损」）。
    pub fn quality_hint(self) -> &'static str {
        match self {
            Self::Q4_0 => "体积最小，质量损失较明显，老格式但兼容性最好",
            Self::Q4KM => "体积与质量平衡最好，多数场景推荐",
            Self::Q5KM => "质量优先，比 Q4_K_M 大约 20%",
            Self::Q8_0 => "接近原始精度，体积约为 FP16 的一半",
            Self::F16 => "不做量化，仅转半精度（BF16/F32 → F16）",
        }
    }
}

/// 量化任务描述。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuantizeRequest {
    /// 源 GGUF 路径
    pub src: PathBuf,
    /// 目标 GGUF 路径
    pub dst: PathBuf,
    /// 目标量化类型
    pub quant: QuantType,
    /// 允许的线程数（`--threads`）。`None` 用引擎默认。
    pub threads: Option<u32>,
}

impl QuantizeRequest {
    /// 由源路径与目标量化类型推导输出路径。
    ///
    /// 输出名形如 `qwen2.5-7b-instruct-q4_k_m.gguf`：
    /// 去掉源文件已有的量化后缀，再拼上目标档位。
    /// 不去掉的话会出现 `...-q8_0-q4_k_m.gguf` 这种叠词。
    pub fn new(src: impl Into<PathBuf>, quant: QuantType) -> Self {
        let src = src.into();
        let dst = derive_output_path(&src, quant);
        Self {
            src,
            dst,
            quant,
            threads: None,
        }
    }

    /// 覆盖输出路径。
    pub fn with_dst(mut self, dst: impl Into<PathBuf>) -> Self {
        self.dst = dst.into();
        self
    }

    /// 生成 `llama-quantize` 参数（不含可执行文件本身）。
    ///
    /// 参数顺序与 llama.cpp 一致：`<src> <dst> <type> [threads]`。
    pub fn to_args(&self) -> Vec<String> {
        let mut args = vec![
            self.src.display().to_string(),
            self.dst.display().to_string(),
            self.quant.cli_value().to_string(),
        ];
        if let Some(t) = self.threads {
            args.push(t.to_string());
        }
        args
    }

    /// 执行前校验。
    ///
    /// 三条硬要求：源文件存在、目标不与源相同（会毁掉原件）、
    /// 目标目录存在。校验放在这里而不是执行时，是为了在**发起前**
    /// 就给出可操作的错误，而不是等跑了一半才发现路径写错。
    pub fn validate(&self) -> MnResult<()> {
        if !self.src.is_file() {
            return Err(MnError::ModelNotFound {
                path: self.src.clone(),
            });
        }
        if self.src == self.dst {
            return Err(MnError::bad_request(format!(
                "输出路径与源文件相同（{}）。量化会写入新文件，\
                 覆盖源文件将导致原始模型永久丢失",
                self.dst.display()
            )));
        }
        if let Some(dir) = self.dst.parent() {
            if !dir.as_os_str().is_empty() && !dir.is_dir() {
                return Err(MnError::bad_request(format!(
                    "输出目录不存在：{}",
                    dir.display()
                )));
            }
        }
        Ok(())
    }
}

/// 由源路径推导输出路径：去掉旧量化后缀，拼上新的。
fn derive_output_path(src: &Path, quant: QuantType) -> PathBuf {
    let stem = src
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "model".to_string());

    // 去掉末尾的量化标记（含 `-` / `_` 分隔符的多种写法）
    let lower = stem.to_ascii_lowercase();
    let mut base = stem.clone();
    for q in QuantType::all() {
        for sep in ['-', '_', '.'] {
            let suffix = format!("{sep}{}", q.slug());
            if lower.ends_with(&suffix) {
                base = stem[..stem.len() - suffix.len()].to_string();
                break;
            }
        }
    }

    let name = format!("{base}-{}.gguf", quant.slug());
    match src.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.join(name),
        _ => PathBuf::from(name),
    }
}

/// 量化工具的位置与可用性。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuantizerProbe {
    /// 可执行文件路径（`None` 表示未找到）
    pub binary: Option<PathBuf>,
    /// 查找过的目录（便于用户排查「为什么没找到」）
    pub searched: Vec<PathBuf>,
}

impl QuantizerProbe {
    /// 是否可用。
    pub fn is_available(&self) -> bool {
        self.binary.is_some()
    }
}

/// 探测 `llama-quantize`。
///
/// 查找顺序与引擎一致：显式路径 → 可执行文件同级 `runtime/` → PATH。
/// 复用 `process::search_dirs` 而不是自己拼，避免两处逻辑分叉。
pub fn probe_quantizer(explicit: Option<&Path>) -> QuantizerProbe {
    let mut searched = Vec::new();

    if let Some(p) = explicit {
        searched.push(p.to_path_buf());
        if p.is_file() {
            return QuantizerProbe {
                binary: Some(p.to_path_buf()),
                searched,
            };
        }
    }

    let name = mangosteen_platform::quantizer_binary_name();
    let dirs = mangosteen_platform::process::search_dirs(&[]);
    for dir in &dirs {
        let cand = dir.join(name);
        searched.push(cand.clone());
        if cand.is_file() {
            return QuantizerProbe {
                binary: Some(cand),
                searched,
            };
        }
    }

    QuantizerProbe {
        binary: None,
        searched,
    }
}

/// 执行量化（**阻塞**，调用方须放后台线程）。
///
/// 为什么必须后台：7B 模型量化要几分钟到几十分钟，放在 HTTP 请求路径里
/// 浏览器会先超时。这与下载任务的教训是同一个（`pulljobs` 的由来）。
///
/// 返回值是产出文件路径与字节数。
pub fn run(req: &QuantizeRequest, binary: &Path) -> MnResult<(PathBuf, u64)> {
    req.validate()?;

    let out = std::process::Command::new(binary)
        .args(req.to_args())
        .output()
        .map_err(|e| MnError::EngineLaunch {
            binary: binary.to_path_buf(),
            reason: format!("无法启动量化工具：{e}"),
        })?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let tail: String = stderr
            .lines()
            .rev()
            .take(12)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
        return Err(MnError::EngineLaunch {
            binary: binary.to_path_buf(),
            reason: format!(
                "量化失败（退出码 {:?}）。常见原因：源文件不是 F16/F32（\
                 已量化的模型不能再量化）、磁盘空间不足。stderr 尾部：\n{tail}",
                out.status.code()
            ),
        });
    }

    let size = std::fs::metadata(&req.dst).map(|m| m.len()).unwrap_or(0);
    if size == 0 {
        return Err(MnError::EngineLaunch {
            binary: binary.to_path_buf(),
            reason: format!(
                "量化进程正常退出，但输出文件不存在或为空：{}",
                req.dst.display()
            ),
        });
    }

    Ok((req.dst.clone(), size))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 输出路径要去掉源文件已有的量化后缀，避免叠词。
    #[test]
    fn output_path_replaces_quant_suffix() {
        let r = QuantizeRequest::new("/m/qwen2.5-7b-instruct-q8_0.gguf", QuantType::Q4KM);
        assert_eq!(
            r.dst.file_name().unwrap().to_string_lossy(),
            "qwen2.5-7b-instruct-q4_k_m.gguf",
            "不应出现 q8_0-q4_k_m 这种叠词"
        );
        assert_eq!(r.dst.parent().unwrap().display().to_string(), "/m");
    }

    /// 源文件没有量化后缀时直接拼上。
    #[test]
    fn output_path_appends_when_no_suffix() {
        let r = QuantizeRequest::new("/m/model-f16.gguf", QuantType::Q5KM);
        assert_eq!(
            r.dst.file_name().unwrap().to_string_lossy(),
            "model-q5_k_m.gguf"
        );
    }

    /// 参数顺序与 llama.cpp 一致：`<src> <dst> <type> [threads]`。
    #[test]
    fn args_follow_llamacpp_order() {
        let mut r = QuantizeRequest::new("/m/a.gguf", QuantType::Q4_0);
        assert_eq!(
            r.to_args(),
            vec!["/m/a.gguf", "/m/a-q4_0.gguf", "Q4_0"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>()
        );
        r.threads = Some(8);
        assert_eq!(r.to_args().last().unwrap(), "8");
    }

    /// 输出路径与源相同必须拒绝——量化会覆盖原件。
    ///
    /// 这是**不可逆**的破坏，必须在发起前拦住。
    #[test]
    fn rejects_dst_equal_to_src() {
        let dir = std::env::temp_dir().join(format!("mn-quant-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("a.gguf");
        std::fs::write(&src, b"x").unwrap();

        let r = QuantizeRequest::new(&src, QuantType::Q4KM).with_dst(&src);
        let err = r.validate().expect_err("覆盖源文件必须拒绝");
        assert!(
            err.to_string().contains("永久丢失"),
            "错误要说清后果，实际：{err}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 源文件不存在时给出明确的「文件不存在」，而不是启动后失败。
    #[test]
    fn rejects_missing_src() {
        let r = QuantizeRequest::new("/nonexistent/nope.gguf", QuantType::Q4KM);
        let err = r.validate().expect_err("源文件不存在必须拒绝");
        assert!(matches!(err, MnError::ModelNotFound { .. }));
    }

    /// 档位解析容错：大小写、连字符、简写都要认。
    #[test]
    fn quant_type_parsing_is_tolerant() {
        for (input, want) in [
            ("q4_k_m", QuantType::Q4KM),
            ("Q4_K_M", QuantType::Q4KM),
            ("q4km", QuantType::Q4KM),
            ("q4-k-m", QuantType::Q4KM),
            ("Q8_0", QuantType::Q8_0),
            ("f16", QuantType::F16),
            ("FP16", QuantType::F16),
        ] {
            assert_eq!(QuantType::parse(input), Some(want), "解析 `{input}` 失败");
        }
        assert_eq!(QuantType::parse("不存在"), None);
    }

    /// 体积比例必须单调递增（Q4_0 < Q4_K_M < Q5_K_M < Q8_0 < F16）。
    ///
    /// 这个顺序是给用户的选档直觉，写反了会误导人以为 Q8 更省空间。
    #[test]
    fn size_ratio_is_monotonic() {
        let rs: Vec<f64> = QuantType::all().iter().map(|q| q.size_ratio()).collect();
        for w in rs.windows(2) {
            assert!(w[0] <= w[1], "体积比例必须递增：{rs:?}");
        }
        assert_eq!(QuantType::F16.size_ratio(), 1.0);
    }

    /// 探测结果必须可排查：报告搜过哪些位置，且报告的路径真实存在。
    ///
    /// 注意不能假设「本机没装 llama-quantize」——开发机往往装了
    /// llama.cpp（Homebrew / 官方包），量化工具就在 PATH 里。
    /// 所以这里只校验**不变式**，不校验「一定找不到」。
    #[test]
    fn probe_reports_searched_dirs() {
        let p = probe_quantizer(Some(Path::new("/definitely/not/here")));
        assert!(!p.searched.is_empty(), "必须给出搜过哪些位置");
        assert_eq!(
            p.searched[0],
            Path::new("/definitely/not/here"),
            "显式路径应排在搜索列表首位（便于排查「为什么没用我指定的」）"
        );
        if let Some(b) = &p.binary {
            assert!(b.is_file(), "报告的路径必须真实存在：{}", b.display());
        }
    }

    /// 显式指定的路径优先于 PATH。
    ///
    /// 用户可能装了多个 llama.cpp 版本，必须能用 `explicit` 锁定。
    #[test]
    fn explicit_path_wins() {
        let dir = std::env::temp_dir().join(format!("mn-quant-explicit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join(mangosteen_platform::quantizer_binary_name());
        std::fs::write(&fake, b"#!/bin/sh\n").unwrap();

        let p = probe_quantizer(Some(&fake));
        assert_eq!(p.binary.as_deref(), Some(fake.as_path()));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
