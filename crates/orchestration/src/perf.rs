//! 部署性能统计：记录每个阶段的开始/结束时间与耗时。
//!
//! 归属：第三层 · 编排调度核心。
//!
//! ## 目的
//!
//! 部署链路横跨云厂商 API、SSH 远程操作、模型下载、进程启动等多个耗时环节，
//! 用户反馈「卡在某一步不知道在干什么」时，需要精确的耗时数据来定位瓶颈。
//!
//! ## 设计
//!
//! - 所有日志输出到 stderr（`[perf]` 前缀），与服务日志一起落盘；
//! - `StageTimer` 记录单阶段耗时，Drop 时自动输出；
//! - `DeployPerfTrace` 记录整个部署的全程分段时间线，结束时输出汇总表。
//!
//! ## 日志格式
//!
//! ```text
//! [perf] ▶ 开始 节点创建 (dep-xxx-qwen25-05b)
//! [perf] ✓ 完成 节点创建 耗时 42.3s (dep-xxx-qwen25-05b)
//! [perf] ✗ 失败 模型下载 耗时 5.1s: <错误> (dep-xxx-qwen25-05b)
//! [perf] ═══ 部署耗时汇总 ═══
//! [perf]   模型评估        0.1s
//! [perf]   节点创建       42.3s  ████
//! [perf]   节点配置       18.7s  ██
//! [perf]   模型下载       65.2s  ██████
//! [perf]   启动模型       12.4s  █
//! [perf]   总计           138.7s
//! ```

use std::time::Instant;

/// 单个阶段的计时器：创建时开始计时，Drop 时输出耗时日志。
pub struct StageTimer {
    label: &'static str,
    deploy_id: String,
    start: Instant,
    /// 是否已成功结束（显式调用 finish 后不再在 Drop 时输出）。
    finished: bool,
}

impl StageTimer {
    /// 开始计时一个阶段。
    pub fn start(label: &'static str, deploy_id: &str) -> Self {
        eprintln!("[perf] ▶ 开始 {label} ({deploy_id})");
        Self {
            label,
            deploy_id: deploy_id.to_string(),
            start: Instant::now(),
            finished: false,
        }
    }

    /// 阶段成功结束，输出耗时。
    pub fn finish(mut self) {
        let secs = self.start.elapsed().as_secs_f64();
        eprintln!(
            "[perf] ✓ 完成 {} 耗时 {:.1}s ({})",
            self.label, secs, self.deploy_id
        );
        self.finished = true;
    }

    /// 阶段失败，输出耗时与错误。
    pub fn fail(self, err: &str) {
        let secs = self.start.elapsed().as_secs_f64();
        eprintln!(
            "[perf] ✗ 失败 {} 耗时 {:.1}s: {err} ({})",
            self.label, secs, self.deploy_id
        );
        // fail 消费 self 但不置 finished——Drop 里不再重复输出
        std::mem::forget(self);
    }
}

impl Drop for StageTimer {
    fn drop(&mut self) {
        if !self.finished {
            let secs = self.start.elapsed().as_secs_f64();
            eprintln!(
                "[perf] ✗ 中断 {} 已耗时 {:.1}s（未显式结束，可能提前返回）({})",
                self.label, secs, self.deploy_id
            );
        }
    }
}

/// 部署全程的分段计时器：记录每个阶段名与耗时，结束时输出汇总表。
pub struct DeployPerfTrace {
    deploy_id: String,
    /// `(阶段名, 耗时秒)` 序列
    stages: Vec<(&'static str, f64)>,
    /// 当前正在计时的阶段
    current: Option<(&'static str, Instant)>,
    /// 整个部署的开始时间
    overall_start: Instant,
}

impl DeployPerfTrace {
    /// 创建全程计时器。
    pub fn new(deploy_id: &str) -> Self {
        eprintln!("[perf] ══ 部署开始 ({deploy_id}) ══");
        Self {
            deploy_id: deploy_id.to_string(),
            stages: Vec::new(),
            current: None,
            overall_start: Instant::now(),
        }
    }

    /// 进入一个新阶段（自动结束上一个阶段）。
    pub fn enter(&mut self, label: &'static str) {
        self.leave_current();
        eprintln!("[perf] ▶ 阶段 {} ({})", label, self.deploy_id);
        self.current = Some((label, Instant::now()));
    }

    /// 结束当前阶段并输出描述。
    pub fn leave(&mut self, msg: &str) {
        self.leave_current();
        eprintln!("[perf]   · {msg} ({})", self.deploy_id);
    }

    fn leave_current(&mut self) {
        if let Some((label, start)) = self.current.take() {
            let secs = start.elapsed().as_secs_f64();
            self.stages.push((label, secs));
            eprintln!(
                "[perf] ✓ 阶段 {} 耗时 {:.1}s ({})",
                label, secs, self.deploy_id
            );
        }
    }

    /// 记录阶段内的子步骤说明（不打断计时）。
    pub fn note(&self, msg: &str) {
        let elapsed = self
            .current
            .as_ref()
            .map(|(_, t)| t.elapsed().as_secs_f64())
            .unwrap_or(0.0);
        eprintln!(
            "[perf]   · {} (阶段内已耗时 {:.1}s) ({})",
            msg, elapsed, self.deploy_id
        );
    }

    /// 部署结束（成功或失败），输出全程汇总。
    pub fn finish(&mut self, ok: bool) {
        self.leave_current();
        let total = self.overall_start.elapsed().as_secs_f64();
        eprintln!(
            "[perf] ═══ 部署耗时汇总 ({}) {} ═══",
            self.deploy_id,
            if ok { "成功" } else { "失败" }
        );
        // 柱状图宽度基准：最长的阶段占 40 字符
        let max = self
            .stages
            .iter()
            .map(|(_, s)| *s)
            .fold(0.0f64, f64::max)
            .max(0.001);
        for (label, secs) in &self.stages {
            let bar_len = ((secs / max) * 40.0).round() as usize;
            let bar = "█".repeat(bar_len);
            eprintln!("[perf]   {label:<10} {:>8.1}s  {bar}", secs);
        }
        eprintln!("[perf]   {注:<10} {:>8.1}s", total, 注 = "总计");
        eprintln!("[perf] ════════════════════════════");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_timer_reports_elapsed() {
        let t = StageTimer::start("测试阶段", "dep-test");
        std::thread::sleep(std::time::Duration::from_millis(30));
        t.finish();
    }

    #[test]
    fn perf_trace_records_stages() {
        let mut trace = DeployPerfTrace::new("dep-test");
        trace.enter("阶段A");
        std::thread::sleep(std::time::Duration::from_millis(20));
        trace.enter("阶段B");
        std::thread::sleep(std::time::Duration::from_millis(20));
        trace.finish(true);
        assert_eq!(trace.stages.len(), 2);
        assert!(trace.stages[0].1 >= 0.02);
        assert!(trace.stages[1].1 >= 0.02);
    }
}
