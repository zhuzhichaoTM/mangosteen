//! 配置模型：配置驱动原则的唯一载体。
//!
//! 归属：**基础能力层 · 配置管理模块**。
//!
//! ## 约束
//!
//! 1. **服务商列表、调度策略、硬件参数全部配置化**，改配置不需要重新编译。
//! 2. 模型字段全部用 `snake_case`，与 TOML 书写习惯一致。
//! 3. 每个字段都提供默认值（`serde(default)`），缺失即取默认，
//!    保证旧版配置在新版本下仍可加载（向前兼容）。
//! 4. 密钥字段单独用 [`SecretRef`] 表达，支持「内联值 / 环境变量 / 文件」三种来源，
//!    避免把明文密钥写进版本控制。
//!
//! ## 配置文件的搜索顺序
//!
//! 由 [`ConfigPaths`] 定义（平台差异在 `mangosteen-platform` 中收敛）。
//! 优先级从高到低：`--config` 显式指定 → 环境变量 → 用户配置目录 → 内置默认。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::estimate::KvCacheType;
use crate::error::{MnError, MnResult};
use crate::policy::{ScoreWeights, SelectionStrategy};

/// 密钥引用。三种来源互斥，按顺序解析。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretRef {
    /// 直接内联（**不推荐**，仅用于本地临时调试）
    Inline(String),
    /// 从环境变量读取
    Env(String),
    /// 从文件读取（文件中为单行密钥，自动 trim）
    File(PathBuf),
    /// 无密钥（部分服务商的本地 mock / 免费额度）
    None,
}

impl Default for SecretRef {
    fn default() -> Self {
        SecretRef::None
    }
}

impl SecretRef {
    /// 解析出实际密钥值。
    ///
    /// * `env_lookup` 允许测试注入假的环境变量读取器；
    /// * 生产代码传 `|k| std::env::var(k).ok()`。
    pub fn resolve<F>(&self, env_lookup: F) -> MnResult<String>
    where
        F: Fn(&str) -> Option<String>,
    {
        match self {
            SecretRef::Inline(v) => {
                if v.is_empty() {
                    Err(MnError::config("secret", "内联密钥为空"))
                } else {
                    Ok(v.clone())
                }
            }
            SecretRef::Env(key) => env_lookup(key).ok_or_else(|| {
                MnError::config(
                    "secret.env",
                    format!("环境变量 `{key}` 未设置或非 UTF-8"),
                )
            }),
            SecretRef::File(path) => {
                let content = std::fs::read_to_string(path).map_err(|e| {
                    MnError::io(format!("读取密钥文件 {}", path.display()), &e)
                })?;
                let trimmed = content.trim().to_string();
                if trimmed.is_empty() {
                    Err(MnError::config(
                        "secret.file",
                        format!("密钥文件 {} 内容为空", path.display()),
                    ))
                } else {
                    Ok(trimmed)
                }
            }
            SecretRef::None => Ok(String::new()),
        }
    }

    /// 是否为「无密钥」。
    pub fn is_none(&self) -> bool {
        matches!(self, SecretRef::None)
    }

    /// 展示用的脱敏形式（用于 `mangosteen config show`）。
    pub fn redacted(&self) -> String {
        match self {
            SecretRef::Inline(_) => "<inline:***redacted***>".to_string(),
            SecretRef::Env(k) => format!("${{{k}}}"),
            SecretRef::File(p) => format!("file:{}", p.display()),
            SecretRef::None => "<none>".to_string(),
        }
    }
}

/// 单个云服务商配置。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// 驱动名（对应已编译进二进制的驱动实现）
    pub driver: String,
    /// 是否启用
    #[serde(default = "yes")]
    pub enabled: bool,
    /// 展示名（覆盖驱动默认）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// 密钥
    #[serde(default)]
    pub secret: SecretRef,
    /// 驱动专属参数。<b>不参与上层逻辑</b>，由驱动自行解释。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub options: BTreeMap<String, String>,
    /// 该服务商的相对权重系数，用于在同分时微调偏好（1.0 为中性）
    #[serde(default = "one")]
    pub priority: f64,
    /// 驱动 profile 覆盖。
    ///
    /// 用于两种情况：
    /// 1. 厂商 API 改版后修正端点/字段，**不必等新版本发布**；
    /// 2. 接入未内置的服务商（此时 `driver` 也需相应填写）。
    ///
    /// 类型为自由 JSON 对象，由云端链路反序列化为 `RestDriverProfile`，
    /// 从而让本 crate 不依赖云端层（保持分层干净）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<serde_json::Value>,
}

fn yes() -> bool {
    true
}

fn one() -> f64 {
    1.0
}

impl ProviderConfig {
    /// 校验配置项。
    pub fn validate(&self, key: &str) -> MnResult<()> {
        if self.driver.trim().is_empty() {
            return Err(MnError::config(
                format!("providers.{key}.driver"),
                "driver 不能为空",
            ));
        }
        if !self.priority.is_finite() || self.priority < 0.0 {
            return Err(MnError::config(
                format!("providers.{key}.priority"),
                "priority 必须为非负有限数",
            ));
        }
        Ok(())
    }
}

/// 云端通用配置（与具体服务商无关）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CloudConfig {
    /// 实例 root 登录密码。
    ///
    /// ## 为什么放在通用段而不是服务商段
    ///
    /// 「建完实例要能登录进去」是所有服务商的共性需求，只是达成方式不同：
    /// 支持注入 SSH key 的服务商用 `identity_file`（不需要本字段），
    /// 只支持密码登录的（如优云智算）需要本字段。
    ///
    /// ## 安全
    ///
    /// 这是明文密码，**不要**直接写在配置里。用引用形式：
    ///
    /// ```toml
    /// [cloud]
    /// ssh_password = { file = "/path/to/pw" }    # 文件内容为单行密码
    /// # 或
    /// ssh_password = { env = "MN_CLOUD_SSH_PASSWORD" }
    /// ```
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_password: Option<SecretRef>,
    /// 云端工作目录：模型、llama-server、日志都放在这里。
    #[serde(default = "default_remote_workdir")]
    pub remote_workdir: String,
    /// llama.cpp 预编译包下载地址。
    ///
    /// 远端没有 `llama-server` 时会下载这个 zip 并解压。留空表示
    /// 「镜像里应该自带」——没有就报错，不尝试在远端编译。
    #[serde(default)]
    pub llama_server_url: String,
}

fn default_remote_workdir() -> String {
    "/root/mangosteen".to_string()
}

impl Default for CloudConfig {
    fn default() -> Self {
        Self {
            ssh_password: None,
            remote_workdir: default_remote_workdir(),
            llama_server_url: String::new(),
        }
    }
}

impl CloudConfig {
    /// 解析出密码原文。`None`/空值都视为「不配置密码」。
    pub fn resolved_password<F>(&self, env_lookup: F) -> MnResult<Option<String>>
    where
        F: Fn(&str) -> Option<String>,
    {
        match &self.ssh_password {
            None | Some(SecretRef::None) => Ok(None),
            Some(r) => r.resolve(env_lookup).map(Some),
        }
    }
}

/// 调度策略配置。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SchedulerConfig {
    /// 「本地优先」总开关。
    ///
    /// **默认且必须为 `true`**。此字段存在仅为应对极端场景
    /// （如本地 GPU 被占用需临时全量走云端），关闭时 CLI 会打印显著警告。
    /// 违反本地优先原则需用户显式确认。
    #[serde(default = "yes")]
    pub local_first: bool,
    /// 四维权重
    #[serde(default)]
    pub weights: ScoreWeights,
    /// 云端节点选择策略。
    ///
    /// `balanced`（默认）走四维加权打分；`cheapest` 在满足约束的候选里
    /// 直接按价格升序取第一个。后者不看延迟/硬件偏好——「最便宜」就是
    /// 字面意思，只在同价时才用库存与名称做稳定排序。
    #[serde(default)]
    pub strategy: SelectionStrategy,
    /// 总降级重试次数上限（本地失败 → 换云端节点）
    #[serde(default = "default_max_fallbacks")]
    pub max_fallbacks: u32,
    /// 引擎启动后等待 `/health` 就绪的超时（秒）
    #[serde(default = "default_launch_timeout")]
    pub launch_timeout_secs: u64,
    /// 云端实例创建超时（秒）
    #[serde(default = "default_provision_timeout")]
    pub provision_timeout_secs: u64,
    /// 单次部署可接受的最高时价（元/小时）。`None` 不限。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_price_per_hour: Option<f64>,
    /// 是否只考虑厂商声称「有库存」的候选。
    ///
    /// ## 为什么要能关掉
    ///
    /// 部分厂商的库存接口是**模拟调度**而非实时预占。实测优云智算：
    /// `CheckCompShareResourceCapacity` 对全部规格返回
    /// `ResourceEnough: false`，但同一组合直接调创建接口却能成功。
    /// 此时若强制要求库存，会把唯一能用的节点也过滤掉，
    /// 表现为「没有符合约束的规格」——而实际上机器就在那儿。
    ///
    /// 关掉后由真实创建失败来兜底：创建失败会明确报错并回滚，
    /// 不会产生「以为部署了其实没部署」的假象。
    #[serde(default = "yes")]
    pub require_stock: bool,
    /// GPU 型号偏好表覆盖（归一化型号 → 系数）
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub gpu_preference: BTreeMap<String, f64>,
    /// 本地模型名 → 云端引擎模型名的映射。
    ///
    /// 云端镜像自带 Ollama，它按**官方 tag** 拉权重，命名规则与本地
    /// GGUF 文件名不同（`qwen2.5-0.5b-instruct` → `qwen2.5:0.5b-instruct`）。
    /// 不配置时按「家族-尺寸-变体」启发式推导；推导不对就在这里显式覆盖，
    /// 例如：
    ///
    /// ```toml
    /// [scheduler.cloud_model_map]
    /// "qwen2.5-0.5b-instruct" = "qwen2.5:0.5b"
    /// ```
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub cloud_model_map: BTreeMap<String, String>,
}

fn default_max_fallbacks() -> u32 {
    2
}
fn default_launch_timeout() -> u64 {
    120
}
fn default_provision_timeout() -> u64 {
    600
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            local_first: true,
            weights: ScoreWeights::default(),
            strategy: SelectionStrategy::default(),
            max_fallbacks: default_max_fallbacks(),
            launch_timeout_secs: default_launch_timeout(),
            provision_timeout_secs: default_provision_timeout(),
            max_price_per_hour: None,
            require_stock: true,
            gpu_preference: BTreeMap::new(),
            cloud_model_map: BTreeMap::new(),
        }
    }
}

impl SchedulerConfig {
    /// 校验。
    pub fn validate(&self) -> MnResult<()> {
        self.weights.validate()?;
        if self.max_fallbacks > 10 {
            return Err(MnError::config(
                "scheduler.max_fallbacks",
                "重试次数上限过大（>10），可能导致部署风暴",
            ));
        }
        if self.launch_timeout_secs == 0 {
            return Err(MnError::config(
                "scheduler.launch_timeout_secs",
                "不能为 0",
            ));
        }
        if let Some(p) = self.max_price_per_hour {
            if !p.is_finite() || p <= 0.0 {
                return Err(MnError::config(
                    "scheduler.max_price_per_hour",
                    "必须为正数",
                ));
            }
        }
        Ok(())
    }
}

/// 推理引擎。
///
/// ## 为什么现在支持多种
///
/// 原基线是「推理引擎唯一 llama.cpp」，理由是参数语义与资源估算可以复用。
/// 但用户的实际需求分化了：有人要 llama.cpp 的单文件 GGUF 便利，
/// 有人已经有 vLLM / SGLang 的集群，有人要 TensorRT 的极限吞吐。
/// 因此改为**可配置选择**，默认仍是 llama.cpp（本地优先、单文件、零依赖）。
///
/// ## 诚实说明当前状态
///
/// 只有 `llama_cpp` 是完整实现并经过验证的。`ollama` 走它自己的
/// 服务进程，适配中。其余三种（vLLM / SGLang / TensorRT-LLM）
/// 需要 safetensors 或编译产物，与本项目的 GGUF 台账模型不同，
/// 目前仅登记为「可选但未验证」——选中时部署会明确报错，
/// 不会假装能跑。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InferenceEngine {
    /// llama.cpp 的 `llama-server`（默认，完整支持）
    #[default]
    LlamaCpp,
    /// Ollama 服务（适配中）
    Ollama,
    /// vLLM（未验证）
    Vllm,
    /// SGLang（未验证）
    Sglang,
    /// NVIDIA TensorRT-LLM（未验证）
    TensorrtLlm,
}

impl InferenceEngine {
    /// 全部可选引擎（供 UI 下拉与文档生成）。
    pub fn all() -> &'static [InferenceEngine] {
        &[
            Self::LlamaCpp,
            Self::Ollama,
            Self::Vllm,
            Self::Sglang,
            Self::TensorrtLlm,
        ]
    }

    /// 稳定标识（配置值 / HTTP 入参）。
    pub fn id(self) -> &'static str {
        match self {
            Self::LlamaCpp => "llama_cpp",
            Self::Ollama => "ollama",
            Self::Vllm => "vllm",
            Self::Sglang => "sglang",
            Self::TensorrtLlm => "tensorrt_llm",
        }
    }

    /// 展示名。
    pub fn display_name(self) -> &'static str {
        match self {
            Self::LlamaCpp => "llama.cpp",
            Self::Ollama => "Ollama",
            Self::Vllm => "vLLM",
            Self::Sglang => "SGLang",
            Self::TensorrtLlm => "TensorRT-LLM",
        }
    }

    /// 从字符串解析。
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "llama_cpp" | "llamacpp" | "llama.cpp" => Some(Self::LlamaCpp),
            "ollama" => Some(Self::Ollama),
            "vllm" => Some(Self::Vllm),
            "sglang" => Some(Self::Sglang),
            "tensorrt_llm" | "tensorrt" | "trt" => Some(Self::TensorrtLlm),
            _ => None,
        }
    }

    /// 当前是否**已实现**。
    ///
    /// UI 据此标注状态，避免用户选了一个其实跑不起来的引擎。
    ///
    /// - `LlamaCpp`：完整实现（拉起 llama-server 进程）
    /// - `Ollama`：复用已运行的守护进程（不拉进程，没在跑就报错）
    /// - 其余：未适配，选中后部署会明确拒绝
    pub fn is_supported(self) -> bool {
        matches!(self, Self::LlamaCpp | Self::Ollama)
    }

    /// 是否复用外部常驻服务（而非本进程拉起）。
    pub fn reuses_external_service(self) -> bool {
        matches!(self, Self::Ollama)
    }

    /// 该引擎要求的模型格式（用于下载/部署时的提示）。
    pub fn required_format(self) -> &'static str {
        match self {
            Self::LlamaCpp | Self::Ollama => "gguf",
            Self::Vllm | Self::Sglang | Self::TensorrtLlm => "safetensors",
        }
    }

    /// 可执行文件名（用于探测是否已安装）。
    pub fn binary_name(self) -> &'static str {
        match self {
            Self::LlamaCpp => "llama-server",
            Self::Ollama => "ollama",
            Self::Vllm => "vllm",
            Self::Sglang => "sglang",
            Self::TensorrtLlm => "trtllm-serve",
        }
    }
}

/// 本地部署默认参数。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LocalConfig {
    /// llama-server 可执行文件路径。`None` 表示在 PATH 中查找。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine_path: Option<PathBuf>,
    /// 默认上下文长度（请求未指定时使用）
    #[serde(default = "default_ctx")]
    pub default_context: u32,
    /// 上下文长度下限。低于此值不认为「本地可用」。
    #[serde(default = "default_min_ctx")]
    pub min_context: u32,
    /// 默认 KV 精度
    #[serde(default = "default_kv")]
    pub kv_cache: KvCacheType,
    /// 默认批大小
    #[serde(default = "default_batch")]
    pub batch_size: u32,
    /// 是否允许纯 CPU 回退
    #[serde(default = "yes")]
    pub allow_cpu_fallback: bool,
    /// 是否允许部分卸载
    #[serde(default = "yes")]
    pub allow_partial_offload: bool,
    /// 端口分配起始值（网关会自动寻找空闲端口）
    #[serde(default = "default_port_base")]
    pub port_range_start: u16,
    /// 端口分配结束值
    #[serde(default = "default_port_end")]
    pub port_range_end: u16,
    /// 启动 llama-server 时额外注入的环境变量。
    ///
    /// 典型用途：动态库搜索路径。例如 Homebrew 安装的 `llama-server`
    /// 在部分版本上缺少 `LC_RPATH`，需要
    /// `DYLD_FALLBACK_LIBRARY_PATH=/usr/local/lib` 才能找到同目录的 `.dylib`。
    /// Mangosteen 会自动探测常见的库目录并注入，此配置用于「自动探测失败时
    /// 手动指定」，而不是让用户每次都手写。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub engine_env: BTreeMap<String, String>,
    /// 是否允许自动注入动态库搜索路径（默认开启）。
    ///
    /// 关闭后只使用 `engine_env` 显式指定的值，便于排查"是不是注入导致的"。
    #[serde(default = "yes")]
    pub auto_library_path: bool,
    /// 是否用 `mlock` 把模型锁在物理内存里（默认 **false**）。
    ///
    /// 默认关闭的理由：锁定内存能避免换页、让延迟更稳定，但代价是
    /// **挤压系统其它部分**。个人桌面机通常同时开着浏览器等程序，
    /// 锁定模型会触发内存压缩，实测反而更慢。
    ///
    /// 只有专用机 / 服务器这类「内存几乎只给推理用」的场景才建议开启。
    #[serde(default)]
    pub mlock: bool,
    /// 推理引擎（默认 llama.cpp）。
    ///
    /// 可配置是刻意的：不同用户已有不同栈（llama.cpp / Ollama / vLLM…），
    /// 强制单一引擎会把已经装好别的引擎的人挡在门外。
    #[serde(default)]
    pub engine: InferenceEngine,
}

fn default_ctx() -> u32 {
    8192
}
fn default_min_ctx() -> u32 {
    2048
}
fn default_kv() -> KvCacheType {
    KvCacheType::F16
}
fn default_batch() -> u32 {
    512
}
fn default_port_base() -> u16 {
    11_435
}
fn default_port_end() -> u16 {
    11_999
}

impl Default for LocalConfig {
    fn default() -> Self {
        Self {
            engine_path: None,
            default_context: default_ctx(),
            min_context: default_min_ctx(),
            kv_cache: default_kv(),
            batch_size: default_batch(),
            allow_cpu_fallback: true,
            allow_partial_offload: true,
            port_range_start: default_port_base(),
            port_range_end: default_port_end(),
            engine_env: BTreeMap::new(),
            auto_library_path: true,
            mlock: false,
            engine: InferenceEngine::LlamaCpp,
        }
    }
}

impl LocalConfig {
    /// 校验。
    pub fn validate(&self) -> MnResult<()> {
        if self.default_context < self.min_context {
            return Err(MnError::config(
                "local.default_context",
                format!(
                    "默认上下文 {} 小于下限 {}",
                    self.default_context, self.min_context
                ),
            ));
        }
        if self.min_context < 512 {
            return Err(MnError::config(
                "local.min_context",
                "下限不得低于 512（再小无实用价值）",
            ));
        }
        if self.batch_size == 0 || self.batch_size > 8192 {
            return Err(MnError::config(
                "local.batch_size",
                "必须在 1..=8192 之间",
            ));
        }
        if self.port_range_start >= self.port_range_end {
            return Err(MnError::config(
                "local.port_range_start",
                "端口区间起始必须小于结束",
            ));
        }
        if self.port_range_start < 1024 {
            return Err(MnError::config(
                "local.port_range_start",
                "不得使用特权端口（<1024）",
            ));
        }
        Ok(())
    }

    /// 端口区间内某个偏移对应的端口（越界时从起点回绕）。
    pub fn port_at_offset(&self, offset: u32) -> u16 {
        let span = (self.port_range_end - self.port_range_start) as u32 + 1;
        self.port_range_start + (offset % span) as u16
    }
}

/// 模型存储与访问控制配置。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StorageConfig {
    /// 模型库根目录
    pub models_dir: PathBuf,
    /// 下载缓存目录
    pub cache_dir: PathBuf,
    /// 允许访问的本地路径白名单（前缀匹配）。
    ///
    /// **文件访问控制模块**据此拒绝越权读取。空表示仅允许 `models_dir` / `cache_dir`。
    #[serde(default)]
    pub allowed_read_roots: Vec<PathBuf>,
    /// 是否在**登记时**立即计算 SHA-256。
    ///
    /// 默认 `false`：7B 模型约 4.5 GiB，全量哈希在机械盘上需数十秒，
    /// 会让 `models add` 的体验极差。哈希在 `models verify` 时按需计算
    /// （首次计算并记录，后续比对），既能校验完整性又不阻塞登记。
    #[serde(default)]
    pub verify_hash: bool,
    /// 单个模型文件体积上限（字节）。防止误指向超大文件。
    #[serde(default = "default_max_model_size")]
    pub max_model_bytes: u64,
    /// 默认模型源名（`models pull` 未写 `<源>:` 前缀时使用）
    #[serde(default = "default_model_source")]
    pub default_source: String,
}

fn default_model_source() -> String {
    "modelscope".to_string()
}

fn default_max_model_size() -> u64 {
    200 * (1u64 << 30) // 200 GiB
}

impl StorageConfig {
    /// 校验。
    pub fn validate(&self) -> MnResult<()> {
        if self.models_dir.as_os_str().is_empty() {
            return Err(MnError::config("storage.models_dir", "不能为空"));
        }
        if self.max_model_bytes == 0 {
            return Err(MnError::config(
                "storage.max_model_bytes",
                "不能为 0",
            ));
        }
        Ok(())
    }

    /// 给定路径是否在允许读取的范围内。
    ///
    /// 授权范围 = `models_dir` ∪ `cache_dir` ∪ `allowed_read_roots`。
    /// 使用**规范化的词法前缀比较**，不跟随符号链接解析（避免 TOCTOU）。
    pub fn is_read_allowed(&self, path: &Path) -> bool {
        let mut roots: Vec<&Path> = vec![self.models_dir.as_path(), self.cache_dir.as_path()];
        roots.extend(self.allowed_read_roots.iter().map(|p| p.as_path()));
        roots.iter().any(|root| lexical_starts_with(path, root))
    }
}

/// 词法层面的前缀比较：`path` 是否位于 `root` 之下（或等于 `root`）。
///
/// 与 `Path::starts_with` 不同，这里先做 `..` 归约，防止
/// `/models/../../etc/passwd` 这类绕过。
pub fn lexical_starts_with(path: &Path, root: &Path) -> bool {
    let p = lexical_normalize(path);
    let r = lexical_normalize(root);
    p.starts_with(&r)
}

/// 纯词法归一化：消解 `.` 与 `..`，不触碰文件系统。
pub fn lexical_normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                // 无法再上溯时保留 `..`（相对路径场景）
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        out
    }
}

/// 网关配置。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GatewayConfig {
    /// 网关监听端口
    #[serde(default = "default_gateway_port")]
    pub port: u16,
    /// 网关监听地址。默认仅本机。
    #[serde(default = "default_loopback")]
    pub host: String,
    /// 是否要求客户端携带 API Key
    #[serde(default)]
    pub require_api_key: bool,
    /// 网关自身的 API Key（`require_api_key` 为真时必填）
    #[serde(default)]
    pub api_key: SecretRef,
}

fn default_gateway_port() -> u16 {
    crate::DEFAULT_GATEWAY_PORT
}
fn default_loopback() -> String {
    "127.0.0.1".to_string()
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            port: default_gateway_port(),
            host: default_loopback(),
            require_api_key: false,
            api_key: SecretRef::None,
        }
    }
}

impl GatewayConfig {
    /// 校验。
    pub fn validate(&self) -> MnResult<()> {
        if self.port < 1024 {
            return Err(MnError::config(
                "gateway.port",
                "不得使用特权端口（<1024）",
            ));
        }
        if self.require_api_key && self.api_key.is_none() {
            return Err(MnError::config(
                "gateway.api_key",
                "require_api_key 为真时必须提供 api_key",
            ));
        }
        Ok(())
    }
}

/// 根配置。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    /// 配置版本，用于未来迁移
    #[serde(default = "default_config_version")]
    pub config_version: u32,
    /// 网关配置
    #[serde(default)]
    pub gateway: GatewayConfig,
    /// 调度策略
    #[serde(default)]
    pub scheduler: SchedulerConfig,
    /// 本地部署
    #[serde(default)]
    pub local: LocalConfig,
    /// 云端通用配置
    #[serde(default)]
    pub cloud: CloudConfig,
    /// 存储与访问控制
    pub storage: StorageConfig,
    /// 云服务商表（键为配置内唯一名）
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
    /// 模型源表（键为源名）。为空时使用内置 ModelScope + HuggingFace。
    ///
    /// 配置驱动：新增 / 修改模型源不需要改代码。厂商改版时可在配置里
    /// 覆盖 `url_template` 修正。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub model_sources: BTreeMap<String, crate::source::ModelSource>,
}

fn default_config_version() -> u32 {
    1
}

impl Config {
    /// 组装模型源注册表。
    ///
    /// 内置 ModelScope + HuggingFace 始终存在；配置里的 `model_sources`
    /// 按**同名覆盖**合并。这样用户可以：
    /// * 只写一行 `[model_sources.modelscope] url_template = "..."` 修正厂商改版；
    /// * 或新增自定义源，不必等新版本。
    pub fn model_source_registry(&self) -> MnResult<crate::source::ModelSourceRegistry> {
        // 内置源清单只能有一处定义：早先这里手写了 modelscope + huggingface，
        // 而 `ModelSourceRegistry::builtin()` 里也有一份——新增 hf-mirror 时
        // 只改了后者，于是配置路径上永远看不到新源。
        // 现在统一从 builtin() 取，避免两处再次分叉。
        let mut sources: Vec<crate::source::ModelSource> =
            crate::source::ModelSourceRegistry::builtin()
                .all()
                .to_vec();
        for (name, cfg) in &self.model_sources {
            let mut s = cfg.clone();
            if s.name.trim().is_empty() {
                s.name = name.clone();
            }
            match sources.iter_mut().find(|x| x.name == s.name) {
                Some(slot) => *slot = s,
                None => sources.push(s),
            }
        }
        crate::source::ModelSourceRegistry::new(
            sources,
            self.storage.default_source.clone(),
        )
    }

    /// 校验全部配置。
    pub fn validate(&self) -> MnResult<()> {
        if self.config_version == 0 {
            return Err(MnError::config("config_version", "不能为 0"));
        }
        self.gateway.validate()?;
        self.scheduler.validate()?;
        self.local.validate()?;
        self.storage.validate()?;
        for (k, p) in &self.providers {
            p.validate(k)?;
        }
        // 至少一个启用的服务商，除非用户明确只要本地算力
        Ok(())
    }

    /// 已启用的服务商名列表（有序）。
    pub fn enabled_providers(&self) -> Vec<&str> {
        self.providers
            .iter()
            .filter(|(_, p)| p.enabled)
            .map(|(k, _)| k.as_str())
            .collect()
    }

    /// 是否有任何云端能力可用。
    pub fn has_cloud_capability(&self) -> bool {
        !self.enabled_providers().is_empty()
    }

    /// 从 TOML 文本解析并校验。
    pub fn from_toml(text: &str) -> MnResult<Self> {
        let cfg: Config = toml::from_str(text)
            .map_err(|e| MnError::config("toml", format!("解析失败：{e}")))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// 序列化为 TOML（密钥按引用形式输出，不泄露明文）。
    pub fn to_toml(&self) -> MnResult<String> {
        toml::to_string_pretty(self)
            .map_err(|e| MnError::config("toml", format!("序列化失败：{e}")))
    }

    /// 从文件加载。
    pub fn load(path: &Path) -> MnResult<Self> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            MnError::ConfigIo {
                path: path.to_path_buf(),
                reason: e.to_string(),
            }
        })?;
        Config::from_toml(&text).map_err(|e| match e {
            MnError::Config { field, reason } => MnError::ConfigIo {
                path: path.to_path_buf(),
                reason: format!("{field}: {reason}"),
            },
            other => other,
        })
    }

    /// 写入文件（自动创建父目录）。
    pub fn save(&self, path: &Path) -> MnResult<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| MnError::io(format!("创建目录 {}", parent.display()), &e))?;
        }
        let text = self.to_toml()?;
        std::fs::write(path, text)
            .map_err(|e| MnError::io(format!("写入 {}", path.display()), &e))
    }

    /// 内置示例配置。`mangosteen init` 输出此内容。
    pub fn sample(models_dir: PathBuf, cache_dir: PathBuf) -> Self {
        let mut providers = BTreeMap::new();
        providers.insert(
            "autodl".to_string(),
            ProviderConfig {
                driver: "autodl".into(),
                enabled: true,
                display_name: Some("AutoDL".into()),
                secret: SecretRef::Env("AUTODL_API_TOKEN".into()),
                options: BTreeMap::new(),
                priority: 1.0,
                profile: None,
            },
        );
        providers.insert(
            "ucloud".to_string(),
            ProviderConfig {
                driver: "ucloud".into(),
                enabled: true,
                display_name: Some("优云智算".into()),
                secret: SecretRef::Env("UCLOUD_API_KEY".into()),
                options: BTreeMap::new(),
                priority: 1.0,
                profile: None,
            },
        );
        providers.insert(
            "aliyun".to_string(),
            ProviderConfig {
                driver: "aliyun".into(),
                enabled: false,
                display_name: Some("阿里云".into()),
                secret: SecretRef::Env("ALIBABA_CLOUD_ACCESS_KEY_SECRET".into()),
                options: BTreeMap::from([(
                    "access_key_id".to_string(),
                    "LTAI********".to_string(),
                )]),
                priority: 1.0,
                profile: None,
            },
        );
        Self {
            config_version: 1,
            gateway: GatewayConfig::default(),
            scheduler: SchedulerConfig::default(),
            local: LocalConfig::default(),
            cloud: CloudConfig::default(),
            storage: StorageConfig {
                models_dir,
                cache_dir,
                allowed_read_roots: Vec::new(),
                verify_hash: false,
                max_model_bytes: default_max_model_size(),
                default_source: default_model_source(),
            },
            providers,
            model_sources: BTreeMap::new(),
        }
    }
}

/// 配置文件的候选路径。
///
/// 平台差异（`~/Library/Application Support` vs `~/.config` vs `%APPDATA%`）
/// 由 `mangosteen-platform` 提供，这里只定义结构。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigPaths {
    /// 显式指定的路径（最高优先级）
    pub explicit: Option<PathBuf>,
    /// 用户配置目录（平台相关）
    pub user_config_dir: PathBuf,
    /// 系统级配置目录（只读兜底）
    pub system_config_dir: Option<PathBuf>,
}

impl ConfigPaths {
    /// 按优先级返回待尝试的配置文件路径。
    pub fn candidates(&self) -> Vec<PathBuf> {
        let mut v = Vec::new();
        if let Some(p) = &self.explicit {
            v.push(p.clone());
        }
        v.push(self.user_config_dir.join("config.toml"));
        if let Some(sys) = &self.system_config_dir {
            v.push(sys.join("config.toml"));
        }
        v
    }

    /// 找到第一个存在的配置文件。
    pub fn discover(&self) -> Option<PathBuf> {
        self.candidates().into_iter().find(|p| p.is_file())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Quantization;

    fn base_config() -> Config {
        Config::sample(PathBuf::from("/tmp/m/models"), PathBuf::from("/tmp/m/cache"))
    }

    #[test]
    fn sample_config_is_valid() {
        assert!(base_config().validate().is_ok());
    }

    #[test]
    fn sample_roundtrips_through_toml() {
        let c = base_config();
        let text = c.to_toml().unwrap();
        let back = Config::from_toml(&text).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn tomllib_defaults_fill_missing_sections() {
        // 只给最小必填（storage），其余取默认
        let text = r#"
[storage]
models_dir = "/m"
cache_dir = "/c"
"#;
        let c = Config::from_toml(text).unwrap();
        assert_eq!(c.gateway.port, crate::DEFAULT_GATEWAY_PORT);
        assert!(c.scheduler.local_first, "local_first 必须默认 true");
        assert_eq!(c.local.default_context, default_ctx());
        assert!(c.providers.is_empty());
        assert!(!c.has_cloud_capability());
    }

    #[test]
    fn disabled_providers_not_counted() {
        let c = base_config();
        let enabled = c.enabled_providers();
        assert!(enabled.contains(&"autodl"));
        assert!(enabled.contains(&"ucloud"));
        assert!(!enabled.contains(&"aliyun")); // 示例里默认关闭
    }

    #[test]
    fn secret_inline_and_env() {
        let s = SecretRef::Inline("abc".into());
        assert_eq!(s.resolve(|_| None).unwrap(), "abc");
        assert_eq!(s.redacted(), "<inline:***redacted***>");

        let s = SecretRef::Env("MY_KEY".into());
        assert_eq!(
            s.resolve(|k| if k == "MY_KEY" { Some("v".into()) } else { None })
                .unwrap(),
            "v"
        );
        assert!(s.resolve(|_| None).is_err());
        assert_eq!(s.redacted(), "${MY_KEY}");
    }

    #[test]
    fn secret_none_resolves_empty() {
        assert_eq!(SecretRef::None.resolve(|_| None).unwrap(), "");
        assert!(SecretRef::None.is_none());
    }

    #[test]
    fn secret_empty_inline_rejected() {
        assert!(SecretRef::Inline(String::new()).resolve(|_| None).is_err());
    }

    #[test]
    fn storage_access_control_blocks_escape() {
        let s = StorageConfig {
            models_dir: PathBuf::from("/opt/mangosteen/models"),
            cache_dir: PathBuf::from("/opt/mangosteen/cache"),
            allowed_read_roots: vec![PathBuf::from("/data/gguf")],
            verify_hash: false,
            max_model_bytes: 1 << 30,
            default_source: default_model_source(),
        };
        assert!(s.is_read_allowed(Path::new("/opt/mangosteen/models/q.gguf")));
        assert!(s.is_read_allowed(Path::new("/data/gguf/other.gguf")));
        assert!(!s.is_read_allowed(Path::new("/etc/passwd")));
        // 路径穿越必须被拦住
        assert!(!s.is_read_allowed(Path::new("/opt/mangosteen/models/../../etc/passwd")));
        // 前缀相同但不是子目录，也不应放行
        assert!(!s.is_read_allowed(Path::new("/opt/mangosteen/models-evil/x.gguf")));
    }

    #[test]
    fn lexical_normalize_handles_dot_segments() {
        assert_eq!(
            lexical_normalize(Path::new("/a/b/../c/./d")),
            PathBuf::from("/a/c/d")
        );
        assert_eq!(lexical_normalize(Path::new("")), PathBuf::from("."));
        assert_eq!(lexical_normalize(Path::new("/a/b/..")), PathBuf::from("/a"));
    }

    #[test]
    fn local_config_validation() {
        let mut l = LocalConfig::default();
        assert!(l.validate().is_ok());

        l.default_context = 1024;
        l.min_context = 2048;
        assert!(l.validate().is_err());

        let mut l = LocalConfig::default();
        l.port_range_start = 80;
        assert!(l.validate().is_err());

        let mut l = LocalConfig::default();
        l.batch_size = 0;
        assert!(l.validate().is_err());
    }

    #[test]
    fn port_allocation_wraps() {
        let l = LocalConfig {
            port_range_start: 11_435,
            port_range_end: 11_437,
            ..Default::default()
        };
        assert_eq!(l.port_at_offset(0), 11_435);
        assert_eq!(l.port_at_offset(2), 11_437);
        assert_eq!(l.port_at_offset(3), 11_435); // 回绕
        assert_eq!(l.port_at_offset(100), 11_436);
    }

    #[test]
    fn scheduler_rejects_excessive_retries() {
        let mut s = SchedulerConfig::default();
        assert!(s.validate().is_ok());
        s.max_fallbacks = 100;
        assert!(s.validate().is_err());
    }

    #[test]
    fn gateway_requires_key_when_enabled() {
        let mut g = GatewayConfig::default();
        assert!(g.validate().is_ok());
        g.require_api_key = true;
        assert!(g.validate().is_err(), "开启鉴权但无密钥必须报错");
        g.api_key = SecretRef::Env("GATEWAY_KEY".into());
        assert!(g.validate().is_ok());
    }

    #[test]
    fn config_paths_priority_order() {
        let p = ConfigPaths {
            explicit: Some(PathBuf::from("/explicit/config.toml")),
            user_config_dir: PathBuf::from("/home/u/.config/mangosteen"),
            system_config_dir: Some(PathBuf::from("/etc/mangosteen")),
        };
        let c = p.candidates();
        assert_eq!(c.len(), 3);
        assert_eq!(c[0], PathBuf::from("/explicit/config.toml"));
        assert_eq!(c[1], PathBuf::from("/home/u/.config/mangosteen/config.toml"));
        assert_eq!(c[2], PathBuf::from("/etc/mangosteen/config.toml"));
    }

    #[test]
    fn invalid_toml_reports_field() {
        let text = "[storage]\nmodels_dir = ''\ncache_dir = '/c'\n";
        let e = Config::from_toml(text).unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("storage.models_dir"), "got: {msg}");
    }

    #[test]
    fn quant_and_kv_parse_from_config_text() {
        let text = r#"
[storage]
models_dir = "/m"
cache_dir = "/c"

[local]
kv_cache = "q8_0"
"#;
        let c = Config::from_toml(text).unwrap();
        assert_eq!(c.local.kv_cache, KvCacheType::Q8_0);
        // 确保 Quantization 的 serde 表达与文本形式一致
        let q: Quantization = "Q4_K_M".parse().unwrap();
        assert_eq!(q, Quantization::Q4KM);
    }
}

#[cfg(test)]
mod registry_consistency_tests {
    use super::*;

    /// 默认配置下，注册表必须包含全部内置源。
    ///
    /// 这条守的是一个真实踩过的坑：`model_source_registry()` 曾手写
    /// `vec![modelscope, huggingface]`，与 `ModelSourceRegistry::builtin()`
    /// 形成两份清单。新增 hf-mirror 时只改了后者，导致配置路径上
    /// 永远报「未知模型源 hf-mirror」，而 UI 的默认源正是它。
    #[test]
    fn default_config_exposes_all_builtin_sources() {
        let cfg = Config::sample("/tmp/models".into(), "/tmp/cache".into());
        let reg = cfg.model_source_registry().unwrap();

        for expected in ["modelscope", "hf", "hf-mirror"] {
            assert!(
                reg.get(expected).is_some(),
                "内置源 `{expected}` 不在注册表里——内置清单又分叉了"
            );
        }
    }

    /// 只有 hf-mirror / hf 支持关键字搜索；modelscope 必须明确不支持。
    #[test]
    fn search_capability_matches_reality() {
        let reg = crate::source::ModelSourceRegistry::builtin();
        assert!(reg
            .get("hf-mirror")
            .and_then(|s| s.search_url_template.as_ref())
            .is_some());
        assert!(
            reg.get("modelscope")
                .and_then(|s| s.search_url_template.as_ref())
                .is_none(),
            "ModelScope 没有公开检索接口，不应声明支持搜索"
        );
    }
}
