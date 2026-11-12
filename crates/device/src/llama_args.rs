//! llama.cpp CLI 参数适配。
//!
//! 归属：引擎插件实现。核心部署计划只描述资源意图；该模块负责把它映射为
//! llama-server 专属参数。新增引擎时新增自己的适配器，不改调度或领域层。

use mangosteen_core::plan::LocalPlan;

/// 将通用本地部署意图转换为 llama.cpp 命令行参数。
pub fn llama_cpp_args(plan: &LocalPlan, bind_host: &str, port: u16) -> Vec<String> {
    let mut args = vec![
        "--model".to_string(),
        plan.model_path.clone(),
        "--ctx-size".to_string(),
        plan.context_size.to_string(),
        "--threads".to_string(),
        plan.threads.to_string(),
        "--batch-size".to_string(),
        plan.batch_size.to_string(),
        "--n-gpu-layers".to_string(),
        plan.gpu_layers.to_string(),
        "--cache-type-k".to_string(),
        plan.kv_cache_type_k.as_cli_value().to_string(),
        "--cache-type-v".to_string(),
        plan.kv_cache_type_v.as_cli_value().to_string(),
        "--host".to_string(),
        bind_host.to_string(),
        "--port".to_string(),
        port.to_string(),
    ];
    if plan.flash_attention {
        // 远端 llama-server 版本要求显式枚举值；仅传 `--flash-attn`
        // 会把紧随其后的 `--mmproj` 误当成该参数的值。
        args.extend(["--flash-attn".to_string(), "on".to_string()]);
    }
    // llama-server 当前版本默认启用 mmap，且部分远端发行包已经移除了
    // 显式 `--mmap` 参数。不能把通用计划字段直接翻译成该参数，
    // 否则服务会在加载模型前报 `invalid argument: --mmap`。
    // `use_mmap=false` 也不追加 `--no-mmap`：不同版本的参数名不一致，
    // 由运行时默认行为处理，避免因兼容性参数导致启动失败。
    if plan.use_mlock {
        args.push("--mlock".to_string());
    }
    if let Some(mmproj) = &plan.mmproj_path {
        args.extend(["--mmproj".to_string(), mmproj.clone()]);
    }
    if let Some(draft) = &plan.draft_model_path {
        args.extend([
            "--model-draft".to_string(),
            draft.clone(),
            "--spec-type".to_string(),
            "draft-mtp".to_string(),
            "--n-gpu-layers-draft".to_string(),
            plan.gpu_layers.to_string(),
            "--spec-draft-type-k".to_string(),
            plan.kv_cache_type_k.as_cli_value().to_string(),
            "--spec-draft-type-v".to_string(),
            plan.kv_cache_type_v.as_cli_value().to_string(),
        ]);
    }
    // 特定模型模板的 llama.cpp 参数由调度器注入；动态端口和模型路径
    // 仍由本适配器统一生成，防止模板覆盖节点端口映射。
    //
    // ## 去重：同一个 flag 只能出现一次
    //
    // 历史上 `--spec-draft-n-max` 在这里硬编码为 `4`，而模板的
    // `draft_n_max: 4` 又经 `extra_args` 追加一次。llama-server 对重复参数
    // 只发 warning 并取最后一个值——但那行 warning 会顶掉真正有用的启动
    // 诊断，排障时看到 `specified multiple times` 很容易误判成失败原因。
    //
    // 规则：适配器已生成的 flag 一律不让模板重复添加；模板独有的 flag
    // 照常追加（模板是模型专属调参，优先级最高）。
    let adapter_flags: Vec<String> = args
        .iter()
        .filter(|a| a.starts_with("--"))
        .cloned()
        .collect();
    let mut skip_next = false;
    for extra in &plan.extra_args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if extra.starts_with("--") {
            if adapter_flags.contains(extra) {
                // 连同紧随的取值一起跳过，避免取值被当成独立位置参数
                skip_next = true;
                continue;
            }
            args.push(extra.clone());
        } else {
            args.push(extra.clone());
        }
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use mangosteen_core::config::InferenceEngine;
    use mangosteen_core::estimate::{assess, AssessRequest};
    use mangosteen_core::hardware::{Accelerator, AcceleratorVendor, HardwareProfile, HostProfile};
    use mangosteen_core::model::{ModelSpec, Quantization};

    fn plan() -> LocalPlan {
        let spec = ModelSpec {
            name: "qwen2.5-7b-instruct".into(),
            alias: None,
            params_b: 7.0,
            quant: Quantization::Q4KM,
            n_layers: 28,
            n_heads: 28,
            n_kv_heads: 4,
            embedding_dim: 3584,
            vocab_size: 152064,
            native_context: 32768,
            file_size_bytes: 4 << 30,
        };
        let hw = HardwareProfile {
            os: "linux".into(),
            arch: "x86_64".into(),
            host: HostProfile {
                total_memory_bytes: 64 << 30,
                available_memory_bytes: 48 << 30,
                physical_cores: 16,
                logical_cores: 24,
                cpu_model: "test".into(),
            },
            accelerators: vec![Accelerator {
                name: "RTX 4090".into(),
                vendor: AcceleratorVendor::Nvidia,
                vram_bytes: 24 << 30,
                unified_memory_bytes: 0,
                compute_capability: None,
                fp16_tflops: None,
                memory_bandwidth_gbps: None,
            }],
        };
        let assessment = assess(&spec, &hw, &AssessRequest::default().with_context(4096));
        let mut plan = LocalPlan::from_assessment(&spec, "/models/q.gguf", &assessment, 11516);
        plan.engine = InferenceEngine::LlamaCpp;
        plan
    }

    /// 真实模板驱动：uncensored 与 default 生成的命令必须包含全部
    /// 用户在模板里声明的参数，且 **uncensored 不得引用外置草稿模型**。
    ///
    /// 这条测试守的是「模板字段 → CLI 参数」的翻译完整性：`ubatch` 曾经
    /// 只被解析、从不翻译成 `--ubatch-size`（死字段），用户在模板里改它
    /// 毫无效果。
    #[test]
    fn builtin_templates_translate_to_expected_cli() {
        use mangosteen_core::template::BUILTIN_TEMPLATES;

        for (model, slot, _name, text) in BUILTIN_TEMPLATES {
            if *model != "qwen3.8-27b" || *slot != "uncensored" {
                continue;
            }
            let spec = mangosteen_core::template::resolve(model, text).expect("模板可解析");
            let mut p = plan();
            p.context_size = spec.context;
            p.batch_size = spec.batch;
            p.gpu_layers = spec.gpu_layers;
            p.flash_attention = spec.flash_attn;
            p.mmproj_path = spec.mmproj.clone();
            p.draft_model_path = spec.draft_model.clone();
            p.extra_args = spec.extra_args.clone();

            let args = llama_cpp_args(&p, "0.0.0.0", 8080);
            let joined = args.join(" ");

            // 模板声明的参数必须全部出现在命令行里
            for flag in ["--ubatch-size", "--cache-reuse", "--jinja", "--metrics"] {
                assert!(joined.contains(flag), "缺少模板声明的 {flag}：{joined}");
            }
            assert!(joined.contains("512"), "ubatch 的值应下发：{joined}");
            assert!(joined.contains("--spec-draft-n-max"), "{joined}");
            assert!(
                joined.contains("--mmproj"),
                "多模态投影必须下发：{joined}"
            );

            // uncensored 不带外置草稿：FastMTP 侧车需要私有补丁，
            // 官方 llama-server 加载会报 output.weight 形状错误。
            assert!(
                !joined.contains("--model-draft"),
                "uncensored 模板不应引用外置草稿模型：{joined}"
            );
            assert!(
                !args.iter().any(|a| a.contains("FastMTP")),
                "不应出现 FastMTP 侧车路径：{joined}"
            );
        }
    }

    /// 同一 flag 绝不允许出现两次。
    ///
    /// 实测踩到：适配器硬编码 `--spec-draft-n-max 4`，模板的
    /// `draft_n_max: 4` 又经 extra_args 追加一次。llama-server 只发一行
    /// DEPRECATED warning 并取最后一个值——那行 warning 会占据启动日志的
    /// 显著位置，把真正的失败原因挤下去，排障方向直接被带偏。
    #[test]
    fn duplicated_flag_is_not_emitted_twice() {
        let mut p = plan();
        p.draft_model_path = Some("/models/draft.gguf".into());
        p.extra_args = vec![
            "--parallel".into(),
            "1".into(),
            "--spec-draft-n-max".into(),
            "4".into(),
        ];
        let args = llama_cpp_args(&p, "0.0.0.0", 8080);
        let n = args.iter().filter(|a| *a == "--spec-draft-n-max").count();
        assert_eq!(n, 1, "flag 重复注入：{args:?}");
        // 模板独有的参数必须保留
        assert!(args.contains(&"--parallel".to_string()), "{args:?}");
    }

    /// 适配器已生成、模板未声明的 flag 也不该被模板覆盖成两份。
    #[test]
    fn template_flag_does_not_duplicate_adapter_flag() {
        let mut p = plan();
        p.extra_args = vec!["--ctx-size".into(), "4096".into()];
        let args = llama_cpp_args(&p, "0.0.0.0", 8080);
        let n = args.iter().filter(|a| *a == "--ctx-size").count();
        assert_eq!(n, 1, "适配器已有的 flag 不应被模板重复追加：{args:?}");
    }

    #[test]
    fn parameters_are_complete_ordered_and_bindable() {
        let p = plan();
        let args = llama_cpp_args(&p, "0.0.0.0", 8080);
        for flag in [
            "--model",
            "--ctx-size",
            "--threads",
            "--batch-size",
            "--n-gpu-layers",
            "--cache-type-k",
            "--cache-type-v",
            "--host",
            "--port",
        ] {
            let pos = args
                .iter()
                .position(|arg| arg == flag)
                .expect("required flag");
            assert!(pos + 1 < args.len());
        }
        assert_eq!(
            args[args.iter().position(|a| a == "--host").unwrap() + 1],
            "0.0.0.0"
        );
        assert_eq!(
            args[args.iter().position(|a| a == "--port").unwrap() + 1],
            "8080"
        );
        assert!(args.contains(&"/models/q.gguf".to_string()));
    }
}
