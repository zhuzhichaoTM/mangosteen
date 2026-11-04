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
//! 只有「可能真的在服务」的记录才值得探测，其余一律跳过、按台账展示：
//!
//! | 台账状态 | 探测结果 | 动作 |
//! |---|---|---|
//! | `ready` | 端口健康 | 保持 `ready` |
//! | `ready` | **连续 [`DOWNGRADE_AFTER_MISSES`] 轮**探测失败 | 降级 `failed` |
//! | `failed` | 端口健康 | 恢复 `ready`（台账自愈） |
//! | `failed` | 端口不可用 | 保持 `failed` |
//! | `stopped` / `planned` / `provisioning` / `destroyed` | 不探测 | 保持原状 |
//!
//! `stopped` 是用户的显式动作态（停止服务后隧道必然不通），不能因为
//! 「探测不到」就降级成 `failed`，否则破坏「停止保留实例、可再启动」
//! 的生命周期。
//!
//! ## 两个回归教训（判定条件必须写对）
//!
//! 1. **单次探测失败不能立即降级**：llama-server 加载模型期间 `/health`
//!    返回 503，探测只认 200。部署刚完成的几秒内一次瞬时失败就把
//!    `ready` 翻成 `failed`，而旧状态机不允许 `failed → ready`，
//!    记录从此永久卡死，对话页签看不到模型。
//! 2. **无实例的记录不能探测**：台账里有大量历史脏数据（早期失败尝试，
//!    `instance_id` 为空）与本地部署（`local_access_port()` 为 `None`），
//!    它们的端口字段可能与其他活跃隧道的端口撞号。拿一个不属于自己的
//!    端口的探测结果回写状态，会把脏记录「复活」成 ready 或把好记录
//!    误杀。因此探测资格要求：云端实例标识非空 + 有本地端口 + 是该模型
//!    **最新创建**的那条记录（与 `dedupe_by_model` 同口径）。
//!
//! 「已部署」不由这里定义——它看台账里有没有实例 ID；这里只回答
//! 「现在是否真的在服务」。

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use mangosteen_base::state::{DeploymentRecord, DeploymentStatus};

use crate::gateway::Gateway;
use crate::protocol::BackendTarget;

/// 探测间隔。
///
/// 与云端监控的 5 秒同理：更密的频率只是白烧 CPU，用户感知不到差异。
const PROBE_INTERVAL: Duration = Duration::from_secs(5);

/// 单次探测的操作超时。
///
/// 只探测本机回环端口，400 毫秒已远超正常握手时间；给足余量是为了
/// 容忍机器在编译/推理满载时的调度抖动。
const PROBE_TIMEOUT: Duration = Duration::from_millis(400);

/// `ready` 降级为 `failed` 前必须连续失败的探测轮数。
///
/// 单轮失败几乎总是瞬时态（模型加载期 503、隧道重建、本机满载抖动），
/// 立即降级会造成「部署完成却显示失败」。两轮（约 10 秒）足够滤掉
/// 瞬时失败，又不至于让真死的部署在 UI 上挂太久。
const DOWNGRADE_AFTER_MISSES: u32 = 2;

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

/// 探测目标：从台账记录中提取的最小判定字段。
struct ProbeTarget {
    id: String,
    /// 模型完整 ID（名字+量化），与 `dedupe_by_model` 同键。
    model_id: String,
    /// 与 `dedupe_by_model` 一致的新旧依据。
    created_at: i64,
    updated_at: i64,
    port: Option<u16>,
    /// 云端实例标识非空（云厂商实例），或自定义节点有设备绑定。
    owns_instance: bool,
    status: DeploymentStatus,
}

fn probe_target_of(rec: &DeploymentRecord) -> ProbeTarget {
    // 探测资格看「是否拥有真实算力资源」：
    //   · 云厂商实例 → 容器 ID 非空
    //   · 自定义节点 → 没有容器 ID，但设备绑定存在（机器本身就是实例），
    //     与 `pick_reusable` 的口径一致。
    // 两者皆空的记录是早期失败尝试的脏数据，端口字段不可信，跳过。
    let owns_instance = match &rec.plan.origin {
        mangosteen_core::plan::ComputeOrigin::Cloud { instance_id, .. } => {
            let has_device = rec
                .plan
                .cloud
                .as_ref()
                .and_then(|c| c.device_id.as_ref())
                .map(|d| !d.trim().is_empty())
                .unwrap_or(false);
            !instance_id.trim().is_empty() || has_device
        }
    };
    ProbeTarget {
        id: rec.plan.id.clone(),
        model_id: mangosteen_core::model::ModelEntry::derive_id(
            &rec.plan.model.name,
            rec.plan.model.quant,
        ),
        created_at: rec.plan.created_at,
        updated_at: rec.updated_at,
        port: rec.plan.local_access_port(),
        owns_instance,
        status: rec.status,
    }
}

/// 启动后台探测线程。
pub fn spawn_runtime_probe(
    gateway: Arc<Gateway>,
    backend_snapshot: Arc<RwLock<BTreeMap<String, BackendTarget>>>,
    probe: RuntimeProbe,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        // ready→failed 的降级计数：跨探测轮次累计，只在连续失败时触发。
        let mut misses: BTreeMap<String, u32> = BTreeMap::new();
        loop {
            // ① 只读台账（微秒级）
            let targets: Vec<ProbeTarget> = {
                let gw = gateway.as_ref();
                gw.list_deployments()
                    .unwrap_or_default()
                    .iter()
                    .map(probe_target_of)
                    .collect()
            };

            // ② 每模型只认最新创建的那条记录（与 dedupe_by_model 同口径）。
            //    旧记录的端口探测结果不代表当前部署，不能拿来回写状态。
            let mut newest: BTreeMap<String, (i64, i64)> = BTreeMap::new();
            for t in &targets {
                let key = (t.created_at, t.updated_at);
                match newest.get(&t.model_id) {
                    Some(cur) if *cur >= key => {}
                    _ => {
                        newest.insert(t.model_id.clone(), key);
                    }
                }
            }

            // ③ 做本地端口探测，按需回写台账。
            let mut snapshot = BTreeMap::new();
            let mut changed = false;
            for t in targets {
                let eligible = t.owns_instance
                    && matches!(
                        t.status,
                        DeploymentStatus::Ready | DeploymentStatus::Failed
                    )
                    && t.port.is_some()
                    && newest
                        .get(&t.model_id)
                        .is_some_and(|n| *n == (t.created_at, t.updated_at));
                if !eligible {
                    // 未探测的记录不出现在快照里：is_running 返回 None，
                    // 前端按台账状态展示，而不是断言「已停止」。
                    continue;
                }
                let mapped = mangosteen_platform::process::probe_http_ok(
                    "127.0.0.1",
                    t.port.unwrap(),
                    "/health",
                    PROBE_TIMEOUT,
                );
                if mapped {
                    misses.remove(&t.id);
                } else {
                    *misses.entry(t.id.clone()).or_insert(0) += 1;
                }
                snapshot.insert(t.id.clone(), mapped);

                let downgrade = t.status == DeploymentStatus::Ready
                    && !mapped
                    && misses.get(&t.id).copied().unwrap_or(0) >= DOWNGRADE_AFTER_MISSES;
                let recover = t.status == DeploymentStatus::Failed && mapped;
                if downgrade || recover {
                    // ④ 只在锁内做微秒级台账状态同步；不做网络操作。
                    {
                        let gw = gateway.as_ref();
                        if gw.refresh_deployment_mapping(&t.id, mapped).is_ok() {
                            changed = true;
                        }
                    }
                }
            }
            probe.store(snapshot);

            // ⑤ 台账状态翻转后重建对话路由快照，让探测自愈立刻生效。
            if changed {
                crate::http::handlers::inference::rebuild_snapshot(&gateway, &backend_snapshot);
            }

            std::thread::sleep(PROBE_INTERVAL);
        }
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
