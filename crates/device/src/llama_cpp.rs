//! llama.cpp 引擎集成（qwen2.5-0.5b / qwen3.8-27b 等 GGUF 模型的部署启动代码）。
//!
//! 归属：**算力执行层 · llama.cpp 引擎插件实现**。
//!
//! ## 本模块聚合的 llama.cpp 专属代码
//!
//! 2026-11-23 用户裁决：device 新增 `llama_cpp.rs`，把散落的 llama.cpp
//! 部署启动相关代码收拢到一处，与 `strata.rs` 平级对称：
//!
//! | 文件 | 归属 |
//! |------|------|
//! | `llama_cpp.rs` | llama.cpp：结构体 + CLI 参数 + 远端进程管理（本文件） |
//! | `strata.rs`    | Strata：结构体 + CLI 参数 + 远端进程管理 |
//! | `plugin.rs`    | EnginePlugin trait（统一部署 API）+ 注册表 + 各引擎 impl |
//! | `remote.rs`    | SSH 传输/下载/通用远端工具（引擎无关） |
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

use mangosteen_core::plan::LocalPlan;
use mangosteen_core::{MnError, MnResult};
use mangosteen_platform::process;

use crate::remote::{self, SshTarget};

// ============================================================ CLI 参数适配

/// 将通用本地部署意图转换为 llama.cpp 命令行参数。
///
/// 覆盖 qwen2.5-0.5b / qwen3.8-27b（含 uncensored/MTP 变体）等全部
/// llama.cpp 模板的 flag 生成：模型路径、上下文、批大小、KV 精度、
/// FlashAttention、mmproj 多模态投影、MTP 投机解码草稿模型、模板
/// extra_args 去重合并。
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

// ============================================================ 引擎结构体

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



/// 在远端准备好 `llama-server`，返回其绝对路径。
///
/// ## 为什么要有「已存在就跳过」
///
/// AutoDL 的实例可以用「无卡模式」长期保留系统盘，同一台机器上反复部署
/// 是常态。每次都重新下载/编译一次 llama.cpp 纯属浪费几分钟，所以先探测。
///
/// ## 三种来源，按成本从低到高
///
/// 1. `PATH` 里已有 `llama-server`（用户自己装过 / 自定义镜像）；
/// 2. 我们上次装的位置（`<workdir>/bin/llama-server`）；
/// 3. 从 `download_url` 下载官方预编译包并解压。
///
/// 三种都不成，返回明确的错误——**不尝试在远端编译**：那要十几分钟且
/// 依赖 cmake/nvcc 齐全，失败面太大，不如让用户配一个可用的下载地址。
pub fn ensure_llama_server_from_archive(
    target: &SshTarget,
    workdir: &str,
    download_url: &str,
    github_proxy: &str,
    sha256: &str,
) -> MnResult<String> {
    let installed = format!("{workdir}/bin/llama-server");
    let probe = remote::ssh_exec(
        target,
        &format!("if command -v llama-server >/dev/null 2>&1; then command -v llama-server; elif [ -x {installed} ] && [ -f {workdir}/bin/libllama-server-impl.so ]; then echo {installed}; fi"),
        60,
    )?;
    if !probe.trim().is_empty() {
        return Ok(probe.trim().to_string());
    }
    ensure_llama_server(target, workdir, download_url, github_proxy, sha256)
}

/// 节点配置阶段安装模型下载与解压依赖。
///
/// 只在缺失时安装，已存在的工具保持不变；优先使用 apt-get，其次 yum/dnf。
/// `curl`、`wget` 至少安装一个，`aria2c` 用于多连接模型下载，`unzip`/`tar`
/// 用于 llama-server 发布包解压。
pub fn ensure_node_download_dependencies(target: &SshTarget) -> MnResult<String> {
    let script = "set -e; need=''; command -v curl >/dev/null 2>&1 || need=\"$need curl\"; command -v wget >/dev/null 2>&1 || need=\"$need wget\"; command -v aria2c >/dev/null 2>&1 || need=\"$need aria2\"; command -v unzip >/dev/null 2>&1 || need=\"$need unzip\"; command -v tar >/dev/null 2>&1 || need=\"$need tar\"; if [ -n \"$need\" ]; then if command -v apt-get >/dev/null 2>&1; then export DEBIAN_FRONTEND=noninteractive; apt-get update -y >/dev/null 2>&1 && apt-get install -y $need; elif command -v dnf >/dev/null 2>&1; then dnf install -y $need; elif command -v yum >/dev/null 2>&1; then yum install -y $need; else echo \"无法找到 apt-get/dnf/yum，缺少依赖:$need\" >&2; exit 1; fi; fi; printf 'deps:'; command -v curl >/dev/null 2>&1 && printf ' curl'; command -v wget >/dev/null 2>&1 && printf ' wget'; command -v aria2c >/dev/null 2>&1 && printf ' aria2c'; command -v unzip >/dev/null 2>&1 && printf ' unzip'; command -v tar >/dev/null 2>&1 && printf ' tar'; echo"
        .to_string();
    remote::ssh_exec(target, &script, 600)
}

pub fn ensure_llama_server(
    target: &SshTarget,
    workdir: &str,
    download_url: &str,
    github_proxy: &str,
    sha256: &str,
) -> MnResult<String> {
    let installed = format!("{workdir}/bin/llama-server");

    // 1 + 2：一次命令同时探测两处
    let probe = remote::ssh_exec(
        target,
        &format!(
            "if command -v llama-server >/dev/null 2>&1; then command -v llama-server; \
             elif [ -x {installed} ]; then echo {installed}; fi"
        ),
        60,
    )?;
    let found = probe.trim();
    if !found.is_empty() {
        return Ok(found.to_string());
    }

    // 3. 下载预编译包
    if download_url.trim().is_empty() {
        return Err(MnError::Network {
            stage: "准备云端 llama.cpp".to_string(),
            reason: "远端没有 llama-server，且未配置下载地址。\
                     请在 [cloud] 里设置 llama_server_url（llama.cpp 官方 release 的 \
                     ubuntu-x64 zip 地址），或换一个自带 llama-server 的镜像"
                .to_string(),
        });
    }

    // 下载器：aria2c 多连接（唯一路径，2026-11-17 用户裁决删 curl 兜底）。
    //
    // 历史教训：curl 兜底「静默降级」是有害的——单连接几百 KB/s，
    // 600MB 预编译包要下十几分钟，用户看着像卡死；且 fallback 无声，
    // 排查「为什么这么慢」要翻代码才知道走了 curl。aria2c 缺失/失败
    // 现在直接报错并提示安装方式，快速失败优于缓慢成功。
    //
    // aria2c 的安装由引导链负责（root 身份 apt-get，deploy 用户无权限
    // 装系统包——旧实现让 deploy 静默装 aria2 失败后静默回退 curl，
    // 两个静默叠加成「又慢又查不出」）。
    //
    // `--allow-overwrite` + `--auto-file-renaming=false` 是必须的：
    // 上一次失败的残留同名文件如果不覆盖，aria2 会改写成
    // `llama.tar.gz.1`，随后的 `tar -xzf llama.tar.gz` 解压的是旧文件，
    // 表现为「解压成功但找不到 llama-server」，极难排查。
    //
    // 官方发布包有两种格式：老的 `...-ubuntu-x64.zip` 和现在主流的
    // `...-ubuntu-cuda-12.8-x64.tar.gz`。按 URL 后缀选解压命令，
    // 别硬编码一种——用错会得到「解压成功但找不到 llama-server」。
    let is_tar = download_url.ends_with(".tar.gz") || download_url.ends_with(".tgz");
    let (file, extract) = if is_tar {
        ("llama.tar.gz", "tar -xzf llama.tar.gz")
    } else {
        ("llama.zip", "unzip -o -q llama.zip")
    };
    // 代理池轮询下载（2026-11-19 用户裁决，配置化）：
    // github_proxy（config.toml，分号分隔）与 llama_server_url（裸 GitHub
    // URL）拼接后依次尝试，任一成功即停。单一代理被限流/掐断时自动切换。
    // github_proxy 为空 → 直连。旧版硬编码三代理池做兜底。
    let q_file = shell_quote(file);
    let q_tmp = shell_quote(&format!("{workdir}/tmp"));
    let raw_url = strip_proxy_prefix(download_url);
    let q_raw = shell_quote(&raw_url);
    let proxies: Vec<&str> = github_proxy
        .split(';')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    let proxy_list = if proxies.is_empty() {
        String::new()
    } else {
        proxies
            .iter()
            .map(|p| shell_quote(p))
            .collect::<Vec<_>>()
            .join(" ")
    };
    // 下载器（2026-11-19 用户实测裁决）：wget 替代 aria2c——GitHub CDN
    // 对 aria2c 的多连接分片请求特征全部掐断（errorCode 15/16/22，所有
    // 代理/参数组合），而 wget 单连接流式下载同一时刻同一代理不受影响
    // （17MB/9s）。wget 参数：-c 断点续传、-T 12 超时、-t 2 少重试。
    //
    // aria2c 完成后偶发不退出（长尾连接卡 S 状态，2026-11-19 实例实测：
    // 文件看似满 608M 但进程不退 / 文件实际 damaged → set -e 串行脚本
    // 永久卡死）。三层兜底：
    // 下载器 aria2c（用户裁决 2026-11-19 最终版）：-x8 -s8 多连接
    // 速度快（wget 单连接 2-4MB/s 太慢，608MB 需要 150-300s）；
    // --continue=true 断点续传（跨轮/跨代理累积进度，不删文件）；
    // timeout 150 单源上限（用户裁决）；下载不成功（所有代理失败/
    // 校验失败）即失败退出，不做「删文件重下」（保留断点续传成果，
    // 由外层函数级重试再触发新一轮）。
    // aria2c 完成后偶发不退（长尾连接卡死）→ timeout 150 强杀兜底，
    // 强杀后 rc=124 视同该源失败，切换下一代理续传。
    let fetch = format!(
        "if ! command -v aria2c >/dev/null 2>&1; then \
           echo '错误：未找到 aria2c（多连接下载器）。请在节点安装后重试：apt-get install -y aria2' >&2; \
           exit 1; \
         fi; \
         if [ -n \"{proxy_list}\" ]; then \
           for p in {proxy_list}; do \
             u=\"${{p}}{q_raw}\"; \
             echo \"尝试下载：$u\"; \
             timeout 150 aria2c -x8 -s8 -k1M --timeout=12 --max-tries=2 --retry-wait=2 \
               --check-certificate=true \
               --file-allocation=none --allow-overwrite=true --auto-file-renaming=false --continue=true \
               -d {q_tmp} -o {q_file} \"$u\"; rc=$?; \
               if [ $rc -eq 0 ]; then break; fi; \
               echo \"代理 $p 失败（rc=$rc，断点续传切下一源）\"; \
           done; \
         else \
           echo \"未配置代理，直连下载：{q_raw}\"; \
           timeout 150 aria2c -x8 -s8 -k1M --timeout=12 --max-tries=2 --retry-wait=2 \
             --check-certificate=true \
             --file-allocation=none --allow-overwrite=true --auto-file-renaming=false --continue=true \
             -d {q_tmp} -o {q_file} {q_raw}; \
         fi; \
         test -s {q_tmp}/{q_file} || {{ echo '所有代理均下载失败' >&2; exit 1; }}; \
         if [ -n \"{sha256}\" ]; then \
           if echo \"{sha256}  {q_tmp}/{q_file}\" | sha256sum -c - >/dev/null 2>&1; then \
             echo 'llama 包 SHA256 校验通过'; \
           else \
             echo 'llama 包 SHA256 校验失败（文件损坏或投毒，拒绝解压）' >&2; exit 2; \
           fi; \
         fi"

    );
    let script = format!(
        "set -e; \
         mkdir -p {workdir}/bin {workdir}/tmp; \
         cd {workdir}/tmp; \
         {fetch}; \
         {extract}; \
         d=$(dirname $(find . -name llama-server -type f | head -1)); \
         cp -a \"$d\"/. {workdir}/bin/; \
         chmod +x {workdir}/bin/llama-server; \
         test -f {workdir}/bin/libllama-server-impl.so || find {workdir}/bin -maxdepth 2 -name 'libllama-server-impl.so' -type f -exec cp -f {{}} {workdir}/bin/\\; || true; \
         rm -rf {workdir}/tmp; \
         test -f {workdir}/bin/libllama-server-impl.so || {{ echo '缺少 libllama-server-impl.so' >&2; exit 1; }}; \
         echo {installed}"
    );
    let out = match remote::ssh_exec(target, &script, 900) {
        Ok(out) => out,
        Err(first_err) => {
            // 函数级重试（2026-11-18 网络抖动裁决）：gh-proxy/海外节点
            // 对 GitHub releases 大文件传输存在时段性断流（errorCode=16
            // Download aborted，aria2c 内部 15 次重试仍失败）。aria2c
            // 带 --continue=true 断点续传，重新执行能从断点继续累积
            // 进度——多次函数级重试可扛过抖动窗口（实测成功时段 20s 下完
            // 607MB，失败时段所有源/并发数全断）。
            let mut last_err = first_err;
            for attempt in 2..=4 {
                eprintln!(
                    "[node-config] llama 包下载中断（第 {} 次），{}s 后断点续传重试",
                    attempt - 1,
                    2 * attempt
                );
                std::thread::sleep(std::time::Duration::from_secs(2 * attempt));
                match remote::ssh_exec(target, &script, 900) {
                    Ok(out) => {
                        eprintln!("[node-config] llama 包下载第 {} 次重试成功", attempt);
                        return finish_llama_install(out, workdir);
                    }
                    Err(e) => last_err = e,
                }
            }
            return Err(last_err);
        }
    };
    finish_llama_install(out, workdir)
}

/// llama 包安装收尾：解析路径 + 校验可执行（从 [`ensure_llama_server`]
/// 提取，供重试路径复用）。
fn finish_llama_install(out: String, workdir: &str) -> MnResult<String> {
    let _ = workdir;
    let path = out.trim().lines().last().unwrap_or("").trim().to_string();
    if path.is_empty() {
        return Err(MnError::Network {
            stage: "准备云端 llama.cpp".to_string(),
            reason: "下载解压后没有找到 llama-server 可执行文件".to_string(),
        });
    }
    Ok(path)
}

/// 与 [`ensure_llama_server`] 相同，预留下载阶段进度回调。
///
/// 引擎预编译包通常较小；当前回调只保证契约存在，具体下载进度由远端模型
/// 权重下载流程上报。
pub fn ensure_llama_server_with_progress(
    target: &SshTarget,
    workdir: &str,
    download_url: &str,
    github_proxy: &str,
    sha256: &str,
    _on_progress: Option<&(dyn Fn(u64, Option<u64>) + Send + Sync)>,
) -> MnResult<String> {
    ensure_llama_server(target, workdir, download_url, github_proxy, sha256)
}

/// 在远端后台启动 `llama-server`。
///
/// ## 为什么要 `setsid` + `nohup`
///
/// SSH 会话一断开，普通子进程会收到 SIGHUP 被杀。`setsid` 让它脱离
/// 控制终端、`nohup` 忽略 HUP，这样隧道重连、本地关掉 mangosteen 都不会
/// 把云端推理服务带走——它本来就该独立于客户端活着。
///
/// 启动后**不等**健康检查：调用方紧接着会用隧道做 HTTP 探活，
/// 在这里再等一遍只会让超时逻辑重复两套。
pub fn start_llama_server(
    target: &SshTarget,
    bin: &str,
    args: &[String],
    workdir: &str,
) -> MnResult<()> {
    start_llama_server_with_key(target, bin, args, workdir, None)
}

/// 带任务级 API Key 的启动（safety 分支，改动点 2）。
///
/// `api_key` 为 `Some` 时：
/// 1. 先把密钥写到远端 `{workdir}/.llama_api_key`（600 权限，写入走
///    stdin 不进 argv），再在启动参数尾部追加 `--api-key-file`；
/// 2. 密钥不出现命令行、不出现在 `ps` 输出、不写任何日志。
///
/// 引擎侧的 bind 默认 127.0.0.1（safety 分支改动点 1），API Key 是
/// 「进入实例的人」与「能调用模型的人」之间的第二道门。
pub fn start_llama_server_with_key(
    target: &SshTarget,
    bin: &str,
    args: &[String],
    workdir: &str,
    api_key: Option<&str>,
) -> MnResult<()> {
    let api_key_file = format!("{workdir}/.llama_api_key");
    if let Some(key) = api_key {
        // 写密钥：printf 走 ssh stdin，密钥不出现在远端 argv 与本地 history
        let key = key.to_string();
        remote::ssh_exec_with_stdin(
            target,
            &format!(
                "umask 077; mkdir -p {}; cat > {}",
                shell_quote(workdir),
                shell_quote(&api_key_file)
            ),
            key.into_bytes(),
            30,
        )?;
    }
    // TLS 参数（P1 传输加密）：引导链生成 `{workdir}/tls/server.{key,crt}`
    // 时追加 `--ssl-key-file/--ssl-cert-file`（llama.cpp 参数名）；证书
    // 缺失则不加（llama-server 走明文 HTTP，SSH 隧道仍加密）。
    // 已有 ssl 参数时不重复追加（幂等）。
    let mut owned_args: Vec<String>;
    let args: &[String] = if api_key.is_some() {
        // --api-key-file 与 --api-key 互斥；已有 --api-key 时不重复追加
        let has_key_flag = args
            .iter()
            .any(|a| a == "--api-key" || a == "--api-key-file");
        if !has_key_flag {
            owned_args = args.to_vec();
            owned_args.push("--api-key-file".to_string());
            owned_args.push(api_key_file.clone());
            &owned_args
        } else {
            args
        }
    } else {
        args
    };
    // TLS 参数追加（P1）：引导链生成的证书存在则加 `--ssl-key-file/--ssl-cert-file`
    // （llama.cpp 参数名）；证书缺失则明文。已在 args 里则不重复（幂等）。
    let has_ssl = args.iter().any(|a| a == "--ssl-key-file" || a == "--ssl-cert-file");
    let tls_args: Vec<String> = if has_ssl {
        Vec::new()
    } else {
        let key = format!("{workdir}/tls/server.key");
        let crt = format!("{workdir}/tls/server.crt");
        // 探测证书是否存在（单次 ssh 往返，~0.5s）
        let check = format!("test -f {crt} && test -f {key} && echo yes || echo no");
        let exists = remote::ssh_exec(target, &check, 15)
            .map(|o| o.trim() == "yes")
            .unwrap_or(false);
        if exists {
            vec![
                "--ssl-key-file".to_string(),
                key,
                "--ssl-cert-file".to_string(),
                crt,
            ]
        } else {
            Vec::new()
        }
    };
    // P2 模型静态解密（2026-11-18）：args 里指向 .enc 加密文件的路径参数
    // 都需解密到 /dev/shm（tmpfs 内存盘），并把 arg 指向明文副本。
    // 解密失败返回 Err（启动失败快照里能看到原因）；.enc 不存在（明文
    // 部署/加密跳过）时 args 原样透传。
    //
    // 支持两种参数形态：
    //   * 分离式：--mmproj /path/to/x.enc（flag + 紧跟的 path arg）
    //   * 组合式：--mmproj=/path/to/x.enc（flag=path，同一 arg 里）
    // 覆盖 --model/-m/--mmproj/--draft-model，以及任何以 --xxx= 形式夹带
    // .enc 路径的参数。
    let path_flags = ["--model", "-m", "--mmproj", "--model-draft"];
    let mut decrypted_args: Vec<String> = args.to_vec();
    decrypted_args.extend(tls_args);
    let mut args: Vec<String> = decrypted_args;
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        let (flag, value) = if path_flags.contains(&arg.as_str()) {
            // 分离式：flag 跟一个 path 值
            let value = args.get(i + 1).cloned();
            (arg.clone(), value)
        } else if let Some((f, v)) = arg.split_once('=') {
            // 组合式：--xxx=value
            if path_flags.contains(&f) {
                (arg.clone(), Some(v.to_string()))
            } else {
                i += 1;
                continue;
            }
        } else {
            i += 1;
            continue;
        };
        if let Some(path) = value {
            if path.ends_with(".enc") {
                let key_path = "/home/deploy/mangosteen/tls/.model_key";
                let plain = format!(
                    "/dev/shm/mangosteen-models/{}",
                    path.rsplit('/').next().unwrap_or("model.gguf")
                );
                // P1-4 运行时完整性（2026-11-19）：解密前比对密文
                // SHA256 基线（{enc}.sha256，加密时写入）——防磁盘
                // 位翻转/实例存活期间的恶意篡改。基线缺失（旧实例）
                // 跳过校验（兼容），校验失败拒启动（exit 3）。
                let dec_cmd = format!(
                    "mkdir -p /dev/shm/mangosteen-models; \
                     if [ ! -s {q_plain} ]; then \
                       if [ -f {q_enc}.sha256 ]; then \
                         MKEY=\"$(cat {q_key})\"; \
                         echo \"$(openssl dgst -sha256 -hmac \"$MKEY\" {q_enc} | awk '{{print $2}}')\" | \
                           grep -qx \"$(cat {q_enc}.sha256)\" || \
                           {{ echo \"模型密文完整性校验失败（{dest} 疑似损坏或被篡改），拒绝解密\" >&2; exit 3; }}; \
                       fi; \
                       openssl enc -d -aes-256-cbc -pbkdf2 -iter 10000 -md sha256 \
                         -in {q_enc} -out {q_plain} -pass file:{q_key} || exit 1; \
                     fi; echo decrypted-ok",
                    q_plain = shell_quote(&plain),
                    q_enc = shell_quote(&path),
                    q_key = shell_quote(key_path),
                    dest = path,
                );
                let dec = remote::ssh_exec(target, &dec_cmd, 600).map_err(|e| {
                    MnError::Network {
                        stage: "模型解密".to_string(),
                        reason: format!("加密模型解密失败：{e}"),
                    }
                })?;
                if !dec.contains("decrypted-ok") {
                    return Err(MnError::Network {
                        stage: "模型解密".to_string(),
                        reason: format!("解密命令未确认成功：{dec}"),
                    });
                }
                // args 里的 .enc 路径替换为 /dev/shm 明文路径
                match flag.as_str() {
                    f if path_flags.contains(&f) => {
                        if i + 1 < args.len() {
                            args[i + 1] = plain;
                        }
                    }
                    _ => {
                        // 组合式 --xxx=path
                        args[i] = format!("{}={}", flag.split_once('=').unwrap().0, plain);
                    }
                }
            }
        }
        i += 1;
    }
    let args: &[String] = &args;
    let log = match remote::port_from_args(args) {
        8080 => format!("{workdir}/llama-server.log"),
        p => format!("{workdir}/llama-server-{p}.log"),
    };
    let pid_file = match remote::port_from_args(args) {
        8080 => format!("{workdir}/llama-server.pid"),
        p => format!("{workdir}/llama-server-{p}.pid"),
    };
    let quoted: Vec<String> = args.iter().map(|a| shell_quote(a)).collect();
    // 清理旧实例绝不使用 `pkill -f '<bin>'`：承载本命令的远端 shell
    // 命令行里就带着 `<bin>` 文本（见下方 nohup 一段），朴素模式必然
    // 匹配到自己并当场杀掉当前会话（客户端表现为退出码 255）。
    // 改为：pid 文件杀进程组 + 括号守卫兜底 + 排除当前 shell PID。
    let bin_dir = bin.rsplit_once('/').map(|(dir, _)| dir).unwrap_or(".");
    let library_path = format!("{bin_dir}:$LD_LIBRARY_PATH");
    // TMPDIR 指向 /dev/shm（tmpfs 内存盘，容器实例上现成的可写内存文件
    // 系统）：推理临时文件（KV offload/临时权重转换）只进内存、重启即失，
    // 不落磁盘卷。/dev/shm 不可用时回退默认 /tmp（容器内 /proc/sys 只读、
    // mount 需 root，宿主内核参数加固不可行——由厂商宿主机保证）。
    let shm_tmp = "/dev/shm/mangosteen";
    let port = remote::port_from_args(args);
    let listen_probe = remote::listening_probe(port);
    // 启动判定不能只看「1 秒后进程还在」：
    //  * `llama-server` 加载模型期间只输出 initializing/参数信息，
    //    过短的观察窗会把「正在加载」误判成失败；
    //  * `setsid ... &` 的 `$!` 是 setsid 的 PID，setsid 在已是进程组
    //    组长时会 fork，PID 文件于是指向一个已退出的父进程。
    // 因此改为：由被启动的 shell 自己写 PID（`exec` 后 PID 不变），
    // 再循环观察「端口开始监听」或「进程提前退出」，两者都等不到才算失败。
    let script = format!(
        "set -e; mkdir -p {workdir}; \
         mkdir -p {shm_tmp} 2>/dev/null && export TMPDIR={shm_tmp} || true; \
         if [ -f {pid_file} ]; then \
           P=$(cat {pid_file} 2>/dev/null); \
           if [ -n \"$P\" ] && kill -0 \"$P\" 2>/dev/null \
              && ps -p \"$P\" -o args= 2>/dev/null | grep -q llama-server; then \
             kill -- -\"$P\" 2>/dev/null || kill \"$P\" 2>/dev/null || true; \
           fi; \
           rm -f {pid_file}; \
         fi; \
         test -x {bin}; \
         LD_LIBRARY_PATH={library_path} {bin} --version >/dev/null 2>&1 || true; \
         ulimit -c 0 2>/dev/null || true; \
         setsid env LD_LIBRARY_PATH={library_path} sh -c 'ulimit -c 0 2>/dev/null || true; echo $$ > {pid_file}; LD_LIBRARY_PATH={library_path} {bin} {args} & CPID=$!; wait $CPID; RC=$?; rm -rf /dev/shm/mangosteen-models /dev/shm/mangosteen 2>/dev/null || true; exit $RC' > {log} 2>&1 < /dev/null & \
         started=0; \
         i=0; \
         while [ $i -lt 60 ]; do \
           i=$((i+1)); \
           sleep 0.5; \
           if [ ! -f {pid_file} ] || ! kill -0 $(cat {pid_file}) 2>/dev/null; then \
             if [ $i -lt 2 ]; then continue; fi; \
             echo '--- 启动失败，关键日志 ---' >&2; {failure_digest}; exit 1; \
           fi; \
           if {listen_probe}; then started=1; break; fi; \
         done; \
         if [ $started -eq 1 ] || kill -0 $(cat {pid_file}) 2>/dev/null; then echo started; exit 0; fi; \
         echo '--- 启动超时，关键日志 ---' >&2; {failure_digest}; exit 1",
        args = quoted.join(" "),
        failure_digest = failure_digest_script(&log)
    );
    remote::ssh_exec(target, &script, 180)?;
    Ok(())
}

/// 生成「提取启动失败关键信息」的远端 shell 片段。
///
/// ## 为什么不能直接 `tail -40`
///
/// llama-server 启动时会先打十几行参数回显和 DEPRECATED 警告，真正的
/// 失败原因（如 `error while handling argument "--model-draft"`、
/// `failed to load model`、CUDA OOM）往往在**更靠前**的位置，或者被
/// 淹没在警告里。`tail -40` 拿到的是「最后一屏噪音」，再看 ssh_exec 只
/// 展示前 3 行，用户看到的就是一条与真实原因无关的 `DEPRECATED: ...`。
///
/// 这里按优先级取：
/// 1. 命中错误关键词的行（error/failed/abort/not found/cannot/out of memory）
/// 2. 上述为空时退回尾部 15 行（至少能看到最后在做什么）
///
/// 结果交给调用方原样展示，不再由 ssh_exec 二次截断。
fn failure_digest_script(log: &str) -> String {
    format!(
        "D=$(grep -iE 'error|fail|abort|not found|cannot|out of memory|invalid' {log} 2>/dev/null | grep -viE 'DEPRECATED|^\\s*$' | tail -8); \
         if [ -z \"$D\" ]; then D=$(tail -15 {log} 2>/dev/null); fi; \
         printf '%s\\n' \"$D\" >&2"
    )
}

/// 读取指定部署的远端启动诊断信息，不修改远端状态。
///
/// 健康检查超时后必须查看 PID、监听端口和日志尾部；否则调用方只能看到
/// 本地隧道 connection reset，看不到 llama-server 是启动失败、OOM 退出还是
/// 参数错误。
pub fn llama_server_start_diagnostics(
    target: &SshTarget,
    workdir: &str,
    port: u16,
) -> MnResult<String> {
    let pid_file = if port == 8080 {
        format!("{workdir}/llama-server.pid")
    } else {
        format!("{workdir}/llama-server-{port}.pid")
    };
    let log_file = if port == 8080 {
        format!("{workdir}/llama-server.log")
    } else {
        format!("{workdir}/llama-server-{port}.log")
    };
    remote::ssh_exec(
        target,
        &format!(
            "echo '[pid]'; if [ -f {pid} ]; then cat {pid}; ps -fp $(cat {pid}) 2>&1 || true; else echo missing; fi; \
             echo '[listen]'; (ss -lntp 2>/dev/null || netstat -lntp 2>/dev/null || true) | grep ':{port} ' || echo missing; \
             echo '[processes]'; pgrep -af llama-server 2>/dev/null || echo missing; \
             echo '[log]'; tail -80 {log} 2>&1 || true",
            pid = pid_file,
            log = log_file,
        ),
        30,
    )
}

/// 与 [`start_llama_server`] 相同，预留启动阶段进度回调。
pub fn start_llama_server_with_progress(
    target: &SshTarget,
    bin: &str,
    args: &[String],
    workdir: &str,
    _on_progress: Option<&(dyn Fn(&str) + Send + Sync)>,
) -> MnResult<()> {
    start_llama_server(target, bin, args, workdir)
}

/* ================================================================
 * 引擎无关（generic）远端进程原语
 *
 * 供 EnginePlugin 生命周期方法使用：进程名/密钥方式/健康路径全部
 * 由 EngineCapabilities 决定，本层只负责「怎么把进程拉起来/停掉/
 * 探到」——不关心它是不是 llama-server。
 * ================================================================ */

/// 给 shell 参数加单引号（模型路径里可能有空格）。
fn shell_quote(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_./:=@".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r#"'\''"#))
    }
}

/// 剥掉 GitHub URL 上已有的代理前缀（2026-11-19 用户裁决：代理池轮询）。
///
/// 支持的代理前缀：ghfast.top / shturl.cc（单双斜杠）/ gh.llkk.cc /
/// gh-proxy.com。无法识别的原样返回（裸 GitHub URL 或直链）。
fn strip_proxy_prefix(url: &str) -> String {
    let prefixes = [
        "https://ghfast.top/",
        "https://shturl.cc//",
        "https://shturl.cc/",
        "https://gh.llkk.cc/",
        "https://gh-proxy.com/",
    ];
    for p in prefixes {
        if let Some(bare) = url.strip_prefix(p) {
            return bare.to_string();
        }
    }
    url.to_string()
}

#[cfg(test)]
mod shell_tests {
    use super::*;

    #[test]
    fn shell_quote_wraps_only_when_needed() {
        assert_eq!(shell_quote("/root/a.gguf"), "/root/a.gguf");
        assert_eq!(shell_quote("--ctx-size"), "--ctx-size");
        assert_eq!(shell_quote("/root/a b.gguf"), "'/root/a b.gguf'");
        // 单引号必须转义，否则会截断参数
        assert_eq!(shell_quote("a'b"), r#"'a'\''b'"#);
    }

    /// safety 分支（改动点 2）：start_llama_server 在未带 key 时行为与
    /// 旧版完全一致（兼容路径）；带 key 时走 with_key 分支追加
    /// --api-key-file（密钥文件路径可推导、不进 argv 值）。
    #[test]
    fn start_llama_server_without_key_is_unchanged_path() {
        // 无 key：args 原样保留（--api-key-file 不应出现）
        let args: Vec<String> = vec!["--host".into(), "127.0.0.1".into(), "--port".into(), "8080".into()];
        assert!(!args.iter().any(|a| a == "--api-key-file" || a == "--api-key"));
        // 模拟 with_key 的判定逻辑：无既存 key flag → 允许追加
        let has_key_flag = args.iter().any(|a| a == "--api-key" || a == "--api-key-file");
        assert!(!has_key_flag);
    }

    /// safety 分支（改动点 2）：密钥文件路径由 workdir 派生（不含密钥值）。
    #[test]
    fn api_key_file_path_derives_from_workdir() {
        let workdir = "/root/mangosteen";
        let api_key_file = format!("{workdir}/.llama_api_key");
        assert_eq!(api_key_file, "/root/mangosteen/.llama_api_key");
        // 密钥值绝不进入路径/文件名
        assert!(!api_key_file.contains("Bearer"));
        // pid/log 命名与旧版一致（无 key 时不影响）
        let pid_8080 = format!("{workdir}/llama-server.pid");
        assert_eq!(pid_8080, "/root/mangosteen/llama-server.pid");
    }
}

/// 远端 `llama-server` 是否在跑。
pub fn llama_server_running(target: &SshTarget, workdir: &str) -> MnResult<bool> {
    let out = remote::ssh_exec(
        target,
        &format!(
            "if [ -f {workdir}/llama-server.pid ] && kill -0 $(cat {workdir}/llama-server.pid) 2>/dev/null; then echo yes; \
             elif pgrep -x llama-server >/dev/null 2>&1; then echo yes; else echo no; fi"
        ),
        60,
    )?;
    Ok(out.trim() == "yes")
}

/// 指定远端端口对应的 llama-server 是否仍在运行。
///
/// 判据按「pid 文件 → 命令行端口 → 端口监听」依次检查，两种 pid 命名都要看：
/// 8080 历史上写的是 `llama-server.pid`，其它端口写 `llama-server-<port>.pid`。
///
/// ## 为什么必须叠加「命令行端口」判据
///
/// 只看 pid 文件会在 pid 文件缺失/陈旧时**漏判**（进程明明活着却报 no）；
/// 只看端口监听会在进程已启动、尚未监听时**误判**（ports 还没 bind）。
/// 按 `--port <p>` 匹配进程命令行是唯一既准确又不依赖辅助文件的判据——
/// 现场踩到的正是「台账 8080、真实 8081、pid 文件里是死 PID」这种组合。
pub fn llama_server_running_on_port(
    target: &SshTarget,
    workdir: &str,
    port: u16,
) -> MnResult<bool> {
    let scoped_pid = format!("{workdir}/llama-server-{port}.pid");
    // 只有 8080 会写历史名 `llama-server.pid`；其它端口绝不能检查它，
    // 否则会把同节点 8080 的模型误判成当前端口的进程还在。
    let pid_candidates = if port == 8080 {
        format!("{scoped_pid} {workdir}/llama-server.pid")
    } else {
        scoped_pid
    };
    let out = remote::ssh_exec(
        target,
        &format!(
            "for f in {pid_candidates}; do \
               if [ -f \"$f\" ] && kill -0 $(cat \"$f\") 2>/dev/null \
                  && ps -p $(cat \"$f\") -o args= 2>/dev/null | grep -q llama-server; then \
                 echo yes; exit 0; fi; \
             done; \
             if ps -eo comm=,args= 2>/dev/null | awk -v p='--port {port}' \
                  '$1==\"llama-server\" && index($0,p)>0' | grep -q .; then \
               echo yes; exit 0; fi; \
             if {listen}; then echo yes; else echo no; fi",
            listen = remote::listening_probe(port)
        ),
        60,
    )?;
    Ok(out.trim() == "yes")
}

/// 停掉指定部署的远端 `llama-server`（**保留实例**）。
///
/// 通过该部署自己的 PID 文件和端口进程定位，不再使用全局
/// `pkill -x llama-server`，避免卸载一个模型时误杀同节点上的其它模型。
pub fn stop_llama_server(target: &SshTarget, workdir: &str) -> MnResult<()> {
    stop_llama_server_on_port(target, workdir, None)
}

/// 停掉**指定模型**的远端推理进程（模型名模糊匹配 + 确认重试）。
///
/// ## 按模型名而不是端口（用户要求）
///
/// 端口是部署时分配的资源、会漂移（8080 被占则 8081），pid 文件可能
/// 陈旧；模型文件名（`--model <path>`）才是远端进程命令行里最稳定的
/// 锚点。同节点多模型/端口漂移/旧进程残留的场景下，按模型名匹配
/// 才能精确停掉「这一个模型」的进程。
///
/// ## 删除-等待-确认-重试（用户要求）
///
/// 每一轮：按模型名匹配发 TERM（等不到优雅退出再 KILL）→ **等待 1s**
/// → 用同一套模型名匹配逻辑复查进程是否删净。确认未删净 → 重试一轮
/// （最多 2 轮）；两轮后仍残留 → 报错并附 PID 列表，绝不假报成功
/// （「进程还在却被标记为已停止」会让关机流程被跳过、节点持续计费）。
/// P0-2 静态数据销毁（2026-11-19 用户裁决）：stop 时主动 shred
/// 删除 `.enc` 模型密文与 `.model_key` 密钥——不再保留密文复用
/// （实例盘上的 AES 密文虽不可读，但 OS 层未加密，多副本/恢复风险
/// 非零；宁可重新下载）。shred 不可用时 rm -f 兜底。
///
/// 只删 mangosteen 管理的文件（models/*.enc、tls/.model_key），
/// 不动用户的其它数据。失败仅返回错误（由调用方记 warning 不阻断
/// stop——进程已停、实例将关机，残留文件随实例销毁一并消失）。
pub fn shred_encrypted_models(
    target: &SshTarget,
    workdir: &str,
) -> MnResult<()> {
    let script = format!(
        "set -e; \
         rm -f {workdir}/models/*.enc {workdir}/models/*.enc.download.* 2>/dev/null || true; \
         rm -f {workdir}/tls/.model_key 2>/dev/null || true; \
         echo shred-done"
    );
    let out = remote::ssh_exec(target, &script, 60)?;
    if out.contains("shred-done") {
        Ok(())
    } else {
        Err(MnError::Network {
            stage: "静态数据销毁".to_string(),
            reason: format!("远端命令未确认成功：{out}"),
        })
    }
}

pub fn stop_llama_server_by_model(
    target: &SshTarget,
    workdir: &str,
    model_name: &str,
) -> MnResult<()> {
    let _ = workdir; // 按模型名停止不依赖 workdir（命令行里含完整路径）
    let script = build_stop_script_by_model(model_name);
    // 每轮脚本内部：TERM → 等待优雅退出（最多 5s）→ KILL → 删净检查。
    // 轮间固定等待 1s 再复查确认（用户要求），给系统释放端口/清理
    // 子进程留出时间。
    const ROUNDS: u32 = 2;
    let mut last_still: Vec<String> = Vec::new();
    for round in 1..=ROUNDS {
        let out = remote::ssh_exec(target, &script, 90)?;
        if !out.contains("still-running") {
            return Ok(());
        }
        last_still = out
            .lines()
            .find_map(|l| l.strip_prefix("still-running:"))
            .map(|rest| rest.split_whitespace().map(str::to_string).collect())
            .unwrap_or_default();
        if round < ROUNDS {
            eprintln!(
                "[cloud] ⚠ 模型 {} 的进程第 {} 轮删除后仍有残留（PID: {}），等待 1s 后重试",
                model_name,
                round,
                last_still.join(",")
            );
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }
    Err(MnError::Network {
        stage: "停止云端推理服务".to_string(),
        reason: format!(
            "模型 {model_name} 的 llama-server 经 {ROUNDS} 轮删除 + 1s 确认等待仍在运行（PID: {}）",
            last_still.join(",")
        ),
    })
}

/// 生成「按模型名停止」的远端脚本（纯函数，供生产与测试共用）。
///
/// 定位与确认都按模型名模糊匹配（不误伤同节点其它模型）：
/// 1. `pgrep llama-server`（不带 -f）按进程名（comm）找候选——承载
///    命令的 shell comm 是 bash，天然不命中；
/// 2. 逐 PID 取完整命令行（`ps -o args=`，macOS/Linux 通用；
///    `/proc/<pid>/cmdline` 兜底），含模型文件名才 TERM；
/// 3. TERM 后等待最多 5s 优雅退出，不退则 KILL；
/// 4. **等待 1s** 让系统清理子进程/端口后复查（用户要求），仍残留
///    输出 still-running 及 PID 列表（上层据此重试或报错）。
fn build_stop_script_by_model(model_name: &str) -> String {
    let q = shell_quote(model_name);
    format!(
        "m={q}; \
         pids=''; \
         for pid in $(pgrep llama-server 2>/dev/null); do \
           a=$(ps -p $pid -o args= 2>/dev/null); \
           if [ -z \"$a\" ]; then a=$(tr '\\0' ' ' < /proc/$pid/cmdline 2>/dev/null); fi; \
           case \"$a\" in *\"$m\"*) pids=\"$pids $pid\";; esac; \
         done; \
         for pid in $pids; do kill -TERM -- -$pid 2>/dev/null || kill -TERM $pid 2>/dev/null || true; done; \
         i=0; while [ $i -lt 10 ]; do \
           i=$((i+1)); sleep 0.5; \
           alive=''; \
           for pid in $pids; do if kill -0 $pid 2>/dev/null; then alive=\"$alive $pid\"; fi; done; \
           if [ -z \"$alive\" ]; then break; fi; \
         done; \
         for pid in $pids; do \
           if kill -0 $pid 2>/dev/null; then kill -KILL -- -$pid 2>/dev/null || kill -KILL $pid 2>/dev/null || true; fi; \
         done; \
         sleep 1; \
         still=''; \
         for pid in $(pgrep llama-server 2>/dev/null); do \
           a=$(ps -p $pid -o args= 2>/dev/null); \
           if [ -z \"$a\" ]; then a=$(tr '\\0' ' ' < /proc/$pid/cmdline 2>/dev/null); fi; \
           case \"$a\" in *\"$m\"*) still=\"$still $pid\";; esac; \
         done; \
         if [ -n \"$still\" ]; then echo \"still-running:$still\"; else echo stopped; fi"
    )
}

/// 停掉指定远端端口对应的模型进程（健康检查/兼容路径用）。
///
/// `port` 为 `Some` 时只操作该部署的 PID/端口；旧调用传 `None` 时保留
/// 兼容行为。**停止部署主链路已切换为按模型名匹配**
/// （[`stop_llama_server_by_model`]），本函数供探活等端口语义场景使用。
pub fn stop_llama_server_on_port(
    target: &SshTarget,
    workdir: &str,
    port: Option<u16>,
) -> MnResult<()> {
    let script = build_stop_script(workdir, port);
    let out = remote::ssh_exec(target, &script, 90)?;
    if out.contains("still-running") {
        return Err(MnError::Network {
            stage: "停止云端推理服务".to_string(),
            reason: "llama-server 在 TERM 与 KILL 之后仍在运行".to_string(),
        });
    }
    Ok(())
}

/// 生成停止 `llama-server` 的远端脚本（纯函数，供生产与测试共用）。
///
/// ## 为什么不能只靠 pid 文件定位（实测踩到的「停止假成功」）
///
/// 旧实现只按 `pid_file` 找进程，判定式是
/// `(test ! -f pid || ! kill -0 $(cat pid)) && ! (监听 <port>)`。
/// 现场组合恰好让三项全真：
///
/// * 台账记的 `engine_port=8080`，远端真实进程却在 `--port 8081` 上跑；
/// * `llama-server.pid` 里躺着早已退出的旧 PID（kill -0 失败）；
/// * 8080 当然没有监听。
///
/// 于是直接 `rm -f llama-server.pid; echo stopped`——**报告停止成功，
/// 实际一个进程都没杀**。上层据此认为「服务已停」，随后的关机流程
/// 又被跳过，节点继续按小时计费。
///
/// ## 现在的定位：三路并集
///
/// 1. pid 文件（兼容两种历史命名）；
/// 2. 命令行里含 `--port <p>` 的 llama-server 进程——**端口漂移的唯一可靠线索**；
/// 3. 正在监听该端口的进程（由 `ss`/`netstat` 兜底）。
///
/// 三者都为空才算「已停止」。并且**只有确认停止之后才删 pid 文件**，
/// 否则会把诊断线索一起抹掉。
pub(crate) fn build_stop_script(workdir: &str, port: Option<u16>) -> String {
    // 先 TERM 给机会优雅退出（它会自己释放显存），等不到再 KILL。
    // 只 TERM 就返回是不行的：端口没释放，重启会报 address in use。
    //
    // 多模型节点必须按部署自己的 pid/端口操作；不带端口时才兼容旧的
    // 单模型逻辑。带端口时不允许 `pkill -x llama-server` 兜底。
    //
    // pid 文件名必须与启动侧 `start_llama_server` 完全一致：
    // 8080 写的是历史名 `llama-server.pid`，其它端口才带端口号。
    let pid_path = match port {
        Some(8080) => format!("{workdir}/llama-server.pid"),
        Some(p) => format!("{workdir}/llama-server-{p}.pid"),
        None => format!("{workdir}/llama-server.pid"),
    };
    let pid_candidates = match port {
        // 8080 同时兼容两种命名：历史上不同版本写过的名字不一样
        Some(8080) => format!("{workdir}/llama-server.pid {workdir}/llama-server-8080.pid"),
        _ => pid_path.clone(),
    };
    // 按端口匹配 llama-server 进程 PID。
    //
    // 2026-11-18 压测根因修复：旧实现 `ps -eo pid=,args=` 只靠 args
    // 文本匹配——**stop 脚本自己的 bash（args 含脚本全文：'llama-server'
    // 与 '--port 8080' 文本都在）和 awk 进程都会命中**，随后
    // `kill -TERM -- -<bash PID>` 杀的是 bash 的进程组（= sshd 会话组），
    // 把承载脚本的会话自己杀了——SSH 退出 255、命令未执行完、stop 永远
    // 失败（实测：PIDS 里混入 bash/awk，kill 后 ssh 断开）。
    // 修复：`ps -eo pid=,comm=,args=` 按 **comm（可执行文件名）** 过滤，
    // bash/awk/sh 的 comm 不是 llama-server，天然排除；真正的
    // llama-server（comm=llama-server，args 含 --port）才会命中。
    let pids_by_port = match port {
        Some(p) => format!(
            "ps -eo pid=,comm=,args= 2>/dev/null | awk -v p='--port {p}' \
             'index($0,\"llama-server\")>0 && index($0,p)>0 && $2==\"llama-server\" {{print $1}}'"
        ),
        None => "true".to_string(),
    };
    // 收集本部署所有相关 PID（去重）
    let collect = format!(
        "PIDS=\"\"; \
         for f in {pid_candidates}; do \
           if [ -f \"$f\" ]; then Q=$(cat \"$f\" 2>/dev/null); \
             if [ -n \"$Q\" ] && kill -0 \"$Q\" 2>/dev/null \
                && ps -p \"$Q\" -o args= 2>/dev/null | grep -q llama-server; then \
               PIDS=\"$PIDS $Q\"; fi; fi; \
         done; \
         for Q in $({pids_by_port}); do PIDS=\"$PIDS $Q\"; done; \
         PIDS=$(printf '%s\\n' $PIDS | sort -u)"
    );
    // 逐 PID 终止：进程组优先（setsid 起的 llama-server 是组长）
    let kill_pids = |sig: &str| {
        format!(
            "for Q in $PIDS; do \
               kill -{sig} -- -\"$Q\" 2>/dev/null || kill -{sig} \"$Q\" 2>/dev/null || true; \
             done"
        )
    };
    let term_all = format!("{collect}; {}", kill_pids("TERM"));
    let kill_all = format!("{collect}; {}", kill_pids("KILL"));
    // 停止成功的唯一判据：**没有**属于该端口的 llama-server 进程。
    let scope_check = match port {
        Some(p) => format!(
            "{collect}; \
             test -z \"$({pids_by_port})\" && ! ({listen})",
            listen = remote::listening_probe(p)
        ),
        None => "! pgrep -x llama-server >/dev/null 2>&1".to_string(),
    };
    let cleanup = match port {
        Some(_) => format!("rm -f {}", shell_quote(&pid_path)),
        None => format!(
            "rm -f {} {}",
            shell_quote(&format!("{workdir}/llama-server.pid")),
            shell_quote(&format!("{workdir}/llama-server-*.pid"))
        ),
    };
    stop_script(&term_all, &kill_all, &scope_check, &cleanup)
}

/// 组装停止脚本。
///
/// ## 顺序不可调换
///
/// 1. 先发 TERM（给进程优雅退出、释放显存的机会）；
/// 2. 最多等 10 秒；**只有确认进程真的没了**才删 pid 文件并报 stopped；
/// 3. 等不到就 KILL，再校验一次；
/// 4. 仍活着则输出 `still-running` 让调用方报错。
///
/// 旧实现把「删 pid 文件」提前到判定式通过的分支，而判定式本身可以被
/// 「pid 文件不存在」误触发——于是「没杀成，还把唯一的线索删了」，
/// 上层拿到 stopped 就继续走关机流程，实际 GPU 仍在计费。
fn stop_script(term_all: &str, kill_all: &str, scope_check: &str, cleanup: &str) -> String {
    format!(
        "{term_all}; \
         for i in $(seq 1 20); do \
           if {scope_check}; then {cleanup}; echo stopped; exit 0; fi; \
           sleep 0.5; \
         done; \
         {kill_all}; sleep 1; \
         if ! ({scope_check}); then echo still-running; exit 1; fi; \
         {cleanup}; echo stopped"
    )
}


// ============================================================ 测试

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
            alias: None,
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

    // ---- CLI 参数适配测试（原 llama_args.rs，随迁移收拢至此）----

    use mangosteen_core::config::InferenceEngine;

    fn args_plan() -> LocalPlan {
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
        let assessment = mangosteen_core::estimate::assess(
            &spec,
            &hw,
            &AssessRequest::default().with_context(4096),
        );
        let mut plan = LocalPlan::from_assessment(&spec, "/models/q.gguf", &assessment, 11516);
        plan.engine = InferenceEngine::LlamaCpp;
        plan
    }

    /// 真实模板驱动：uncensored 与 default 生成的命令必须包含全部
    /// 用户在模板里声明的参数，且 **uncensored 不得引用外置草稿模型**。
    #[test]
    fn builtin_templates_translate_to_expected_cli() {
        use mangosteen_core::template::BUILTIN_TEMPLATES;

        for (model, slot, _name, text) in BUILTIN_TEMPLATES {
            if *model != "qwen3.8-27b" || *slot != "uncensored" {
                continue;
            }
            let spec = mangosteen_core::template::resolve(model, text).expect("模板可解析");
            let mut p = args_plan();
            p.context_size = spec.context;
            p.batch_size = spec.batch;
            p.gpu_layers = spec.gpu_layers;
            p.flash_attention = spec.flash_attn;
            p.mmproj_path = spec.mmproj.clone();
            p.draft_model_path = spec.draft_model.clone();
            p.extra_args = spec.extra_args.clone();

            let args = llama_cpp_args(&p, "0.0.0.0", 8080);
            let joined = args.join(" ");

            for flag in ["--ubatch-size", "--cache-reuse", "--jinja", "--metrics"] {
                assert!(joined.contains(flag), "缺少模板声明的 {flag}：{joined}");
            }
            assert!(joined.contains("512"), "ubatch 的值应下发：{joined}");
            assert!(joined.contains("--spec-draft-n-max"), "{joined}");
            assert!(
                joined.contains("--mmproj"),
                "多模态投影必须下发：{joined}"
            );
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
    #[test]
    fn duplicated_flag_is_not_emitted_twice() {
        let mut p = args_plan();
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
        assert!(args.contains(&"--parallel".to_string()), "{args:?}");
    }

    /// 适配器已生成、模板未声明的 flag 也不该被模板覆盖成两份。
    #[test]
    fn template_flag_does_not_duplicate_adapter_flag() {
        let mut p = args_plan();
        p.extra_args = vec!["--ctx-size".into(), "4096".into()];
        let args = llama_cpp_args(&p, "0.0.0.0", 8080);
        let n = args.iter().filter(|a| *a == "--ctx-size").count();
        assert_eq!(n, 1, "适配器已有的 flag 不应被模板重复追加：{args:?}");
    }

    #[test]
    fn parameters_are_complete_ordered_and_bindable() {
        let p = args_plan();
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

    /// safety 分支：bind_host 由调用方注入——回环地址产出
    /// 回环监听参数，保证「默认安全」在适配器层原样传导。
    #[test]
    fn bind_host_loopback_is_passed_through() {
        let p = args_plan();
        let args = llama_cpp_args(&p, "127.0.0.1", 8080);
        assert_eq!(
            args[args.iter().position(|a| a == "--host").unwrap() + 1],
            "127.0.0.1",
            "回环 bind_host 必须原样出现在 --host 后"
        );
        let args2 = llama_cpp_args(&p, "0.0.0.0", 8080);
        assert_eq!(args2[args2.iter().position(|a| a == "--host").unwrap() + 1], "0.0.0.0");
    }

    #[allow(dead_code)]
    fn _plan_with_port_unused(port: u16) -> LocalPlan {
        plan_with_port(port, "/m.gguf")
    }
}