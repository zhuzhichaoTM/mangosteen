//! 部署运行态探测：区分「已部署但已停止」与「正在运行」。
//!
//! 归属：**API 网关层**（部署页签的运行状态数据源）。
//!
//! ## 为什么不能放进请求路径
//!
//! 判定「是否在跑」只能靠探测本地隧道端口上的 `/health`。单次探测虽有
//! 上限（约 400 毫秒），但部署页每几秒轮询一次，且探测失败要等满超时；
//! 放在请求里会让「一个已停止的部署」把页面拖成卡顿。
//!
//! 因此与云端监控同样的做法：**后台线程周期探测 → 写共享快照 →
//! HTTP 端点只读内存**。
//!
//! ## 判定口径
//!
//! | 台账状态 | 探测结果 | 对外 running |
//! |---|---|---|
//! | `ready` | 端口可用 | `true` |
//! | `ready` | 端口不可用（进程崩了/隧道断了） | `false` |
//! | `stopped` / `failed` | — | `false`（不浪费探测） |
//!
//! 「已部署」不由这里定义——它看台账里有没有实例 ID；这里只回答
//! 「现在是否真的在服务」。

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::gateway::Gateway;

/// 探测间隔。
///
/// 与云端监控的 5 秒同理：更密的频率只是白烧 CPU，用户感知不到差异。
const PROBE_INTERVAL: Duration = Duration::from_secs(5);

/// 单次探测的操作超时。
///
/// 只探测本机回环端口，400 毫秒已远超正常握手时间；给足余量是为了
/// 容忍机器在编译/推理满载时的调度抖动。
const PROBE_TIMEOUT: Duration = Duration::from_millis(400);

/// 运行态缓存句柄。可跨线程共享。
#[derive(Clone, Default)]
pub struct RuntimeProbe {
    inner: Arc<Mutex<BTreeMap<String, bool>>>,
}

impl RuntimeProbe {
    /// 空缓存。
    pub fn new() -> Self {
        Self::default()
    }

    /// 读某个部署的运行态。
    ///
    /// 返回 `None` 表示「还没探测过」（网关刚启动或刚部署完），
    /// 前端应按台账状态展示，而不是断言「已停止」。
    pub fn is_running(&self, deployment_id: &str) -> Option<bool> {
        let Ok(c) = self.inner.lock() else {
            return None;
        };
        c.get(deployment_id).copied()
    }

    /// 整体替换一次探测结果。
    pub fn store(&self, snapshot: BTreeMap<String, bool>) {
        let Ok(mut c) = self.inner.lock() else { return };
        *c = snapshot;
    }
}

/// 启动后台探测线程。
pub fn spawn_runtime_probe(
    gateway: Arc<Mutex<Gateway>>,
    probe: RuntimeProbe,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || loop {
        // ① 持锁：只读台账（微秒级）
        let targets: Vec<(
            String,
            Option<u16>,
            Option<u16>,
            mangosteen_base::state::DeploymentStatus,
        )> = {
            let Ok(gw) = gateway.lock() else { return };
            gw.list_deployments()
                .unwrap_or_default()
                .into_iter()
                .map(|r| {
                    (
                        r.plan.id.clone(),
                        r.plan.local_access_port(),
                        r.plan.cloud.as_ref().map(|c| c.engine_port),
                        r.status,
                    )
                })
                .collect()
        };

        // ② 释放锁：做本地端口探测，并按映射结果回写台账。
        let mut snapshot = BTreeMap::new();
        for (id, port, remote_port, state) in targets {
            let eligible = matches!(
                state,
                mangosteen_base::state::DeploymentStatus::Ready
                    | mangosteen_base::state::DeploymentStatus::Stopped
                    | mangosteen_base::state::DeploymentStatus::Failed
            );
            let mapped = eligible
                && port.is_some()
                && remote_port.is_some()
                && mangosteen_platform::process::probe_http_ok(
                    "127.0.0.1",
                    port.unwrap(),
                    "/health",
                    PROBE_TIMEOUT,
                );
            snapshot.insert(id.clone(), mapped);

            // ③ 只在锁内做微秒级台账状态同步；不做网络操作。
            if let Ok(mut gw) = gateway.lock() {
                let _ = gw.refresh_deployment_mapping(&id, mapped);
            }
        }
        probe.store(snapshot);

        std::thread::sleep(PROBE_INTERVAL);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_deployment_reports_none() {
        let p = RuntimeProbe::new();
        assert_eq!(p.is_running("dep-1"), None, "未探测过不能断言已停止");
    }

    #[test]
    fn store_then_read_roundtrip() {
        let p = RuntimeProbe::new();
        let mut m = BTreeMap::new();
        m.insert("dep-1".to_string(), true);
        m.insert("dep-2".to_string(), false);
        p.store(m);
        assert_eq!(p.is_running("dep-1"), Some(true));
        assert_eq!(p.is_running("dep-2"), Some(false));
    }

    #[test]
    fn empty_snapshot_clears_previous() {
        let p = RuntimeProbe::new();
        let mut m = BTreeMap::new();
        m.insert("dep-1".to_string(), true);
        p.store(m);
        // 台账被清空后必须整体替换，否则会残留已不存在的部署
        p.store(BTreeMap::new());
        assert_eq!(p.is_running("dep-1"), None);
    }
}
