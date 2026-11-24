//! 引擎管理 · 插件注册表。
//!
//! ## 为什么需要这一层
//!
//! 基线要求「**推理引擎插件化**：新增推理框架仅需要在引擎管理模块新增插件，
//! 不改动调度主链路」。在此之前，网关里是这样写的：
//!
//! ```ignore
//! if engine == InferenceEngine::Ollama {  /* Ollama 专用分支 */ }
//! // 否则走 llama.cpp
//! ```
//!
//! 这种写法把「引擎差异」泄漏到了网关层。每接一个新框架就要在网关里
//! 加一个 `if`，还要同时改探活、启动、停止三处——正是基线明令禁止的
//! 「改动调度主链路」。
//!
//! 现在把差异收敛到 [`EnginePlugin`]：网关只问「当前引擎对应哪个插件」，
//! 然后调用插件的方法。**新增引擎 = 新增一个 impl + 在注册表里加一行**，
//! 网关代码一行不动。
//!
//! ## 生命周期差异也在这里吸收
//!
//! 两种引擎的进程模型根本不同，但对外语义一致：
//!
//! | 引擎 | 进程归属 | 停止语义 |
//! |------|---------|---------|
//! | llama.cpp | 本进程拉起 `llama-server` | 杀自己拉起的子进程 |
//! | Ollama | 复用用户已启动的常驻服务 | 不杀（那是用户的软件） |
//!
//! 这个差异由各自的插件实现吸收，网关不需要知道。

use std::sync::Arc;
use std::time::Duration;

use mangosteen_core::config::{EngineCapabilities, InferenceEngine};
use mangosteen_core::plan::EngineLaunchConfig;
use mangosteen_core::plan::LocalPlan;
use mangosteen_core::MnResult;

use crate::llama_cpp::{llama_cpp_args, LlamaCppEngine};
use crate::strata::StrataEngine;

/// 一个推理引擎插件。
///
/// 实现者负责该引擎的全部「形态差异」：怎么生成远端启动配置、
/// 怎么判断活着、启动前要检查什么、**怎么在远端准备/启停引擎进程**。
///
/// 本地进程启动（`launch`）已撤销：Mangosteen 只部署到云端/远端节点
/// （本地推理交由 Ollama 承担），编排层只消费 [`EnginePlugin::launch_config`]
/// 生成远端 SSH 拉起参数。
///
/// ## 生命周期方法（远端 SPI）
///
/// `ensure_engine` / `start` / `stop` / `is_running` 把「引擎进程在远端
/// 节点上如何存在」收敛进插件。provider 与编排层只调用这些方法，
/// 不再直接拼 `llama-server` 二进制路径或 pid 文件名——
/// 这是 Strata 等 Second Engine 能不改主链路接入的前提。
pub trait EnginePlugin: std::fmt::Debug + Send + Sync {
    /// 对应的引擎标识。
    fn engine(&self) -> InferenceEngine;

    /// 引擎能力表（健康检查路径 / 端口 / TLS / 密钥方式等）。
    fn capabilities(&self) -> EngineCapabilities {
        self.engine().capabilities()
    }

    /// 为目标节点生成标准化进程启动配置。
    ///
    /// 实现负责引擎专属 flags；调用方只指定绑定地址/端口，不解释配置内容。
    fn launch_config(&self, plan: &LocalPlan, bind_host: &str, port: u16) -> EngineLaunchConfig;

    /// 探活一个**已在运行**的实例。
    ///
    /// 用于状态刷新：返回 `false` 时台账会被修正为失败态。
    /// 必须是**一次性**探测（几百毫秒），不能循环等待。
    fn probe(&self, plan: &LocalPlan) -> bool;

    /// 启动前自检（可选）。
    ///
    /// 默认放行。典型用途：检查引擎是否已安装 / 守护进程是否在跑，
    /// 好给出比「启动超时」更有用的提示。
    fn precheck(&self) -> MnResult<()> {
        Ok(())
    }

    /// 远端准备引擎二进制：确保引擎可执行文件就位。
    ///
    /// * `url` —— 引擎发行包/安装脚本地址（配置驱动，可独立于 app
    ///   版本演进）；返回 `Ok(None)` 表示「引擎已由镜像/系统提供」。
    /// * 实现必须幂等：已存在且可执行时直接返回。
    fn ensure_engine(
        &self,
        target: &crate::remote::SshTarget,
        workdir: &str,
        url: Option<&str>,
    ) -> MnResult<Option<String>> {
        let _ = (target, workdir, url);
        Ok(None)
    }

    /// 在远端节点拉起引擎进程。
    ///
    /// `launch` 来自 [`Self::launch_config`]（args + 任务级密钥）。
    /// 密钥注入方式由 [`Self::capabilities`] 的 `api_key_mode` 决定：
    /// File 模式由实现写远端 600 文件并追加启动参数；Env 模式注入
    /// 进程环境变量——绝不进 argv、绝不进日志。
    fn start(
        &self,
        target: &crate::remote::SshTarget,
        workdir: &str,
        launch: &EngineLaunchConfig,
    ) -> MnResult<()> {
        let _ = (target, workdir, launch);
        Err(mangosteen_core::MnError::bad_request(format!(
            "{} 引擎尚未实现远端启动（start）",
            self.engine().display_name()
        )))
    }

    /// 停止远端节点上本引擎的进程（保留节点）。
    ///
    /// 默认实现按端口/进程名兜底；引擎有专属停止命令（如
    /// `strata launch --stop`）时覆写。
    fn stop(&self, target: &crate::remote::SshTarget, workdir: &str) -> MnResult<()> {
        let caps = self.capabilities();
        crate::remote::stop_engine_process(target, workdir, caps.pid_name, None)
    }

    /// 指定端口上的引擎进程是否仍在运行。
    fn is_running(
        &self,
        target: &crate::remote::SshTarget,
        workdir: &str,
        port: u16,
    ) -> MnResult<bool> {
        let caps = self.capabilities();
        crate::remote::engine_process_running_on_port(target, workdir, caps.pid_name, port)
    }

    /// TLS 协商（多态，2026-11-23 用户裁决）：调用方传入台账里记录的
    /// `tls` 意图，插件返回该引擎**实际**是否启用 TLS。
    ///
    /// - llama.cpp：`supports_tls=true`，请求即结果
    /// - Strata：只监听 HTTP，任何请求都返回 false
    ///
    /// 网关转发（inference.rs）与健康检查（provision.rs）都调用本方法，
    /// 不再出现 `matches!(...Strata)` 分支散落在各处的局面。
    fn tls_enabled(&self, requested: bool) -> bool {
        requested && self.capabilities().supports_tls
    }

    /// 健康检查 URL 拼装（多态）：按引擎能力生成 scheme/host/port/path 组合。
    ///
    /// 默认实现基于能力表（TLS 走 host=llama-server，否则 127.0.0.1）；
    /// 引擎可覆写做定制。
    fn health_url(&self, bind_host: &str, port: u16, tls: bool) -> String {
        let caps = self.capabilities();
        let scheme = if self.tls_enabled(tls) {
            "https"
        } else {
            "http"
        };
        let host = if self.tls_enabled(tls) {
            "llama-server" // 自签证书 CN
        } else {
            bind_host
        };
        format!("{scheme}://{host}:{port}{}", caps.health_path)
    }

    /// 预下载模型的远端落位目录（多态，2026-11-23 用户裁决）。
    ///
    /// 以 llama.cpp 部署链路为参考：预下载 GGUF 平铺在 `{workdir}/models/`。
    /// 引擎自管存储（Strata）覆写为 `models/<family>/<quant>/` 子目录，
    /// 与 Strata 内部布局一致，启动时直接复用已下载文件。
    fn model_download_dir(&self, workdir: &str, plan: &LocalPlan) -> String {
        let _ = plan;
        format!("{}/models", workdir.trim_end_matches('/'))
    }

    /// 启动参数适配（多态）：预下载文件已存在时调整启动参数。
    ///
    /// llama.cpp 默认不动（--model 路径即下载路径，天然复用）。
    /// Strata 覆写：移除 --family/--model，追加 --model-path 指向已下载文件
    /// （避免 Strata 重新下载 23.8GB，2026-11-22 实测踩坑）。
    fn adapt_launch_args(&self, args: &mut Vec<String>, model_path: &str) {
        let _ = (args, model_path);
    }

    /// 启动前文件检查（多态）：预下载模式引擎校验模型文件存在性。
    ///
    /// llama.cpp：文件不存在 → 报错指向「模型下载」阶段。
    /// Strata：自管存储，预下载文件不存在也可由引擎自行下载 → 默认跳过。
    fn preflight_model_check(&self, model_path: &str) -> Result<(), String> {
        if self.capabilities().supports_model_path {
            Err(format!(
                "远端模型文件不存在：{model_path}。请在「模型下载」阶段确认文件已完整落盘"
            ))
        } else {
            Ok(())
        }
    }
}

// ============================================================ llama.cpp

impl EnginePlugin for LlamaCppEngine {
    fn engine(&self) -> InferenceEngine {
        InferenceEngine::LlamaCpp
    }

    fn launch_config(&self, plan: &LocalPlan, bind_host: &str, port: u16) -> EngineLaunchConfig {
        EngineLaunchConfig {
            args: llama_cpp_args(plan, bind_host, port),
            // API Key 由编排层按任务生成后填充（safety 分支，改动点 2）；
            // 引擎插件不感知密钥内容，只透传启动参数。
            api_key: None,
        }
    }

    fn probe(&self, plan: &LocalPlan) -> bool {
        mangosteen_platform::process::probe_http_ok(
            &plan.host,
            plan.port,
            "/health",
            Duration::from_millis(400),
        )
    }

    fn precheck(&self) -> MnResult<()> {
        // 只做定位，不跑 `--version`（本机实测 20~30 秒，不能进请求路径）
        self.locate().map(|_| ())
    }

    /// llama.cpp：下载发行包到 `{workdir}/bin/`（幂等）。
    ///
    /// 无 URL 或代理信息时退回「PATH / 镜像自带」探测（历史行为）。
    fn ensure_engine(
        &self,
        target: &crate::remote::SshTarget,
        workdir: &str,
        url: Option<&str>,
    ) -> MnResult<Option<String>> {
        let url = url.filter(|u| !u.trim().is_empty());
        // 已存在且可执行 → 直接复用（幂等）
        let bin = format!("{}/bin/llama-server", workdir.trim_end_matches('/'));
        let probe = crate::remote::ssh_exec(
            target,
            &format!("if [ -x {bin} ]; then echo ok; else echo missing; fi"),
            60,
        )?;
        if probe.trim() == "ok" {
            return Ok(Some(bin));
        }
        match url {
            Some(url) => {
                crate::llama_cpp::ensure_llama_server_from_archive(target, workdir, url, "", "")?;
                Ok(Some(bin))
            }
            // 无 URL：沿用镜像/系统 PATH 里的 llama-server（历史行为）
            None => Ok(None),
        }
    }

    fn start(
        &self,
        target: &crate::remote::SshTarget,
        workdir: &str,
        launch: &EngineLaunchConfig,
    ) -> MnResult<()> {
        let bin = format!("{}/bin/llama-server", workdir.trim_end_matches('/'));
        crate::llama_cpp::start_llama_server_with_key(
            target,
            &bin,
            &launch.args,
            workdir,
            launch.api_key.as_deref(),
        )
    }

    fn stop(&self, target: &crate::remote::SshTarget, workdir: &str) -> MnResult<()> {
        crate::llama_cpp::stop_llama_server(target, workdir)
    }

    fn is_running(
        &self,
        target: &crate::remote::SshTarget,
        workdir: &str,
        port: u16,
    ) -> MnResult<bool> {
        crate::llama_cpp::llama_server_running_on_port(target, workdir, port)
    }
}

// ============================================================ Ollama

/// 从本地模型文件路径推导 Ollama 里应使用的模型名。
///
/// Ollama 用 `name:tag` 形式（如 `qwen2.5:3b-local`）。我们的下载文件名
/// 形如 `qwen2.5-0.5b-instruct-q4_k_m.gguf`，无法可靠反推。
/// 因此取文件名主干（去掉 `.gguf`），由 `attach` 做前缀匹配并容忍 tag 差异。
pub fn model_name_for_ollama(path: &str) -> String {
    std::path::Path::new(path)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string())
}

// ============================================================ 注册表

/// 引擎插件注册表。
///
/// 只登记**已实现**的插件。未实现的引擎（vLLM / SGLang / TensorRT-LLM）
/// 刻意不在此处出现——查询会得到 `None`，网关据此给出「尚未适配」的
/// 明确拒绝，而不是假装能跑然后超时。
#[derive(Debug, Default)]
pub struct EngineRegistry {
    plugins: Vec<Arc<dyn EnginePlugin>>,
}

impl EngineRegistry {
    /// 空注册表（测试用）。
    pub fn empty() -> Self {
        Self::default()
    }

    /// 注册一个插件。
    pub fn register(mut self, plugin: Arc<dyn EnginePlugin>) -> Self {
        // 同一引擎只保留首个：重复注册会让「用哪个实现」变得不确定
        if !self.plugins.iter().any(|p| p.engine() == plugin.engine()) {
            self.plugins.push(plugin);
        }
        self
    }

    /// 用内置插件构造（llama.cpp + Ollama + Strata）。
    ///
    /// 这是**唯一**的内置登记点：新增引擎只改这里，网关不动。
    pub fn with_builtin(llama: LlamaCppEngine, strata: StrataEngine) -> Self {
        Self::empty()
            .register(Arc::new(llama))
            .register(Arc::new(strata))
    }

    /// 取指定引擎的插件。
    pub fn get(&self, engine: InferenceEngine) -> Option<&Arc<dyn EnginePlugin>> {
        self.plugins.iter().find(|p| p.engine() == engine)
    }

    /// 已登记的引擎列表。
    pub fn registered(&self) -> Vec<InferenceEngine> {
        self.plugins.iter().map(|p| p.engine()).collect()
    }

    /// 是否已登记该引擎。
    pub fn has(&self, engine: InferenceEngine) -> bool {
        self.plugins.iter().any(|p| p.engine() == engine)
    }
}

/// 全局内置插件查找（多态入口，2026-11-23 用户裁决）。
///
/// 网关/编排层只需要一个引擎标识就能拿到插件，调用 `tls_enabled` /
/// `health_url` 等多态方法——不再写 `matches!(...Strata)` 分支。
/// 未注册的引擎（如旧台账）回落 None，调用方按「保守支持」处理。
pub fn plugin_for(engine: InferenceEngine) -> Option<&'static dyn EnginePlugin> {
    // 内置插件静态注册（进程级单例，无锁）
    use std::sync::OnceLock;
    static REGISTRY: OnceLock<EngineRegistry> = OnceLock::new();
    let reg = REGISTRY.get_or_init(|| {
        EngineRegistry::with_builtin(
            LlamaCppEngine::default(),
            StrataEngine::new(),
        )
    });
    // 借用静态注册表里的插件：生命周期与静态变量绑定
    reg.get(engine).map(|arc| arc.as_ref() as &dyn EnginePlugin)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> EngineRegistry {
        EngineRegistry::with_builtin(
            LlamaCppEngine::default(),
            StrataEngine::new(),
        )
    }

    /// 已实现的引擎都能查到插件。
    #[test]
    fn builtin_plugins_are_registered() {
        let r = registry();
        assert!(r.has(InferenceEngine::LlamaCpp));
        assert!(r.has(InferenceEngine::Strata));
        // Ollama 已移除（2026-11-23 用户裁决）
        assert!(!r.has(InferenceEngine::Ollama));
        assert_eq!(r.registered().len(), 2);
    }

    /// 未适配的引擎必须查不到——网关据此明确拒绝，而不是假装能跑。
    ///
    /// 这是「不静默失败」的基础：如果这里返回了某个默认插件，
    /// 用户会看到「部署成功」但根本没有服务在监听。
    #[test]
    fn unimplemented_engines_are_absent() {
        let r = registry();
        for e in [
            InferenceEngine::Vllm,
            InferenceEngine::Sglang,
            InferenceEngine::TensorrtLlm,
        ] {
            assert!(!r.has(e), "{} 尚未适配，不应有插件", e.display_name());
            assert!(r.get(e).is_none());
        }
    }

    /// 重复注册同一引擎时保留首个，避免「用哪个实现」不确定。
    #[test]
    fn duplicate_registration_keeps_first() {
        let r = EngineRegistry::empty()
            .register(Arc::new(LlamaCppEngine::default()))
            .register(Arc::new(LlamaCppEngine::default()));
        assert_eq!(r.registered().len(), 1);
    }

    /// Ollama 的模型名推导：去掉 .gguf 与目录。
    #[test]
    fn ollama_model_name_strips_path_and_suffix() {
        assert_eq!(
            model_name_for_ollama("/models/qwen2.5-0.5b-instruct-q4_k_m.gguf"),
            "qwen2.5-0.5b-instruct-q4_k_m"
        );
        assert_eq!(model_name_for_ollama("qwen2.5.gguf"), "qwen2.5");
    }
}
