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

/// 探测连续失败多少轮后触发隧道自愈重建（约 15 秒）。
///
/// 瞬时失败（模型加载期 503、网络抖动）不应触发重建；持续失败才说明
/// 隧道真的断了（如 CLI 部署后未通知网关），值得发起一次 SSH 重建。
const RESTORE_AFTER_MISSES: u32 = 3;

/// 隧道重建冷却期：同一部署两次重建尝试的最小间隔。
///
/// 实例已关机时 SSH 会一直超时，每轮都试是浪费；60 秒节流既能让
/// 真正的隧道断裂在合理时间内自愈，又不会反复冲击已关机的实例。
const RESTORE_COOLDOWN: Duration = Duration::from_secs(60);

/// 单次探测的操作超时。
///
/// 2 秒不是拍脑袋：本地回环往返本应是毫秒级，但隧道场景（云端部署）
/// 的 `/health` 要走 SSH 加密隧道 + 公网 RTT，实测 P40 纯 CPU 推理节点
/// 常态 1.0~1.7 秒（含远端 llama-server 线程调度抖动），400ms 的旧值
/// 误判率约 40%——探测线程连续 3 轮失败还会触发不必要的隧道重建
/// （重建期间端口被占，真正的隧道反而建不起来，状态永久卡 unactive）。
/// 代价是失败探测要等满 2 秒，但探测在后台线程、5 秒一轮，用户无感。
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// `ready` 降级为 `failed` 前必须连续失败的探测轮数。
///
/// 已废弃：探测线程不再修改 DeploymentStatus，只更新 run_status。
/// 保留常量以免代码引用，但不再使用。
#[allow(dead_code)]
const DOWNGRADE_AFTER_MISSES: u32 = 3;

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
    /// TLS 是否启用（P1 传输加密）：true 时 /health 探活走 https（自签
    /// 证书 curl -k）。云端引擎由引导链证书以 --ssl-* 启动后，明文探测
    /// 永远失败，会把 https 引擎误判为未运行。
    tls: bool,
    /// P0 证书钉扎路径（{config_dir}/tls/<instance>.crt）——Some 时
    /// --cacert 强校验，None（旧台账/证书未拉回）降级 --insecure。
    tls_cert_path: Option<std::path::PathBuf>,
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
            rec.plan.model.alias.as_deref(),
        ),
        created_at: rec.plan.created_at,
        updated_at: rec.updated_at,
        port: rec.plan.local_access_port(),
        owns_instance,
        tls: rec.plan.cloud.as_ref().map(|c| c.tls).unwrap_or(false),
        tls_cert_path: rec
            .plan
            .origin
            .instance_id()
            .map(|id| {
                mangosteen_platform::paths::config_dir(&mangosteen_platform::paths::real_env)
                    .join("tls")
                    .join(format!("{id}.crt"))
            })
            .filter(|p| p.is_file()),
        status: rec.status,
    }
}

/// 启动后台探测线程。
    /// https 探活（P1 TLS）：curl -k 请求 /health，判 200。
    /// 用于云端引擎以 --ssl-* 启动后的 runprobe 探测——手写 TCP 的
    /// probe_http_ok 无法完成 TLS 握手，会对 https 引擎永远返回 false。
    fn probe_https_ok(
        host: &str,
        port: u16,
        path: &str,
        timeout: std::time::Duration,
        cacert: Option<&std::path::Path>,
    ) -> bool {
        // P0 TLS 证书钉扎（2026-11-19）：有本地钉扎证书（{config_dir}/tls/
        // <instance>.crt）时 --cacert 强校验，指纹不符/缺失才降级 --insecure
        // （兼容旧台账——引导链未拉回证书的实例）。
        //
        // 修复（2026-11-20 实测）：证书 CN=llama-server（引导链自签），
        // --cacert 校验时 curl 同时校验 hostname——URL 用 127.0.0.1 会报
        // 「certificate subject name 'llama-server' does not match target
        // host name '127.0.0.1'」(curl 60)，探测恒失败 → run_status 永远
        // unactive。改为：URL host 用证书 CN（llama-server），--resolve
        // 把 (llama-server, port) 映射到 127.0.0.1——证书链钉扎与 hostname
        // 校验同时成立（钉扎保护完整保留）。无钉扎证书时维持 127.0.0.1。
        let (url, resolve) = if cacert.is_some() {
            (
                format!("https://llama-server:{port}{path}"),
                Some(format!("llama-server:{port}:127.0.0.1")),
            )
        } else {
            (format!("https://{host}:{port}{path}"), None)
        };
        let mut cmd = std::process::Command::new("curl");
        cmd.arg("--silent")
            .arg("--show-error")
            .arg("--noproxy")
            .arg("*");
        if let Some(cert) = cacert {
            cmd.arg("--cacert").arg(cert);
            if let Some(r) = resolve {
                cmd.arg("--resolve").arg(r);
            }
        } else {
            cmd.arg("--insecure");
        }
        cmd.arg("--max-time")
            .arg(timeout.as_secs().to_string())
            .arg("--output")
            .arg("/dev/null")
            .arg("--write-out")
            .arg("%{http_code}")
            .arg(&url)
            .output()
            .map(|o| {
                o.status.success()
                    && String::from_utf8_lossy(&o.stdout).trim().starts_with("200")
            })
            .unwrap_or(false)
    }

pub fn spawn_runtime_probe(
    gateway: Arc<Gateway>,
    backend_snapshot: Arc<RwLock<BTreeMap<String, BackendTarget>>>,
    probe: RuntimeProbe,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        // ready→failed 的降级计数：跨探测轮次累计，只在连续失败时触发。
        let mut misses: BTreeMap<String, u32> = BTreeMap::new();
        // 隧道重建节流：记录每个部署上次尝试重建的时间（Instant），
        // 冷却期内不重复尝试（实例已关机时避免反复 SSH 超时浪费）
        let mut last_restore: BTreeMap<String, std::time::Instant> = BTreeMap::new();
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

            // ③ 做本地端口探测，只探测 Ready 状态的记录，探测结果只更新 run_status，
            //    不修改 DeploymentStatus（避免后台线程误降级状态文件）。
            let mut snapshot = BTreeMap::new();
            for t in targets {
                let eligible = t.owns_instance
                    && t.status == DeploymentStatus::Ready
                    && t.port.is_some()
                    && newest
                        .get(&t.model_id)
                        .is_some_and(|n| *n == (t.created_at, t.updated_at));
                if !eligible {
                    continue;
                }
                let mapped = if t.tls {
                    // TLS（P1）：引擎以 https 启动（自签证书）——手写 TCP
                    // HTTP 探测无法完成 TLS 握手，走系统 curl。P0 证书钉扎：
                    // 有本地钉扎证书时 --cacert 强校验（防隧道端点 MITM）。
                    probe_https_ok(
                        "127.0.0.1",
                        t.port.unwrap(),
                        "/v1/models",
                        PROBE_TIMEOUT,
                        t.tls_cert_path.as_deref(),
                    )
                } else {
                    mangosteen_platform::process::probe_http_ok(
                        "127.0.0.1",
                        t.port.unwrap(),
                        "/v1/models",
                        PROBE_TIMEOUT,
                    )
                };

                // 隧道自愈（二道防线）：探测连续失败达到阈值时，
                // 主动重建 SSH 隧道。覆盖「CLI 部署后未通知网关」或
                // 「隧道意外断开」场景，让网关节扭自行恢复。
                if !mapped {
                    let miss = misses.entry(t.id.clone()).or_insert(0);
                    *miss += 1;
                    let cooled_down = last_restore
                        .get(&t.id)
                        .map(|last| last.elapsed() >= RESTORE_COOLDOWN)
                        .unwrap_or(true);
                    if *miss >= RESTORE_AFTER_MISSES && cooled_down {
                        let gw = gateway.as_ref();
                        match gw.restore_deployment_tunnel(&t.id) {
                            Ok(()) => {
                                eprintln!("[probe] {}：探测失败，已重建 SSH 隧道", t.id);
                                last_restore.insert(t.id.clone(), std::time::Instant::now());
                                // 重建后留待下一轮探测验证
                            }
                            Err(e) => {
                                // P1 实例漂移（2026-11-19）：厂商明确返回
                                // 实例不存在（回收/删除）→ 部署自动标记
                                // Failed（防「幽灵 ready」）。其余错误
                                // （SSH 超时/关机）只进冷却，等实例恢复。
                                let err_msg = e.to_string();
                                if err_msg.contains("instance_not_found")
                                    || err_msg.contains("实例")
                                        && err_msg.contains("不存在")
                                {
                                    let gw2 = gateway.as_ref();
                                    let _ = gw2.mark_deployment_lost(
                                        &t.id,
                                        "实例不存在（instance_not_found）",
                                    );
                                    eprintln!(
                                        "[probe] {}：实例已被回收，部署自动标记失败",
                                        t.id
                                    );
                                }
                                // 实例可能已关机/删除：记录时间进入冷却，
                                // 避免每轮都发起 SSH 尝试
                                last_restore.insert(t.id.clone(), std::time::Instant::now());
                                eprintln!("[probe] {}：隧道重建失败（进入冷却）：{}", t.id, e);
                            }
                        }
                    }
                } else {
                    misses.remove(&t.id);
                }
                snapshot.insert(t.id.clone(), mapped);

                // 探测结果只更新 run_status（active/unactive），不修改 DeploymentStatus
                let gw = gateway.as_ref();
                let _ = gw.update_run_status(&t.id, mapped);
            }
            probe.store(snapshot);

            // 台账运行状态翻转后重建对话路由快照，让探测自愈立刻生效。
            crate::http::handlers::inference::rebuild_snapshot(&gateway, &backend_snapshot);

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
