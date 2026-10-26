//! 多云统一领域建模：算力实例抽象与驱动接口。
//!
//! 归属：**算力执行层 · 多云统一适配层**。
//!
//! ## 设计目标
//!
//! 上层（网关层调度决策器、CLI、TUI）只认识本模块定义的 [`Instance`] 与
//! [`ProviderDriver`]，**完全不感知** AutoDL / 优云智算 / 阿里云的 API 差异。
//! 新增服务商 = 新增一个 `impl ProviderDriver` + 一份配置，不动上层代码。
//!
//! ## 状态机
//!
//! ```text
//!  Pending ──create──▶ Creating ──ok──▶ Created ──start──▶ Starting ──ok──▶ Running
//!      │                   │               │                  │               │
//!      └──────fail────────▶ Failed ◀──────┴──────────────────┴───────────────┤
//!                                              stop ◀───────────────────────┘
//!                                               │
//!                                            Stopped ──start──▶ Starting
//!                                               │
//!                                          destroy ──▶ Destroyed (终态)
//! ```
//!
//! 状态迁移的合法性由 [`InstanceState::can_transition_to`] 集中定义，
//! 驱动实现**不得**自行发明状态。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::{MnError, MnResult};

/// 算力实例生命周期状态。跨厂商归一化后的唯一状态集。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstanceState {
    /// 本地已登记，尚未向厂商提交
    Pending,
    /// 厂商侧创建中（分配资源）
    Creating,
    /// 已创建，未启动（关机状态，不计费或按存储计费）
    Created,
    /// 启动中（装驱动、拉起容器）
    Starting,
    /// 运行中，可承载推理服务
    Running,
    /// 停止中（厂商正在关机）
    Stopping,
    /// 已停止，保留磁盘
    Stopped,
    /// 已销毁，不可恢复
    Destroyed,
    /// 失败，需人工介入或销毁重建
    Failed,
}

impl InstanceState {
    /// 是否终态。
    pub fn is_terminal(self) -> bool {
        matches!(self, InstanceState::Destroyed)
    }

    /// 是否可以在该状态下执行推理。
    pub fn is_serving(self) -> bool {
        matches!(self, InstanceState::Running)
    }

    /// 是否仍在计费（计算资源）。
    pub fn is_billing_compute(self) -> bool {
        matches!(
            self,
            InstanceState::Starting
                | InstanceState::Running
                | InstanceState::Stopping
                | InstanceState::Creating
        )
    }

    /// 校验状态迁移。非法迁移返回 [`MnError::InvalidTransition`]。
    ///
    /// 允许的迁移集刻意保守：例如 `Destroyed` 不可复活，`Created` 不能直接
    /// 跳到 `Running`（必须先经 `Starting`），保证驱动实现无法跳过探测步骤。
    pub fn can_transition_to(self, next: InstanceState) -> bool {
        use InstanceState::*;
        matches!(
            (self, next),
            (Pending, Creating)
                | (Pending, Failed)
                | (Creating, Created)
                | (Creating, Failed)
                | (Created, Starting)
                | (Created, Failed)
                | (Created, Destroyed)
                | (Starting, Running)
                | (Starting, Failed)
                | (Starting, Stopped)
                | (Running, Stopping)
                | (Running, Stopped)
                | (Running, Failed)
                | (Stopping, Stopped)
                | (Stopping, Failed)
                | (Stopped, Starting)
                | (Stopped, Destroyed)
                | (Stopped, Failed)
                | (Failed, Destroyed)
                | (Failed, Creating)
                | (Pending, Destroyed)
        )
    }

    /// 执行迁移校验，非法则返回错误。
    pub fn transition(self, next: InstanceState, instance_id: &str, action: &str) -> MnResult<Self> {
        if self.can_transition_to(next) {
            Ok(next)
        } else {
            Err(MnError::InvalidTransition {
                instance: instance_id.to_string(),
                from: self.as_str().to_string(),
                action: action.to_string(),
            })
        }
    }

    /// 稳定字符串标识。
    pub fn as_str(self) -> &'static str {
        match self {
            InstanceState::Pending => "pending",
            InstanceState::Creating => "creating",
            InstanceState::Created => "created",
            InstanceState::Starting => "starting",
            InstanceState::Running => "running",
            InstanceState::Stopping => "stopping",
            InstanceState::Stopped => "stopped",
            InstanceState::Destroyed => "destroyed",
            InstanceState::Failed => "failed",
        }
    }
}

impl std::fmt::Display for InstanceState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 网络访问信息。云端实例的「可用地址」完全由本结构表达。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct NetworkEndpoint {
    /// SSH 主机（公网 IP 或域名）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_host: Option<String>,
    /// SSH 端口
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_port: Option<u16>,
    /// 实例内部推理服务地址（隧道建立后本地映射到 127.0.0.1）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inference_host: Option<String>,
    /// 实例内部推理端口
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inference_port: Option<u16>,
    /// 厂商是否直接提供了公网 HTTP 入口（部分服务商的「自定义服务」功能）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_url: Option<String>,
    /// SSH 登录密码（部分厂商直接下发密码，无需自行设置）。
    ///
    /// 与 [`mangosteen_core::config::CloudConfig::ssh_password`] 的关系：
    /// 这里存的是**厂商为这台实例生成/下发的密码**（AutoDL 的
    /// `root_password`），后者是用户自己指定的统一密码（优云需要主动设）。
    /// 隧道优先用这里的值——它一定是这台机器的真实密码。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_password: Option<String>,
}

impl NetworkEndpoint {
    /// 是否具备建立隧道所需的最小信息。
    pub fn can_build_tunnel(&self) -> bool {
        self.ssh_host.is_some() && self.ssh_port.is_some() && self.inference_port.is_some()
    }

    /// `ssh_host:ssh_port` 简写。
    pub fn ssh_target(&self) -> Option<String> {
        match (&self.ssh_host, self.ssh_port) {
            (Some(h), Some(p)) => Some(format!("{h}:{p}")),
            _ => None,
        }
    }
}

/// 归一化的加速卡型号描述。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GpuSpec {
    /// 归一化型号名，如 `RTX 4090`
    pub model: String,
    /// 卡数
    pub count: u32,
    /// 单卡显存字节数
    pub vram_bytes: u64,
}

impl GpuSpec {
    /// 总显存。
    pub fn total_vram_bytes(&self) -> u64 {
        self.vram_bytes.saturating_mul(self.count as u64)
    }
}

/// 归一化的实例规格。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstanceSpec {
    /// 加速卡
    pub gpu: GpuSpec,
    /// CPU 核心数
    pub cpu_cores: u32,
    /// 内存字节数
    pub memory_bytes: u64,
    /// 数据盘字节数
    pub disk_bytes: u64,
    /// 归一化镜像标识。驱动负责映射到厂商私有镜像 ID。
    pub image: String,
}

impl InstanceSpec {
    /// 校验规格合理性。防止把明显不可能的请求发给厂商。
    pub fn validate(&self) -> MnResult<()> {
        if self.gpu.count == 0 {
            return Err(MnError::bad_request("gpu.count 必须大于 0"));
        }
        if self.gpu.vram_bytes == 0 {
            return Err(MnError::bad_request("gpu.vram_bytes 必须大于 0"));
        }
        if self.cpu_cores == 0 {
            return Err(MnError::bad_request("cpu_cores 必须大于 0"));
        }
        if self.memory_bytes == 0 {
            return Err(MnError::bad_request("memory_bytes 必须大于 0"));
        }
        if self.disk_bytes == 0 {
            return Err(MnError::bad_request("disk_bytes 必须大于 0"));
        }
        if self.image.trim().is_empty() {
            return Err(MnError::bad_request("image 不能为空"));
        }
        Ok(())
    }
}

/// 厂商报价（用于节点选择器的价格维度）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PriceQuote {
    /// 元/小时
    pub per_hour_cny: f64,
    /// 计费粒度（秒）。0 表示按小时计整。
    pub billing_granularity_secs: u32,
    /// 是否支持按量随时释放
    pub spot_release_allowed: bool,
    /// 备注（如「新用户券后价」），仅供展示
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl PriceQuote {
    /// 估算 `secs` 秒的花费。
    pub fn cost_for(&self, secs: u64) -> f64 {
        let hours = secs as f64 / 3600.0;
        if self.billing_granularity_secs == 0 {
            return self.per_hour_cny * hours.ceil();
        }
        let units = (secs as f64 / self.billing_granularity_secs as f64).ceil().max(1.0);
        let unit_hours = self.billing_granularity_secs as f64 / 3600.0;
        units * unit_hours * self.per_hour_cny
    }
}

/// 可用实例规格（厂商当前可创建的可选项）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AvailableSpec {
    /// 规格 ID（厂商私有，驱动内部使用）
    pub spec_id: String,
    /// 归一化后的规格描述
    pub spec: InstanceSpec,
    /// 报价
    pub price: PriceQuote,
    /// 归一化地域/可用区
    pub region: String,
    /// 当前库存（`None` 表示厂商不提供）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stock: Option<u32>,
    /// 厂商侧额外属性（不参与打分，仅供展示/排障）
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, String>,
}

impl AvailableSpec {
    /// 是否有库存。厂商不提供库存字段时视为有货。
    pub fn is_in_stock(&self) -> bool {
        self.stock.map(|s| s > 0).unwrap_or(true)
    }
}

/// 归一化后的算力实例。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Instance {
    /// 厂商侧实例唯一 ID
    pub id: String,
    /// 驱动名（服务商标识）
    pub provider: String,
    /// 生命周期状态
    pub state: InstanceState,
    /// 实例规格
    pub spec: InstanceSpec,
    /// 网络访问信息
    pub network: NetworkEndpoint,
    /// 创建时间（Unix 秒）
    pub created_at: i64,
    /// 当前报价
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price: Option<PriceQuote>,
    /// 厂商侧展示名/备注
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

impl Instance {
    /// 更新状态并校验迁移合法性。
    pub fn with_state(mut self, next: InstanceState, action: &str) -> MnResult<Self> {
        self.state = self.state.transition(next, &self.id, action)?;
        Ok(self)
    }
}

/// 单个监控指标的采样点。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MetricPoint {
    /// 采样时间（Unix 秒，厂商侧时间）
    pub timestamp: i64,
    /// 指标值
    pub value: f64,
}

/// 归一化的实例监控快照（跨厂商）。
///
/// ## 为什么所有使用率都是 0~1 的比例
///
/// 厂商接口普遍返回 **0~100 的百分数**，而本项目的本地监控
/// （`mangosteen_platform::resmon` 与 `/api/sysinfo`）用 **0~1 比例**。
/// 两套单位混用会让前端出现「CPU 3500%」这种荒谬显示，所以
/// 「百分数 → 比例」的换算在驱动内一次完成，上层只认比例。
///
/// ## 未知指标
///
/// 厂商新增的指标不丢弃，放进 [`Self::extra`]（原始键 → 最新值），
/// 便于排障与后续扩展；不建模也不会导致解析失败。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct InstanceMetrics {
    /// 实例 ID
    pub instance_id: String,
    /// 最新数据点的时间（Unix 秒）。`0` 表示厂商未给出
    pub sampled_at: i64,
    /// CPU 使用率（0~1）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_usage: Option<f64>,
    /// 内存使用率（0~1）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_usage: Option<f64>,
    /// GPU 使用率（0~1）。多卡取**最忙**的一张
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_usage: Option<f64>,
    /// 显存使用率（0~1）。多卡取最忙的一张
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_memory_usage: Option<f64>,
    /// 系统盘使用率（0~1）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_usage: Option<f64>,
    /// 数据盘使用率（0~1）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_disk_usage: Option<f64>,
    /// 未建模的指标（厂商原始键 → 最新值，仍为百分数原值）
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, f64>,
}

impl InstanceMetrics {
    /// 是否有任何可用指标。
    pub fn has_any(&self) -> bool {
        self.cpu_usage.is_some()
            || self.memory_usage.is_some()
            || self.gpu_usage.is_some()
            || self.gpu_memory_usage.is_some()
            || self.disk_usage.is_some()
            || self.data_disk_usage.is_some()
            || !self.extra.is_empty()
    }
}

/// 驱动的能力声明。网关据此决定是否向该服务商派发请求。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DriverCapabilities {
    /// 支持按量创建
    pub on_demand_create: bool,
    /// 支持停止/启动（保留磁盘）
    pub stop_start: bool,
    /// 支持查询当前报价
    pub price_query: bool,
    /// 支持公网直连推理端口（免隧道）
    pub public_inference_url: bool,
    /// 支持自定义镜像
    pub custom_image: bool,
    /// 支持的归一化 GPU 型号
    pub gpu_models: Vec<String>,
}

/// 云端算力服务商驱动接口。
///
/// ## 实现约束
///
/// 1. **禁止**在实现中 `unwrap` 网络结果或把厂商错误码直接透传给上层；
///    必须归一化为 [`MnError::Provider`]。
/// 2. 所有方法都必须可重入、幂等（除 `create`）：调度决策器会在失败时重试。
/// 3. 不得在此 trait 中暴露任何厂商私有字段，私有信息放在 `extra` /
///    驱动内部状态里。
///
/// 该 trait 需以 `dyn` 方式使用，故全部方法对象安全。
pub trait ProviderDriver: Send + Sync {
    /// 驱动名（配置中的 `name`，唯一）
    fn name(&self) -> &str;

    /// 人类可读展示名
    fn display_name(&self) -> &str;

    /// 能力声明
    fn capabilities(&self) -> DriverCapabilities;

    /// 列出当前可创建规格（含报价）。用于节点选择器打分。
    fn list_available_specs(&self) -> MnResult<Vec<AvailableSpec>>;

    /// 创建实例。`spec_id` 来自 [`AvailableSpec::spec_id`]。
    fn create(&self, spec_id: &str, name: Option<&str>) -> MnResult<Instance>;

    /// 启动实例。
    fn start(&self, instance_id: &str) -> MnResult<Instance>;

    /// 停止实例（保留磁盘）。
    fn stop(&self, instance_id: &str) -> MnResult<Instance>;

    /// 销毁实例。
    fn destroy(&self, instance_id: &str) -> MnResult<()>;

    /// 查询单个实例。
    fn describe(&self, instance_id: &str) -> MnResult<Instance>;

    /// 列出该账号下全部实例。
    fn list(&self) -> MnResult<Vec<Instance>>;

    /// 链路延迟探测（毫秒）。返回到该服务商控制面的往返时延，
    /// 作为节点选择器的延迟维度。默认实现返回 `None` 表示不支持。
    fn probe_latency_ms(&self) -> Option<u32> {
        None
    }

    /// 设置实例的 root 登录密码。
    ///
    /// ## 为什么需要这个能力
    ///
    /// 建立 SSH 隧道总得有个认证方式。允许创建时注入 SSH key 的厂商不需要
    /// 它（用 `identity_file` 即可），但优云智算这类**只支持密码登录**的
    /// 厂商必须由我们主动设一个密码，否则「一键部署」会卡在隧道那一步。
    ///
    /// 默认实现返回「不支持」：新增服务商不必实现它，上层会在需要时
    /// 给出明确错误，而不是静默失败。
    fn reset_root_password(&self, instance_id: &str, password: &str) -> MnResult<()> {
        let _ = (instance_id, password);
        Err(MnError::Provider {
            provider: self.name().to_string(),
            code: "reset_password_unsupported".to_string(),
            message: "该服务商不支持由程序设置实例密码，请在控制台配置 SSH 密钥".to_string(),
        })
    }

    /// 查询实例实时监控（CPU / 内存 / GPU / 磁盘使用率）。
    ///
    /// ## 返回值语义
    ///
    /// * `Ok(Some(m))` — 取到了指标（`m.has_any()` 可能为 `false`，
    ///   表示实例存在但厂商暂无数据，例如刚开机的一两分钟内）；
    /// * `Ok(None)` — **该服务商未提供监控接口**，或实例不在返回列表里；
    /// * `Err(..)` — 调用失败（鉴权、网络、厂商错误码）。
    ///
    /// 默认实现返回 `Ok(None)`：监控是可选能力，新增服务商不必实现它。
    /// 上层据此在设备页显示「该服务商未提供监控」而不是报错。
    ///
    /// 实现约束：**必须把厂商的百分数换算成 0~1 比例**（见 [`InstanceMetrics`]）。
    fn instance_metrics(&self, instance_id: &str) -> MnResult<Option<InstanceMetrics>> {
        let _ = instance_id;
        Ok(None)
    }
}

/// 驱动注册表。配置驱动的动态加载点。
///
/// 用 `BTreeMap` 保证遍历顺序稳定（快照测试与打分结果可复现）。
#[derive(Default)]
pub struct DriverRegistry {
    drivers: BTreeMap<String, Box<dyn ProviderDriver>>,
}

impl DriverRegistry {
    /// 空注册表。
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册驱动。重名会覆盖（配置热更新场景下即为预期行为）。
    pub fn register(&mut self, driver: Box<dyn ProviderDriver>) {
        self.drivers
            .insert(driver.name().to_string(), driver);
    }

    /// 按名取驱动。
    pub fn get(&self, name: &str) -> MnResult<&dyn ProviderDriver> {
        self.drivers
            .get(name)
            .map(|d| d.as_ref())
            .ok_or_else(|| MnError::DriverUnavailable {
                provider: name.to_string(),
            })
    }

    /// 是否已注册。
    pub fn contains(&self, name: &str) -> bool {
        self.drivers.contains_key(name)
    }

    /// 全部驱动名（有序）。
    pub fn names(&self) -> Vec<&str> {
        self.drivers.keys().map(|s| s.as_str()).collect()
    }

    /// 驱动数量。
    pub fn len(&self) -> usize {
        self.drivers.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.drivers.is_empty()
    }

    /// 遍历所有驱动。
    pub fn iter(&self) -> impl Iterator<Item = (&str, &dyn ProviderDriver)> {
        self.drivers.iter().map(|(k, v)| (k.as_str(), v.as_ref()))
    }
}

impl std::fmt::Debug for DriverRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DriverRegistry")
            .field("drivers", &self.names())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn happy_path_state_machine() {
        use InstanceState::*;
        let path = [Pending, Creating, Created, Starting, Running, Stopped, Destroyed];
        for w in path.windows(2) {
            assert!(
                w[0].can_transition_to(w[1]),
                "{} → {} should be allowed",
                w[0],
                w[1]
            );
        }
    }

    #[test]
    fn destroyed_is_terminal() {
        for next in [
            InstanceState::Pending,
            InstanceState::Creating,
            InstanceState::Running,
            InstanceState::Stopped,
        ] {
            assert!(!InstanceState::Destroyed.can_transition_to(next));
        }
    }

    #[test]
    fn cannot_skip_starting() {
        assert!(!InstanceState::Created.can_transition_to(InstanceState::Running));
    }

    #[test]
    fn failed_can_be_recreated_or_destroyed() {
        assert!(InstanceState::Failed.can_transition_to(InstanceState::Creating));
        assert!(InstanceState::Failed.can_transition_to(InstanceState::Destroyed));
        assert!(!InstanceState::Failed.can_transition_to(InstanceState::Running));
    }

    #[test]
    fn transition_error_carries_context() {
        let e = InstanceState::Destroyed
            .transition(InstanceState::Running, "i-123", "start")
            .unwrap_err();
        match e {
            MnError::InvalidTransition {
                instance,
                from,
                action,
            } => {
                assert_eq!(instance, "i-123");
                assert_eq!(from, "destroyed");
                assert_eq!(action, "start");
            }
            other => panic!("unexpected {other:?}"),
        }
        // 非法迁移不参与降级重试
        assert!(!e_is_retryable());
    }

    fn e_is_retryable() -> bool {
        InstanceState::Destroyed
            .transition(InstanceState::Running, "x", "y")
            .unwrap_err()
            .is_retryable()
    }

    #[test]
    fn billing_and_serving_flags() {
        assert!(InstanceState::Running.is_serving());
        assert!(InstanceState::Running.is_billing_compute());
        assert!(!InstanceState::Stopped.is_billing_compute());
        assert!(!InstanceState::Created.is_billing_compute());
        assert!(InstanceState::Destroyed.is_terminal());
    }

    #[test]
    fn endpoint_tunnel_readiness() {
        let mut e = NetworkEndpoint::default();
        assert!(!e.can_build_tunnel());
        e.ssh_host = Some("1.2.3.4".into());
        assert!(!e.can_build_tunnel());
        e.ssh_port = Some(22);
        e.inference_port = Some(8080);
        assert!(e.can_build_tunnel());
        assert_eq!(e.ssh_target().unwrap(), "1.2.3.4:22");
    }

    #[test]
    fn price_cost_rounds_up_hourly() {
        let p = PriceQuote {
            per_hour_cny: 2.0,
            billing_granularity_secs: 0,
            spot_release_allowed: true,
            note: None,
        };
        assert_eq!(p.cost_for(1), 2.0);       // 不足 1 小时按 1 小时
        assert_eq!(p.cost_for(3600), 2.0);
        assert_eq!(p.cost_for(3601), 4.0);
    }

    #[test]
    fn price_cost_respects_granularity() {
        let p = PriceQuote {
            per_hour_cny: 3.6, // 0.001 元/秒
            billing_granularity_secs: 60,
            spot_release_allowed: false,
            note: None,
        };
        // 90 秒 → 2 个 60s 单位 → 2 分钟 × 0.06 元/分钟 = 0.12
        assert!((p.cost_for(90) - 0.12).abs() < 1e-9);
    }

    #[test]
    fn spec_validation() {
        let good = InstanceSpec {
            gpu: GpuSpec {
                model: "RTX 4090".into(),
                count: 1,
                vram_bytes: 24 << 30,
            },
            cpu_cores: 16,
            memory_bytes: 60 << 30,
            disk_bytes: 50 << 30,
            image: "ubuntu22-cuda12".into(),
        };
        assert!(good.validate().is_ok());
        assert_eq!(good.gpu.total_vram_bytes(), 24 << 30);

        let mut bad = good.clone();
        bad.gpu.count = 0;
        assert!(bad.validate().is_err());

        let mut bad = good;
        bad.image = "  ".into();
        assert!(bad.validate().is_err());
    }

    #[test]
    fn stock_semantics() {
        let mk = |s| AvailableSpec {
            spec_id: "s".into(),
            spec: InstanceSpec {
                gpu: GpuSpec {
                    model: "RTX 4090".into(),
                    count: 1,
                    vram_bytes: 24 << 30,
                },
                cpu_cores: 16,
                memory_bytes: 60 << 30,
                disk_bytes: 50 << 30,
                image: "i".into(),
            },
            price: PriceQuote {
                per_hour_cny: 1.0,
                billing_granularity_secs: 0,
                spot_release_allowed: true,
                note: None,
            },
            region: "cn".into(),
            stock: s,
            extra: BTreeMap::new(),
        };
        assert!(mk(Some(5)).is_in_stock());
        assert!(!mk(Some(0)).is_in_stock());
        assert!(mk(None).is_in_stock());
    }

    #[test]
    fn registry_lookup_and_errors() {
        let mut reg = DriverRegistry::new();
        assert!(reg.is_empty());
        let err = match reg.get("nope") {
            Ok(_) => panic!("should not resolve missing driver"),
            Err(e) => e,
        };
        assert_eq!(
            err,
            MnError::DriverUnavailable {
                provider: "nope".into()
            }
        );

        reg.register(Box::new(FakeDriver { name: "autodl" }));
        reg.register(Box::new(FakeDriver { name: "aliyun" }));
        assert_eq!(reg.len(), 2);
        assert_eq!(reg.names(), vec!["aliyun", "autodl"]); // BTreeMap 有序
        assert!(reg.contains("autodl"));
        assert_eq!(reg.get("autodl").unwrap().name(), "autodl");
    }

    #[test]
    fn metrics_has_any_covers_every_dimension() {
        let mut m = InstanceMetrics::default();
        assert!(!m.has_any(), "空快照不应被当成有数据");
        m.gpu_usage = Some(0.0);
        assert!(m.has_any(), "0% 也是有效数据，不能与「无数据」混为一谈");
        let m = InstanceMetrics {
            extra: BTreeMap::from([("vendor_x".to_string(), 1.0)]),
            ..Default::default()
        };
        assert!(m.has_any());
    }

    #[test]
    fn monitor_is_optional_capability() {
        // 未实现监控的服务商必须返回 Ok(None) 而不是报错，
        // 否则设备页会把「不支持」显示成故障
        let d = FakeDriver { name: "autodl" };
        assert_eq!(d.instance_metrics("i-1").unwrap(), None);
    }

    struct FakeDriver {
        name: &'static str,
    }

    impl ProviderDriver for FakeDriver {
        fn name(&self) -> &str {
            self.name
        }
        fn display_name(&self) -> &str {
            self.name
        }
        fn capabilities(&self) -> DriverCapabilities {
            DriverCapabilities::default()
        }
        fn list_available_specs(&self) -> MnResult<Vec<AvailableSpec>> {
            Ok(vec![])
        }
        fn create(&self, _: &str, _: Option<&str>) -> MnResult<Instance> {
            Err(MnError::bad_request("not implemented"))
        }
        fn start(&self, _: &str) -> MnResult<Instance> {
            Err(MnError::bad_request("not implemented"))
        }
        fn stop(&self, _: &str) -> MnResult<Instance> {
            Err(MnError::bad_request("not implemented"))
        }
        fn destroy(&self, _: &str) -> MnResult<()> {
            Err(MnError::bad_request("not implemented"))
        }
        fn describe(&self, _: &str) -> MnResult<Instance> {
            Err(MnError::bad_request("not implemented"))
        }
        fn list(&self) -> MnResult<Vec<Instance>> {
            Ok(vec![])
        }
    }
}
