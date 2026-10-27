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
        args.push("--flash-attn".to_string());
    }
    if plan.use_mmap {
        args.push("--mmap".to_string());
    }
    if plan.use_mlock {
        args.push("--mlock".to_string());
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
