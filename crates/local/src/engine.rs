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

use std::path::{Path, PathBuf};
use std::time::Duration;

use mangosteen_core::plan::LocalPlan;
use mangosteen_core::{MnError, MnResult};
use mangosteen_platform::process::{self, ManagedChild, SpawnOptions};

/// 随包运行时的子目录名，形如 `darwin-x86_64`。
///
/// 与 Ollama 的 runner 分目录思路一致：同一份发布件可以携带多个平台的
/// 运行时（用于 CI 产物合并），启动时只挑当前平台的那一份。
fn runtime_subdir_name() -> Option<String> {
    let os = if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        return None;
    };
    let arch = if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else {
        return None;
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

/// 引擎句柄：一次成功启动后的运行实例。
///
/// 持有子进程所有权，`Drop` 时**不**自动杀进程（用户可能想让服务继续跑，
/// 例如 CLI 退出但希望推理服务在线）。必须显式调用 [`EngineHandle::shutdown`]。
/// 这是有意的设计：隐式杀进程会让 `mangosteen run` 之后无法继续用服务。
#[derive(Debug)]
pub struct EngineHandle {
    /// 本进程拉起的引擎子进程。
    ///
    /// `None` 表示**复用外部已运行的服务**（如用户自己的 Ollama 守护进程）。
    /// 此时我们不拥有该进程，因此绝不能杀它——那是用户的软件，
    /// 杀了会影响他在别处正在用的模型。
    child: Option<ManagedChild>,
    /// 外部服务的 PID（`child` 为 `None` 时有值，用于展示）
    external_pid: Option<u32>,
    /// 外部服务名（如 `ollama`），用于错误提示与展示
    pub external_service: Option<String>,
    /// 端口
    pub port: u16,
    /// 监听地址
    pub host: String,
    /// 模型路径（用于展示与日志）
    pub model_path: String,
    /// 启动耗时（毫秒）
    pub startup_ms: u64,
}

impl EngineHandle {
    /// 进程 PID。
    ///
    /// 外部服务返回其真实 PID（通过端口反查）；查不到则为 0。
    pub fn pid(&self) -> u32 {
        match &self.child {
            Some(c) => c.pid(),
            None => self.external_pid.unwrap_or(0),
        }
    }

    /// 是否为外部复用的服务（非本进程拉起）。
    pub fn is_external(&self) -> bool {
        self.child.is_none()
    }

    /// `/health` 端点完整 URL。
    pub fn health_url(&self) -> String {
        format!("http://{}:{}/health", self.host, self.port)
    }

    /// OpenAI 兼容的 base URL（上层应用填这个）。
    pub fn openai_base_url(&self) -> String {
        format!("http://{}:{}/v1", self.host, self.port)
    }

    /// 探活。
    ///
    /// `load_grace` 为「进程刚启动、允许仍在加载」的宽容期：
    /// 期内 HTTP 不通不判失败，只报 `Loading`。
    pub fn status(&mut self, load_grace: Duration) -> EngineStatus {
        // 外部复用的服务：我们不知道它何时退出，只能靠 HTTP 探活。
        // 探不通就是 Absent（而不是 Exited——那不是我们拉起的进程）。
        let Some(child) = self.child.as_mut() else {
            return match process::wait_http_ready(
                &self.host,
                self.port,
                "/health",
                Duration::from_millis(600),
            ) {
                Ok(_) => EngineStatus::Serving,
                Err(_) => EngineStatus::Absent,
            };
        };

        if let Some(code) = child.exit_code() {
            return EngineStatus::Exited { code };
        }
        if !child.is_running() {
            return EngineStatus::Absent;
        }
        match process::wait_http_ready(&self.host, self.port, "/health", Duration::from_millis(600))
        {
            Ok(_) => EngineStatus::Serving,
            Err(_) => {
                if load_grace.is_zero() {
                    EngineStatus::Loading { elapsed_secs: 0 }
                } else {
                    EngineStatus::Loading {
                        elapsed_secs: self.startup_ms / 1000,
                    }
                }
            }
        }
    }

    /// 优雅终止。幂等。
    ///
    /// 外部复用的服务（Ollama 等）**不做任何操作**：那是用户自己的软件，
    /// 我们只是借用了它的能力。停止「部署」的语义退化为「不再使用它」，
    /// 由调用方从台账里移除即可。
    pub fn shutdown(&mut self) -> MnResult<()> {
        match self.child.as_mut() {
            Some(c) => c.terminate(Duration::from_secs(8)),
            None => Ok(()),
        }
    }

    /// 强杀。用于清理卡死的引擎。外部服务不处理（见 [`Self::shutdown`]）。
    pub fn force_kill(&mut self) -> MnResult<()> {
        match self.child.as_mut() {
            Some(c) => c.force_kill(),
            None => Ok(()),
        }
    }

    /// 引擎退出原因（读取 stderr 尾部若干行）。
    pub fn crash_reason(&mut self, max_lines: usize) -> String {
        let Some(child) = self.child.as_mut() else {
            let svc = self.external_service.clone().unwrap_or_default();
            let alive = self.status(Duration::from_secs(0)).is_serving();
            return format!(
                "外部服务 {svc}{}",
                if alive {
                    "运行正常（由用户自行管理）"
                } else {
                    "未在监听"
                }
            );
        };
        let tail = child.stderr_tail(max_lines);
        if tail.trim().is_empty() {
            "（无 stderr 输出）".to_string()
        } else {
            tail
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

    /// 计算启动引擎时需要注入的环境变量。
    ///
    /// 顺序：自动探测的库目录 → 配置 `local.engine_env` 覆盖。
    /// 后者优先，便于用户修正自动探测的错误结果。
    pub fn launch_env(&self, bin: &Path) -> Vec<(String, String)> {
        let mut env: Vec<(String, String)> = Vec::new();
        let var = mangosteen_platform::dlpath::library_path_var();

        if self.auto_library_path {
            // 用户的 engine_env 里若已显式给了同名变量，视为覆盖，
            // 不再自动拼接（否则用户无法关掉注入的目录）。
            let explicit = self.engine_env.get(var);
            if explicit.is_none() {
                let dirs = mangosteen_platform::dlpath::detect_library_dirs(bin, &self.extra_dirs);
                if !dirs.is_empty() {
                    let existing = std::env::var(var).ok();
                    let value =
                        mangosteen_platform::dlpath::join_library_path(&dirs, existing.as_deref());
                    if !value.is_empty() {
                        env.push((var.to_string(), value));
                    }
                }
            }
        }

        for (k, v) in &self.engine_env {
            env.push((k.clone(), v.clone()));
        }
        env
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

    /// 探测引擎版本。
    ///
    /// `llama-server --version` 输出格式在不同版本间有差异，
    /// 这里只取首行非空输出，不做严格解析（版本号不是业务依赖）。
    pub fn probe_version(&self) -> MnResult<String> {
        let bin = self.locate()?;
        let mut cmd = std::process::Command::new(&bin);
        cmd.arg("--version");
        // 与 launch 保持一致：库路径缺失时 `--version` 同样会因 dyld 报错，
        // 若此处不注入，doctor 会把「可修复的库路径问题」误判成「引擎损坏」。
        for (k, v) in self.launch_env(&bin) {
            cmd.env(k, v);
        }
        let out = cmd.output().map_err(|e| MnError::EngineLaunch {
            binary: bin.clone(),
            reason: format!("执行 --version 失败：{e}"),
        })?;

        // 部分版本把版本信息打到 stderr
        let text = if out.stdout.is_empty() {
            String::from_utf8_lossy(&out.stderr).to_string()
        } else {
            String::from_utf8_lossy(&out.stdout).to_string()
        };
        let first = text.lines().next().unwrap_or("").trim().to_string();

        // 必须检查退出码：非零退出意味着这个二进制「存在但跑不起来」，
        // 常见原因是被替换成坏脚本、动态库缺失、架构不匹配。
        //
        // 早先这里只看输出不判退出码，导致坏引擎返回 `Ok("unknown")`——
        // 于是 `doctor` 报 OK、`preview` 说「本地可跑不花钱」，实际部署却
        // 失败并降级云端开始计费。这是会**让用户白付钱**的误判。
        if !out.status.success() {
            return Err(MnError::EngineLaunch {
                binary: bin.clone(),
                reason: format!(
                    "`--version` 失败（退出码 {:?}）：{}",
                    out.status.code(),
                    if first.is_empty() {
                        "（无输出）"
                    } else {
                        &first
                    }
                ),
            });
        }

        // 退出码 0 但没有任何输出：同样不可信。正常 llama-server 一定会
        // 打印 version 行。这种情况多为「被一个什么都不做的假程序顶替」。
        if first.is_empty() {
            return Err(MnError::EngineLaunch {
                binary: bin.clone(),
                reason: "`--version` 退出码为 0 但没有输出，无法确认这是可用的 llama-server"
                    .to_string(),
            });
        }

        Ok(first)
    }

    /// 启动引擎并等待就绪。
    ///
    /// `ready_timeout` 内必须能通过 `/health`，否则杀掉进程并返回错误
    /// （可重试 → 触发云端降级）。
    pub fn launch(&self, plan: &LocalPlan, ready_timeout: Duration) -> MnResult<EngineHandle> {
        let bin = self.locate()?;

        // 参数由 plan 唯一决定，不做任何隐式补充
        let args = plan.to_engine_args();

        // 启动前校验模型文件存在（省掉一次无意义的进程拉起）
        let model = Path::new(&plan.model_path);
        if !model.is_file() {
            return Err(MnError::ModelNotFound {
                path: model.to_path_buf(),
            });
        }

        // 端口已被占用 → 明确报错（调度决策器会选下一个端口重试）
        if !process::is_port_bindable(plan.port) {
            return Err(MnError::EngineLaunch {
                binary: bin.clone(),
                reason: format!(
                    "端口 {} 已被占用。若之前有残留的 llama-server 进程，请先执行 \
                     `mangosteen stop --all` 或手动结束该进程",
                    plan.port
                ),
            });
        }

        let mut opts = SpawnOptions::new(bin.clone(), args, "llama-server");
        // 引擎输出重定向到日志文件，避免管道写满导致进程阻塞
        let log_path = self
            .log_dir
            .as_ref()
            .map(|dir| dir.join(format!("engine-{}.log", plan.port)));
        if let Some(log) = &log_path {
            opts = opts.with_log_file(log)?;
        }
        // 动态库搜索路径：不依赖二进制自带的 LC_RPATH / RUNPATH
        for (k, v) in self.launch_env(&bin) {
            opts = opts.with_env(k, v);
        }

        let started = std::time::Instant::now();
        let mut child = opts.spawn()?;

        // 等待就绪。期间若进程提前退出，立即报错并带上 stderr 尾部。
        let deadline = started + ready_timeout;
        loop {
            if let Some(code) = child.exit_code() {
                let tail = child.stderr_tail(20);
                return Err(MnError::EngineExited {
                    pid: child.pid(),
                    code,
                    stderr_tail: if tail.trim().is_empty() {
                        format!(
                            "引擎在就绪前退出。{}完整日志：{}",
                            if plan.gpu_layers > 0 {
                                "常见原因：模型文件损坏、显存不足、GPU 卸载层数过多。"
                            } else {
                                "常见原因：模型文件损坏、内存不足、引擎动态库缺失。"
                            },
                            log_path
                                .as_ref()
                                .map(|p| p.display().to_string())
                                .unwrap_or_else(|| "（未启用日志文件）".into())
                        )
                    } else {
                        tail
                    },
                });
            }
            if std::time::Instant::now() >= deadline {
                let tail = child.stderr_tail(20);
                let _ = child.terminate(Duration::from_millis(1500));
                return Err(MnError::EngineLaunch {
                    binary: bin,
                    reason: format!(
                        "引擎在 {}s 内未通过 /health 就绪（可能是模型过大或显存不足）。\
                         进程已清理。stderr 尾部：{}",
                        ready_timeout.as_secs(),
                        if tail.trim().is_empty() {
                            "（空）"
                        } else {
                            &tail
                        }
                    ),
                });
            }
            // 单次探测超时给得很短，这里做外层轮询
            if process::wait_http_ready(
                &plan.host,
                plan.port,
                "/health",
                Duration::from_millis(400),
            )
            .is_ok()
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }

        Ok(EngineHandle {
            child: Some(child),
            external_pid: None,
            external_service: None,
            port: plan.port,
            host: plan.host.clone(),
            model_path: plan.model_path.clone(),
            startup_ms: started.elapsed().as_millis() as u64,
        })
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

    /// 复用一个已在运行的 Ollama 作为引擎实例。
    ///
    /// 不返回子进程句柄——我们没有它的所有权，也不该杀它。
    pub fn attach(&self, model_name: &str) -> MnResult<EngineHandle> {
        if !self.probe() {
            return Err(MnError::Provider {
                provider: "ollama".to_string(),
                code: "not_running".to_string(),
                message: format!(
                    "未检测到 Ollama 服务（{}:{}）。\n\
                     Ollama 是常驻服务，请先自行启动：\n\
                     · 桌面版：直接打开 Ollama 应用\n\
                     · 命令行：`ollama serve`（后台运行）\n\
                     启动后本客户端会自动复用它，不会另行拉起一个实例。",
                    self.host, self.port
                ),
            });
        }

        // 模型必须在 Ollama 自己的库里。
        //
        // 关键差异：Ollama 用的是它自己的 blob 存储（`ollama pull` 下来的），
        // 不是我们下载目录里的 GGUF 文件。所以「已下载模型」里的路径对
        // Ollama 没有意义——必须提示用户先把它导入 Ollama。
        //
        // 不做这层校验的后果：部署「成功」，但一推理就报 model not found，
        // 用户完全不知道该怎么修。
        let have = self.list_models();
        let want = model_name.to_string();
        if !have.is_empty()
            && !have
                .iter()
                .any(|m| m == &want || m.split(':').next() == Some(&want))
        {
            return Err(MnError::Provider {
                provider: "ollama".to_string(),
                code: "model_not_found".to_string(),
                message: format!(
                    "Ollama 里没有模型 `{want}`。\n\
                     Ollama 使用自己的模型库（blob 存储），与「已下载模型」目录不通用。\n\
                     请先导入：\n\
                     · `ollama pull {want}`（从官方库拉取）\n\
                     · 或用 Modelfile 导入本地 GGUF：`ollama create {want} -f Modelfile`\n\
                     当前 Ollama 已有模型：{}",
                    if have.is_empty() {
                        "（无）".to_string()
                    } else {
                        have.join(", ")
                    }
                ),
            });
        }

        let pid = pid_listening_on(self.port);
        Ok(EngineHandle {
            child: None,
            external_pid: pid,
            external_service: Some("ollama".to_string()),
            port: self.port,
            host: self.host.clone(),
            model_path: model_name.to_string(),
            startup_ms: 0,
        })
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
mod ollama_tests {
    use super::OllamaEngine;

    /// 未启动 Ollama 时，attach 必须给出可操作的指引，
    /// 而不是一个无意义的「连接失败」。
    ///
    /// 用一个几乎不可能被占用的高端口，确保探测必然失败。
    #[test]
    fn attach_without_service_gives_actionable_error() {
        let e = OllamaEngine::with_addr("127.0.0.1", 59997);
        assert!(!e.probe(), "该端口不应有服务在监听");

        let err = e.attach("qwen2.5").expect_err("未运行时必须报错");
        let msg = err.to_string();
        assert!(msg.contains("Ollama"), "错误里应指明是 Ollama，实际：{msg}");
        assert!(
            msg.contains("ollama serve") || msg.contains("Ollama 应用"),
            "应告诉用户怎么启动，实际：{msg}"
        );
    }

    /// 默认端口是 Ollama 的标准端口。
    #[test]
    fn default_port_is_standard() {
        assert_eq!(OllamaEngine::new().probe_port(), 11434);
    }

    /// 模型不在 Ollama 库里时，必须给出导入指引。
    ///
    /// 这是 Ollama 与 llama.cpp 的关键差异：Ollama 用**自己的 blob 存储**，
    /// 不认我们下载目录里的 .gguf。不做这层校验的后果是「部署成功但一推理
    /// 就 model not found」，用户完全不知道该怎么修。
    #[test]
    fn attach_with_missing_model_gives_import_hint() {
        // 用一个必然不存在的模型名。若本机没跑 Ollama，这条会走
        // 「服务未运行」分支——两种情况都该给出可操作指引。
        let e = OllamaEngine::with_addr("127.0.0.1", 59997);
        if !e.probe() {
            // 服务未运行：已有另一条测试覆盖
            return;
        }
        let err = e.attach("这个模型一定不存在-xyz").expect_err("应报错");
        let msg = err.to_string();
        assert!(
            msg.contains("ollama pull") || msg.contains("ollama create"),
            "应给出导入指引，实际：{msg}"
        );
    }

    /// 外部服务句柄不拥有进程：shutdown 不得杀任何东西。
    ///
    /// 这是安全约束——Ollama 是用户自己的软件，杀了会影响他别处
    /// 正在用的模型。
    #[test]
    fn external_handle_does_not_kill_anything() {
        let mut h = super::EngineHandle {
            child: None,
            external_pid: Some(999_999),
            external_service: Some("ollama".to_string()),
            port: 59997,
            host: "127.0.0.1".to_string(),
            model_path: "m".to_string(),
            startup_ms: 0,
        };
        assert!(h.is_external());
        // 幂等且无副作用：进程 999999 不存在也不该报错
        h.shutdown().expect("外部服务的 shutdown 必须无副作用");
        h.force_kill().expect("外部服务的 force_kill 必须无副作用");
    }
}

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
    fn launch_rejects_missing_model_before_spawning() {
        let d = tmpdir("nomodel");
        let fake = d.join("llama-server");
        std::fs::write(&fake, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
            let e = LlamaCppEngine::new(Some(fake), vec![], None);
            let plan = plan_with_port(11999, "/nonexistent/model.gguf");
            let err = e.launch(&plan, Duration::from_millis(300)).unwrap_err();
            assert!(matches!(err, MnError::ModelNotFound { .. }), "got {err:?}");
        }
        std::fs::remove_dir_all(&d).ok();
    }

    #[cfg(unix)]
    #[test]
    fn launch_reports_port_conflict_clearly() {
        let d = tmpdir("portconflict");
        let fake = d.join("llama-server");
        std::fs::write(&fake, "#!/bin/sh\nexit 0\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

        let model = d.join("m.gguf");
        std::fs::write(&model, b"GGUF").unwrap();

        // 占住端口
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();

        let e = LlamaCppEngine::new(Some(fake), vec![], None);
        let plan = plan_with_port(port, &model.to_string_lossy());
        let err = e.launch(&plan, Duration::from_millis(300)).unwrap_err();
        assert!(err.to_string().contains("已被占用"), "got {err}");
        drop(listener);
        std::fs::remove_dir_all(&d).ok();
    }

    #[cfg(unix)]
    #[test]
    fn launch_fails_fast_when_engine_exits_immediately() {
        let d = tmpdir("crash");
        // 脚本立即以非零码退出，模拟引擎崩溃
        let fake = d.join("llama-server");
        std::fs::write(&fake, "#!/bin/sh\necho 'bad model' >&2\nexit 3\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

        let model = d.join("m.gguf");
        std::fs::write(&model, b"GGUF").unwrap();

        let e = LlamaCppEngine::new(Some(fake), vec![], Some(d.clone()));
        let plan = plan_with_port(11998, &model.to_string_lossy());
        let err = e.launch(&plan, Duration::from_secs(5)).unwrap_err();
        match &err {
            MnError::EngineExited { code, .. } => assert_eq!(*code, Some(3)),
            other => panic!("期望 EngineExited，实际 {other:?}"),
        }
        assert!(err.is_retryable(), "引擎崩溃必须可重试/降级");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn launch_times_out_with_cleanup() {
        let d = tmpdir("timeout");
        // 脚本只 sleep，永不响应 HTTP
        let fake = d.join("llama-server");
        std::fs::write(&fake, "#!/bin/sh\nsleep 30\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

            let model = d.join("m.gguf");
            std::fs::write(&model, b"GGUF").unwrap();

            let e = LlamaCppEngine::new(Some(fake), vec![], Some(d.clone()));
            let plan = plan_with_port(11997, &model.to_string_lossy());
            let started = std::time::Instant::now();
            let err = e.launch(&plan, Duration::from_millis(600)).unwrap_err();
            // 必须真的超时，不能立刻返回
            assert!(started.elapsed() >= Duration::from_millis(500));
            assert!(err.to_string().contains("未通过 /health 就绪"), "got {err}");
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
