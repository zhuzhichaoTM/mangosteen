//! 部署方案模型：资源评估引擎与调度决策器的输出契约。
//!
//! 归属：**API 网关层**。`plan` 是网关内部各组件之间唯一的「部署意图」载体，
//! 也是持久化部署台账的实体类型。
//!
//! 设计要点：
//! * 本地方案与云端方案**不共享生命周期字段**，各自携带真实参数；
//! * `DeploymentPlan` 一经生成即不可变（重建新 plan 表达变更），避免状态漂移；
//! * 所有会影响 llama.cpp 行为的参数都在这里显式化，不允许下游组件自行补默认值。

use serde::{Deserialize, Serialize};

use crate::estimate::{Assessment, KvCacheType, Placement};
use crate::model::ModelSpec;

/// 推理引擎的标准化启动配置。
///
/// 调度与节点配置层只传递配置，不解释引擎参数；具体 flags 由 `EnginePlugin`
/// 生成，执行层将参数原样传给目标引擎。云端与本地共用此契约。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineLaunchConfig {
    /// 已由对应引擎插件生成的进程参数。
    pub args: Vec<String>,
}

/// 算力来源。调度决策器的核心输出，也是一切审计日志的分组维度。
///
/// Mangosteen 只把模型部署到远端 GPU 节点（云端实例 + 已保存 SSH 设备），
/// 本地推理交由 Ollama 承担，因此这里只有 `Cloud` 一种来源。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ComputeOrigin {
    /// 云端算力链路。携服务商与实例标识，供 UI 展示但不影响上层调用方式。
    Cloud {
        provider: String,
        instance_id: String,
    },
}

impl ComputeOrigin {
    /// 展示名。
    pub fn label(&self) -> String {
        match self {
            ComputeOrigin::Cloud { provider, .. } => format!("云端算力 · {provider}"),
        }
    }
}

/// 本地部署参数集合，直接映射为 llama-server 命令行。
///
/// 字段与通用部署意图的对应关系写在每个字段的文档上。引擎专属参数由插件生成。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LocalPlan {
    /// GGUF 模型绝对路径
    pub model_path: String,
    /// 上下文窗口 → `--ctx-size`
    pub context_size: u32,
    /// 线程数 → `--threads`
    pub threads: u32,
    /// 批大小 → `--batch-size`
    pub batch_size: u32,
    /// GPU 卸载层数 → `--n-gpu-layers`。0 表示纯 CPU。
    pub gpu_layers: u32,
    /// KV Cache K 精度 → `--cache-type-k`
    pub kv_cache_type_k: KvCacheType,
    /// KV Cache V 精度 → `--cache-type-v`
    pub kv_cache_type_v: KvCacheType,
    /// 启用 FlashAttention → `--flash-attn`
    pub flash_attention: bool,
    /// 保留内存映射意图供计划和资源评估使用；具体 llama.cpp CLI 适配器
    /// 不再输出显式 `--mmap`，因为部分远端 llama-server 发行包已移除该参数，
    /// 而 mmap 是其默认行为。
    pub use_mmap: bool,
    /// 锁定在内存中防止换出 → `--mlock`。内存充裕时开启可稳定延迟。
    pub use_mlock: bool,
    /// 监听端口 → `--port`
    pub port: u16,
    /// 监听地址。本地链路固定 127.0.0.1，禁止对外暴露。
    pub host: String,
    /// 本次部署使用的推理引擎。
    ///
    /// 为什么要记在 plan 里而不是每次读全局配置：用户可能中途在设置页
    /// 切换引擎，但**已在运行的实例**仍应按它启动时的引擎探活与停止，
    /// 否则切一下引擎就会让所有在跑的实例瞬间被判失败。
    ///
    /// 旧台账没有该字段 → 反序列化时默认 llama.cpp（当时的唯一引擎）。
    #[serde(default)]
    pub engine: crate::config::InferenceEngine,
    /// 多模态投影 GGUF 文件路径。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mmproj_path: Option<String>,
    /// MTP draft GGUF 文件路径。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft_model_path: Option<String>,
    /// LocalPlan 辅助 GGUF 下载清单。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub auxiliary_downloads: Vec<AuxiliaryDownload>,
    /// 引擎专属额外参数（不含动态端口、模型路径）。
    ///
    /// 用于 qwen3.8 等多模态模型的 `--image-max-tokens`、
    /// `--mtmd-batch-max-tokens`、`--parallel` 等 llama.cpp 参数。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_args: Vec<String>,
}

impl LocalPlan {
    /// 由评估结论构造本地部署参数。
    pub fn from_assessment(
        spec: &ModelSpec,
        model_path: impl Into<String>,
        assessment: &Assessment,
        port: u16,
    ) -> Self {
        let gpu_layers = match assessment.placement {
            Placement::FullGpu => spec.n_layers + 1, // 含输出层
            Placement::PartialOffload { gpu_layers } => gpu_layers,
            Placement::CpuOnly | Placement::Impossible => 0,
        };
        Self {
            model_path: model_path.into(),
            context_size: assessment.context_len,
            threads: assessment.threads,
            batch_size: assessment.batch_size,
            gpu_layers,
            kv_cache_type_k: assessment.kv_cache,
            kv_cache_type_v: assessment.kv_cache,
            // FlashAttention 仅在 GPU 参与且量化 KV 时稳定收益；纯 CPU 后端在
            // 部分 llama.cpp 版本上对 FA 支持不完整，故 CPU 路径默认关闭。
            flash_attention: gpu_layers > 0,
            use_mmap: true,
            // mlock **默认关闭**，由配置 `local.mlock` 显式开启。
            //
            // 为什么默认关：mlock 的语义是「锁定在物理内存、不许换出」，
            // 它只在内存真正充裕时才是好事。个人桌面机上浏览器等程序
            // 常驻，锁定模型会挤压系统、触发内存压缩，反而更慢
            // （实测本机 16 GB 内存、swap 已用 12 GB）。
            //
            // 也不能靠「静态总量」判断：`usable_memory()` 是
            // 「总内存 - 25%」，不反映当前实际占用，会高估可用量。
            // 真要稳定延迟（专用机 / 服务器）时由用户显式开启。
            use_mlock: false,
            port,
            host: "127.0.0.1".to_string(),
            // 默认 llama.cpp；网关在落台账前会覆盖为实际选中的引擎
            engine: crate::config::InferenceEngine::default(),
            mmproj_path: None,
            draft_model_path: None,
            auxiliary_downloads: Vec::new(),
            extra_args: Vec::new(),
        }
    }

    /// 参数顺序由对应引擎插件定义；部署意图仅保留数据字段。
    ///
    /// 该方案是否完全驻显存。
    pub fn is_full_gpu(&self, total_layers: u32) -> bool {
        self.gpu_layers >= total_layers
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuxiliaryDownload {
    pub url: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
}

/// 云端实例规格请求（已归一化，不含厂商差异字段）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CloudPlan {
    /// 服务商驱动名，如 `autodl`
    pub provider: String,
    /// 目标加速卡型号（归一化名，如 `RTX 4090`）
    pub gpu_model: String,
    /// 卡数。个人级场景固定 1，保留字段以便扩展多卡非核心场景。
    pub gpu_count: u32,
    /// 单卡显存字节数
    pub vram_bytes_per_gpu: u64,
    /// CPU 核心数
    pub cpu_cores: u32,
    /// 内存字节数
    pub memory_bytes: u64,
    /// 数据盘字节数（需容纳 GGUF + 输出）
    pub disk_bytes: u64,
    /// 镜像标识（由驱动映射为厂商私有镜像 ID）
    pub image: String,
    /// 云端模型路径
    pub model_path: String,
    /// 云端 llama-server 监听端口（实例内）
    pub engine_port: u16,
    /// 多模态投影 GGUF 文件路径（与主模型一起加载）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mmproj_path: Option<String>,
    /// MTP draft GGUF 文件路径。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft_model_path: Option<String>,
    /// 辅助 GGUF 下载清单（例如 mmproj 与 MTP draft）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub auxiliary_downloads: Vec<AuxiliaryDownload>,
    /// 隧道建立后本地映射端口
    pub local_port: u16,
    /// 云端引擎参数（与本地保持同一套语义，确保推理行为一致）
    pub engine: LocalPlan,
    /// 可接受的最高时价（元/小时）。超过则不选该节点。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_price_per_hour: Option<f64>,
    /// **远端直接下载**模型的地址（主链路）。
    ///
    /// ## 为什么这是主链路
    ///
    /// 设备选云端时，模型的「下载」本身就应该发生在云端——本地不必先下一份
    /// 再传上去。那样做既浪费本地带宽与磁盘，也让「本地没装过这个模型」
    /// 变成云端部署的前置条件，与「云端弹性补位」的定位相悖。
    ///
    /// 地址由 `ModelSource::download_url` 拼出（与本地下载**同源同规则**），
    /// 所以云端拿到的就是用户选的那个文件，不是另找的替代品。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_url: Option<String>,
    /// 远端下载的期望文件字节数（**参考值**，不用于强校验）。
    ///
    /// 来源是白名单默认文件的源站元数据，可能与模板实际指定的文件不是
    /// 同一个，大小必然对不上；完整性由「下载器 0 退出的 `.download.ok`
    /// 标记 + 文件非空 + GGUF 魔数」保证。无法获知时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_bytes: Option<u64>,
    /// 远端引擎里的模型名（如 Ollama 的 `qwen2.5:0.5b-instruct`）。
    ///
    /// `None` 表示**不负责加载模型**——此时部署只到「实例+隧道就绪」，
    /// 推理服务由用户自行准备（历史行为）。
    ///
    /// 为什么与 `model_path` 并存：`model_path` 是「文件在远端磁盘的位置」
    /// （llama.cpp 直读文件时需要），而这里要的是「引擎内部登记的模型名」
    /// （Ollama 用 blob 存储，不认任意路径）。两者语义不同，不能合并。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_model: Option<String>,
    /// 云端节点公网地址（SSH host / 公网 IP）。
    ///
    /// 实例创建成功后由编排层回填并写入台账，供 UI 展示「这个模型跑在哪台
    /// 机器上」。旧台账没有该字段时为 `None`，展示端必须显示占位符而不是
    /// 猜测地址。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_ip: Option<String>,
    /// 用户选定的 GPU 型号（用于停止实例复用时匹配同型号节点）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_gpu_model: Option<String>,
    /// 用户选定的规格 ID（`None` 表示自动选点）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_spec_id: Option<String>,
    /// 节点 SSH 端口（登录信息展示用）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_ssh_port: Option<u16>,
    /// 节点 SSH 用户（通常 `root`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_ssh_user: Option<String>,
    /// 部署在**用户自建设备**上时的设备 ID（设置页「设备列表」）。
    ///
    /// 有值时这台机器**不是我们创建的**：停止部署只能停远端推理进程，
    /// 绝不能关机或销毁——那会影响用户的其它用途。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    /// 用户在节点选择弹窗里**指定复用的云端实例 ID**。
    ///
    /// 有值时部署不再创建新实例，而是直接挂到这台已存在（运行中或已停机）
    /// 的云实例上：停机实例先开机，运行中实例直接在其上另起一个引擎端口
    /// 的 llama-server（同机多模型）。为空则按 GPU 型号自动匹配或新建。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reuse_instance_id: Option<String>,
    /// [`Self::reuse_instance_id`] 所属的服务商驱动名。
    ///
    /// 实例 ID 跨厂商不保证唯一，复用必须 `(provider, instance_id)` 成对。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reuse_provider: Option<String>,
}

// 云端运行参数由引擎插件生成；CloudPlan 只保留节点配置数据。

/// 一份完整的部署方案。网关的「唯一真相」。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeploymentPlan {
    /// 部署 ID
    pub id: String,
    /// 模型规格
    pub model: ModelSpec,
    /// 算力来源
    pub origin: ComputeOrigin,
    /// 云端参数
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud: Option<CloudPlan>,
    /// 生成该方案的评估结论快照（审计用）
    pub assessment: Assessment,
    /// 创建时间（Unix 秒）
    pub created_at: i64,
    /// 触发的降级链，按发生顺序记录。
    /// 例如 `["local:insufficient_vram", "cloud:autodl"]`
    #[serde(default)]
    pub fallback_trail: Vec<String>,
}

impl DeploymentPlan {
    /// 对外统一推理入口路径（OpenAI 兼容）。
    pub fn inference_base_path(&self) -> &'static str {
        "/v1"
    }

    /// 本机可访问的推理端口。
    ///
    /// 云端部署走隧道映射后的 `cloud.local_port`。
    pub fn local_access_port(&self) -> Option<u16> {
        match (&self.origin, &self.cloud) {
            (ComputeOrigin::Cloud { .. }, Some(c)) => Some(c.local_port),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::estimate::{AssessRequest, KvCacheType};
    use crate::hardware::{Accelerator, AcceleratorVendor, HardwareProfile, HostProfile};
    use crate::model::Quantization;

    fn spec() -> ModelSpec {
        ModelSpec {
            name: "qwen2.5-7b-instruct".into(),
            params_b: 7.615,
            quant: Quantization::Q4KM,
            n_layers: 28,
            n_heads: 28,
            n_kv_heads: 4,
            embedding_dim: 3584,
            vocab_size: 152_064,
            native_context: 32_768,
            file_size_bytes: 4_683_073_024,
        }
    }

    fn hw(vram_gib: u64) -> HardwareProfile {
        HardwareProfile {
            os: "linux".into(),
            arch: "x86_64".into(),
            host: HostProfile {
                total_memory_bytes: 64 * (1 << 30),
                available_memory_bytes: 40 * (1 << 30),
                physical_cores: 12,
                logical_cores: 24,
                cpu_model: "t".into(),
            },
            accelerators: vec![Accelerator {
                name: "RTX 4090".into(),
                vendor: AcceleratorVendor::Nvidia,
                vram_bytes: vram_gib * (1 << 30),
                unified_memory_bytes: 0,
                compute_capability: Some("sm_89".into()),
                fp16_tflops: Some(165.0),
                memory_bandwidth_gbps: Some(1008.0),
            }],
        }
    }

    #[test]
    fn full_gpu_plan_uses_all_layers() {
        let s = spec();
        let a = crate::estimate::assess(&s, &hw(24), &AssessRequest::default().with_context(8192));
        let p = LocalPlan::from_assessment(&s, "/m.gguf", &a, 11435);
        assert_eq!(p.gpu_layers, s.n_layers + 1);
        assert!(p.is_full_gpu(s.n_layers + 1));
        assert!(p.flash_attention);
        // 全量驻显存时不 mlock
        assert!(!p.use_mlock);
    }

    /// 纯 CPU 计划：关 FA、不卸载 GPU 层，且**默认不 mlock**。
    ///
    /// 语义已变更：早先「非全量 GPU 就开 mlock」等于纯 CPU 一律锁内存。
    /// 但 mlock 会挤压系统其它部分，在个人桌面机（浏览器常驻、
    /// 内存本就紧张）上反而更慢。现在改为默认关闭，由
    /// `config.local.mlock` 显式开启。
    #[test]
    fn cpu_plan_disables_flash_attn_and_gpu_layers() {
        let s = spec();
        let mut h = hw(0);
        h.accelerators.clear();
        let a = crate::estimate::assess(&s, &h, &AssessRequest::default().with_context(4096));
        let p = LocalPlan::from_assessment(&s, "/m.gguf", &a, 11435);
        assert_eq!(p.gpu_layers, 0);
        assert!(!p.flash_attention);
        assert!(
            !p.use_mlock,
            "纯 CPU 默认不锁内存：锁定会挤压系统，个人机上反而更慢"
        );
    }

    #[test]
    fn cloud_plan_keeps_node_configuration_data() {
        let s = spec();
        let a = crate::estimate::assess(&s, &hw(24), &AssessRequest::default().with_context(4096));
        let engine = LocalPlan::from_assessment(&s, "/root/models/q.gguf", &a, 8080);
        let cp = CloudPlan {
            download_url: None,
            download_bytes: None,
            remote_model: None,
            node_ip: None,
            requested_spec_id: None,
            requested_gpu_model: None,
            node_ssh_port: None,
            node_ssh_user: None,
            device_id: None,
            reuse_instance_id: None,
            reuse_provider: None,
            provider: "autodl".into(),
            gpu_model: "RTX 4090".into(),
            gpu_count: 1,
            vram_bytes_per_gpu: 24 << 30,
            cpu_cores: 16,
            memory_bytes: 60 << 30,
            disk_bytes: 50 << 30,
            image: "ubuntu22-cuda12".into(),
            model_path: "/root/models/q.gguf".into(),
            engine_port: 8080,
            mmproj_path: None,
            draft_model_path: None,
            auxiliary_downloads: Vec::new(),
            local_port: 11435,
            engine,
            max_price_per_hour: Some(2.5),
        };
        assert_eq!(cp.engine_port, 8080);
        assert_eq!(cp.engine.engine, crate::config::InferenceEngine::default());
    }

    #[test]
    fn origin_labels() {
        assert_eq!(
            ComputeOrigin::Cloud {
                provider: "autodl".into(),
                instance_id: "i-1".into()
            }
            .label(),
            "云端算力 · autodl"
        );
    }

    #[test]
    fn plan_access_port_resolution() {
        let s = spec();
        let a = crate::estimate::assess(&s, &hw(24), &AssessRequest::default().with_context(4096));
        let local = LocalPlan::from_assessment(&s, "/m.gguf", &a, 11435);

        let cp = CloudPlan {
            download_url: None,
            download_bytes: None,
            remote_model: None,
            node_ip: None,
            requested_spec_id: None,
            requested_gpu_model: None,
            node_ssh_port: None,
            node_ssh_user: None,
            device_id: None,
            reuse_instance_id: None,
            reuse_provider: None,
            provider: "autodl".into(),
            gpu_model: "RTX 4090".into(),
            gpu_count: 1,
            vram_bytes_per_gpu: 24 << 30,
            cpu_cores: 16,
            memory_bytes: 60 << 30,
            disk_bytes: 50 << 30,
            image: "ubuntu22-cuda12".into(),
            model_path: "/m.gguf".into(),
            engine_port: 8080,
            mmproj_path: None,
            draft_model_path: None,
            auxiliary_downloads: Vec::new(),
            local_port: 12000,
            engine: local,
            max_price_per_hour: None,
        };
        let cloud_plan = DeploymentPlan {
            id: "d2".into(),
            model: s,
            origin: ComputeOrigin::Cloud {
                provider: "autodl".into(),
                instance_id: "i-9".into(),
            },
            cloud: Some(cp),
            assessment: a,
            created_at: 0,
            fallback_trail: vec!["local:insufficient_vram".into()],
        };
        assert_eq!(cloud_plan.local_access_port(), Some(12000));
    }

    #[test]
    fn plan_roundtrips_through_json() {
        let s = spec();
        let a = crate::estimate::assess(
            &s,
            &hw(24),
            &AssessRequest::default()
                .with_context(4096)
                .with_kv_cache(KvCacheType::Q8_0),
        );
        let plan = DeploymentPlan {
            id: "rt".into(),
            model: s.clone(),
            origin: ComputeOrigin::Cloud {
                provider: "autodl".into(),
                instance_id: "i-9".into(),
            },
            cloud: Some(CloudPlan {
                download_url: None,
                download_bytes: None,
                remote_model: None,
                node_ip: None,
                requested_spec_id: None,
                requested_gpu_model: None,
                node_ssh_port: None,
                node_ssh_user: None,
                device_id: None,
                // 复用字段必须参与序列化往返：UI 传给后端后要落台账，
                // 丢失会导致「选了复用节点却新建实例」。
                reuse_instance_id: Some("i-keep".into()),
                reuse_provider: Some("autodl".into()),
                provider: "autodl".into(),
                gpu_model: "RTX 4090".into(),
                gpu_count: 1,
                vram_bytes_per_gpu: 24 << 30,
                cpu_cores: 16,
                memory_bytes: 60 << 30,
                disk_bytes: 50 << 30,
                image: "ubuntu22-cuda12".into(),
                model_path: "/m.gguf".into(),
                engine_port: 8080,
                mmproj_path: None,
                draft_model_path: None,
                auxiliary_downloads: Vec::new(),
                local_port: 12000,
                engine: LocalPlan::from_assessment(&s, "/m.gguf", &a, 11435),
                max_price_per_hour: None,
            }),
            assessment: a,
            created_at: 1_700_000_000,
            fallback_trail: vec![],
        };
        let json = serde_json::to_string(&plan).unwrap();
        let back: DeploymentPlan = serde_json::from_str(&json).unwrap();
        assert_eq!(back, plan);
    }
}
