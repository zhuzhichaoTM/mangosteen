//! 模型源（远端仓库）的领域模型。
//!
//! 归属：**领域层**（零平台代码、零 IO）。
//!
//! ## 为什么需要这一层抽象
//!
//! 「从哪里下模型」是会变的：用户可能在 ModelScope（国内快）、HuggingFace
//! （全量）、或者公司内网的私有仓库。如果把 URL 拼装散落在下载实现里，
//! 新增一个源就要改下载逻辑，违反配置驱动原则。
//!
//! 因此这里只定义**怎么描述一个源**，以及**怎么把 (源, 仓库, 文件) 解析成
//! 具体下载地址**。真正的字节搬运留给基础能力层。
//!
//! ## 参照 Ollama 的做法
//!
//! Ollama 用 `<registry>/<namespace>/<model>:<tag>` 定位模型，把「标识」
//! 与「传输」解耦。Mangosteen 同样用一个可解析的标识
//! `源:仓库/文件` 来表达，例如：
//!
//! ```text
//! modelscope:Qwen/Qwen2.5-0.5B-Instruct-GGUF/qwen2.5-0.5b-instruct-q4_k_m.gguf
//! hf:Qwen/Qwen2.5-0.5B-Instruct-GGUF/qwen2.5-0.5b-instruct-q4_k_m.gguf
//! ```
//!
//! 省略源时按配置的默认源解析（见 [`ModelSourceRegistry::default_source`]）。

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::{MnError, MnResult};
use crate::model::Quantization;

/// 一个可下载的模型文件标识。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRef {
    /// 源名（小写，如 `modelscope` / `hf`）
    pub source: String,
    /// 仓库标识，如 `Qwen/Qwen2.5-0.5B-Instruct-GGUF`
    pub repo: String,
    /// 仓库内文件名，如 `qwen2.5-0.5b-instruct-q4_k_m.gguf`
    pub file: String,
    /// 分支 / 修订，`None` 表示源默认（通常 `main` / `master`）
    pub revision: Option<String>,
}

impl ModelRef {
    /// 构造。
    pub fn new(
        source: impl Into<String>,
        repo: impl Into<String>,
        file: impl Into<String>,
    ) -> Self {
        Self {
            source: source.into(),
            repo: repo.into(),
            file: file.into(),
            revision: None,
        }
    }

    /// 指定修订。
    pub fn with_revision(mut self, rev: impl Into<String>) -> Self {
        self.revision = Some(rev.into());
        self
    }

    /// 从文件名推断量化档位。
    ///
    /// GGUF 文件名约定是 `<模型>-<量化>.gguf`，量化段由 llama.cpp 的
    /// 命名规范决定（`Q4_K_M` / `q4_k_m` / `IQ4_XS` 等）。
    ///
    /// 推断不出来返回 `None`——**不要猜**。错误猜测会让资源评估基于
    /// 错误的位宽做判断，宁可让用户显式给（或按文件大小反推）。
    pub fn quant_from_filename(&self) -> Option<Quantization> {
        let stem = self
            .file
            .rsplit('/')
            .next()
            .unwrap_or(&self.file)
            .trim_end_matches(".gguf");
        // 从后往前找第一段能被 Quantization 解析的 token
        let mut parts: Vec<&str> = stem.split(&['-', '.'][..]).collect();
        while let Some(tok) = parts.pop() {
            if let Some(q) = parse_quant_token(tok) {
                return Some(q);
            }
            // 最多回退 3 段（形如 `Q4_K_M` 会被拆成 3 段）
            if stem.len() - parts.join("-").len() > 24 {
                break;
            }
        }
        None
    }

    /// 推导本地落盘文件名。
    ///
    /// 取仓库内文件名的最后一段：避免把 `Qwen/Qwen2.5-...-GGUF` 这种
    /// 目录结构带进本地路径。
    pub fn local_file_name(&self) -> String {
        self.file
            .rsplit('/')
            .next()
            .unwrap_or(&self.file)
            .to_string()
    }

    /// 解析标识字符串。
    ///
    /// 支持两种写法：
    /// * `源:仓库/文件[@修订]`
    /// * `仓库/文件[@修订]`（源取 `default_source`）
    ///
    /// 注意「仓库」必须恰好两段（`owner/name`）。少于两段说明用户漏了
    /// owner，报错比猜测友好。
    pub fn parse(spec: &str, default_source: &str) -> MnResult<Self> {
        let spec = spec.trim();
        if spec.is_empty() {
            return Err(MnError::bad_request("模型标识为空"));
        }

        // 源前缀只在「第一个 / 之前出现冒号」时才认，避免 Windows 盘符误判
        let (source, rest) = match spec.split_once(':') {
            Some((s, r)) if !s.contains('/') && !s.contains('\\') && s.len() < 32 => {
                if !s
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                {
                    return Err(MnError::bad_request(format!(
                        "模型源名 `{s}` 含非法字符（只允许字母、数字、下划线、连字符）"
                    )));
                }
                (s.to_ascii_lowercase(), r)
            }
            _ => (default_source.to_ascii_lowercase(), spec),
        };

        let rest = rest.trim_start_matches('/');

        // 修订后缀 `@rev`（模型名本身不会含 `@`）
        let (rest, revision) = match rest.rsplit_once('@') {
            Some((body, rev)) if !rev.is_empty() && !rev.contains('/') => {
                (body, Some(rev.to_string()))
            }
            _ => (rest, None),
        };

        // 结构判定优先于扩展名判定：`<owner>/<repo>/<file>` 至少三段。
        // 段数不足时一定是「漏了文件名」，此时若先判扩展名，
        // `Qwen/Qwen2.5-0.5B-Instruct-GGUF` 会因为模型名里的 `.` 被误报成
        // 「不是 GGUF 文件」，把用户引向错误的排查方向。
        let segs: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
        if segs.len() < 3 {
            return Err(MnError::bad_request(format!(
                "模型标识 `{spec}` 缺少文件名。正确格式：\
                 `<源>:<owner>/<仓库>/<文件.gguf>`，例如 \
                 `modelscope:Qwen/Qwen2.5-0.5B-Instruct-GGUF/qwen2.5-0.5b-instruct-q4_k_m.gguf`"
            )));
        }

        let file = *segs.last().expect("len>=3");
        let repo = segs[..segs.len() - 1].join("/");

        if !file.to_ascii_lowercase().ends_with(".gguf") {
            return Err(MnError::bad_request(format!(
                "文件名 `{file}` 不是 .gguf 格式。按「模型格式统一」约束，\
                 Mangosteen 只接受 GGUF"
            )));
        }

        if repo.trim().is_empty() {
            return Err(MnError::bad_request(format!(
                "模型标识 `{spec}` 缺少 owner/仓库名。正确格式：`<owner>/<仓库>/<文件.gguf>`"
            )));
        }

        Ok(Self {
            source,
            repo,
            file: file.to_string(),
            revision,
        })
    }

    /// 反解为标识字符串。
    pub fn to_spec(&self) -> String {
        let mut s = format!("{}:{}/{}", self.source, self.repo, self.file);
        if let Some(r) = &self.revision {
            s.push('@');
            s.push_str(r);
        }
        s
    }
}

impl fmt::Display for ModelRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_spec())
    }
}

/// 把文件名里的量化 token 解析为枚举。
///
/// 单独成函数便于单测覆盖各种命名变体（`Q4_K_M` / `q4_k_m` / `Q4KM`）。
fn parse_quant_token(tok: &str) -> Option<Quantization> {
    let t = tok.to_ascii_lowercase();
    Some(match t.as_str() {
        "f32" => Quantization::F32,
        "f16" | "fp16" => Quantization::F16,
        "bf16" => Quantization::Bf16,
        "q8_0" | "q80" => Quantization::Q8_0,
        "q6_k" | "q6k" => Quantization::Q6K,
        "q5_k_m" | "q5km" => Quantization::Q5KM,
        "q5_k_s" | "q5ks" => Quantization::Q5KS,
        "q5_1" | "q51" => Quantization::Q5_1,
        "q5_0" | "q50" => Quantization::Q5_0,
        "q4_k_m" | "q4km" => Quantization::Q4KM,
        "q4_k_s" | "q4ks" => Quantization::Q4KS,
        "q4_1" | "q41" => Quantization::Q4_1,
        "q4_0" | "q40" => Quantization::Q4_0,
        "q3_k_l" | "q3kl" => Quantization::Q3KL,
        "q3_k_m" | "q3km" => Quantization::Q3KM,
        "q3_k_s" | "q3ks" => Quantization::Q3KS,
        "q2_k" | "q2k" => Quantization::Q2K,
        "iq1_s" | "iq1s" => Quantization::Iq1S,
        "iq2_xxs" | "iq2xxs" => Quantization::Iq2Xxs,
        "iq2_xs" | "iq2xs" => Quantization::Iq2Xs,
        "iq2_s" | "iq2s" => Quantization::Iq2S,
        "iq3_xxs" | "iq3xxs" => Quantization::Iq3Xxs,
        "iq3_xs" | "iq3xs" => Quantization::Iq3Xs,
        "iq3_s" | "iq3s" => Quantization::Iq3S,
        "iq4_xs" | "iq4xs" => Quantization::Iq4Xs,
        "iq4_nl" | "iq4nl" => Quantization::Iq4Nl,
        _ => return None,
    })
}

/// 模型源的传输形态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceKind {
    /// ModelScope：`/models/<repo>/resolve/<rev>/<file>`
    ModelScope,
    /// HuggingFace：`/<repo>/resolve/<rev>/<file>`
    HuggingFace,
    /// 自定义：用 `url_template` 拼装
    Custom,
}

/// 一个模型源的定义。
///
/// 全部字段都可配置，符合「配置驱动原则」：源改版 / 新增源不需要改代码。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelSource {
    /// 源名（小写）
    pub name: String,
    /// 显示名
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// 传输形态
    pub kind: SourceKind,
    /// 基础地址（不含尾斜杠），如 `https://www.modelscope.cn`
    pub base_url: String,
    /// 默认修订
    #[serde(default)]
    pub default_revision: String,
    /// 自定义源的 URL 模板；可用占位符 `{repo}` `{file}` `{revision}`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url_template: Option<String>,
    /// 元数据接口模板（可选）。用于取官方 SHA-256 与真实大小。
    /// 占位符同上。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata_url_template: Option<String>,
    /// 仓库搜索接口模板（可选）。占位符 `{base}` `{query}` `{limit}`。
    ///
    /// `None` 表示该源不支持「按关键字找仓库」——此时 CLI/UI 应只提供
    /// 「已知仓库路径列出文件」的模式，而不是报一个含糊的错误。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_url_template: Option<String>,
}

impl ModelSource {
    /// ModelScope 内置定义。
    pub fn modelscope() -> Self {
        Self {
            name: "modelscope".into(),
            display_name: Some("ModelScope 魔搭".into()),
            kind: SourceKind::ModelScope,
            base_url: "https://www.modelscope.cn".into(),
            default_revision: "master".into(),
            url_template: None,
            metadata_url_template: Some(
                "{base}/api/v1/models/{repo}/repo/files?Recursive=true".into(),
            ),
            // ModelScope 的仓库检索接口未公开（页面前端走内部网关，
            // 多个候选路径实测均 404）。这里显式声明不支持，
            // 让上层能给出「请改用 hf-mirror 搜索，或直接填仓库路径」的准确提示。
            search_url_template: None,
        }
    }

    /// HuggingFace 国内镜像。
    ///
    /// 为什么单独做一个源：`huggingface.co` 在国内多数网络下不可达
    /// （实测直连超时），而 `hf-mirror.com` 提供**完全相同**的 API 形态
    /// （`/api/models?search=`、`/api/models/<repo>?blobs=true`、`/<repo>/resolve/...`），
    /// 且支持按关键字搜索仓库。它是当前唯一「可搜索 + 可下载 + 国内直连」的源。
    pub fn hf_mirror() -> Self {
        Self {
            name: "hf-mirror".into(),
            display_name: Some("HuggingFace 镜像（可搜索）".into()),
            kind: SourceKind::HuggingFace,
            base_url: "https://hf-mirror.com".into(),
            default_revision: "main".into(),
            url_template: None,
            metadata_url_template: Some("{base}/api/models/{repo}?blobs=true".into()),
            search_url_template: Some(
                "{base}/api/models?search={query}&limit={limit}&offset={offset}&full=false".into(),
            ),
        }
    }

    /// HuggingFace 内置定义。
    pub fn huggingface() -> Self {
        Self {
            name: "hf".into(),
            display_name: Some("HuggingFace".into()),
            kind: SourceKind::HuggingFace,
            base_url: "https://huggingface.co".into(),
            default_revision: "main".into(),
            url_template: None,
            metadata_url_template: Some("{base}/api/models/{repo}?blobs=true".into()),
            search_url_template: Some(
                "{base}/api/models?search={query}&limit={limit}&offset={offset}&full=false".into(),
            ),
        }
    }

    /// 实际生效的修订号。
    pub fn revision_for(&self, r: &ModelRef) -> String {
        r.revision
            .clone()
            .unwrap_or_else(|| self.default_revision.clone())
    }

    /// 拼出下载地址。
    pub fn download_url(&self, r: &ModelRef) -> MnResult<String> {
        let rev = self.revision_for(r);
        let url = match self.kind {
            SourceKind::ModelScope => format!(
                "{}/models/{}/resolve/{}/{}",
                self.base_url.trim_end_matches('/'),
                r.repo,
                rev,
                r.file
            ),
            SourceKind::HuggingFace => format!(
                "{}/{}/resolve/{}/{}",
                self.base_url.trim_end_matches('/'),
                r.repo,
                rev,
                r.file
            ),
            SourceKind::Custom => {
                let tpl = self.url_template.as_ref().ok_or_else(|| {
                    MnError::config(
                        format!("model_sources.{}.url_template", self.name),
                        "自定义源必须提供 url_template",
                    )
                })?;
                tpl.replace("{base}", self.base_url.trim_end_matches('/'))
                    .replace("{repo}", &r.repo)
                    .replace("{file}", &r.file)
                    .replace("{revision}", &rev)
            }
        };
        if !url.starts_with("https://") && !url.starts_with("http://") {
            return Err(MnError::config(
                format!("model_sources.{}", self.name),
                format!("拼出的地址不是 HTTP(S)：{url}"),
            ));
        }
        Ok(url)
    }

    /// 校验。
    pub fn validate(&self) -> MnResult<()> {
        if self.name.trim().is_empty() {
            return Err(MnError::config("model_sources.<name>", "源名不能为空"));
        }
        if self.base_url.trim().is_empty() {
            return Err(MnError::config(
                format!("model_sources.{}.base_url", self.name),
                "不能为空",
            ));
        }
        if self.kind == SourceKind::Custom && self.url_template.is_none() {
            return Err(MnError::config(
                format!("model_sources.{}.url_template", self.name),
                "自定义源必须提供 url_template",
            ));
        }
        Ok(())
    }
}

/// 模型源注册表。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSourceRegistry {
    sources: Vec<ModelSource>,
    default_source: String,
}

impl ModelSourceRegistry {
    /// 用内置源构造（ModelScope 为默认）。
    pub fn builtin() -> Self {
        Self {
            sources: vec![
                ModelSource::modelscope(),
                ModelSource::hf_mirror(),
                ModelSource::huggingface(),
            ],
            default_source: "modelscope".into(),
        }
    }

    /// 构造。
    pub fn new(sources: Vec<ModelSource>, default_source: impl Into<String>) -> MnResult<Self> {
        if sources.is_empty() {
            return Err(MnError::config("model_sources", "至少需要一个模型源"));
        }
        for s in &sources {
            s.validate()?;
        }
        let default_source = default_source.into().to_ascii_lowercase();
        if !sources.iter().any(|s| s.name == default_source) {
            return Err(MnError::config(
                "storage.default_source",
                format!(
                    "默认源 `{default_source}` 未定义。可用：{}",
                    sources
                        .iter()
                        .map(|s| s.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ));
        }
        Ok(Self {
            sources,
            default_source,
        })
    }

    /// 默认源名。
    pub fn default_source(&self) -> &str {
        &self.default_source
    }

    /// 按名查找。
    pub fn get(&self, name: &str) -> Option<&ModelSource> {
        let n = name.to_ascii_lowercase();
        self.sources.iter().find(|s| s.name == n)
    }

    /// 全部源。
    pub fn all(&self) -> &[ModelSource] {
        &self.sources
    }

    /// 解析标识并取出对应源。
    pub fn resolve(&self, spec: &str) -> MnResult<(ModelSource, ModelRef)> {
        let r = ModelRef::parse(spec, &self.default_source)?;
        let src = self.get(&r.source).ok_or_else(|| {
            MnError::bad_request(format!(
                "未知模型源 `{}`。可用：{}",
                r.source,
                self.sources
                    .iter()
                    .map(|s| s.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })?;
        Ok((src.clone(), r))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_full_spec() {
        let r = ModelRef::parse(
            "modelscope:Qwen/Qwen2.5-0.5B-Instruct-GGUF/qwen2.5-0.5b-instruct-q4_k_m.gguf",
            "hf",
        )
        .unwrap();
        assert_eq!(r.source, "modelscope");
        assert_eq!(r.repo, "Qwen/Qwen2.5-0.5B-Instruct-GGUF");
        assert_eq!(r.file, "qwen2.5-0.5b-instruct-q4_k_m.gguf");
    }

    #[test]
    fn parse_uses_default_source_when_absent() {
        let r = ModelRef::parse(
            "Qwen/Qwen2.5-0.5B-Instruct-GGUF/qwen2.5-0.5b-instruct-q4_k_m.gguf",
            "modelscope",
        )
        .unwrap();
        assert_eq!(r.source, "modelscope");
    }

    #[test]
    fn parse_rejects_non_gguf() {
        let e = ModelRef::parse("Qwen/repo/model.safetensors", "modelscope").unwrap_err();
        assert!(e.to_string().contains("GGUF"), "got {e}");
    }

    #[test]
    fn parse_rejects_missing_file() {
        let e = ModelRef::parse("Qwen/Qwen2.5-0.5B-Instruct-GGUF", "modelscope").unwrap_err();
        assert!(e.to_string().contains("缺少文件名"), "got {e}");

        // 只写 owner 一段：也没有文件名
        let e2 = ModelRef::parse("Qwen", "modelscope").unwrap_err();
        assert!(!e2.to_string().is_empty(), "got {e2}");
    }

    #[test]
    fn parse_handles_subdir_file() {
        let r = ModelRef::parse("Qwen/repo/sub/q4_k_m.gguf", "modelscope").unwrap();
        assert_eq!(r.repo, "Qwen/repo/sub");
        assert_eq!(r.file, "q4_k_m.gguf");
    }

    #[test]
    fn source_name_must_be_plain_ascii() {
        let e = ModelRef::parse("model scope:Repo/x.gguf", "hf").unwrap_err();
        // 含空格 → 不被当作源前缀，整体作为 repo 解析 → 缺文件名
        assert!(!e.to_string().is_empty());
    }

    #[test]
    fn quant_from_filename_covers_common_forms() {
        let cases = [
            ("qwen2.5-0.5b-instruct-q4_k_m.gguf", Quantization::Q4KM),
            ("model-Q8_0.gguf", Quantization::Q8_0),
            ("model-q5_k_s.gguf", Quantization::Q5KS),
            ("model-iq4_xs.gguf", Quantization::Iq4Xs),
            ("model-f16.gguf", Quantization::F16),
            ("model-q6_k.gguf", Quantization::Q6K),
        ];
        for (file, want) in cases {
            let r = ModelRef::new("modelscope", "o/r", file);
            assert_eq!(r.quant_from_filename(), Some(want), "文件名 {file}");
        }
    }

    #[test]
    fn quant_from_filename_returns_none_when_unknown() {
        // 不猜：认不出来就返回 None，由调用方按文件大小反推或要求显式指定
        let r = ModelRef::new("modelscope", "o/r", "mystery-model.gguf");
        assert_eq!(r.quant_from_filename(), None);
    }

    #[test]
    fn local_file_name_strips_dirs() {
        let r = ModelRef::new("modelscope", "o/r", "sub/dir/model.gguf");
        assert_eq!(r.local_file_name(), "model.gguf");
    }

    #[test]
    fn modelscope_url_is_correct() {
        let s = ModelSource::modelscope();
        let r = ModelRef::new(
            "modelscope",
            "Qwen/Qwen2.5-0.5B-Instruct-GGUF",
            "qwen2.5-0.5b-instruct-q4_k_m.gguf",
        );
        let u = s.download_url(&r).unwrap();
        assert_eq!(
            u,
            "https://www.modelscope.cn/models/Qwen/Qwen2.5-0.5B-Instruct-GGUF/resolve/master/qwen2.5-0.5b-instruct-q4_k_m.gguf"
        );
    }

    #[test]
    fn hf_url_uses_main_revision() {
        let s = ModelSource::huggingface();
        let r = ModelRef::new("hf", "Qwen/Qwen2.5-0.5B-Instruct-GGUF", "a.gguf");
        let u = s.download_url(&r).unwrap();
        assert_eq!(
            u,
            "https://huggingface.co/Qwen/Qwen2.5-0.5B-Instruct-GGUF/resolve/main/a.gguf"
        );
    }

    #[test]
    fn custom_source_requires_template() {
        let mut s = ModelSource::modelscope();
        s.kind = SourceKind::Custom;
        s.url_template = None;
        assert!(s.validate().is_err());

        s.url_template = Some("{base}/dl/{repo}/{file}".into());
        assert!(s.validate().is_ok());
        let r = ModelRef::new("modelscope", "o/r", "a.gguf");
        assert_eq!(
            s.download_url(&r).unwrap(),
            "https://www.modelscope.cn/dl/o/r/a.gguf"
        );
    }

    #[test]
    fn registry_rejects_unknown_default() {
        assert!(ModelSourceRegistry::new(vec![ModelSource::modelscope()], "nope").is_err());
    }

    #[test]
    fn registry_resolves_shorthand() {
        let reg = ModelSourceRegistry::builtin();
        let (src, r) = reg.resolve("Qwen/repo/a.gguf").unwrap();
        assert_eq!(src.name, "modelscope");
        assert_eq!(r.repo, "Qwen/repo");
    }

    #[test]
    fn registry_rejects_unknown_source() {
        let reg = ModelSourceRegistry::builtin();
        let e = reg.resolve("nope:o/r/a.gguf").unwrap_err();
        assert!(e.to_string().contains("未知模型源"), "got {e}");
    }

    #[test]
    fn roundtrip_spec_preserves_revision() {
        let r = ModelRef::new("hf", "o/r", "a.gguf").with_revision("v1");
        assert_eq!(r.to_spec(), "hf:o/r/a.gguf@v1");
        let p = ModelRef::parse(&r.to_spec(), "modelscope").unwrap();
        assert_eq!(p.source, "hf");
        assert_eq!(p.repo, "o/r");
        assert_eq!(p.file, "a.gguf");
        assert_eq!(p.revision.as_deref(), Some("v1"), "@rev 后缀必须被解析");
    }
}
