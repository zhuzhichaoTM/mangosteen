//! llama.cpp 引擎集成。
//!
//! 归属：**算力执行层 · 本地算力链路 · 核心内核**。
//!
//! ## 唯一推理引擎
//!
//! 按「推理引擎唯一性原则」，本模块只支持 llama.cpp 的 `llama-server` 可执行文件。
//! 不提供 vLLM / TensorRT-LLM 的适配点——即使将来需要，也需用户明确确认后
//! 才能在架构基线层面变更。
//!
//! ## 生命周期
//!
//! ```text
//!  locate()  找到 llama-server
//!     │
//!  launch()  拉起进程 + 等待 /health 就绪
//!     │
//!  status()  探活（进程 + HTTP）
//!     │
//!  shutdown() 优雅终止 → 超时强杀
//! ```
//!
//! ## 为什么 `locate` 失败要区分「未安装」与「不可执行」
//!
//! 前者是常见的首次使用场景，应给出安装指引；后者通常意味着用户指定了坏路径，
//! 必须明确指出。两者都返回 [`MnError::EngineLaunch`]（可重试 → 触发云端降级），
//! 但 `reason` 文案不同，CLI 会把它们翻译成不同的建议。

use std::path::PathBuf;
use std::time::Duration;

use mangosteen_core::{MnError, MnResult};
use mangosteen_platform::process;

/// 随包运行时的子目录名，形如 `darwin-x86_64`。
///
/// 与 Ollama 的 runner 分目录思路一致：同一份发布件可以携带多个平台的
/// 运行时（用于 CI 产物合并），启动时只挑当前平台的那一份。
///
/// 平台标识统一取自 `mangosteen_platform`，本层不再自写 `cfg!(target_os)`。
fn runtime_subdir_name() -> Option<String> {
    let os = match mangosteen_platform::os_name() {
        "macos" => "darwin",
        "windows" => "windows",
        "linux" => "linux",
        _ => return None,
    };
    let arch = match mangosteen_platform::arch_name() {
        "aarch64" => "aarch64",
        "x86_64" => "x86_64",
        _ => return None,
    };
    Some(format!("{os}-{arch}"))
}

/// 引擎运行状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineStatus {
    /// 进程不存在
    Absent,
    /// 进程在跑但 HTTP 未就绪（模型加载中）
    Loading {
        /// 已等待秒数
        elapsed_secs: u64,
    },
    /// 进程在跑且 HTTP 已就绪
    Serving,
    /// 进程已退出
    Exited {
        /// 退出码
        code: Option<i32>,
    },
}

impl EngineStatus {
    /// 是否可承载推理请求。
    pub fn is_serving(&self) -> bool {
        matches!(self, EngineStatus::Serving)
    }

    /// 是否已终止。
    pub fn is_dead(&self) -> bool {
        matches!(self, EngineStatus::Absent | EngineStatus::Exited { .. })
    }

    /// 人类可读描述。
    pub fn describe(&self) -> String {
        match self {
            EngineStatus::Absent => "未启动".to_string(),
            EngineStatus::Loading { elapsed_secs } => {
                format!("模型加载中（已等待 {elapsed_secs}s）")
            }
            EngineStatus::Serving => "服务就绪".to_string(),
            EngineStatus::Exited { code: Some(c) } => format!("进程已退出（退出码 {c}）"),
            EngineStatus::Exited { code: None } => "进程被信号终止".to_string(),
        }
    }
}



/// llama.cpp 引擎管理器。
///
/// 负责定位二进制、构造参数、拉起进程。**无状态**，可反复使用。
///
/// ## 运行时布局（参照 Ollama 的集成方式）
///
/// Ollama 不把 llama.cpp 当成「系统里已经装好的东西」，而是自己分发一套
/// runner：`llama-server` 可执行文件与它的动态库放在同一个目录树里，
/// 启动时由宿主进程**显式指定库搜索路径**，因此不依赖用户的
/// Homebrew / 系统包管理器状态，也不受 `LC_RPATH` 是否正确的影响。
///
/// Mangosteen 采用同一思路，但优先复用系统已有引擎（降低分发体积）：
///
/// ```text
/// 1. 随包运行时：<exe_dir>/runtime/**       ← 若存在则优先，完全自包含
/// 2. 系统安装：  PATH 中的 llama-server     ← brew / 发行版包 / 手工安装
/// 3. 显式指定：  local.engine_path           ← 兜底，优先级最高
/// ```
///
/// 无论走哪条路径，**库搜索路径都由本模块计算后注入子进程**，
/// 不去假设二进制自带的 `LC_RPATH` / `RUNPATH` 是对的。
#[derive(Debug, Clone)]
pub struct LlamaCppEngine {
    /// 显式指定的二进制路径（配置 `local.engine_path`）
    explicit_path: Option<PathBuf>,
    /// 额外搜索目录（如随包分发的 `bin/`、`runtime/`）
    extra_dirs: Vec<PathBuf>,
    /// 日志目录（引擎输出重定向于此）
    log_dir: Option<PathBuf>,
    /// 用户显式指定的额外环境变量（配置 `local.engine_env`）
    engine_env: std::collections::BTreeMap<String, String>,
    /// 是否自动探测并注入动态库搜索路径
    auto_library_path: bool,
}

impl Default for LlamaCppEngine {
    fn default() -> Self {
        Self {
            explicit_path: None,
            extra_dirs: Vec::new(),
            log_dir: None,
            engine_env: std::collections::BTreeMap::new(),
            auto_library_path: true,
        }
    }
}

impl LlamaCppEngine {
    /// 新建。
    pub fn new(
        explicit_path: Option<PathBuf>,
        extra_dirs: Vec<PathBuf>,
        log_dir: Option<PathBuf>,
    ) -> Self {
        Self {
            explicit_path,
            extra_dirs,
            log_dir,
            ..Default::default()
        }
    }

    /// 设置用户显式指定的环境变量。
    pub fn with_engine_env(mut self, env: std::collections::BTreeMap<String, String>) -> Self {
        self.engine_env = env;
        self
    }

    /// 关闭自动库路径注入（排查「是不是注入导致的问题」时用）。
    pub fn with_auto_library_path(mut self, on: bool) -> Self {
        self.auto_library_path = on;
        self
    }

    /// 从配置构造。
    ///
    /// `extra_dirs` 默认包含：
    /// * 可执行文件同级目录与 `bin/`（把 llama-server 放在 mangosteen 旁边）
    /// * `runtime/` 及其子目录（随包分发的引擎，Ollama runner 风格布局）
    pub fn from_config(engine_path: Option<PathBuf>, log_dir: Option<PathBuf>) -> Self {
        let mut extra = Vec::new();
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                extra.push(dir.to_path_buf());
                extra.push(dir.join("bin"));
                // 随包运行时：runtime/ 下可能再按平台/架构分子目录
                extra.push(dir.join("runtime"));
                if let Some(sub) = runtime_subdir_name() {
                    let rt = dir.join("runtime").join(&sub);
                    extra.push(rt.clone());
                    extra.push(rt.join("bin"));
                }
            }
        }
        Self {
            explicit_path: engine_path,
            extra_dirs: extra,
            log_dir,
            ..Default::default()
        }
    }



    /// 定位 llama-server。
    ///
    /// 优先级：显式路径 → 额外目录 → `PATH`。
    pub fn locate(&self) -> MnResult<PathBuf> {
        // 1. 显式指定：必须存在且可执行，否则明确报错（不静默回退）
        if let Some(p) = &self.explicit_path {
            if !p.exists() {
                return Err(MnError::EngineLaunch {
                    binary: p.clone(),
                    reason: format!(
                        "配置项 local.engine_path 指向的文件不存在。请修正配置，\
                         或删除该配置项让 Mangosteen 自动在 PATH 中查找 {}",
                        mangosteen_platform::engine_binary_name()
                    ),
                });
            }
            if !process::is_executable(p) {
                return Err(MnError::EngineLaunch {
                    binary: p.clone(),
                    reason: "文件存在但没有可执行权限（Unix 下需 chmod +x）".to_string(),
                });
            }
            return Ok(p.clone());
        }

        // 2. 搜索
        match process::find_executable(mangosteen_platform::engine_binary_name(), &self.extra_dirs)
        {
            Some(p) => Ok(p),
            None => Err(MnError::EngineLaunch {
                binary: PathBuf::from(mangosteen_platform::engine_binary_name()),
                reason: format!(
                    "未找到 {}。Mangosteen 依赖 llama.cpp 的 llama-server 作为唯一推理引擎。\
                     安装方式：\n  · macOS/Linux: brew install llama.cpp\n  \
                     · Windows: 下载 llama.cpp release 后把 llama-server.exe 加入 PATH\n  \
                     · 或在配置文件中设置 local.engine_path 指向二进制绝对路径",
                    mangosteen_platform::engine_binary_name()
                ),
            }),
        }
    }





    /// 检查端口是否可用（供调度决策器的端口分配逻辑使用）。
    pub fn is_port_available(port: u16) -> bool {
        process::is_port_bindable(port)
    }
}

/// Ollama 引擎适配。
///
/// ## 与 llama.cpp 的根本差异
///
/// llama.cpp 是「一次部署 = 一个进程」：我们拉起 `llama-server`，
/// 它监听一个端口，我们拥有这个进程、可以随时杀掉它。
///
/// Ollama 是**常驻守护进程**：它自己在 11434 端口上服务所有模型，
/// 用 `ollama run` / API 拉起模型。我们既不该、也不能「为一次部署
/// 单独启动一个 Ollama」——那会和用户已有的 Ollama 冲突（端口占用）。
///
/// 因此这里的策略是**复用**：
/// 1. 探测 11434 上是否已有 Ollama 在服务；
/// 2. 有 → 直接用它的 OpenAI 兼容端点（`/v1`），我们不拥有该进程；
/// 3. 没有 → 明确报错，让用户自己启动 Ollama，
///    而不是偷偷替他拉起一个（那会与他的 Ollama 使用习惯冲突，
///    且我们无法替他管理模型文件）。
#[derive(Debug, Clone)]
pub struct OllamaEngine {
    /// 显式指定地址（配置 `local.engine_env.OLLAMA_HOST` 或默认）
    host: String,
    port: u16,
}

impl Default for OllamaEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl OllamaEngine {
    /// Ollama 默认监听端口。
    pub const DEFAULT_PORT: u16 = 11434;

    /// 新建，使用默认地址。
    pub fn new() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: Self::DEFAULT_PORT,
        }
    }

    /// 自定义地址（便于测试与自定义安装）。
    pub fn with_addr(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
        }
    }

    /// 监听端口（供展示与测试）。
    pub fn probe_port(&self) -> u16 {
        self.port
    }

    /// 监听地址（供错误提示与插件层使用）。
    pub fn host(&self) -> &str {
        &self.host
    }

    /// 探测 Ollama 是否已在运行。
    ///
    /// 用根路径 `/api/version`（Ollama 自带 API），比 `/v1/models` 更可靠：
    /// 后者在没有任何模型时也可能返回 200，无法区分「服务在跑」与
    /// 「服务没跑但端口被占用」。
    pub fn probe(&self) -> bool {
        process::probe_http_ok(
            &self.host,
            self.port,
            "/api/version",
            Duration::from_millis(800),
        )
    }

    /// 列出 Ollama 库里已有的模型名。
    ///
    /// 用 `/api/tags`（Ollama 原生接口）。拿不到时返回空——由调用方
    /// 决定是「明确报错」还是「放过」。
    pub fn list_models(&self) -> Vec<String> {
        let out = std::process::Command::new("curl")
            .arg("--fail")
            .arg("--silent")
            // 引擎服务通常在 localhost/内网，禁用环境代理避免临时代理
            // 消亡后探测集体失败
            .arg("--noproxy")
            .arg("*")
            .arg("--max-time")
            .arg("5")
            .arg(format!("http://{}:{}/api/tags", self.host, self.port))
            .output();
        let Ok(o) = out else { return Vec::new() };
        if !o.status.success() {
            return Vec::new();
        }
        let body = String::from_utf8_lossy(&o.stdout);
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) else {
            return Vec::new();
        };
        v.get("models")
            .and_then(|m| m.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.get("name").and_then(|n| n.as_str()))
                    .map(|s| s.to_string())
                    .collect()
            })
            .unwrap_or_default()
    }


}

/// 通过端口反查监听进程的 PID（用于展示外部服务）。
///
/// 用 `lsof -ti :port`，和平台上其它地方保持一致；
/// 查不到返回 `None`（不影响功能，只影响展示）。
fn pid_listening_on(port: u16) -> Option<u32> {
    let out = std::process::Command::new("lsof")
        .arg("-ti")
        .arg(format!(":{port}"))
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .and_then(|s| s.trim().parse::<u32>().ok())
}

#[cfg(test)]

#[cfg(test)]
mod tests {
    use super::*;
    use mangosteen_core::estimate::{AssessRequest, KvCacheType};
    use mangosteen_core::hardware::{Accelerator, AcceleratorVendor, HardwareProfile, HostProfile};
    use mangosteen_core::model::{ModelSpec, Quantization};

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mn-engine-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

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

    fn hw() -> HardwareProfile {
        HardwareProfile {
            os: "linux".into(),
            arch: "x86_64".into(),
            host: HostProfile {
                total_memory_bytes: 64 << 30,
                available_memory_bytes: 48 << 30,
                physical_cores: 12,
                logical_cores: 24,
                cpu_model: "t".into(),
            },
            accelerators: vec![Accelerator {
                name: "RTX 4090".into(),
                vendor: AcceleratorVendor::Nvidia,
                vram_bytes: 24 << 30,
                unified_memory_bytes: 0,
                compute_capability: Some("sm_89".into()),
                fp16_tflops: Some(165.0),
                memory_bandwidth_gbps: Some(1008.0),
            }],
        }
    }

    fn plan_with_port(port: u16, model: &str) -> LocalPlan {
        let a = mangosteen_core::estimate::assess(
            &spec(),
            &hw(),
            &AssessRequest::default()
                .with_context(4096)
                .with_kv_cache(KvCacheType::F16),
        );
        LocalPlan::from_assessment(&spec(), model, &a, port)
    }

    #[test]
    fn missing_explicit_path_is_actionable() {
        let e = LlamaCppEngine::new(
            Some(PathBuf::from("/nonexistent/llama-server")),
            vec![],
            None,
        );
        let err = e.locate().unwrap_err();
        assert!(matches!(err, MnError::EngineLaunch { .. }));
        assert!(err.to_string().contains("local.engine_path"));
        assert!(err.is_retryable(), "引擎缺失应触发云端降级");
    }

    #[test]
    fn not_installed_gives_install_hint() {
        // 用一个必定不存在的名字
        let e = LlamaCppEngine::new(None, vec![], None);
        // 无法控制 PATH，这里只验证错误类型与文案结构（若本机已装则跳过）
        if let Err(err) = e.locate() {
            assert!(matches!(err, MnError::EngineLaunch { .. }));
            let msg = err.to_string();
            assert!(msg.contains("llama-server"));
            assert!(msg.contains("llama.cpp"), "应给出安装指引：{msg}");
        }
    }

    #[test]
    fn explicit_path_wins_over_path_search() {
        let d = tmpdir("explicit");
        let fake = d.join("llama-server");
        std::fs::write(&fake, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
            let e = LlamaCppEngine::new(Some(fake.clone()), vec![], None);
            assert_eq!(e.locate().unwrap(), fake);
        }
        std::fs::remove_dir_all(&d).ok();
    }

    #[cfg(unix)]
    #[test]
    fn non_executable_explicit_path_is_rejected_with_reason() {
        let d = tmpdir("noexec");
        let f = d.join("llama-server");
        std::fs::write(&f, "not executable").unwrap();
        let e = LlamaCppEngine::new(Some(f.clone()), vec![], None);
        let err = e.locate().unwrap_err();
        assert!(err.to_string().contains("可执行权限"), "got {err}");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn extra_dirs_are_searched() {
        let d = tmpdir("extradir");
        let fake = d.join("llama-server");
        std::fs::write(&fake, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
            let e = LlamaCppEngine::new(None, vec![d.clone()], None);
            assert_eq!(e.locate().unwrap(), fake);
        }
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn engine_status_descriptions() {
        assert_eq!(EngineStatus::Absent.describe(), "未启动");
        assert_eq!(EngineStatus::Serving.describe(), "服务就绪");
        assert!(EngineStatus::Loading { elapsed_secs: 3 }
            .describe()
            .contains("加载中"));
        assert!(EngineStatus::Exited { code: Some(1) }
            .describe()
            .contains("退出码 1"));
        assert!(EngineStatus::Exited { code: None }
            .describe()
            .contains("信号"));
        assert!(EngineStatus::Serving.is_serving());
        assert!(EngineStatus::Absent.is_dead());
        assert!(EngineStatus::Exited { code: Some(0) }.is_dead());
        assert!(!EngineStatus::Loading { elapsed_secs: 1 }.is_serving());
    }

    #[test]
    fn from_config_includes_binary_sibling_dirs() {
        let e = LlamaCppEngine::from_config(None, None);
        // 至少应尝试当前可执行文件目录及其 bin/
        assert!(!e.extra_dirs.is_empty());
    }

    #[test]
    fn port_availability_probe() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(!LlamaCppEngine::is_port_available(port));
        drop(listener);
    }
}
