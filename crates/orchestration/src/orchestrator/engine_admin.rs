//! 引擎管理与版本缓存（第三层·模块1 引擎管理的编排侧）。
//!
//! 归属：第三层 · 编排调度核心。
//! `Orchestrator` 的字段在 `super` 中定义，子模块可直接访问。

use super::*;

impl Orchestrator {
    /// 取**已缓存**的引擎版本。
    ///
    /// 只读缓存，绝不在请求路径上执行 `--version`。原因：该命令在部分安装
    /// 上要 20~30 秒，而 `status` 是 UI 高频轮询的接口。首次访问返回
    /// `None`（界面显示「探测中」），预热线程稍后填充。
    ///
    /// 这样把「慢」从**每个请求**转移到**启动时一次**，且不阻塞启动。
    pub(crate) fn cached_engine_version(&self, path: &std::path::Path) -> Option<String> {
        self.engine_version_cache.lock().ok().and_then(|g| {
            g.as_ref()
                .filter(|(p, _)| p == path)
                .and_then(|(_, r)| r.as_ref().ok().cloned())
        })
    }
    /// 取缓存的引擎版本（供 HTTP 接口展示，不触发探测）。
    pub fn engine_version(&self) -> Option<String> {
        let path = self.local_engine.locate().ok()?;
        self.cached_engine_version(&path)
    }
    /// 判断引擎是否有问题——**带超时**，兼顾准确与响应速度。
    ///
    /// ## 三种输入，三种结论
    ///
    /// | 情况 | 耗时 | 结论 |
    /// |------|------|------|
    /// | 缓存已有结果 | 0 | 直接给出（可用 / 不可用+原因）|
    /// | 无缓存，引擎响应快 | < 超时 | 现场探测并回填缓存，准确 |
    /// | 无缓存，引擎很慢 | = 超时 | 返回「未知」，由调用方保守处理 |
    ///
    /// ## 为什么必须带超时而不是「只读缓存」
    ///
    /// 只读缓存会带来**危险的错报**：服务刚启动、缓存还空时，一个坏掉的
    /// 引擎会被当成「本地可用」，UI 告诉用户「本地可跑、不花钱」；用户
    /// 确认后引擎启动失败、自动降级云端开始计费 —— 用户被意外收费。
    ///
    /// 而纯同步探测又会超时（`--version` 在本机要 27 秒）。
    ///
    /// 折中：给探测设一个短上限（[`ENGINE_PROBE_BUDGET`]）。假引擎、修好的
    /// 引擎通常毫秒级返回，能立刻给出准确结论；只有真正慢的引擎才会走到
    /// 超时分支，此时如实回答「未知」而不是编造结论。
    pub(crate) fn engine_problem_bounded(&self) -> Option<String> {
        let path = self.local_engine.locate().ok()?;

        // 1) 缓存命中 → 零成本
        if let Some(result) = self.cached_engine_result(&path) {
            return match result {
                Ok(_) => None,
                Err(msg) => Some(first_line(&msg)),
            };
        }

        // 2) 现场探测，最多等 ENGINE_PROBE_BUDGET
        match self.probe_with_budget(&path) {
            Some(Ok(_)) => None,
            Some(Err(msg)) => Some(first_line(&msg)),
            // 超时：结论未知。返回一句可操作的话，让 UI 能解释清楚。
            None => Some(format!(
                "引擎状态尚未确认（版本探测超过 {} 秒未返回）。                 为避免误报「本地免费」后实际降级云端产生费用，请稍后重试预估",
                ENGINE_PROBE_BUDGET.as_secs()
            )),
        }
    }
    /// 取缓存中的完整结果（成功或失败）。
    pub(crate) fn cached_engine_result(
        &self,
        path: &std::path::Path,
    ) -> Option<Result<String, String>> {
        self.engine_version_cache
            .lock()
            .ok()?
            .as_ref()
            .filter(|(p, _)| p == path)
            .map(|(_, r)| r.clone())
    }
    /// 在预算时间内探测引擎版本；超时返回 `None`。
    ///
    /// 超时后不杀线程：让它自然结束并回填缓存，这样下一次调用就命中缓存。
    /// 代价是一个后台线程短暂存活，比「反复探测」划算得多。
    pub(crate) fn probe_with_budget(
        &self,
        path: &std::path::Path,
    ) -> Option<Result<String, String>> {
        let engine = self.local_engine.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let r = engine.probe_version().map_err(|e| e.to_string());
            let _ = tx.send(r);
        });

        match rx.recv_timeout(ENGINE_PROBE_BUDGET) {
            Ok(result) => {
                // 回填缓存，后续请求零成本
                if let Ok(mut g) = self.engine_version_cache.lock() {
                    *g = Some((path.to_path_buf(), result.clone()));
                }
                Some(result)
            }
            Err(_) => None,
        }
    }
    /// 取出「用于预热」的引擎副本与路径。
    ///
    /// 预热必须分三步：**取快照 → 锁外探测 → 写回结果**。
    /// 因为 `probe_version` 在部分安装上要 20~30 秒，若在持锁状态下执行，
    /// 期间所有需要网关锁的 API（sysinfo / settings / status）都会被阻塞，
    /// 表现就是「页面能打开但所有接口超时」。
    pub fn warm_up_snapshot(&self) -> Option<(LlamaCppEngine, PathBuf)> {
        let path = self.local_engine.locate().ok()?;
        if self.cached_engine_version(&path).is_some() {
            return None;
        }
        Some((self.local_engine.clone(), path))
    }
    /// 写回预热得到的引擎版本（成功）。
    pub fn set_engine_version(&self, path: PathBuf, version: String) {
        if let Ok(mut g) = self.engine_version_cache.lock() {
            *g = Some((path, Ok(version)));
        }
    }
    /// 写回预热探测的失败结果。
    ///
    /// 记录失败同样重要：否则预估接口每次都要重跑 27 秒的 `--version`
    /// 才能知道引擎坏了，而它恰恰是用户点「部署」前的必经接口。
    pub fn set_engine_failure(&self, path: PathBuf, reason: String) {
        if let Ok(mut g) = self.engine_version_cache.lock() {
            *g = Some((path, Err(reason)));
        }
    }
    /// **同步**预热引擎版本缓存（等探测结果，不设上限）。
    ///
    /// ## 与 [`the gateway warm-up thread`] 的分工
    ///
    /// | | `spawn_warm_up` | 本方法 |
    /// |---|---|---|
    /// | 时机 | 进程启动时 | 调用方显式要求 |
    /// | 阻塞 | 否（后台线程） | 是（等探测返回） |
    /// | 用途 | 生产环境首屏不卡 | 需要**确定性**结论的场合 |
    ///
    /// ## 为什么需要同步版本
    ///
    /// `preview` 的本地可行性依赖一次带 2 秒预算的探测
    /// （[`ENGINE_PROBE_BUDGET`]）。缓存为空时，若探测超预算，结论是
    /// 「引擎状态未知」→ 本地被判不可行。
    ///
    /// 这个保守方向在生产里是对的（宁可让用户重试，也不能谎报「免费」），
    /// 但在 e2e 里会变成**假失败**：几十个用例并行跑，CPU 被抢占，
    /// 一个假引擎脚本的进程启动也能超过 2 秒，于是「本地可行」被误判成
    /// 「未知」。测试里先调本方法把缓存填好，`preview` 就只读缓存、
    /// 结果确定。
    ///
    /// 返回 `None` 表示找不到引擎（此时无需预热，也无可预热）。
    ///
    /// 返回值是 `Result<String, String>` 而不是 `MnResult<_>`：与
    /// `engine_version_cache` 里存的形式一致（错误已压成一行文本），
    /// 调用方不需要再处理 `MnError` 的细节。
    pub fn warm_up_engine(&self) -> Option<Result<String, String>> {
        let (engine, path) = self.warm_up_snapshot()?;
        let result = match engine.probe_version() {
            Ok(v) => {
                self.set_engine_version(path, v.clone());
                Ok(v)
            }
            Err(e) => {
                let msg = e.to_string();
                self.set_engine_failure(path, msg.clone());
                Err(msg)
            }
        };
        Some(result)
    }
}
