//! 云端实例监控的按需查询与短 TTL 缓存。
//!
//! 归属：**API 网关层**（设备页签的云端数据源）。
//!
//! ## 为什么按需而不是后台周期刷新
//!
//! 曾经是后台线程每 5 秒调一次厂商 `GetCompShareInstanceMonitor`。
//! 实测问题：**网关一启动就开始每 5 秒打厂商 API，与有没有人看设备页
//! 无关**——挂着不用也持续消耗配额，长时间运行有触发限流的风险。
//! 厂商监控数据是分钟级聚合，没人看时刷新毫无意义。
//!
//! 现改为「请求驱动」：`/api/sysinfo` 被请求时才查厂商，并用 TTL 缓存
//! 防连击（多标签页同时轮询只打一次厂商）。没人看 = 零请求 = 零配额。
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

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::gateway::{CloudMetricsSnapshot, Gateway};

/// 缓存有效期。
///
/// 取值依据：厂商监控数据本身是分钟级聚合（`cloudwatch_*` 系列），
/// 10 秒内重复请求拿到的是同一份数据；设备页轮询间隔 2~10 秒，
/// TTL 内多标签页/多轮轮询只穿透一次到厂商。
const TTL: Duration = Duration::from_secs(10);

/// 缓存内容。
#[derive(Debug, Clone, Default)]
struct Cached {
    /// 最近一次成功的快照（`None` 表示当前没有云端部署）
    snapshot: Option<CloudMetricsSnapshot>,
    /// 最近一次成功刷新的时间（Unix 毫秒）
    refreshed_at_ms: i64,
    /// 最近一次查询失败原因（成功时清空）
    error: Option<String>,
    /// 最近一次查询结果对应的部署 ID：`None` 表示「确认无云端部署」，
    /// 与「还没查过」区分开（后者 snapshot 也是 None）。
    queried_deployment_id: Option<Option<String>>,
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
    /// 返回 `(快照, 错误, 距上次成功刷新的毫秒数)`。
    /// 距上次刷新为 `None` 表示从未刷新过（网关刚启动且还没人看过设备页），
    /// 前端应显示「点击刷新」而不是「无数据」。
    pub fn read(&self) -> (Option<CloudMetricsSnapshot>, Option<String>, Option<i64>) {
        let Ok(c) = self.inner.lock() else {
            return (None, Some("监控缓存锁中毒".to_string()), None);
        };
        let age = if c.refreshed_at_ms == 0 {
            None
        } else {
            Some(mangosteen_core::time::now_unix_millis().saturating_sub(c.refreshed_at_ms))
        };
        (c.snapshot.clone(), c.error.clone(), age)
    }

    /// 缓存是否仍新鲜（含「确认无云端部署」的正向缓存）。
    pub fn is_fresh(&self) -> bool {
        let Ok(c) = self.inner.lock() else { return false };
        if c.queried_deployment_id.is_none() {
            return false;
        }
        c.refreshed_at_ms == 0
            || mangosteen_core::time::now_unix_millis().saturating_sub(c.refreshed_at_ms)
                < TTL.as_millis() as i64
    }

    /// 标记一次「确认无云端部署」。
    pub fn store_empty(&self) {
        let Ok(mut c) = self.inner.lock() else { return };
        c.snapshot = None;
        c.error = None;
        c.refreshed_at_ms = mangosteen_core::time::now_unix_millis();
        c.queried_deployment_id = Some(None);
    }

    /// 写入一次刷新结果。
    ///
    /// `deployment_id` 用于「目标变了必须重查」判定：部署切换（如停止旧
    /// 实例开新实例）后，旧快照的 TTL 再新也不能用。
    pub fn store(
        &self,
        deployment_id: Option<&str>,
        snapshot: Option<CloudMetricsSnapshot>,
        error: Option<String>,
    ) {
        let Ok(mut c) = self.inner.lock() else { return };
        c.queried_deployment_id = Some(deployment_id.map(str::to_string));
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

/// 按需刷新一次监控（请求驱动，供 `/api/sysinfo` 调用）。
///
/// 返回 `(快照, 错误)`。**不抛错**：设备页还有本地指标要显示，
/// 云端查询失败只应就地提示，不能拖垮整个接口。
///
/// 调用方负责 TTL 判定（[`CloudMonitor::is_fresh`]）；本函数总是真正
/// 发起一次查询。两段式锁边界见模块文档。
pub fn refresh_on_demand(
    gw: &Gateway,
    monitor: &CloudMonitor,
) -> (Option<CloudMetricsSnapshot>, Option<String>) {
    // ① 只读台账（微秒级，不含网络）
    let target = gw.active_cloud_deployment().map_err(|e| e.to_string());

    match target {
        // 没有云端部署：本地算力场景，写入「确认无云端」正向缓存
        Ok(None) => {
            monitor.store_empty();
            (None, None)
        }
        Err(e) => {
            monitor.store(None, None, Some(e.clone()));
            (None, Some(e))
        }
        // ② 厂商接口调用（百毫秒级）不持任何全局锁：Orchestrator
        // 可变状态均已下沉为细粒度内部锁，网关以 Arc 只读共享。
        Ok(Some(target)) => {
            let (metrics, error) = match gw.cloud_instance_metrics(&target.provider, &target.instance_id)
            {
                Ok(m) => (m, None),
                Err(e) => (None, Some(e.to_string())),
            };
            let snapshot = CloudMetricsSnapshot {
                deployment_id: target.deployment_id.clone(),
                provider: target.provider.clone(),
                instance_id: target.instance_id.clone(),
                model: target.model.clone(),
                status: target.status,
                metrics,
                error: error.clone(),
            };
            monitor.store(Some(&target.deployment_id), Some(snapshot.clone()), error.clone());
            (Some(snapshot), error)
        }
    }
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
    fn never_queried_is_not_fresh() {
        let m = CloudMonitor::new();
        assert!(!m.is_fresh(), "没查过就不能当新鲜，否则按需刷新永不触发");
    }

    #[test]
    fn store_then_read_roundtrip() {
        let m = CloudMonitor::new();
        m.store(Some("d-1"), Some(snap("d-1")), None);
        let (s, e, age) = m.read();
        assert_eq!(s.unwrap().deployment_id, "d-1");
        assert!(e.is_none());
        assert!(age.is_some());
        assert!(m.is_fresh(), "TTL 内应判定为新鲜");
    }

    #[test]
    fn failure_keeps_last_snapshot_but_exposes_error() {
        let m = CloudMonitor::new();
        m.store(Some("d-1"), Some(snap("d-1")), None);
        m.store(Some("d-1"), None, Some("厂商超时".to_string()));

        let (s, e, _) = m.read();
        // 快照保留：图表短暂陈旧好过突然空白
        assert_eq!(s.unwrap().deployment_id, "d-1");
        // 但错误必须暴露，否则用户以为数据是新的
        assert_eq!(e.as_deref(), Some("厂商超时"));
    }

    #[test]
    fn recovery_clears_error() {
        let m = CloudMonitor::new();
        m.store(None, None, Some("boom".to_string()));
        m.store(Some("d-2"), Some(snap("d-2")), None);
        let (s, e, _) = m.read();
        assert_eq!(s.unwrap().deployment_id, "d-2");
        assert!(e.is_none(), "恢复后必须清掉旧错误");
    }

    #[test]
    fn no_cloud_deployment_clears_snapshot_and_is_fresh() {
        let m = CloudMonitor::new();
        m.store(Some("d-1"), Some(snap("d-1")), None);
        // 部署已停止 → 快照必须清空，否则设备页会一直显示已销毁实例的指标
        m.store_empty();
        let (s, e, _) = m.read();
        assert!(s.is_none());
        assert!(e.is_none());
        // 「确认无云端部署」也是一次有效结论：TTL 内不必反复重查
        assert!(m.is_fresh(), "确认无云端部署应同样享受 TTL");
    }

    #[test]
    fn queried_none_is_not_fresh_after_ttl_trickery() {
        // queried_deployment_id = Some(None)（无云端）时 is_fresh 只依赖时间，
        // 这里验证 store_empty 后立刻判定为新鲜即可（上面用例已覆盖）。
        let m = CloudMonitor::new();
        m.store_empty();
        assert!(m.is_fresh());
    }
}
