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
