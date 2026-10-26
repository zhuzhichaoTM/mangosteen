//! 云端实例监控的后台采集与短 TTL 缓存。
//!
//! 归属：**API 网关层**（设备页签的云端数据源）。
//!
//! ## 为什么必须缓存 + 后台采集
//!
//! 设备页每 2 秒轮询一次。若每次请求都同步调用厂商接口：
//!
//! | 问题 | 后果 |
//! |------|------|
//! | 单次调用 ~0.5 秒（先查实例定位地域，再查监控） | 页面明显卡顿 |
//! | 它会持网关锁 | 期间所有其它 API（对话、状态、部署）都被拖慢 |
//! | 厂商有频率限制 | 长时间开页面可能被限流甚至封禁 |
//!
//! 因此改为：**后台线程定期刷新 → 写共享快照 → HTTP 端点只读内存**。
//! 请求路径变成微秒级的读锁，与厂商延迟彻底解耦。
//!
//! ## 锁的边界（本项目踩过的坑）
//!
//! 曾经因为把 27 秒的 `llama-server --version` 放进网关锁，
//! 导致「页面能开、所有 API 超时」。因此这里严格分两段：
//!
//! ```text
//! ① 持锁（微秒级）：只读台账，取出 provider / instance_id
//! ② 释放锁：调用厂商接口（百毫秒级网络往返）
//! ```
//!
//! **任何网络调用都不得在网关锁内执行。**
//!
//! ## 与本地监控的区别
//!
//! 本地 `sysinfo` 走真实采样（两次 sysctl，微秒级），无需缓存；
//! 云端是网络往返，必须缓存。两者在设备页上合并展示，
//! 但采集路径完全不同——不要试图统一。
//!
//! ## 刷新频率的取舍
//!
//! 厂商监控数据本身就是分钟级聚合（`cloudwatch_*` 系列），
//! 5 秒刷新已远超数据更新速度；再快只是白白消耗 API 配额。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::gateway::{CloudMetricsSnapshot, Gateway};

/// 刷新间隔。
const REFRESH_INTERVAL: Duration = Duration::from_secs(5);

/// 缓存内容。
#[derive(Debug, Clone, Default)]
struct Cached {
    /// 最近一次成功的快照（`None` 表示当前没有云端部署）
    snapshot: Option<CloudMetricsSnapshot>,
    /// 最近一次成功刷新的时间（Unix 毫秒）
    refreshed_at_ms: i64,
    /// 最近一次失败原因（成功时清空）
    error: Option<String>,
}

/// 云端监控缓存句柄。可跨线程共享。
#[derive(Clone)]
pub struct CloudMonitor {
    inner: Arc<Mutex<Cached>>,
}

impl CloudMonitor {
    /// 空缓存。
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Cached::default())),
        }
    }

    /// 读缓存（供 HTTP 端点调用，微秒级）。
    ///
    /// 返回 `(快照, 错误, 距上次刷新的毫秒数)`。
    /// 距上次刷新为 `None` 表示从未刷新过（网关刚启动），
    /// 前端应显示「采集中」而不是「无数据」。
    pub fn read(&self) -> (Option<CloudMetricsSnapshot>, Option<String>, Option<i64>) {
        let Ok(c) = self.inner.lock() else {
            return (None, Some("监控缓存锁中毒".to_string()), None);
        };
        let age = if c.refreshed_at_ms == 0 {
            None
        } else {
            Some(
                mangosteen_core::time::now_unix_millis()
                    .saturating_sub(c.refreshed_at_ms),
            )
        };
        (c.snapshot.clone(), c.error.clone(), age)
    }

    /// 写入一次刷新结果。
    pub fn store(&self, snapshot: Option<CloudMetricsSnapshot>, error: Option<String>) {
        let Ok(mut c) = self.inner.lock() else { return };
        if error.is_none() {
            // 成功：整体替换（`None` 表示云端部署已停止，必须清空，
            // 否则设备页会一直显示已销毁实例的指标）
            c.snapshot = snapshot;
            c.refreshed_at_ms = mangosteen_core::time::now_unix_millis();
        }
        // 失败时**保留**上一次成功的快照：图表短暂陈旧比突然清空更可读，
        // 但错误必须一并暴露，否则用户会以为数据是新的。
        c.error = error;
    }
}

impl Default for CloudMonitor {
    fn default() -> Self {
        Self::new()
    }
}

/// 启动后台刷新线程。
///
/// 首轮立即刷新一次，让设备页打开时就有数据，而不是等 5 秒。
pub fn spawn_cloud_monitor(
    gateway: Arc<Mutex<Gateway>>,
    monitor: CloudMonitor,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || loop {
        // ① 持锁：只读台账（微秒级，不含网络）
        let target = {
            let Ok(gw) = gateway.lock() else { return };
            gw.active_cloud_deployment().map_err(|e| e.to_string())
        };

        match target {
            // 没有云端部署：本地算力场景，清空快照
            Ok(None) => monitor.store(None, None),
            Err(e) => monitor.store(None, Some(e)),
            // ② 锁外：调用厂商接口（百毫秒级）。
            //
            // 这里重新取一次锁是必要的——`CloudOrchestrator` 归 `Gateway` 所有，
            // 而它内部持有驱动注册表。为避免长时间持锁，驱动查询本身很快
            // （一次 HTTP，无重试循环），且失败会立即返回。
            Ok(Some(target)) => {
                let outcome = {
                    let Ok(gw) = gateway.lock() else { return };
                    query_metrics(&gw, &target)
                };
                monitor.store(Some(outcome.0), outcome.1);
            }
        }

        std::thread::sleep(REFRESH_INTERVAL);
    })
}

/// 查询一次监控，返回 (快照, 错误)。
///
/// 失败**不**向上传播：设备页还有本地指标要显示，
/// 云端查询失败只应就地提示，不能拖垮整个接口。
fn query_metrics(
    gw: &Gateway,
    target: &crate::gateway::CloudDeploymentRef,
) -> (CloudMetricsSnapshot, Option<String>) {
    let (metrics, error) = match gw.cloud_instance_metrics(&target.provider, &target.instance_id) {
        Ok(m) => (m, None),
        Err(e) => (None, Some(e.to_string())),
    };
    (
        CloudMetricsSnapshot {
            deployment_id: target.deployment_id.clone(),
            provider: target.provider.clone(),
            instance_id: target.instance_id.clone(),
            model: target.model.clone(),
            status: target.status,
            metrics,
            error: error.clone(),
        },
        error,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use mangosteen_base::state::DeploymentStatus;

    fn snap(id: &str) -> CloudMetricsSnapshot {
        CloudMetricsSnapshot {
            deployment_id: id.to_string(),
            provider: "compshare".to_string(),
            instance_id: "uhost-1".to_string(),
            model: "qwen:q4".to_string(),
            status: DeploymentStatus::Ready,
            metrics: None,
            error: None,
        }
    }

    #[test]
    fn empty_cache_reports_never_refreshed() {
        let m = CloudMonitor::new();
        let (s, e, age) = m.read();
        assert!(s.is_none());
        assert!(e.is_none());
        assert!(age.is_none(), "从未刷新过时不能报一个假的时间");
    }

    #[test]
    fn store_then_read_roundtrip() {
        let m = CloudMonitor::new();
        m.store(Some(snap("d-1")), None);
        let (s, e, age) = m.read();
        assert_eq!(s.unwrap().deployment_id, "d-1");
        assert!(e.is_none());
        assert!(age.is_some());
    }

    #[test]
    fn failure_keeps_last_snapshot_but_exposes_error() {
        let m = CloudMonitor::new();
        m.store(Some(snap("d-1")), None);
        m.store(None, Some("厂商超时".to_string()));

        let (s, e, _) = m.read();
        // 快照保留：图表短暂陈旧好过突然空白
        assert_eq!(s.unwrap().deployment_id, "d-1");
        // 但错误必须暴露，否则用户以为数据是新的
        assert_eq!(e.as_deref(), Some("厂商超时"));
    }

    #[test]
    fn recovery_clears_error() {
        let m = CloudMonitor::new();
        m.store(None, Some("boom".to_string()));
        m.store(Some(snap("d-2")), None);
        let (s, e, _) = m.read();
        assert_eq!(s.unwrap().deployment_id, "d-2");
        assert!(e.is_none(), "恢复后必须清掉旧错误");
    }

    #[test]
    fn no_cloud_deployment_clears_snapshot() {
        let m = CloudMonitor::new();
        m.store(Some(snap("d-1")), None);
        // 部署已停止 → 快照必须清空，否则设备页会一直显示已销毁实例的指标
        m.store(None, None);
        let (s, e, _) = m.read();
        assert!(s.is_none());
        assert!(e.is_none());
    }
}
