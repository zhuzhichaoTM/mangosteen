//! 模板 → 云端部署规格的桥接层。
//!
//! 归属：**编排层 · 配置驱动**。
//!
//! 把「请求里的模板正文」翻译成 [`mangosteen_core::template::ModelDeploySpec`]，
//! 供 `build_cloud_plan` 使用。独立成模块是为了让 `scheduler.rs` 的改动
//! 收敛成一行调用，而不是把解析逻辑塞进已很长的方案构造函数。

use mangosteen_core::template::{self, ModelDeploySpec};
use mangosteen_core::MnResult;

use crate::assess::DeploymentRequest;
use crate::Evaluated;

/// 优先用请求里的配置模板正文解释部署规格。
///
/// 返回 `None` 表示调用方没提供模板正文（CLI / e2e / 本地文件路径），
/// 由 [`legacy_spec`] 走既有的名称推导路径。
///
/// ## 模型名怎么定
///
/// `resolve` 用模型名选 schema。优先取请求里的 `model_id`（UI 下拉框的值，
/// 与 schema 注册名一致）；缺失时退回评估出的模型名，并去掉量化后缀
/// （`qwen3.8-27b-ud:q4_k_m` → `qwen3.8-27b-ud`）。
pub(crate) fn spec_from_template(
    evaluated: &Evaluated,
    req: &DeploymentRequest,
) -> MnResult<Option<ModelDeploySpec>> {
    let Some(text) = req
        .config_template_text
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return Ok(None);
    };
    let model = req
        .model_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| base_model_name(&evaluated.spec.name));
    Ok(Some(template::resolve(&model, text)?))
}

/// 无模板正文时的兼容路径：按模型名与下载信息推导部署规格。
///
/// 保留它是为了不破坏 CLI / e2e 等不发模板的调用方。**UI 主链路必须走
/// [`spec_from_template`]**——那才是「模板驱动部署」的落点。
pub(crate) fn legacy_spec(
    evaluated: &Evaluated,
    req: &DeploymentRequest,
) -> MnResult<ModelDeploySpec> {
    let remote_file = req
        .model_download
        .as_ref()
        .and_then(|d| crate::scheduler::download_file_name(&d.url))
        .unwrap_or_else(|| crate::scheduler::model_file_name(&evaluated.spec.name));
    let download_url = req
        .model_download
        .as_ref()
        .map(|d| d.url.clone())
        .unwrap_or_else(|| format!("https://hf-mirror.com/{remote_file}"));
    Ok(ModelDeploySpec {
        template_name: req
            .config_template
            .clone()
            .unwrap_or_else(|| "default".to_string()),
        model_file: remote_file,
        download_url,
        aux_files: Vec::new(),
        context: evaluated.assessment.context_len,
        gpu_layers: evaluated.spec.n_layers,
        batch: evaluated.assessment.batch_size,
        ubatch: 128,
        cache_k: "f16".to_string(),
        cache_v: "f16".to_string(),
        flash_attn: false,
        mmproj: None,
        draft_model: None,
        draft_gpu_layers: None,
        draft_n_max: None,
        extra_args: Vec::new(),
    })
}

/// 模板里的 KV 缓存类型字符串 → 引擎枚举。
///
/// 模板写的是 llama.cpp 术语（`q4_0`/`f16`）；未知值退回 `F16`
/// （安全默认：不静默降精度）。
pub(crate) fn parse_kv_cache(s: &str) -> mangosteen_core::estimate::KvCacheType {
    use mangosteen_core::estimate::KvCacheType;
    match s.trim().to_ascii_lowercase().as_str() {
        "q4_0" => KvCacheType::Q4_0,
        "q8_0" => KvCacheType::Q8_0,
        _ => KvCacheType::F16,
    }
}

/// 从模型名去掉量化后缀：`qwen3.8-27b-ud:q4_k_m` → `qwen3.8-27b-ud`。
fn base_model_name(name: &str) -> String {
    name.trim()
        .to_ascii_lowercase()
        .split(':')
        .next()
        .unwrap_or("")
        .to_string()
}



#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_name_strips_quant_suffix() {
        assert_eq!(base_model_name("qwen3.8-27b-ud:q4_k_m"), "qwen3.8-27b-ud");
        assert_eq!(base_model_name("Qwen2.5-0.5B"), "qwen2.5-0.5b");
    }

    /// 端到端：用户改过的模板必须原样影响部署规格。
    ///
    /// 这是「用户自定义配置参数是否生效」的硬断言——下载地址、文件名、
    /// 上下文、引擎参数全部跟着正文走。
    #[test]
    fn user_edited_template_drives_deploy_spec() {
        let text = "template: Uncensored\n\
                    source: https://hf-mirror.com/\n\
                    repository: HauhauCS/My-Custom-Repo\n\
                    model: my-custom-model.gguf\n\
                    context: 32768\n\
                    gpu_layers: 88\n\
                    batch: 1024\n\
                    ubatch: 256\n\
                    cache_k: q8_0\n\
                    cache_v: q8_0\n\
                    flash_attn: off\n\
                    parallel: 2\n";
        let s = mangosteen_core::template::resolve("qwen3.8-27b", text).unwrap();
        // 用户写的仓库与文件名必须生效（绝不回落到内置默认）
        assert_eq!(
            s.download_url,
            "https://hf-mirror.com/HauhauCS/My-Custom-Repo/resolve/main/my-custom-model.gguf"
        );
        assert!(!s.download_url.contains("unsloth"));
        assert!(!s.download_url.contains("HauhauCS/Qwen3.8-27B-Uncensored"));
        // 数值参数同样跟正文走
        assert_eq!(s.context, 32768);
        assert_eq!(s.gpu_layers, 88);
        assert_eq!(s.batch, 1024);
        assert_eq!(s.ubatch, 256);
        assert_eq!(s.cache_k, "q8_0");
        assert!(!s.flash_attn, "模板写 off 就必须是 false");
        assert!(s.extra_args.contains(&"--parallel".to_string()));
        // 模板没写 multimodal/speculative → 不启用
        assert!(s.mmproj.is_none());
        assert!(s.draft_model.is_none());
    }

    #[test]
    fn kv_cache_mapping() {
        use mangosteen_core::estimate::KvCacheType;
        assert_eq!(parse_kv_cache("q4_0"), KvCacheType::Q4_0);
        assert_eq!(parse_kv_cache("Q8_0"), KvCacheType::Q8_0);
        assert_eq!(parse_kv_cache("f16"), KvCacheType::F16);
        // 未知值不静默降精度
        assert_eq!(parse_kv_cache("weird"), KvCacheType::F16);
    }
}
