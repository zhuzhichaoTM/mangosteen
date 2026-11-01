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
            "--spec-draft-n-max".to_string(),
            "4".to_string(),
            "--spec-draft-type-k".to_string(),
            plan.kv_cache_type_k.as_cli_value().to_string(),
            "--spec-draft-type-v".to_string(),
            plan.kv_cache_type_v.as_cli_value().to_string(),
        ]);
    }
    // 特定模型模板的 llama.cpp 参数由调度器注入；动态端口和模型路径
    // 仍由本适配器统一生成，防止模板覆盖节点端口映射。
    args.extend(plan.extra_args.iter().cloned());
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
