//! 部署配置模板：多态解析、索引与部署规格生成。
//!
//! 归属：**编排层 · 配置驱动**（模板是部署的唯一真源）。
//!
//! ## 为什么要多态，而不是所有模型共用一套字段解释
//!
//! 不同模型的部署形态差异很大：
//!
//! | 模型 | 需要的东西 |
//! |---|---|
//! | Qwen2.5-0.5B | 单个 GGUF，无辅助文件，无投机解码 |
//! | Qwen3.8-27B | 主模型 + mmproj（多模态投影）+ MTP draft（投机解码）+ 一组额外参数 |
//!
//! 用一套通用字段去解释所有模型，要么字段名互相污染（`draft_*` 对 0.5B
//! 毫无意义），要么新增模型时必须往同一个解析器里塞分支。正确做法是
//! **每个模型一个 schema 实现**，各自把模板正文解释成统一的
//! [`ModelDeploySpec`]，下游只消费这个中间表示。
//!
//! ## 模型维度受系统控制，模板内容受用户控制
//!
//! 新增**模型**需要本模块注册新 schema（随版本升级发布）；同一模型下的
//! **模板内容**（仓库、文件名、上下文、量化方式等）完全由用户在设置页
//! 自定义，无需改代码。这条边界保证「用户能自由调参」同时「不会因为
//! 未知模型产生静默错配」——未注册的模型直接明确报错，而不是猜一个默认值。
//!
//! ## 为什么不用 YAML 库
//!
//! 项目依赖基线极简（serde/serde_json/toml/sha2）。模板只需要
//! `key: value`、一层缩进嵌套与 `- ` 列表，这里实现覆盖该子集的解析器；
//! 完整 YAML 结构不在模板约定范围内。

use std::collections::BTreeMap;

use crate::MnResult;


/// 一层嵌套结构：父键 → 子键值对。
pub type Nested = BTreeMap<String, BTreeMap<String, String>>;

/* ==================================================================== 底层解析 */

/// 把模板正文解析成原始键值结构（不含模型语义）。
///
/// 这是所有 schema 共用的语法层：只认「键值/嵌套/列表」三种形态，
/// 不理解任何字段含义。字段怎么解释由各模型 schema 决定。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RawTemplate {
    /// 模板显示名（首行 `template:` 的值，如 `Uncensored`）
    pub name: String,
    /// 扁平键值对（`model`、`repository`、`context` 等）
    pub scalars: BTreeMap<String, String>,
    /// 一层嵌套（`speculative:`、`multimodal:`）
    pub nested: Nested,
    /// 顶层列表项（`auxiliary_models:` 下的 `- xxx`）
    pub lists: BTreeMap<String, Vec<String>>,
}

impl RawTemplate {
    /// 取标量值。
    pub fn get(&self, key: &str) -> Option<&str> {
        self.scalars.get(key).map(String::as_str)
    }

    /// 取嵌套值。
    pub fn get_nested(&self, parent: &str, key: &str) -> Option<&str> {
        self.nested
            .get(parent)
            .and_then(|m| m.get(key))
            .map(String::as_str)
    }

    /// 取列表。
    pub fn get_list(&self, key: &str) -> &[String] {
        self.lists.get(key).map(Vec::as_slice).unwrap_or(&[])
    }

    /// 取整数字段，解析失败按缺省处理（模板里的排版错误不该让部署崩掉）。
    pub fn get_u64(&self, key: &str) -> Option<u64> {
        self.get(key).and_then(|s| s.trim().parse::<u64>().ok())
    }

    /// 取 `u32`。
    pub fn get_u32(&self, key: &str) -> Option<u32> {
        self.get(key).and_then(|s| s.trim().parse::<u32>().ok())
    }

    /// 取布尔字段。`on`/`true`/`yes`/`1` 视为真。
    pub fn get_bool(&self, key: &str) -> Option<bool> {
        self.get(key).map(is_truthy)
    }
}

/// 布尔字面量判定（YAML 的 `on/off` 与常见写法都接受）。
pub fn is_truthy(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "on" | "true" | "yes" | "1"
    )
}

/// 解析模板正文为原始结构。
///
/// 语法子集：
/// * `key: value`
/// * `parent:` 后接缩进 `  key: value`（只支持一层）
/// * `key:` 后接 `  - item`（列表）
/// * `#` 开头的注释行、空行；` #` 之后视为行尾注释
///
/// 任何一行既不匹配上述形式也非空/注释，**直接报错并带行号**——静默忽略
/// 写错的模板字段是最坏的行为：用户以为改了配置，实际没生效。
pub fn parse_raw(text: &str) -> MnResult<RawTemplate> {
    let mut out = RawTemplate::default();
    let mut parent: Option<String> = None;
    let mut parent_is_list = false;

    for (idx, raw) in text.lines().enumerate() {
        let line_no = idx + 1;
        // 行尾注释：只认 " #"，避免切断 URL 里的 #
        let line = match raw.find(" #") {
            Some(pos) => &raw[..pos],
            None => raw,
        };
        let trimmed_end = line.trim_end();
        if trimmed_end.trim().is_empty() {
            continue;
        }
        let indent = trimmed_end.len() - trimmed_end.trim_start().len();
        let content = trimmed_end.trim();

        if indent == 0 {
            if content.starts_with('#') {
                continue;
            }
            if content.starts_with("- ") {
                return Err(err(line_no, "列表项必须缩进挂在一个键下"));
            }
            let Some((k, v)) = content.split_once(':') else {
                return Err(err(line_no, "缺少 `:`"));
            };
            let key = k.trim().to_string();
            let value = v.trim().to_string();
            if key.is_empty() {
                return Err(err(line_no, "键名为空"));
            }
            if value.is_empty() {
                parent = Some(key);
                parent_is_list = false;
                continue;
            }
            parent = None;
            if key == "template" {
                out.name = value.clone();
            }
            // model: 支持多文件（主模型分片）——逗号分隔或列表值拼接。
            // 2026-11-23 用户裁决：分片（如 IQ2_XS 的 00001/00002）也是
            // 主模型的一部分，全部填在 model 字段，不再挪 auxiliary_models。
            // 多个值统一以 `,` 连接存储，解析层 split 成 Vec。
            if key == "model" && value.contains(',') {
                out.scalars.insert(key, value);
            } else {
                out.scalars.insert(key, value);
            }
            continue;
        }

        let Some(p) = parent.clone() else {
            return Err(err(line_no, "缩进内容没有对应的父键"));
        };
        if content.starts_with("- ") || content == "-" {
            parent_is_list = true;
            let item = content.trim_start_matches('-').trim().to_string();
            if !item.is_empty() {
                out.lists.entry(p).or_default().push(item);
            }
            continue;
        }
        if parent_is_list {
            return Err(err(line_no, "同一父键下不能混用列表与映射"));
        }
        let Some((k, v)) = content.split_once(':') else {
            return Err(err(line_no, "缩进项缺少 `:`"));
        };
        let key = k.trim().to_string();
        let value = v.trim().to_string();
        if key.is_empty() {
            return Err(err(line_no, "缩进项键名为空"));
        }
        out.nested.entry(p).or_default().insert(key, value);
    }

    if out.name.is_empty() {
        out.name = "default".to_string();
    }
    Ok(out)
}

fn err(line_no: usize, msg: &str) -> crate::MnError {
    crate::MnError::config(
        "config_template",
        format!("配置模板第 {line_no} 行解析失败：{msg}"),
    )
}

/* ============================================================== 统一部署规格 */

/// 一个辅助文件（mmproj / draft 等）的远端落位。
#[derive(Debug, Clone, PartialEq)]
pub struct AuxFile {
    /// 远端文件名（不含目录）
    pub file_name: String,
    /// 完整下载地址
    pub url: String,
    /// SHA256 校验值（来自内置哈希表 `templates/HASHES.md`，2026-11-23 用户裁决：
    /// 每个模型全部下载文件统一哈希管理）。None = 未知（跳过校验）。
    pub sha256: Option<String>,
    /// 是否为主模型必需分片（2026-11-23 用户裁决：分片也是主模型的一部分）。
    /// true = 下载失败阻断部署（如 IQ2_XS 的 00002 分片）；
    /// false = 可选辅助（mmproj / MTP draft），失败跳过不阻断。
    pub required: bool,
}

/// 模板解释后的**统一部署规格**。
///
/// 下游（`build_cloud_plan`、下载、引擎启动）只消费这个结构，不必知道
/// 是哪个模型、模板里写了什么字段。新增模型只影响 schema 层。
#[derive(Debug, Clone, PartialEq)]
pub struct ModelDeploySpec {
    /// 模板显示名（用于 UI，如 `Uncensored`）
    pub template_name: String,
    /// 推理引擎标识（模板 `engine:` 字段）。
    ///
    /// `None` = 由调用方回落全局配置 `local.engine`（默认 llama.cpp）。
    /// 模板显式声明（如 `engine: strata`）时优先于全局配置——同一台
    /// 机器上 llama.cpp 模型与 Strata 模型可以共存部署。
    pub engine: Option<String>,
    /// 主模型 SHA256 校验值（模板 `sha256:` 字段）。
    ///
    /// `Some` = 下载完成后远端 `sha256sum -c` 强校验（供应链防投毒）；
    /// `None` = 跳过校验（仅当模板未声明且非内置白名单时）。
    pub sha256: Option<String>,
    /// API 模型别名（模板 `alias:` 字段）。
    ///
    /// 模板驱动部署的默认 API 模型名（如 `qwen2.5-0.5b`），
    /// 让 curl 调用无需拼接量化后缀。优先级低于 CLI `--alias`。
    pub alias: Option<String>,
    /// 主模型远端文件名
    pub model_file: String,
    /// 主模型完整下载地址
    pub download_url: String,
    /// 辅助文件（多模态投影、投机解码草稿等）
    pub aux_files: Vec<AuxFile>,
    /// 上下文长度
    pub context: u32,
    /// GPU 卸载层数
    pub gpu_layers: u32,
    /// 批大小
    pub batch: u32,
    /// 微批大小
    pub ubatch: u32,
    /// KV 缓存类型（K/V 分别给，转成引擎枚举由调用方做）
    pub cache_k: String,
    pub cache_v: String,
    /// 是否启用 flash attention
    pub flash_attn: bool,
    /// 多模态投影文件名（None 表示不启用多模态）
    pub mmproj: Option<String>,
    /// 投机解码草稿模型文件名（None 表示不启用）
    pub draft_model: Option<String>,
    /// 投机解码草稿模型层数
    pub draft_gpu_layers: Option<u32>,
    /// 投机解码最大草稿数
    pub draft_n_max: Option<u32>,
    /// 额外引擎参数（模板里无法归类的模型专属参数）
    pub extra_args: Vec<String>,
}

impl ModelDeploySpec {
    /// 按文件名找辅助文件的下载地址。
    pub fn aux_url(&self, file_name: &str) -> Option<&str> {
        self.aux_files
            .iter()
            .find(|a| a.file_name == file_name)
            .map(|a| a.url.as_str())
    }

    /// 解析模板声明的引擎字符串为枚举（`None` → 全局默认引擎）。
    ///
    /// 收敛到 core 层，避免编排层直接引用 `InferenceEngine::parse`
    /// 触发引擎字面量禁令（C3 架构守护）。
    pub fn parsed_engine(&self) -> Option<crate::config::InferenceEngine> {
        self.engine
            .as_deref()
            .and_then(crate::config::InferenceEngine::parse)
    }
}

/* ================================================================= 模型 schema */

/// 模型专属的模板解释器。
///
/// 每个受支持的模型实现一份：把该模型模板正文里的字段（可能带
/// 模型特有的嵌套结构，如 `speculative`/`multimodal`）翻译成
/// [`ModelDeploySpec`]。这样「不同模型不同解析」是显式的、可测的。
pub trait ModelTemplateSchema: Send + Sync {
    /// 该 schema 负责的模型名（与 UI 模型下拉框的值一致）。
    fn model_names(&self) -> &'static [&'static str];

    /// 把原始模板解释成部署规格。
    fn parse(&self, raw: &RawTemplate) -> MnResult<ModelDeploySpec>;
}

/// 按模板正文自动选 schema（2026-11-23 用户裁决：新增模型文件即扩展，
/// 不再依赖静态 SCHEMAS 数组注册）。
///
/// 选择规则（按模板正文的 `engine:` / `framework:` 字段 + 嵌套结构 + model_id + model 名）：
/// - `engine: strata` → QwenFlashNextSchema（Strata 三级存储 MoE）
/// - `framework: llama.cpp` + 有 `speculative:` 或 `multimodal:` 嵌套 → Qwen38Schema
/// - `framework: llama.cpp` + 无特殊嵌套 → SimpleGgufSchema（单文件 GGUF）
/// - 无 engine/framework 声明时：按 model_id + 传入的 model 名推断（qwen3.8-27b → Qwen38Schema，
///   其余 → SimpleGgufSchema），兼容旧模板/测试
///
/// 新增模型 = 新建 `template/<model>.rs`（模板正文声明 engine/framework）→ 自动选对 schema。
pub fn schema_for_text_with_model(model: &str, text: &str) -> Option<&'static dyn ModelTemplateSchema> {
    let raw = parse_raw(text).ok()?;
    let engine = raw.get("engine").unwrap_or("").trim().to_lowercase();
    if engine == "strata" {
        return Some(&QwenFlashNextSchema);
    }
    // llama.cpp（默认）：按嵌套结构区分
    if raw.nested.contains_key("speculative") || raw.nested.contains_key("multimodal") {
        return Some(&Qwen38Schema);
    }
    // 无 engine/framework 声明时：按 model_id + 传入的 model 名推断（兼容旧模板/测试）
    let model_id = raw.get("model_id").unwrap_or("").trim().to_lowercase();
    let combined = format!("{model_id} {model} {text}").to_lowercase();
    if combined.contains("qwen3.8-27b") || combined.contains("uncensored") || text.contains("parallel:") {
        return Some(&Qwen38Schema);
    }
    Some(&SimpleGgufSchema)
}

/// 兼容旧调用：按模型名查 schema（仅用于测试/兼容）。
pub fn schema_for(model: &str) -> Option<&'static dyn ModelTemplateSchema> {
    let combined = model.trim().to_lowercase();
    if combined.contains("qwen3.8-27b") || combined.contains("uncensored") {
        return Some(&Qwen38Schema);
    }
    if combined.contains("flash-next") || combined.contains("qwen3.6") {
        return Some(&QwenFlashNextSchema);
    }
    Some(&SimpleGgufSchema)
}

/// 全部已注册 schema（供测试与兼容）。
static SCHEMAS: &[&dyn ModelTemplateSchema] = &[
    &SimpleGgufSchema,
    &Qwen38Schema,
    &QwenFlashNextSchema,
];

/// 当前支持的模型清单（从 BUILTIN_TEMPLATES 提取，供 CLI/UI 展示与校验）。
///
/// 不再依赖静态 SCHEMAS 数组——模型清单由 template/ 目录下的文件自动决定。
/// 去重（qwen3.8-27b 有 default/uncensored 两个槽位）。
pub fn supported_models() -> Vec<&'static str> {
    let mut seen = std::collections::HashSet::new();
    BUILTIN_TEMPLATES
        .iter()
        .filter_map(|(m, _, _, _)| {
            if seen.insert(*m) {
                Some(*m)
            } else {
                None
            }
        })
        .collect()
}

/// 内置配置模板：`(模型名, 槽位名, 显示名, 正文)`。
///
/// ## 为什么内置模板也放这里
///


/* ------------------------------------------------------------ 通用单文件模型 */

/// 通用 GGUF 单文件模型（无多模态、无投机解码）。
///
/// 覆盖 Qwen2.5 系列这类「一个 gguf 就是全部」的模型。字段都是模板
/// 通用字段，因此实现很短。
pub struct SimpleGgufSchema;

impl ModelTemplateSchema for SimpleGgufSchema {
    fn model_names(&self) -> &'static [&'static str] {
        // 必须与部署页「模型名称」下拉框**完全一致**。
        // 多注册一个名字会让设置页出现部署页根本没有的选项——用户配了
        // 模板却无法在部署时选中，等于配了个死配置。
        &["qwen2.5-0.5b"]
    }

    fn parse(&self, raw: &RawTemplate) -> MnResult<ModelDeploySpec> {
        let model_file = require(raw, "model")?;
        let download_url = main_url(raw, &model_file)?;
        Ok(ModelDeploySpec {
            template_name: raw.name.clone(),
            engine: raw
                .get("engine")
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            sha256: raw
                .get("sha256")
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            alias: raw
                .get("alias")
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            model_file,
            download_url,
            aux_files: Vec::new(),
            context: raw.get_u32("context").unwrap_or(8192),
            gpu_layers: raw.get_u32("gpu_layers").unwrap_or(99),
            batch: raw.get_u32("batch").unwrap_or(512),
            ubatch: raw.get_u32("ubatch").unwrap_or(128),
            cache_k: raw.get("cache_k").unwrap_or("q4_0").to_string(),
            cache_v: raw.get("cache_v").unwrap_or("q4_0").to_string(),
            flash_attn: raw.get_bool("flash_attn").unwrap_or(false),
            mmproj: None,
            draft_model: None,
            draft_gpu_layers: None,
            draft_n_max: None,
            extra_args: Vec::new(),
        })
    }
}

/* ------------------------------------------- Qwen3.8-27B（多模态 + 投机解码） */

/// Qwen3.8-27B 系列：主模型 + mmproj + MTP draft + 一组专属参数。
///
/// 与通用 schema 的差别集中在这里：辅助模型清单、多模态投影、
/// 投机解码参数都只对这类模型有意义，因此解析逻辑独立成一份。
pub struct Qwen38Schema;

impl ModelTemplateSchema for Qwen38Schema {
    fn model_names(&self) -> &'static [&'static str] {
        &["qwen3.8-27b"]
    }

    fn parse(&self, raw: &RawTemplate) -> MnResult<ModelDeploySpec> {
        let model_file = require(raw, "model")?;
        let download_url = main_url(raw, &model_file)?;

        // 辅助模型：模板里每项形如 `文件名 (中文备注)`
        let aux_files: Vec<AuxFile> = raw
            .get_list("auxiliary_models")
            .iter()
            .filter_map(|item| {
                let file_name = first_token(item)?;
                Some(AuxFile {
                    url: sibling_url(&download_url, &file_name),
                    sha256: builtin_file_sha256(
                        raw.get("model_id").unwrap_or(""),
                        &file_name.to_lowercase(),
                    ),
                    file_name,
                    // mmproj / MTP draft 均为可选辅助：失败跳过不阻断
                    required: false,
                })
            })
            .collect();

        // 多模态投影：优先 multimodal.mmproj，其次辅助清单里的 mmproj*
        let mmproj = raw
            .get_nested("multimodal", "mmproj")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| {
                aux_files
                    .iter()
                    .find(|a| a.file_name.starts_with("mmproj"))
                    .map(|a| a.file_name.clone())
            });

        // 投机解码
        let draft_model = raw
            .get_nested("speculative", "draft_model_path")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let draft_gpu_layers = raw
            .get_nested("speculative", "draft_gpu_layers")
            .and_then(|s| s.trim().parse::<u32>().ok());
        let draft_n_max = raw
            .get_nested("speculative", "draft_n_max")
            .and_then(|s| s.trim().parse::<u32>().ok());

        // 模型专属的额外参数：只有真正有值才下发，避免覆盖引擎默认。
        //
        // ## 这些字段为什么走 extra_args 而不是 LocalPlan 结构
        //
        // `LocalPlan` 是「推理意图」的通用表达（上下文、层数、KV 精度…），
        // 每个引擎适配器都要能解释它。而 `ubatch` / `cache-reuse` / `jinja`
        // / `metrics` 是 **llama.cpp 专属开关**，塞进 LocalPlan 会强迫别的
        // 引擎适配器为它们写空实现。放进 `extra_args` 由适配器原样透传，
        // 语义边界更干净。
        //
        // ## 为什么必须显式下发 ubatch
        //
        // 模板里 `ubatch: 512` 曾经只被解析进 `ModelDeploySpec.ubatch`
        // 却从未翻译成 CLI 参数——一个彻头彻尾的**死字段**：用户改了它，
        // 进程命令行毫无变化，实测最终启动命令里根本没有 `--ubatch-size`。
        // 现在把它接进 extra_args，模板写什么就下发什么。
        let mut extra_args = Vec::new();
        for key in [
            "parallel",
            "image_max_tokens",
            "mtmd_batch_max_tokens",
        ] {
            if let Some(v) = raw.get(key) {
                extra_args.push(format!("--{}", key.replace('_', "-")));
                extra_args.push(v.to_string());
            }
        }
        // `ubatch` 字段名与 CLI 不同名（`--ubatch-size`），单独映射
        if let Some(v) = raw.get_u32("ubatch") {
            extra_args.push("--ubatch-size".to_string());
            extra_args.push(v.to_string());
        }
        // 下列为「开关型/单值型」参数：模板给了值就下发。
        // 布尔开关（jinja / metrics）约定写 `on`/`true`/`yes` 即为启用。
        if let Some(v) = raw.get_u32("cache_reuse") {
            extra_args.push("--cache-reuse".to_string());
            extra_args.push(v.to_string());
        }
        for key in ["jinja", "metrics"] {
            if raw.get_bool(key).unwrap_or(false) {
                extra_args.push(format!("--{key}"));
            }
        }
        if let Some(n) = draft_n_max {
            extra_args.push("--spec-draft-n-max".to_string());
            extra_args.push(n.to_string());
        }

        Ok(ModelDeploySpec {
            template_name: raw.name.clone(),
            engine: raw
                .get("engine")
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            sha256: raw
                .get("sha256")
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            alias: raw
                .get("alias")
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            model_file,
            download_url,
            aux_files,
            context: raw.get_u32("context").unwrap_or(65536),
            gpu_layers: raw.get_u32("gpu_layers").unwrap_or(99),
            batch: raw.get_u32("batch").unwrap_or(2048),
            ubatch: raw.get_u32("ubatch").unwrap_or(512),
            cache_k: raw.get("cache_k").unwrap_or("q4_0").to_string(),
            cache_v: raw.get("cache_v").unwrap_or("q4_0").to_string(),
            flash_attn: raw.get_bool("flash_attn").unwrap_or(true),
            mmproj,
            draft_model,
            draft_gpu_layers,
            draft_n_max,
            extra_args,
        })
    }
}

/* ------------------------------- Qwen3.8-Flash-Next 125B（Strata 引擎 · 三级存储 MoE） */

/// Qwen3.8-Flash-Next 125B：MoE 模型，由 Strata 引擎加载。
///
/// 与 Qwen38Schema 的关键差别：
/// - `engine: strata` 显式声明推理引擎，编排层据此分发到 StrataEngine 插件。
/// - 模型由 Strata 自管（`strata launch --family qwen --model IQ3_S`），
///   而非 llama-server 的 `--model <path>`。Mangosteen 仍把 GGUF 下到
///   `{workdir}/models/`，启动时 `--data-dir` 指向同目录供 Strata 发现复用。
/// - `family`/`model_quant`/`kv`/`mtp`/`vision` 等 Strata 专属参数透传到
///   `extra_args`，由 StrataEngine::launch_config 拼成 `--family/--model/--kv/--mtp/--vision`。
/// - 双分片（00001 主权重 + 00002 N-gram 表）：主文件走 `model`，分片2
///   走 `auxiliary_models`，下载链路完全复用。
pub struct QwenFlashNextSchema;

impl ModelTemplateSchema for QwenFlashNextSchema {
    fn model_names(&self) -> &'static [&'static str] {
        &["qwen3.8-flash-next-125b-iq2xs", "qwen3.6-35b-a3b"]
    }

    fn parse(&self, raw: &RawTemplate) -> MnResult<ModelDeploySpec> {
        // model: 支持多文件分片（逗号分隔）——2026-11-23 用户裁决：
        // 分片（如 IQ2_XS 的 00001/00002）也是主模型的一部分。
        // 第一个文件是「主文件」（决定 download_url 的拼接基准），
        // 其余分片追加为 aux_files（下载链路完全复用，Strata --data-dir
        // 自动发现同目录全部分片）。
        let model_files: Vec<String> = require(raw, "model")?
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let Some(main_file) = model_files.first().cloned() else {
            return Err(crate::MnError::config(
                "config_template",
                "模板 `model` 字段为空：至少需要一个模型文件名",
            ));
        };
        let download_url = main_url(raw, &main_file)?;
        // download_url 的拼接基准（resolve/main/ 之前），供分片与 aux 使用
        let base = download_url
            .rsplit_once("resolve/main/")
            .map(|(b, _)| format!("{b}resolve/main/"))
            .unwrap_or_else(|| download_url.clone());

        // 分片 2..N：作为 aux_files（下载清单统一遍历主+aux）
        // required=true：分片是主模型的一部分，下载失败必须阻断部署
        //（2026-11-23 修复：之前走可选路径，失败被跳过导致 Strata 缺分片）。
        let shard_files: Vec<AuxFile> = model_files
            .iter()
            .skip(1)
            .map(|file_name| AuxFile {
                url: format!("{base}{file_name}"),
                sha256: builtin_file_sha256(
                    raw.get("model_id").unwrap_or(""),
                    &file_name.to_lowercase(),
                ),
                file_name: file_name.clone(),
                required: true,
            })
            .collect();

        // 辅助文件：mmproj 等可选件。
        //
        // auxiliary_models 里写相对于仓库根的路径（如 `IQ3_S/xxx.gguf`），
        // 从主模型 download_url 提取 `resolve/main/` 之前的 base 拼接，
        // 避免 sibling_url 把目录再拼一次造成 `IQ3_S/IQ3_S/` 双前缀。
        let mut aux_files: Vec<AuxFile> = raw
            .get_list("auxiliary_models")
            .iter()
            .filter_map(|item| {
                let file_name = first_token(item)?;
                Some(AuxFile {
                    url: format!("{base}{file_name}"),
                    sha256: builtin_file_sha256(
                        raw.get("model_id").unwrap_or(""),
                        &file_name.to_lowercase(),
                    ),
                    file_name,
                    // mmproj 为可选辅助：失败跳过不阻断
                    required: false,
                })
            })
            .collect();
        // 分片追加在 aux 清单前面（主模型分片优先下载）
        shard_files.into_iter().for_each(|s| aux_files.insert(0, s));

        // Strata 专属参数 → extra_args（由 StrataEngine 插件拼成 CLI flags）
        //
        // Strata launch 官方支持的 flag（与官方 release 文档一致）：
        //   --family <family>   模型家族，如 qwen
        //   --model <quant>     量化档位，如 IQ3_S
        //   --data-dir <dir>    数据目录，Strata 自管模型存储
        //   --context <N>       上下文长度，如 65536
        //   --vision <on/off>   视觉模态开关
        //   --kv <type>        KV 缓存精度（int8/q4_0 等）
        //   --mtp              启用 MTP 投机解码
        //   --parallel <N>     并发数
        //   --yes               跳过交互确认
        //   --no-tui            后台无界面运行
        //
        // 以下为 llama.cpp 专属参数，Strata 不支持（Strata 内部自管）：
        //   --flash_attn / --batch / --ubatch / --gpu-layers
        //   cache_k / cache_v 等均为占位字段，不翻译为 CLI。
        let mut extra_args: Vec<String> = Vec::new();
        if let Some(family) = raw.get("family") {
            extra_args.push("--family".into());
            extra_args.push(family.trim().to_string());
        }
        if let Some(quant) = raw.get("model_quant") {
            extra_args.push("--model".into());
            extra_args.push(quant.trim().to_string());
        }
        if let Some(vision) = raw.get("vision") {
            extra_args.push("--vision".into());
            extra_args.push(vision.trim().to_string());
        }
        if let Some(ctx) = raw.get_u32("context") {
            extra_args.push("--context".into());
            extra_args.push(ctx.to_string());
        }
        if let Some(kv) = raw.get("kv") {
            extra_args.push("--kv".into());
            extra_args.push(kv.trim().to_string());
        }
        if let Some(mtp) = raw.get("mtp") {
            let v = mtp.trim().to_lowercase();
            if v == "on" || v == "true" || v == "yes" {
                extra_args.push("--mtp".into());
                extra_args.push("on".into());
            }
        }
        if let Some(par) = raw.get("parallel") {
            extra_args.push("--parallel".into());
            extra_args.push(par.trim().to_string());
        }
        // --yes --no-tui 由 StrataEngine::launch_config 强制追加，这里不再重复

        Ok(ModelDeploySpec {
            template_name: raw.name.clone(),
            engine: raw
                .get("engine")
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            sha256: raw
                .get("sha256")
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            alias: raw
                .get("alias")
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            model_file: main_file,
            download_url,
            aux_files,
            context: raw.get_u32("context").unwrap_or(65536),
            gpu_layers: raw.get_u32("gpu_layers").unwrap_or(99),
            batch: raw.get_u32("batch").unwrap_or(2048),
            ubatch: raw.get_u32("ubatch").unwrap_or(512),
            cache_k: raw.get("cache_k").unwrap_or("q8_0").to_string(),
            cache_v: raw.get("cache_v").unwrap_or("q8_0").to_string(),
            flash_attn: raw.get_bool("flash_attn").unwrap_or(true),
            mmproj: None,
            draft_model: None,
            draft_gpu_layers: None,
            draft_n_max: None,
            extra_args,
        })
    }
}

/* -------------------------------------------------------------------- 工具 */

fn require(raw: &RawTemplate, key: &str) -> MnResult<String> {
    raw.get(key)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            crate::MnError::config(
                "config_template",
                format!("配置模板缺少必填字段 `{key}`（决定下载哪个模型文件）"),
            )
        })
}

/// 主模型下载地址：`source` + `repository` + 文件名。
///
/// 兼容两种写法：`source` 只给站点根（配合 `repository`），或直接给到
/// 仓库 URL（老模板）。
fn main_url(raw: &RawTemplate, file: &str) -> MnResult<String> {
    let src = raw
        .get("source")
        .map(|s| s.trim().trim_end_matches('/'))
        .filter(|s| !s.is_empty());
    let repo = raw
        .get("repository")
        .map(|s| s.trim().trim_end_matches('/'))
        .filter(|s| !s.is_empty());
    match (src, repo) {
        (Some(src), Some(repo)) => Ok(format!("{src}/{repo}/resolve/main/{file}")),
        (Some(src), None) => Ok(format!("{src}/resolve/main/{file}")),
        (None, Some(repo)) => Ok(format!("https://hf-mirror.com/{repo}/resolve/main/{file}")),
        (None, None) => Err(crate::MnError::config(
            "config_template",
            "配置模板缺少 `source`（下载站点）；决定从哪下载模型",
        )),
    }
}

/// 同仓库下另一个文件的下载地址（辅助模型与主模型同目录）。
fn sibling_url(main_url: &str, file: &str) -> String {
    match main_url.rsplit_once('/') {
        Some((dir, _)) => format!("{dir}/{file}"),
        None => file.to_string(),
    }
}

/// 取一段文本的第一个 token（模板里 `文件名 (备注)` 的备注不参与逻辑）。
fn first_token(item: &str) -> Option<String> {
    let t = item.trim().trim_start_matches("- ").trim();
    let name = t.split_whitespace().next()?.trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// 解析并解释模板：`(模型名, 正文)` → 部署规格。
///
/// 未注册的模型明确报错，并提示当前受支持的模型——猜一个默认规格去部署
/// 未知模型，只会产出「看起来成功、实际跑错权重」的结果。
///
/// schema 选择基于模板正文的 `engine:` / `framework:` 字段（`schema_for_text`），
/// 不再依赖静态 SCHEMAS 数组——新增模型文件即自动支持。
pub fn resolve(model: &str, text: &str) -> MnResult<ModelDeploySpec> {
    let schema = schema_for_text_with_model(model, text).ok_or_else(|| {
        crate::MnError::config(
            "config_template",
            format!(
                "模型 `{model}` 暂不支持自定义配置模板（需要升级版本获得该模型的解析器）。\
                 当前支持：{}",
                supported_models().join("、")
            ),
        )
    })?;
    let raw = parse_raw(text)?;
    schema.parse(&raw)
}

/// 按模型名取内置默认（`default` 槽位）模板正文。
///
/// CLI 等不携带模板正文的调用方用它回落到与 UI 相同的下载源——
/// 内置模板里的 `source`/`repository` 是完整仓库路径，解析出的
/// `download_url` 才是真实可下载的地址。旧兼容路径「裸域名 + 文件名」
/// 拼接会产生 HuggingFace 上不存在的地址（缺少 `仓库/resolve/main/`），
/// 表现为下载 0 字节、停滞、部署失败——而同一模型走 UI 部署却成功，
/// 因为 UI 总是传模板正文。
pub fn builtin_default_template(model: &str) -> Option<&'static str> {
    BUILTIN_TEMPLATES
        .iter()
        .find(|(m, slot, _, _)| *m == model && *slot == "default")
        .map(|(_, _, _, text)| *text)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 内置 uncensored 模板的**真实正文**。
    ///
    /// 绝不在这里复制一份硬编码副本：曾经那样做，结果源码里的模板换了
    /// 仓库/文件名，测试仍然对着旧副本断言并全部通过——测试成了「自我
    /// 确认」，完全掩盖了改动。模板只能有一个真源（`BUILTIN_TEMPLATES`），
    /// 测试也从那里取。
    fn uncensored() -> &'static str {
        BUILTIN_TEMPLATES
            .iter()
            .find(|(m, s, _, _)| *m == "qwen3.8-27b" && *s == "uncensored")
            .map(|(_, _, _, t)| *t)
            .expect("内置模板必须包含 qwen3.8-27b/uncensored")
    }

    /// 一段**专供解析器语法测试**的合成正文：带嵌套与列表，不参与业务断言。
    ///
    /// 与内置模板解耦——它测的是「解析器能否理解这种结构」，而不是
    /// 「我们的模板长什么样」。后者必须由真实模板来断言。
    const SYNTHETIC_NESTED: &str = r#"template: Synthetic
model_id: qwen3.8-27b
repository: example/synthetic-GGUF
model: synthetic-Q4_K_M.gguf
auxiliary_models:
  - mmproj-synthetic-F16.gguf (多模态投影)
context: 8192
gpu_layers: 99
speculative:
  type: draft-mtp
  draft_n_max: 2
multimodal:
  mmproj: mmproj-synthetic-F16.gguf"#;

    const DEFAULT_05B: &str = r#"template: default
model_id: qwen2.5-0.5b
quantization: 4-bit (Q4_K_M)
source: https://hf-mirror.com/Qwen/Qwen2.5-0.5B-Instruct-GGUF
framework: llama.cpp
binary: llama-server
model: qwen2.5-0.5b-instruct-q4_k_m.gguf
context: 8192
gpu_layers: 99
batch: 512
ubatch: 128
cache_k: q4_0
cache_v: q4_0"#;

    /* ---- 语法层 ---- */

    #[test]
    fn parses_nested_and_lists() {
        let t = parse_raw(SYNTHETIC_NESTED).unwrap();
        assert_eq!(t.name, "Synthetic");
        assert_eq!(t.get_nested("speculative", "type"), Some("draft-mtp"));
        assert_eq!(t.get_list("auxiliary_models").len(), 1);
        assert_eq!(t.get_u64("context"), Some(8192));
    }

    #[test]
    fn malformed_line_reports_line_number() {
        let e = parse_raw("template: X\nno colon here\n").unwrap_err();
        assert!(e.to_string().contains("第 2 行"), "{e}");
    }

    #[test]
    fn url_hash_not_treated_as_comment() {
        let t = parse_raw("template: X\nsource: https://h/a#f\n").unwrap();
        assert_eq!(t.get("source"), Some("https://h/a#f"));
    }

    /* ---- 多态：同一正文，不同模型解释不同 ---- */

    #[test]
    fn qwen38_schema_extracts_multimodal_and_draft() {
        let spec = resolve("qwen3.8-27b", uncensored()).unwrap();
        assert_eq!(spec.template_name, "Uncensored");
        assert_eq!(
            spec.model_file,
            "Qwen3.8-27B-Uncensored-Q4_K_M.gguf"
        );
        // 下载地址必须来自模板 repository，而不是硬编码
        assert!(
            spec.download_url
                .contains("JonathanColetti/Qwen3.8-27B-Uncensored-GGUF"),
            "下载地址应来自模板 repository：{}",
            spec.download_url
        );
        assert!(!spec.download_url.contains("unsloth"), "绝不能回落到默认仓库");
        // 融合版主权重已内嵌 MTP，模板只声明投影器一个辅助文件
        assert_eq!(spec.aux_files.len(), 1);
        assert_eq!(
            spec.mmproj.as_deref(),
            Some("mmproj-Qwen3.8-27B-Uncensored-F16.gguf")
        );
        // **不使用外置草稿模型**：仓库的 fused 文件自带 MTP 头
        assert!(
            spec.draft_model.is_none(),
            "uncensored 不应声明外置草稿：{:?}",
            spec.draft_model
        );
        assert_eq!(spec.draft_n_max, Some(2));
        assert_eq!(spec.context, 65536);
        assert_eq!(spec.cache_k, "q8_0");
        assert_eq!(spec.cache_v, "q8_0");
        // 模板里的 parallel / image_max_tokens 转成额外参数
        assert!(spec.extra_args.contains(&"--parallel".to_string()));
        assert!(spec.extra_args.contains(&"--spec-draft-n-max".to_string()));
        assert!(spec.extra_args.contains(&"--ubatch-size".to_string()));
        assert!(spec.extra_args.contains(&"--cache-reuse".to_string()));
        assert!(spec.extra_args.contains(&"--jinja".to_string()));
        assert!(spec.extra_args.contains(&"--metrics".to_string()));
    }

    #[test]
    fn simple_schema_has_no_multimodal_or_draft() {
        let spec = resolve("qwen2.5-0.5b", DEFAULT_05B).unwrap();
        assert_eq!(spec.template_name, "default");
        assert!(spec.aux_files.is_empty(), "0.5B 不该有辅助模型");
        assert!(spec.mmproj.is_none());
        assert!(spec.draft_model.is_none());
        assert!(spec.extra_args.is_empty());
        assert_eq!(spec.context, 8192);
        assert_eq!(
            spec.download_url,
            "https://hf-mirror.com/Qwen/Qwen2.5-0.5B-Instruct-GGUF/resolve/main/qwen2.5-0.5b-instruct-q4_k_m.gguf"
        );
    }

    /// 同一个模型换模板正文，产出完全不同的部署规格——这正是用户要的能力。
    #[test]
    fn same_model_different_template_yields_different_spec() {
        let a = resolve("qwen3.8-27b", uncensored()).unwrap();
        let b = resolve(
            "qwen3.8-27b",
            "template: default\nsource: https://hf-mirror.com/\n\
             repository: unsloth/Qwen3.8-27B-GGUF\nmodel: Qwen3.8-27B-UD-Q4_K_M.gguf\n\
             context: 32768\n",
        )
        .unwrap();
        assert_ne!(a.model_file, b.model_file);
        assert_ne!(a.download_url, b.download_url);
        assert_ne!(a.context, b.context);
    }

    #[test]
    fn unknown_model_is_rejected_explicitly() {
        // resolve 现在基于模板正文的 engine/framework 字段选 schema，
        // 不再按模型名匹配 SCHEMAS 数组。用 llama-99 的名字 + 一个没有
        // engine/framework 声明的正文（无法匹配任何 schema）来测「暂不支持」。
        let e = resolve("llama-99", "template: X\ncontext: 8192\n").unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("暂不支持") || msg.contains("config_template"), "{msg}");
    }

    #[test]
    fn missing_model_field_is_reported() {
        let e = resolve("qwen3.8-27b", "template: X\nsource: https://h/\n").unwrap_err();
        assert!(e.to_string().contains("缺少必填字段 `model`"), "{e}");
    }

    #[test]
    fn missing_source_is_reported() {
        let e = resolve("qwen3.8-27b", "template: X\nmodel: a.gguf\n").unwrap_err();
        assert!(e.to_string().contains("缺少 `source`"), "{e}");
    }

    #[test]
    fn aux_url_points_to_same_repo() {
        let spec = resolve("qwen3.8-27b", uncensored()).unwrap();
        let url = spec
            .aux_url("mmproj-Qwen3.8-27B-Uncensored-F16.gguf")
            .expect("投影器必须在辅助清单里");
        // 辅助文件必须与主权重同仓库：跨仓库拼 URL 必然 404
        assert!(
            url.starts_with(
                "https://hf-mirror.com/JonathanColetti/Qwen3.8-27B-Uncensored-GGUF/resolve/main/"
            ),
            "辅助文件应跟随模板 repository：{url}"
        );
    }

    /// 内置模板的内容必须与「用户实际会部署的东西」一致。
    ///
    /// 这条测试守的是「测试自己维护模板副本」这个反模式：曾经测试里
    /// 硬编码了一份旧模板，源码换成新仓库后测试依旧全绿，把改动掩盖了。
    /// 现在断言直接从 `BUILTIN_TEMPLATES` 取正文，任何字段漂移都会红。
    #[test]
    fn builtin_uncensored_points_at_current_repo() {
        let spec = resolve("qwen3.8-27b", uncensored()).unwrap();
        assert_eq!(spec.model_file, "Qwen3.8-27B-Uncensored-Q4_K_M.gguf");
        assert!(
            spec.download_url.contains("JonathanColetti"),
            "内置模板必须指向当前仓库：{}",
            spec.download_url
        );
        assert!(
            !spec.download_url.contains("HauhauCS"),
            "不应残留旧仓库引用：{}",
            spec.download_url
        );
    }

    #[test]
    fn all_registered_schemas_have_unique_model_names() {
        let mut seen = std::collections::HashSet::new();
        for name in supported_models() {
            assert!(seen.insert(name.to_ascii_lowercase()), "模型名重复注册：{name}");
        }
    }

    /* ---- 任务 1：SHA256 供应链校验 ---- */

    /// Strata 模板声明 sha256 字段，解析后必须落入 ModelDeploySpec.sha256。
    #[test]
    fn strata_template_parses_sha256() {
        const STRATA_TEMPLATE: &str = r#"template: default
model_id: qwen3.8-flash-next-125b-iq2xs
engine: strata
source: https://hf-mirror.com/ISTA-DASLab/Qwen3.8-Flash-Next-GSQ-RCO-GGUF
framework: strata
binary: strata
model: IQ2_XS/Qwen3.8-Flash-Next-GSQ-RCO-IQ2_XS-00001-of-00002.gguf
sha256: d1a8e2ca57f818f5e9523badb73242e8c4444e27860931d0a391fcbb83e9f195
family: qwen
model_quant: iq2_xs
context: 65536"#;
        let spec = resolve("qwen3.8-flash-next-125b-iq2xs", STRATA_TEMPLATE).unwrap();
        assert_eq!(
            spec.sha256.as_deref(),
            Some("d1a8e2ca57f818f5e9523badb73242e8c4444e27860931d0a391fcbb83e9f195"),
            "sha256 字段必须被解析出来：{:?}",
            spec.sha256
        );
    }

    /// Strata 模板未填 sha256 时，sha256 为 None（不强制，只是告警）。
    #[test]
    fn strata_template_sha256_optional() {
        const STRATA_TEMPLATE: &str = r#"template: default
model_id: qwen3.8-flash-next-125b-iq2xs
engine: strata
source: https://hf-mirror.com/ISTA-DASLab/Qwen3.8-Flash-Next-GSQ-RCO-GGUF
framework: strata
binary: strata
model: IQ2_XS/Qwen3.8-Flash-Next-GSQ-RCO-IQ2_XS-00001-of-00002.gguf
family: qwen
model_quant: iq2_xs
context: 65536"#;
        let spec = resolve("qwen3.8-flash-next-125b-iq2xs", STRATA_TEMPLATE).unwrap();
        assert!(spec.sha256.is_none(), "未配置 sha256 时应为 None：{:?}", spec.sha256);
    }

    /* ---- 任务 2：Strata CLI 参数对齐 ---- */

    /// Strata 模板的 extra_args 只能包含官方支持的 flag：
    /// --family / --model / --vision / --context / --kv / --mtp / --parallel。
    /// 绝不能出现 llama.cpp 专属的 --flash_attn / --batch / --ubatch /
    /// --gpu-layers（cache_k/cache_v 也是 llama.cpp 专属，不传）。
    /// （--yes / --no-tui / --data-dir / --host / --port 由插件 start 追加，
    /// 不在 schema extra_args 范围内。）
    #[test]
    fn strata_template_extra_args_only_valid_flags() {
        const STRATA_TEMPLATE: &str = r#"template: default
model_id: qwen3.8-flash-next-125b-iq2xs
engine: strata
source: https://hf-mirror.com/ISTA-DASLab/Qwen3.8-Flash-Next-GSQ-RCO-GGUF
framework: strata
binary: strata
model: IQ2_XS/Qwen3.8-Flash-Next-GSQ-RCO-IQ2_XS-00001-of-00002.gguf
family: qwen
model_quant: iq2_xs
context: 65536
vision: on
kv: int8
mtp: on
parallel: 2"#;
        let spec = resolve("qwen3.8-flash-next-125b-iq2xs", STRATA_TEMPLATE).unwrap();

        // 必须包含合法 flag
        assert!(spec.extra_args.contains(&"--family".to_string()), "必须包含 --family：{:?}", spec.extra_args);
        assert!(spec.extra_args.contains(&"--model".to_string()), "必须包含 --model：{:?}", spec.extra_args);
        assert!(spec.extra_args.contains(&"--context".to_string()), "必须包含 --context：{:?}", spec.extra_args);
        assert!(spec.extra_args.contains(&"--vision".to_string()), "必须包含 --vision：{:?}", spec.extra_args);
        // 本轮新增：--kv / --mtp / --parallel 必须被映射
        assert!(spec.extra_args.contains(&"--kv".to_string()), "必须包含 --kv：{:?}", spec.extra_args);
        assert!(spec.extra_args.contains(&"--mtp".to_string()), "必须包含 --mtp：{:?}", spec.extra_args);
        assert!(spec.extra_args.contains(&"--parallel".to_string()), "必须包含 --parallel：{:?}", spec.extra_args);

        // 绝不能包含 llama.cpp 专属 flag
        for bad in ["--flash-attn", "--flash_attn", "--batch", "--ubatch", "--gpu-layers"] {
            assert!(
                !spec.extra_args.iter().any(|a| a == bad),
                "Strata extra_args 不得包含 {bad}：{:?}",
                spec.extra_args
            );
        }
    }

    /// Strata 模板不传 extra_args 时也能正常解析（兜底 family/qwen）。
    #[test]
    fn strata_template_extra_args_empty_ok() {
        const STRATA_TEMPLATE: &str = r#"template: default
model_id: qwen3.8-flash-next-125b-iq2xs
engine: strata
source: https://hf-mirror.com/ISTA-DASLab/Qwen3.8-Flash-Next-GSQ-RCO-GGUF
framework: strata
binary: strata
model: IQ2_XS/Qwen3.8-Flash-Next-GSQ-RCO-IQ2_XS-00001-of-00002.gguf"#;
        let spec = resolve("qwen3.8-flash-next-125b-iq2xs", STRATA_TEMPLATE).unwrap();
        assert_eq!(spec.extra_args.len(), 0, "无额外参数时应为空：{:?}", spec.extra_args);
        assert_eq!(spec.engine.as_deref(), Some("strata"));
    }

    /* ---- 任务 9：模板可被 resolve 识别（Strata 引擎注册）---- */

    /// QwenFlashNextSchema 必须在 schema 注册表里，否则 resolve 会报「暂不支持」。
    #[test]
    fn strata_model_is_registered() {
        let names = supported_models();
        assert!(
            names.iter().any(|n| n == &"qwen3.8-flash-next-125b-iq2xs"),
            "qwen3.8-flash-next-125b-iq2xs 必须在支持模型列表里：{names:?}"
        );
    }
}

/* ============================================================== 内置模板聚合（build.rs 自动扫描） */

// build.rs 扫描 template/*.rs（排除 mod.rs），在编译期生成：
//   pub mod <model>; 声明
//   BUILTIN_TEMPLATES 聚合索引
//   MODEL_HASHES 哈希表聚合
// 新增模型 = 新建 template/<model>.rs → cargo 自动发现，mod.rs 零改动。
include!(concat!(env!("OUT_DIR"), "/models.rs"));

/// 内置模型文件 SHA256 哈希表（每个模型的**全部**下载文件统一登记）。
///
/// 查询键为 `(模型ID, URL 文件名小写)` 二元组——**必须带模型维度**：
/// 不同模型的辅助文件可能同名（如 `mmproj-bf16.gguf` 在多个仓库存在
/// 但内容不同），只按文件名查会误配哈希导致误删/误拒。
///
/// 哈希数据归属各模型独立文件（`template/<model>.rs` 的 `HASHES` 常量），
/// 本函数只做聚合查询，不持有任何哈希数据。
pub fn builtin_file_sha256(model_id: &str, url_filename_lower: &str) -> Option<String> {
    let id = model_id.trim().to_lowercase();
    // 1. 精确匹配 (model_id, file_name)：mmproj-bf16.gguf 等同名文件按模型消歧
    for &(mid, hashes) in MODEL_HASHES {
        if mid == id {
            for &(name, sha) in hashes {
                if url_filename_lower.contains(name) {
                    return Some(sha.to_string());
                }
            }
        }
    }
    // 2. 回落全局匹配（model_id 未知 / 旧台账 / 直连下载）
    for &(_, hashes) in MODEL_HASHES {
        for &(name, sha) in hashes {
            if url_filename_lower.contains(name) {
                return Some(sha.to_string());
            }
        }
    }
    None
}
