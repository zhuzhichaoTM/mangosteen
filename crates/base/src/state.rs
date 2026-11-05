//! 部署台账持久化。
//!
//! 归属：**基础能力层**。
//!
//! ## 为什么需要持久化
//!
//! 一次云端部署可能持续数小时并产生真实计费。进程重启后若丢失台账：
//!
//! * 用户看不到自己在跑什么实例 → 忘记关机 → 持续扣费；
//! * 无法清理已创建的实例 → 云端资源泄漏。
//!
//! 因此台账必须在**每次状态变更后立即落盘**（不是退出时保存），
//! 且写入走原子替换。这是「先记账，再动手」原则的落地。

use std::path::{Path, PathBuf};

use mangosteen_core::plan::DeploymentPlan;
use mangosteen_core::{MnError, MnResult};
use serde::{Deserialize, Serialize};

/// 一次部署的运行时状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentStatus {
    /// 已生成方案，尚未动手
    Planned,
    /// 正在拉起（本地引擎启动 / 云端实例创建）
    Provisioning,
    /// 服务就绪，可接受推理请求
    Ready,
    /// 已停止（进程已退出，云端实例保留）
    Stopped,
    /// 部署失败
    Failed,
    /// 已销毁（资源已回收）
    Destroyed,
}

impl DeploymentStatus {
    /// 是否仍占用资源。
    pub fn is_active(self) -> bool {
        matches!(
            self,
            DeploymentStatus::Provisioning | DeploymentStatus::Ready
        )
    }

    /// 是否终态。
    pub fn is_terminal(self) -> bool {
        matches!(self, DeploymentStatus::Destroyed)
    }

    /// 字符串形式。
    pub fn as_str(self) -> &'static str {
        match self {
            DeploymentStatus::Planned => "planned",
            DeploymentStatus::Provisioning => "provisioning",
            DeploymentStatus::Ready => "ready",
            DeploymentStatus::Stopped => "stopped",
            DeploymentStatus::Failed => "failed",
            DeploymentStatus::Destroyed => "destroyed",
        }
    }
}

/// 台账中的一条部署记录。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeploymentRecord {
    /// 部署方案（不可变）
    pub plan: DeploymentPlan,
    /// 当前状态
    pub status: DeploymentStatus,
    /// 本地引擎 PID（本地链路）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// 失败原因
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// 状态最后变更时间
    pub updated_at: i64,
}

impl DeploymentRecord {
    /// 由方案新建记录（初始状态 `Planned`）。
    pub fn from_plan(plan: DeploymentPlan) -> Self {
        Self {
            plan,
            status: DeploymentStatus::Planned,
            pid: None,
            error: None,
            updated_at: mangosteen_core::time::now_unix(),
        }
    }

    /// 状态迁移并刷新时间戳。
    ///
    /// 非法迁移会被拒绝：例如已 `Destroyed` 的记录不允许复活为 `Ready`。
    pub fn transition(&mut self, next: DeploymentStatus) -> MnResult<()> {
        let allowed = matches!(
            (self.status, next),
            (DeploymentStatus::Planned, DeploymentStatus::Provisioning)
                | (DeploymentStatus::Planned, DeploymentStatus::Failed)
                | (DeploymentStatus::Planned, DeploymentStatus::Stopped)
                | (DeploymentStatus::Planned, DeploymentStatus::Destroyed)
                | (DeploymentStatus::Provisioning, DeploymentStatus::Ready)
                | (DeploymentStatus::Provisioning, DeploymentStatus::Failed)
                | (DeploymentStatus::Provisioning, DeploymentStatus::Stopped)
                | (DeploymentStatus::Provisioning, DeploymentStatus::Destroyed)
                | (DeploymentStatus::Ready, DeploymentStatus::Stopped)
                | (DeploymentStatus::Ready, DeploymentStatus::Failed)
                | (DeploymentStatus::Ready, DeploymentStatus::Destroyed)
                | (DeploymentStatus::Stopped, DeploymentStatus::Provisioning)
                | (DeploymentStatus::Stopped, DeploymentStatus::Stopped)
                | (DeploymentStatus::Stopped, DeploymentStatus::Destroyed)
                | (DeploymentStatus::Failed, DeploymentStatus::Provisioning)
                | (DeploymentStatus::Failed, DeploymentStatus::Stopped)
                | (DeploymentStatus::Failed, DeploymentStatus::Destroyed)
                // 探测自愈：`Failed` 只代表「上次探测时端口没在服务」，
                // 不是终局。若后续探测发现本地隧道健康检查通过，说明部署
                // 实际仍在服务，必须允许翻回 `Ready`——否则一次瞬时探测
                // 失败（如模型加载期 /health 返回 503）会把记录永久卡死在
                // `Failed`，对话页签再也看不到该模型。
                | (DeploymentStatus::Failed, DeploymentStatus::Ready)
        );
        if !allowed {
            return Err(MnError::InvalidTransition {
                instance: self.plan.id.clone(),
                from: self.status.as_str().to_string(),
                action: format!("→ {}", next.as_str()),
            });
        }
        self.status = next;
        self.updated_at = mangosteen_core::time::now_unix();
        Ok(())
    }
}

/// 台账文件。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateFile {
    /// 格式版本
    #[serde(default = "one")]
    pub version: u32,
    /// 全部记录
    #[serde(default)]
    pub deployments: Vec<DeploymentRecord>,
}

fn one() -> u32 {
    1
}

impl Default for StateFile {
    fn default() -> Self {
        Self {
            version: 1,
            deployments: Vec::new(),
        }
    }
}

/// 台账存储。
#[derive(Debug, Clone)]
pub struct StateStore {
    path: PathBuf,
}

impl StateStore {
    /// 构造。
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// 台账路径。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 读取。
    pub fn load(&self) -> MnResult<StateFile> {
        match mangosteen_platform::fs::read_to_string_opt(&self.path)? {
            None => Ok(StateFile::default()),
            Some(t) if t.trim().is_empty() => Ok(StateFile::default()),
            Some(t) => {
                // 兼容纯云端重构前的台账：旧记录的 origin.kind=local
                // 已不再是有效部署来源，但历史记录（尤其 destroyed）不能阻塞
                // 网关启动。将其归一为不可用的 legacy cloud 记录；旧 local
                // 字段会由 serde 忽略，后续列表仍可展示/清理。
                let mut value: serde_json::Value =
                    serde_json::from_str(&t).map_err(|e| MnError::ConfigIo {
                        path: self.path.clone(),
                        reason: format!("部署台账 JSON 解析失败：{e}"),
                    })?;
                if let Some(records) = value.get_mut("deployments").and_then(|v| v.as_array_mut()) {
                    for record in records {
                        if record.pointer("/plan/origin/kind").and_then(|v| v.as_str())
                            == Some("local")
                        {
                            if let Some(origin) = record.pointer_mut("/plan/origin") {
                                *origin = serde_json::json!({
                                    "kind": "cloud",
                                    "provider": "legacy-local",
                                    "instance_id": ""
                                });
                            }
                        }
                    }
                }
                serde_json::from_value(value).map_err(|e| MnError::ConfigIo {
                    path: self.path.clone(),
                    reason: format!("部署台账 JSON 解析失败：{e}"),
                })
            }
        }
    }

    /// 写入（原子替换）。
    pub fn save(&self, state: &StateFile) -> MnResult<()> {
        let text = serde_json::to_string_pretty(state)
            .map_err(|e| MnError::config("state", format!("序列化失败：{e}")))?;
        mangosteen_platform::fs::atomic_write_str(&self.path, &text)
    }

    /// 新增记录。同 ID 已存在则覆盖（重建部署场景）。
    pub fn upsert(&self, record: DeploymentRecord) -> MnResult<()> {
        let mut state = self.load()?;
        match state
            .deployments
            .iter_mut()
            .find(|r| r.plan.id == record.plan.id)
        {
            Some(slot) => *slot = record,
            None => state.deployments.push(record),
        }
        state.deployments.sort_by(|a, b| a.plan.id.cmp(&b.plan.id));
        self.save(&state)
    }

    /// 只更新部署方案，保留记录当前的状态、PID 和错误信息。
    ///
    /// 部署准备阶段会逐步补齐节点、端口和实例信息，不能用
    /// `DeploymentRecord::from_plan` 覆盖整条记录，否则会把已经进入
    /// `provisioning` 的记录重置为 `planned`，后续 `planned -> ready`
    /// 就会被状态机正确拒绝。
    pub fn update_plan(&self, id: &str, plan: DeploymentPlan) -> MnResult<()> {
        let mut state = self.load()?;
        let rec = state
            .deployments
            .iter_mut()
            .find(|r| r.plan.id == id)
            .ok_or_else(|| MnError::bad_request(format!("部署 `{id}` 未登记")))?;
        if rec.plan.id != plan.id {
            return Err(MnError::bad_request(format!(
                "部署方案 ID 不一致：记录 `{id}`，方案 `{}`",
                plan.id
            )));
        }
        rec.plan = plan;
        rec.updated_at = mangosteen_core::time::now_unix();
        self.save(&state)
    }

    /// 按 ID 取记录。
    pub fn get(&self, id: &str) -> MnResult<Option<DeploymentRecord>> {
        Ok(self
            .load()?
            .deployments
            .into_iter()
            .find(|r| r.plan.id == id))
    }

    /// 全部记录。
    pub fn list(&self) -> MnResult<Vec<DeploymentRecord>> {
        Ok(self.load()?.deployments)
    }

    /// 仅活跃记录（占用资源的）。CLI 启动时用它提醒用户。
    pub fn list_active(&self) -> MnResult<Vec<DeploymentRecord>> {
        Ok(self
            .load()?
            .deployments
            .into_iter()
            .filter(|r| r.status.is_active())
            .collect())
    }

    /// 更新状态。
    pub fn set_status(
        &self,
        id: &str,
        next: DeploymentStatus,
        error: Option<String>,
    ) -> MnResult<DeploymentRecord> {
        let mut state = self.load()?;
        let rec = state
            .deployments
            .iter_mut()
            .find(|r| r.plan.id == id)
            .ok_or_else(|| MnError::bad_request(format!("部署 `{id}` 未登记")))?;
        rec.transition(next)?;
        rec.error = error;
        if next != DeploymentStatus::Ready {
            // 服务不再就绪时清掉 PID，避免误判进程仍在跑
            if matches!(
                next,
                DeploymentStatus::Stopped | DeploymentStatus::Failed | DeploymentStatus::Destroyed
            ) {
                rec.pid = None;
            }
        }
        let out = rec.clone();
        self.save(&state)?;
        Ok(out)
    }

    /// 记录本地引擎 PID。
    pub fn set_pid(&self, id: &str, pid: u32) -> MnResult<()> {
        let mut state = self.load()?;
        let rec = state
            .deployments
            .iter_mut()
            .find(|r| r.plan.id == id)
            .ok_or_else(|| MnError::bad_request(format!("部署 `{id}` 未登记")))?;
        rec.pid = Some(pid);
        rec.updated_at = mangosteen_core::time::now_unix();
        self.save(&state)
    }

    /// 删除记录。用于清理已销毁的历史。
    pub fn remove(&self, id: &str) -> MnResult<bool> {
        let mut state = self.load()?;
        let before = state.deployments.len();
        state.deployments.retain(|r| r.plan.id != id);
        if state.deployments.len() == before {
            return Ok(false);
        }
        self.save(&state)?;
        Ok(true)
    }

    /// 清理已销毁记录。
    pub fn prune_destroyed(&self) -> MnResult<usize> {
        let mut state = self.load()?;
        let before = state.deployments.len();
        state.deployments.retain(|r| !r.status.is_terminal());
        let removed = before - state.deployments.len();
        if removed > 0 {
            self.save(&state)?;
        }
        Ok(removed)
    }

    /// 清理**终态垃圾**记录：`destroyed` 与 `failed`。
    ///
    /// ## 为什么 `failed` 也算垃圾
    ///
    /// 失败的部署不占用任何资源（实例已销毁或已停放），留在台账里唯一的
    /// 作用是「历史记录」。但失败记录会污染两处逻辑：
    ///   * UI 按模型匹配台账时，旧失败记录的 `instance_id` 为空却可能被
    ///     优先命中，导致实例 ID / 登陆方式显示错误；
    ///   * 累积几十条后，每次 `list()` 都在解析无用数据。
    ///
    /// 因此网关启动时统一清掉这两类终态记录，只保留 `stopped`（复用依据）
    /// 与 `ready`/`provisioning`（活跃）。
    ///
    /// 返回清理掉的记录数。
    pub fn prune_terminal(&self) -> MnResult<usize> {
        let mut state = self.load()?;
        let before = state.deployments.len();
        state
            .deployments
            .retain(|r| !matches!(r.status, DeploymentStatus::Destroyed | DeploymentStatus::Failed));
        let removed = before - state.deployments.len();
        if removed > 0 {
            self.save(&state)?;
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mangosteen_core::estimate::{AssessRequest, Assessment, KvCacheType, Placement};
    use mangosteen_core::model::{ModelSpec, Quantization};
    use mangosteen_core::plan::ComputeOrigin;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mn-state-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn sample_plan(id: &str) -> DeploymentPlan {
        let spec = ModelSpec {
            name: "qwen2.5-7b-instruct".into(),
            params_b: 7.615,
            quant: Quantization::Q4KM,
            n_layers: 28,
            n_heads: 28,
            n_kv_heads: 4,
            embedding_dim: 3584,
            vocab_size: 152_064,
            native_context: 32_768,
            file_size_bytes: 4_683_073_024,
        };
        let assessment = Assessment {
            placement: Placement::FullGpu,
            requirement: mangosteen_core::estimate::ResourceRequirement {
                weights_bytes: 4 << 30,
                kv_cache_bytes: 1 << 30,
                compute_buffer_bytes: 192 << 20,
                runtime_overhead_bytes: 400 << 20,
            },
            context_len: 8192,
            kv_cache: KvCacheType::F16,
            quant: Quantization::Q4KM,
            threads: 12,
            batch_size: 512,
            accelerator_budget_bytes: 24 << 30,
            host_budget_bytes: 48 << 30,
            warnings: vec![],
        };
        DeploymentPlan {
            id: id.to_string(),
            model: spec,
            origin: ComputeOrigin::Cloud {
                provider: "test-provider".to_string(),
                instance_id: "test-instance".to_string(),
            },
            cloud: None,
            assessment,
            created_at: 1_700_000_000,
            fallback_trail: vec![],
        }
    }

    #[test]
    fn empty_state_when_absent() {
        let d = tmpdir("empty");
        let s = StateStore::new(d.join("state.json"));
        assert_eq!(s.load().unwrap(), StateFile::default());
        assert!(s.list().unwrap().is_empty());
        assert!(s.list_active().unwrap().is_empty());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn upsert_persists_immediately() {
        let d = tmpdir("upsert");
        let s = StateStore::new(d.join("state.json"));
        s.upsert(DeploymentRecord::from_plan(sample_plan("d1")))
            .unwrap();

        // 新建 store 实例模拟进程重启
        let s2 = StateStore::new(d.join("state.json"));
        let list = s2.list().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].plan.id, "d1");
        assert_eq!(list[0].status, DeploymentStatus::Planned);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn update_plan_preserves_runtime_state() {
        let d = tmpdir("update-plan");
        let s = StateStore::new(d.join("state.json"));
        s.upsert(DeploymentRecord::from_plan(sample_plan("d1")))
            .unwrap();
        s.set_status("d1", DeploymentStatus::Provisioning, Some("准备中".into()))
            .unwrap();
        s.set_pid("d1", 4242).unwrap();

        let mut plan = sample_plan("d1");
        plan.created_at = 1_800_000_000;
        s.update_plan("d1", plan.clone()).unwrap();
        let r = s.get("d1").unwrap().unwrap();
        assert_eq!(r.plan.created_at, plan.created_at);
        assert_eq!(r.status, DeploymentStatus::Provisioning);
        assert_eq!(r.pid, Some(4242));
        assert_eq!(r.error.as_deref(), Some("准备中"));

        s.set_status("d1", DeploymentStatus::Ready, None).unwrap();
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn happy_path_transitions() {
        let d = tmpdir("trans");
        let s = StateStore::new(d.join("state.json"));
        s.upsert(DeploymentRecord::from_plan(sample_plan("d1")))
            .unwrap();

        s.set_status("d1", DeploymentStatus::Provisioning, None)
            .unwrap();
        s.set_pid("d1", 4242).unwrap();
        let r = s.set_status("d1", DeploymentStatus::Ready, None).unwrap();
        assert_eq!(r.status, DeploymentStatus::Ready);
        assert_eq!(r.pid, Some(4242));

        let r = s.set_status("d1", DeploymentStatus::Stopped, None).unwrap();
        assert_eq!(r.status, DeploymentStatus::Stopped);
        assert_eq!(r.pid, None, "停止后应清 PID");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn illegal_transition_rejected() {
        let d = tmpdir("illegal");
        let s = StateStore::new(d.join("state.json"));
        s.upsert(DeploymentRecord::from_plan(sample_plan("d1")))
            .unwrap();

        // Planned 不能直接到 Ready
        let e = s
            .set_status("d1", DeploymentStatus::Ready, None)
            .unwrap_err();
        assert!(matches!(e, MnError::InvalidTransition { .. }));
        std::fs::remove_dir_all(&d).ok();
    }

    /// 失败后可以「停放」到 stopped：停机保留实例，供再次部署复用。
    ///
    /// 状态机必须允许 `Failed → Stopped`，否则部署失败后取消/卸载会
    /// 因非法迁移报错，用户既停不了机也复用不了实例。
    #[test]
    fn failed_can_park_to_stopped() {
        let d = tmpdir("park");
        let s = StateStore::new(d.join("state.json"));
        s.upsert(DeploymentRecord::from_plan(sample_plan("d1")))
            .unwrap();
        s.set_status("d1", DeploymentStatus::Provisioning, None)
            .unwrap();
        s.set_status("d1", DeploymentStatus::Failed, Some("boom".into()))
            .unwrap();
        s.set_status("d1", DeploymentStatus::Stopped, Some("已停止".into()))
            .unwrap();
        assert_eq!(
            s.get("d1").unwrap().unwrap().status,
            DeploymentStatus::Stopped
        );
        std::fs::remove_dir_all(&d).ok();
    }

    /// 探测自愈：`Failed → Ready` 必须合法。
    ///
    /// 场景：部署成功后一次瞬时探测失败（模型加载期 `/health` 返回
    /// 503、隧道短暂重建等）把台账翻成 `failed`；之后探测确认端口
    /// 仍在服务，必须能翻回 `ready`，否则记录永久卡死，对话页签
    /// 再也看不到该模型。
    #[test]
    fn failed_recovers_to_ready_on_probe() {
        let d = tmpdir("recover");
        let s = StateStore::new(d.join("state.json"));
        s.upsert(DeploymentRecord::from_plan(sample_plan("d1")))
            .unwrap();
        s.set_status("d1", DeploymentStatus::Provisioning, None)
            .unwrap();
        s.set_status("d1", DeploymentStatus::Ready, None).unwrap();
        s.set_status(
            "d1",
            DeploymentStatus::Failed,
            Some("本地端口与远端引擎端口映射不存在或健康检查失败".into()),
        )
        .unwrap();
        let r = s
            .set_status("d1", DeploymentStatus::Ready, Some("探测恢复".into()))
            .unwrap();
        assert_eq!(r.status, DeploymentStatus::Ready);
        std::fs::remove_dir_all(&d).ok();
    }

    /// 还没进入 provisioning 就被取消：`Planned → Stopped` 必须合法。
    ///
    /// 场景：受理完任务、实例还没创建，用户就点了取消。此时台账是
    /// `planned`，取消要把「已保留的资源」标成停止，不能因状态机拒绝
    /// 而让记录卡在 `planned`（UI 会一直显示部署中）。
    #[test]
    fn planned_can_park_to_stopped() {
        let d = tmpdir("park0");
        let s = StateStore::new(d.join("state.json"));
        s.upsert(DeploymentRecord::from_plan(sample_plan("d1")))
            .unwrap();
        s.set_status("d1", DeploymentStatus::Stopped, None).unwrap();
        assert_eq!(
            s.get("d1").unwrap().unwrap().status,
            DeploymentStatus::Stopped
        );
        std::fs::remove_dir_all(&d).ok();
    }

    /// 部署进行中被取消：`Provisioning → Stopped` 必须合法。
    #[test]
    fn provisioning_can_park_to_stopped() {
        let d = tmpdir("park2");
        let s = StateStore::new(d.join("state.json"));
        s.upsert(DeploymentRecord::from_plan(sample_plan("d1")))
            .unwrap();
        s.set_status("d1", DeploymentStatus::Provisioning, None)
            .unwrap();
        s.set_status("d1", DeploymentStatus::Stopped, None).unwrap();
        assert_eq!(
            s.get("d1").unwrap().unwrap().status,
            DeploymentStatus::Stopped
        );
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn destroyed_is_terminal() {
        let d = tmpdir("terminal");
        let s = StateStore::new(d.join("state.json"));
        s.upsert(DeploymentRecord::from_plan(sample_plan("d1")))
            .unwrap();
        s.set_status("d1", DeploymentStatus::Destroyed, None)
            .unwrap();
        assert!(s
            .set_status("d1", DeploymentStatus::Provisioning, None)
            .is_err());
        assert!(s.list_active().unwrap().is_empty());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn failure_path_records_reason() {
        let d = tmpdir("fail");
        let s = StateStore::new(d.join("state.json"));
        s.upsert(DeploymentRecord::from_plan(sample_plan("d1")))
            .unwrap();
        s.set_status("d1", DeploymentStatus::Provisioning, None)
            .unwrap();
        let r = s
            .set_status(
                "d1",
                DeploymentStatus::Failed,
                Some("本地显存不足".to_string()),
            )
            .unwrap();
        assert_eq!(r.error.as_deref(), Some("本地显存不足"));
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn active_listing_filters_correctly() {
        let d = tmpdir("active");
        let s = StateStore::new(d.join("state.json"));
        for id in ["a", "b", "c"] {
            s.upsert(DeploymentRecord::from_plan(sample_plan(id)))
                .unwrap();
        }
        s.set_status("a", DeploymentStatus::Provisioning, None)
            .unwrap();
        s.set_status("a", DeploymentStatus::Ready, None).unwrap();
        s.set_status("b", DeploymentStatus::Provisioning, None)
            .unwrap();
        // c 保持 Planned
        let active = s.list_active().unwrap();
        assert_eq!(active.len(), 2);
        assert!(active.iter().any(|r| r.plan.id == "a"));
        assert!(active.iter().any(|r| r.plan.id == "b"));
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn remove_and_prune() {
        let d = tmpdir("prune");
        let s = StateStore::new(d.join("state.json"));
        s.upsert(DeploymentRecord::from_plan(sample_plan("a")))
            .unwrap();
        s.upsert(DeploymentRecord::from_plan(sample_plan("b")))
            .unwrap();
        s.set_status("a", DeploymentStatus::Destroyed, None)
            .unwrap();

        assert_eq!(s.prune_destroyed().unwrap(), 1);
        assert_eq!(s.list().unwrap().len(), 1);

        assert!(s.remove("b").unwrap());
        assert!(!s.remove("b").unwrap());
        assert!(s.list().unwrap().is_empty());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn prune_terminal_removes_destroyed_and_failed_keeps_stopped() {
        let d = tmpdir("prune_terminal");
        let s = StateStore::new(d.join("state.json"));
        for id in ["ok", "dead", "fail", "stopped"] {
            s.upsert(DeploymentRecord::from_plan(sample_plan(id)))
                .unwrap();
        }
        s.set_status("dead", DeploymentStatus::Destroyed, None)
            .unwrap();
        s.set_status("fail", DeploymentStatus::Failed, Some("boom".into()))
            .unwrap();
        // stopped 是复用依据，必须保留
        s.set_status("stopped", DeploymentStatus::Stopped, None)
            .unwrap();

        assert_eq!(s.prune_terminal().unwrap(), 2);
        let left: Vec<String> = s
            .list()
            .unwrap()
            .into_iter()
            .map(|r| r.plan.id)
            .collect();
        assert!(left.contains(&"ok".to_string()));
        assert!(left.contains(&"stopped".to_string()));
        assert!(!left.contains(&"dead".to_string()));
        assert!(!left.contains(&"fail".to_string()));
        // 幂等：再次清理无记录可删
        assert_eq!(s.prune_terminal().unwrap(), 0);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn unknown_id_errors() {
        let d = tmpdir("unknown");
        let s = StateStore::new(d.join("state.json"));
        assert!(s.set_pid("nope", 1).is_err());
        assert!(s
            .set_status("nope", DeploymentStatus::Provisioning, None)
            .is_err());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn corrupt_file_reports_parse_error() {
        let d = tmpdir("corrupt");
        std::fs::write(d.join("state.json"), "{{{{").unwrap();
        let s = StateStore::new(d.join("state.json"));
        let e = s.load().unwrap_err();
        assert!(e.to_string().contains("JSON"));
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn plan_survives_serialization() {
        let d = tmpdir("ser");
        let s = StateStore::new(d.join("state.json"));
        let plan = sample_plan("d1");
        s.upsert(DeploymentRecord::from_plan(plan.clone())).unwrap();
        let back = s.get("d1").unwrap().unwrap();
        assert_eq!(back.plan, plan);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn status_helpers() {
        assert!(DeploymentStatus::Ready.is_active());
        assert!(DeploymentStatus::Provisioning.is_active());
        assert!(!DeploymentStatus::Stopped.is_active());
        assert!(!DeploymentStatus::Destroyed.is_active());
        assert!(DeploymentStatus::Destroyed.is_terminal());
        assert_eq!(DeploymentStatus::Ready.as_str(), "ready");
    }

    #[test]
    fn assess_request_default_is_sane() {
        // 固化了「默认允许 CPU 回退」这一产品决策
        let r = AssessRequest::default();
        assert!(r.allow_cpu_fallback);
        assert!(r.allow_partial_offload);
    }
}
