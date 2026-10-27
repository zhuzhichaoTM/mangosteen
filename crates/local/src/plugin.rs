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

use mangosteen_core::config::InferenceEngine;
use mangosteen_core::plan::LocalPlan;
use mangosteen_core::MnResult;

use crate::engine::{EngineHandle, LlamaCppEngine, OllamaEngine};

/// 一个推理引擎插件。
///
/// 实现者负责该引擎的全部「形态差异」：能不能启动、怎么启动、
/// 怎么判断活着、停止时要不要动进程。
pub trait EnginePlugin: std::fmt::Debug + Send + Sync {
    /// 对应的引擎标识。
    fn engine(&self) -> InferenceEngine;

    /// 启动（或复用）一个实例。
    ///
    /// `ready_timeout` 是等待就绪的上限；实现可以忽略它
    /// （例如复用常驻服务时无需等待）。
    fn launch(&self, plan: &LocalPlan, ready_timeout: Duration) -> MnResult<EngineHandle>;

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
}

// ============================================================ llama.cpp

impl EnginePlugin for LlamaCppEngine {
    fn engine(&self) -> InferenceEngine {
        InferenceEngine::LlamaCpp
    }

    fn launch(&self, plan: &LocalPlan, ready_timeout: Duration) -> MnResult<EngineHandle> {
        LlamaCppEngine::launch(self, plan, ready_timeout)
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
}

// ============================================================ Ollama

impl EnginePlugin for OllamaEngine {
    fn engine(&self) -> InferenceEngine {
        InferenceEngine::Ollama
    }

    fn launch(&self, plan: &LocalPlan, _ready_timeout: Duration) -> MnResult<EngineHandle> {
        // Ollama 认它自己库里的模型名，不认我们的 .gguf 路径。
        let name = model_name_for_ollama(&plan.model_path);
        self.attach(&name)
    }

    fn probe(&self, _plan: &LocalPlan) -> bool {
        // 注意：**不能**用 `plan.port`。Ollama 是常驻服务，端口固定
        // （默认 11434），不是台账为「一次部署」分配的那个。
        // 用错端口 + 用 `/health`（Ollama 没有该端点）会让部署刚成功
        // 就被判成失败。
        self.probe()
    }

    fn precheck(&self) -> MnResult<()> {
        if self.probe() {
            Ok(())
        } else {
            Err(mangosteen_core::MnError::bad_request(format!(
                "未检测到 Ollama 服务（{}:{}）。请先启动 Ollama 应用或执行 `ollama serve`",
                self.host(),
                self.probe_port()
            )))
        }
    }
}

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

    /// 用内置插件构造（llama.cpp + Ollama）。
    ///
    /// 这是**唯一**的内置登记点：新增引擎只改这里，网关不动。
    pub fn with_builtin(llama: LlamaCppEngine, ollama: OllamaEngine) -> Self {
        Self::empty()
            .register(Arc::new(llama))
            .register(Arc::new(ollama))
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
        self.get(engine).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> EngineRegistry {
        EngineRegistry::with_builtin(LlamaCppEngine::default(), OllamaEngine::new())
    }

    /// 已实现的引擎都能查到插件。
    #[test]
    fn builtin_plugins_are_registered() {
        let r = registry();
        assert!(r.has(InferenceEngine::LlamaCpp));
        assert!(r.has(InferenceEngine::Ollama));
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
