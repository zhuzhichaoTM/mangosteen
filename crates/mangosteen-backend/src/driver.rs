//! 优云智算 GPU 实例驱动（[`ProviderDriver`] 实现）。
//!
//! 归属：**算力执行层 · 云端后端管理**。
//!
//! ## 这个驱动替上层解决了哪些厂商怪癖
//!
//! | 厂商限制（来自官方文档） | 本驱动的处理 |
//! |--------------------------|--------------|
//! | 创建前必须调 `CheckCompShareResourceCapacity` 判库存 | [`CompshareDriver::create`] 内部强制先检查，无库存直接返回可降级的错误 |
//! | `Terminate` **仅** `Stopped` 可用 | [`CompshareDriver::destroy`] 自动「先关机 → 等 Stopped → 再删除」 |
//! | `Start` 仅 `Stopped` 可用、`Stop` 仅 `Running` 可用 | 动作前先查状态，已处于目标态则幂等返回 |
//! | 创建接口只返回实例 ID，不返回详情 | 创建后立即 `Describe` 补齐规格/网络；补查失败也保留 ID 以便回滚 |
//! | 实例操作有 10 分钟并发锁 | 识别 `InstanceOperationInProgress` 并给出可操作提示 |
//! | `Memory` 单位是 MB，机型表是 GB | 单位换算集中在 [`crate::mapping::units`] |
//!
//! ## 与编排器的分工
//!
//! 本驱动**不决定**用哪家服务商，只负责「把这一家的事情做对」。
//! 选点、重试、隧道由 `mangosteen-cloud` 的编排器与网关负责。

use std::time::{Duration, Instant};

use mangosteen_core::cloud::{
    AvailableSpec, DriverCapabilities, Instance, InstanceMetrics, InstanceState, ProviderDriver,
};
use mangosteen_core::{MnError, MnResult};

use crate::client::{CapacityRequest, CompshareClient, CreateRequest, PriceRequest, PROVIDER_NAME};
use crate::config::CompshareConfig;
use crate::mapping::{self, units};
use crate::model::{
    state, BandwidthSwitchResponse, InstancePortsResponse, NetOptimizerResponse, PriceDetail,
    PriceResponse, RefundPriceResponse, SoftwarePortResponse, UHost, UpgradePriceResponse,
};
use crate::retcode::{codes, provider_error};
use crate::sign::Credential;
use crate::transport::Transport;

/// 分页查询单页上限（厂商限制 100）。
const PAGE_LIMIT: u32 = 100;
/// 列表最多翻多少页（防止异常数据导致死循环）。
const MAX_PAGES: u32 = 10;

/// 定时关机的最小延迟（秒）。厂商限制：目标时间必须 ≥ 当前 + 300 秒。
const MIN_STOP_DELAY_SECS: i64 = 300;

/// 优云智算 GPU 实例驱动。
pub struct CompshareDriver<T: Transport> {
    name: String,
    config: CompshareConfig,
    client: CompshareClient<T>,
    /// 默认项目 ID 缓存。
    ///
    /// 定时关机接口把 `ProjectId` 列为必填，而它不在实例信息里，
    /// 只能单独调 `GetProjectList` 查。项目 ID 在账号生命周期内不变，
    /// 因此查一次就缓存——否则每次设置定时关机都多一次网络往返。
    project_id: std::sync::Mutex<Option<String>>,
}

impl<T: Transport> std::fmt::Debug for CompshareDriver<T> {
    /// 手写 `Debug` 而不是 `derive`：`T` 未必实现 `Debug`（如 `Arc<dyn Transport>`），
    /// 且绝不能把凭证打印出来。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompshareDriver")
            .field("name", &self.name)
            .field("region", &self.config.region)
            .field("zone", &self.config.zone)
            .finish()
    }
}

impl<T: Transport> CompshareDriver<T> {
    /// 构造驱动。
    ///
    /// `name` 是配置里的服务商名（如 `compshare`），会出现在台账与 CLI 输出中。
    pub fn new(
        name: impl Into<String>,
        config: CompshareConfig,
        credential: Credential,
        transport: T,
    ) -> MnResult<Self> {
        config.validate()?;
        let client = CompshareClient::new(
            config.endpoint.clone(),
            credential,
            transport,
            config.region.clone(),
            config.zone.clone(),
        )?;
        Ok(Self {
            name: name.into(),
            config,
            client,
            project_id: std::sync::Mutex::new(None),
        })
    }

    /// 配置引用。
    pub fn config(&self) -> &CompshareConfig {
        &self.config
    }

    /// 底层客户端引用（供 CLI 直连查询镜像/可用区）。
    pub fn client(&self) -> &CompshareClient<T> {
        &self.client
    }

    /// 查询平台镜像列表（供选择 `image_id`）。
    pub fn list_images(&self, image_type: Option<&str>) -> MnResult<Vec<crate::model::ImageInfo>> {
        let r = self.client.images(image_type, PAGE_LIMIT, 0)?;
        Ok(r.image_set)
    }

    /// 查询支持的地域/可用区。
    pub fn list_zones(&self) -> MnResult<Vec<crate::model::ZoneInfo>> {
        let r = self.client.zones()?;
        Ok(r.zones)
    }

    /// 检查当前配置的「可用区 + 镜像」组合是否兼容。
    ///
    /// 厂商在可用区列表里会给出 `UnsupportedImageTypes`（实测字段，文档未列）。
    /// 若当前可用区不支持所选镜像类型，创建实例会被以 226603 拒绝。
    /// 提前检查能把「跑到创建才失败」变成「启动前就说清楚」。
    ///
    /// 返回 `Ok(None)` 表示可用区列表里查不到当前可用区（不阻断，交由创建时的
    /// 真实校验兜底）；`Ok(Some(zone))` 表示找到了对应可用区。
    pub fn check_zone_image_compat(&self) -> MnResult<Option<crate::model::ZoneInfo>> {
        let r = self.client.zones_filtered(Some(&self.config.region))?;
        let found = r.zones.into_iter().find(|z| z.zone == self.config.zone);
        if let Some(z) = &found {
            // 配置里没有显式声明镜像类型，本项目的镜像均为平台系统镜像
            if !z.supports_image_type("System") {
                return Err(MnError::Provider {
                    provider: self.name.clone(),
                    code: codes::IMAGE_NOT_SUPPORT_GPU.to_string(),
                    message: format!(
                        "可用区 {}/{}（{}）不支持系统镜像，创建实例会被厂商拒绝。\
                         该可用区为{}可用区，仅支持容器镜像；\
                         请改用虚机可用区（如 cn-wlcb-01 华北二A）。",
                        self.config.region,
                        self.config.zone,
                        z.describe,
                        z.resource_kind()
                    ),
                });
            }
        }
        Ok(found)
    }

    /// 自动创建到「可服务」状态：**库存检查 → 创建 → 等待 Running**。
    ///
    /// 这是「一键拉起 GPU 实例」的完整流程，供 CLI 的 `backend create` 使用。
    /// 与 [`ProviderDriver::create`] 的区别：后者只提交创建请求就返回，
    /// 由编排器负责后续轮询（编排器还要并行做隧道等事）。
    ///
    /// 失败时**必定尝试回滚**已创建的实例：云端资源只要存在就在计费，
    /// 留下孤儿实例比报错更糟。
    pub fn provision_ready(&self, spec_id: &str, tag: &str) -> MnResult<Instance> {
        let created = self.create(spec_id, Some(tag))?;
        match self.wait_until(&created.id, &[InstanceState::Running]) {
            Ok(inst) => Ok(inst),
            Err(e) => {
                let cleanup = self.destroy(&created.id);
                Err(augment_cleanup(e, &created.id, cleanup))
            }
        }
    }

    /// 轮询实例直到进入目标状态之一。
    pub fn wait_until(&self, id: &str, targets: &[InstanceState]) -> MnResult<Instance> {
        let deadline = Instant::now() + Duration::from_secs(self.config.operation_timeout_secs);
        loop {
            let inst = self.describe(id)?;
            if targets.contains(&inst.state) {
                return Ok(inst);
            }
            if inst.state == InstanceState::Destroyed {
                return Err(MnError::Provider {
                    provider: self.name.clone(),
                    code: "instance_destroyed".to_string(),
                    message: format!("实例 {id} 已被销毁，无法继续等待"),
                });
            }
            if Instant::now() >= deadline {
                return Err(MnError::Provider {
                    provider: self.name.clone(),
                    code: "wait_timeout".to_string(),
                    message: format!(
                        "实例 {id} 在 {}s 内未进入 {:?}（当前 {}）。\
                         若为镜像首次启动，可能需要更长初始化时间，可调大 \
                         operation_timeout_secs。",
                        self.config.operation_timeout_secs,
                        targets
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join("/"),
                        inst.state
                    ),
                });
            }
            if self.config.poll_interval_secs > 0 {
                std::thread::sleep(Duration::from_secs(self.config.poll_interval_secs));
            }
        }
    }

    /// 创建前的库存检查。
    ///
    /// 厂商文档明确要求：判库存必须用本接口，不能用 Describe 类接口代替。
    fn ensure_capacity(&self, gpu_type: &str, gpu: u32, cpu: u32, memory_gb: u64) -> MnResult<()> {
        let req = CapacityRequest {
            gpu_type: gpu_type.to_string(),
            machine_type: self.config.machine_type.clone(),
            cpu_platform: self.config.cpu_platform.clone(),
            image_id: self.config.image_id.clone(),
            charge_type: self.config.charge_type.clone(),
            disk_type: self.config.disk_type.clone(),
            disk_size_gb: self.config.disk_size_gb,
        };
        let resp = self.client.check_capacity(&req)?;
        if resp.is_enough(gpu, cpu, memory_gb) {
            return Ok(());
        }
        Err(provider_error(
            &self.name,
            codes::RESOURCE_NOT_ENOUGH,
            &format!(
                "规格 {gpu_type} {gpu}卡/{cpu}核/{memory_gb}GB 当前无库存\
                 （可用区 {}/{}，镜像 {}）",
                self.config.region, self.config.zone, self.config.image_id
            ),
        ))
    }

    /// 查询单个实例的厂商原始结构。
    fn describe_raw(&self, id: &str) -> MnResult<UHost> {
        let ids = [id.to_string()];
        let resp = self.client.describe_instances(Some(&ids), PAGE_LIMIT, 0)?;
        // 服务端可能忽略过滤参数返回全部实例，这里再按 ID 精确匹配一次
        resp.uhost_set
            .into_iter()
            .find(|u| u.id == id)
            .ok_or_else(|| MnError::Provider {
                provider: self.name.clone(),
                code: "instance_not_found".to_string(),
                message: format!("实例 {id} 不存在（可能已被删除或不属于当前账号）"),
            })
    }

    /// **不限地域**查询单个实例的厂商原始结构。
    ///
    /// 与 [`Self::describe_raw`] 的区别：后者会按当前配置的 `region` 过滤。
    /// 实例可能是用另一份配置（或另一个地域）创建的，按当前地域过滤会查不到，
    /// 进而在监控/销毁链路上把「地域不匹配」误判成「实例已不存在」。
    fn describe_raw_any_scope(&self, id: &str) -> MnResult<UHost> {
        let ids = [id.to_string()];
        let resp = self
            .client
            .describe_instances_any_scope(Some(&ids), PAGE_LIMIT, 0)?;
        resp.uhost_set
            .into_iter()
            .find(|u| u.id == id)
            .ok_or_else(|| MnError::Provider {
                provider: self.name.clone(),
                code: "instance_not_found".to_string(),
                message: format!("实例 {id} 不存在（已查询全部地域，可能已被删除）"),
            })
    }

    /// 查询价格明细（单组合）。
    fn price_detail(
        &self,
        gpu_type: &str,
        gpu: u32,
        cpu: u32,
        memory_gb: u64,
    ) -> MnResult<Option<PriceDetail>> {
        let req = PriceRequest {
            gpu_type: gpu_type.to_string(),
            gpu,
            cpu,
            memory_mb: CompshareConfig::memory_mb(memory_gb),
            charge_type: Some(self.config.charge_type.clone()),
            disk_type: self.config.disk_type.clone(),
            disk_size_gb: self.config.disk_size_gb,
        };
        let resp = self.client.price(&req)?;
        Ok(resp.price_details.into_iter().next())
    }

    /// 构造一个「已知 ID 但详情未知」的实例，用于创建后补查失败时的兜底。
    ///
    /// 关键点：**必须保留 ID**。编排器要靠它回滚，否则实例会一直计费。
    fn placeholder_instance(
        &self,
        id: &str,
        gpu_type: &str,
        gpu: u32,
        cpu: u32,
        memory_gb: u64,
        tag: Option<&str>,
    ) -> Instance {
        Instance {
            id: id.to_string(),
            provider: self.name.clone(),
            // 刚提交创建，厂商侧一定还没到 Running
            state: InstanceState::Creating,
            spec: mangosteen_core::cloud::InstanceSpec {
                gpu: mangosteen_core::cloud::GpuSpec {
                    model: mapping::normalize_gpu_model(gpu_type),
                    count: gpu,
                    vram_bytes: units::gb_to_bytes(mapping::known_vram_gb(gpu_type)),
                },
                cpu_cores: cpu,
                memory_bytes: units::gb_to_bytes(memory_gb),
                disk_bytes: units::gb_to_bytes(u64::from(self.config.disk_size_gb)),
                image: self.config.image_id.clone(),
            },
            network: mangosteen_core::cloud::NetworkEndpoint::default(),
            created_at: mangosteen_core::time::now_unix(),
            price: None,
            display_name: tag.map(|t| self.config.instance_name(t)),
        }
    }
    // ============================================================
    // 实例配置管理（改名 / 密码 / 重装 / 升降配 / 计费 / 端口 / 定时关机）
    // ============================================================
    //
    // 这些方法的共同点：**先做状态前置校验，再发请求**。
    //
    // 为什么不在客户端层校验：客户端只管「把参数发对」，
    // 而「这台机器现在能不能改配置」是业务判断，需要先查实例状态。
    // 放在驱动层还能给出**可操作**的提示（「请先关机」而不是
    // 厂商那句「Params invalid」）。

    /// 校验实例处于 `Stopped`，返回其原始结构。
    ///
    /// 升降配 / 重装 / 重置密码（虚机）都要求先关机。厂商对状态不符
    /// 只回笼统的参数错误，用户很难判断该做什么，所以这里提前拦。
    fn require_stopped(&self, id: &str, action: &str) -> MnResult<UHost> {
        let raw = self.describe_raw_any_scope(id)?;
        if raw.state.eq_ignore_ascii_case(state::STOPPED) {
            return Ok(raw);
        }
        Err(MnError::Provider {
            provider: self.name.clone(),
            code: "instance_not_stopped".to_string(),
            message: format!(
                "{action}要求实例已关机，但 {} 当前状态是 {}。\
                 请先执行 `mangosteen cloud stop {}`，再重试。",
                id, raw.state, id
            ),
        })
    }

    /// 是否为容器可用区实例（`InstanceType=Container`）。
    ///
    /// 容器实例与虚机的差异会直接影响可用操作：
    /// * 容器实例支持**运行中**重置密码，虚机必须先关机；
    /// * 只有容器实例能用 `UpdateCompShareInstancePorts` 改对外端口。
    fn is_container(uhost: &UHost) -> bool {
        uhost.instance_type.eq_ignore_ascii_case("container")
    }

    /// 修改实例名称。
    pub fn rename(&self, id: &str, name: &str) -> MnResult<()> {
        self.client.modify_instance_name(id, name)?;
        Ok(())
    }

    /// 重置实例登录密码。
    ///
    /// 虚机要求 `Stopped`，容器实例可运行中重置——按实例类型分别校验，
    /// 一刀切要求关机会让容器实例的合法调用被误拒。
    pub fn reset_password(&self, id: &str, password: &str) -> MnResult<()> {
        let raw = self.describe_raw_any_scope(id)?;
        if !Self::is_container(&raw) && !raw.state.eq_ignore_ascii_case(state::STOPPED) {
            return Err(MnError::Provider {
                provider: self.name.clone(),
                code: "instance_not_stopped".to_string(),
                message: format!(
                    "普通主机实例重置密码前必须先关机，但 {} 当前状态是 {}。\
                     请先执行 `mangosteen cloud stop {}`，再重试。\
                     （容器可用区实例支持运行中重置）",
                    id, raw.state, id
                ),
            });
        }
        self.client.reset_instance_password(id, password)?;
        Ok(())
    }

    /// 重装系统镜像（**仅 `Stopped`**）。
    ///
    /// ⚠ 系统盘数据会被清除，数据盘不受影响。调用方应先确认用户知情。
    pub fn reinstall(&self, id: &str, image_id: &str, password: Option<&str>) -> MnResult<()> {
        self.require_stopped(id, "重装系统")?;
        self.client.reinstall_instance(id, image_id, password)?;
        Ok(())
    }

    /// 升降配：调整 CPU / 内存 / GPU（**仅 `Stopped`**）。
    ///
    /// `memory_gb` 是 GB，内部转成厂商要求的 MB。
    /// 调整完成后实例仍是 `Stopped`，需要另行 start。
    pub fn resize_compute(&self, id: &str, cpu: u32, memory_gb: u64, gpu: u32) -> MnResult<()> {
        self.require_stopped(id, "升降配")?;
        self.client
            .resize_instance_compute(id, cpu, CompshareConfig::memory_mb(memory_gb), gpu)?;
        Ok(())
    }

    /// 升降配：扩容磁盘（**仅 `Stopped`**，只能扩不能缩）。
    pub fn resize_disk(&self, id: &str, disk_id: &str, size_gb: u32) -> MnResult<()> {
        self.require_stopped(id, "磁盘扩容")?;
        self.client.resize_instance_disk(id, disk_id, size_gb)?;
        Ok(())
    }

    /// 变更计费方式（如按量 → 包月）。
    ///
    /// 不预检状态：厂商对可切换的实例有自己的一套判断（含余额、账期），
    /// 我们复刻不准确，交给厂商回答更有依据。
    pub fn switch_charge_type(&self, id: &str, dest: &str) -> MnResult<()> {
        self.client.switch_charge_type(id, dest)?;
        Ok(())
    }

    /// 查询升降配差价（**元**，不是时价）。
    pub fn upgrade_price(
        &self,
        id: &str,
        cpu: Option<u32>,
        memory_gb: Option<u64>,
        gpu: Option<u32>,
    ) -> MnResult<UpgradePriceResponse> {
        self.client
            .upgrade_price(id, cpu, memory_gb.map(CompshareConfig::memory_mb), gpu)
    }

    /// 查询某规格在**全部计费模式**下的价格（用于「按量 vs 包月」对比）。
    pub fn user_price(
        &self,
        gpu_type: &str,
        gpu: u32,
        cpu: u32,
        memory_gb: u64,
    ) -> MnResult<PriceResponse> {
        self.client.user_price(
            gpu_type,
            gpu,
            cpu,
            CompshareConfig::memory_mb(memory_gb),
            Some(self.config.image_id.as_str()).filter(|s| !s.is_empty()),
            self.config.disk_size_gb,
            &self.config.disk_type,
        )
    }

    /// 查询删除实例可退金额（批量）。
    pub fn refund_price(&self, ids: &[String]) -> MnResult<RefundPriceResponse> {
        self.client.refund_price(ids)
    }

    /// 查询账号是否开通网络加速（影响海外模型下载速度）。
    pub fn net_optimizer(&self) -> MnResult<NetOptimizerResponse> {
        self.client.check_net_optimizer()
    }

    /// 查询平台软件端口映射（JupyterLab / SSH / FileBrowser 等）。
    pub fn software_ports(&self) -> MnResult<SoftwarePortResponse> {
        self.client.software_ports()
    }

    /// 查询实例上某软件的访问地址。
    pub fn software_url(&self, id: &str, software: &str) -> MnResult<String> {
        Ok(self.client.software_url(id, software)?.url)
    }

    /// 查询机型族列表（全量机型目录，含未上架）。
    pub fn machine_types(&self) -> MnResult<Vec<crate::model::MachineTypeFamily>> {
        Ok(self.client.machine_type_families()?.families)
    }

    /// 更新容器实例的对外端口（全量替换）。
    ///
    /// 非容器实例直接拒绝：厂商会回「参数非法」，而真正原因是
    /// 「这个接口不适用于虚机」——不提前说清，用户会去查参数拼写。
    pub fn update_ports(
        &self,
        id: &str,
        http_ports: &[u16],
        tcp_ports: &[u16],
    ) -> MnResult<InstancePortsResponse> {
        let raw = self.describe_raw_any_scope(id)?;
        if !Self::is_container(&raw) {
            return Err(MnError::Provider {
                provider: self.name.clone(),
                code: "not_container_instance".to_string(),
                message: format!(
                    "实例 {id} 是普通主机（InstanceType={}），不支持端口管理。\
                     该接口仅适用于容器可用区实例。",
                    raw.instance_type
                ),
            });
        }
        self.client.update_instance_ports(id, http_ports, tcp_ports)
    }

    /// 切换公网带宽归属（`Company` 独享 / `Public` 平台共享）。
    ///
    /// 返回逐实例结果：**顶层成功不代表每个都成功**，
    /// 调用方应检查 [`BandwidthSwitchResponse::failures`]。
    pub fn switch_bandwidth(
        &self,
        ids: &[String],
        target_scope: &str,
    ) -> MnResult<BandwidthSwitchResponse> {
        self.client.switch_bandwidth(ids, target_scope)
    }

    /// 取默认项目 ID（首次调用时查询并缓存）。
    fn default_project_id(&self) -> MnResult<String> {
        if let Ok(guard) = self.project_id.lock() {
            if let Some(id) = guard.as_ref() {
                return Ok(id.clone());
            }
        }
        let resp = self.client.project_list()?;
        let id = resp.default_project_id().ok_or_else(|| MnError::Provider {
            provider: self.name.clone(),
            code: "project_not_found".to_string(),
            message: "账号下没有可用的项目（GetProjectList 返回空）。\
                      定时关机接口要求 ProjectId，无法继续。"
                .to_string(),
        })?;
        let id = id.to_string();
        if let Ok(mut guard) = self.project_id.lock() {
            *guard = Some(id.clone());
        }
        Ok(id)
    }

    /// 设置定时关机（防止忘记关机导致持续计费）。
    ///
    /// `after_secs` 是「多少秒后关机」。厂商要求目标时间
    /// **≥ 当前 + 300 秒**，小于此值会被拒——这里提前拦并说明原因。
    pub fn schedule_stop(&self, id: &str, after_secs: i64) -> MnResult<i64> {
        if after_secs < MIN_STOP_DELAY_SECS {
            return Err(MnError::bad_request(format!(
                "定时关机至少要在 {MIN_STOP_DELAY_SECS} 秒之后（厂商限制），                 当前设置 {after_secs} 秒。若想立刻关机，请直接执行 stop。"
            )));
        }
        let project_id = self.default_project_id()?;
        let stop_at = mangosteen_core::time::now_unix() + after_secs;
        self.client
            .update_stop_scheduler(id, &project_id, stop_at)?;
        Ok(stop_at)
    }

    /// 取消定时关机。
    pub fn cancel_scheduled_stop(&self, id: &str) -> MnResult<()> {
        let project_id = self.default_project_id()?;
        self.client.delete_stop_scheduler(id, &project_id)?;
        Ok(())
    }
}

impl<T: Transport> ProviderDriver for CompshareDriver<T> {
    fn name(&self) -> &str {
        &self.name
    }

    fn display_name(&self) -> &str {
        "优云智算（Compshare）"
    }

    fn capabilities(&self) -> DriverCapabilities {
        DriverCapabilities {
            on_demand_create: true,
            stop_start: true,
            price_query: true,
            // 厂商提供公网 IP，但推理端口需自行放通防火墙；
            // 默认仍走 SSH 隧道，避免依赖用户手工配置安全组
            public_inference_url: false,
            custom_image: true,
            gpu_models: [
                "4090", "4090_48G", "5090", "3090", "3080Ti", "2080Ti", "2080", "A100", "A800",
                "H20", "P40", "V100S",
            ]
            .iter()
            .map(|c| mapping::normalize_gpu_model(c))
            .collect(),
        }
    }

    /// 列出可创建规格：机型表 × 库存检查 × 价格查询。
    ///
    /// 调用量控制：每个 GPU 型号 1 次库存检查 + 每个候选 1 次价格查询，
    /// 价格查询总量受 `max_price_queries` 限制（默认 10）。
    fn list_available_specs(&self) -> MnResult<Vec<AvailableSpec>> {
        let types = self.client.available_types()?;
        let mut out = Vec::new();
        let mut price_budget = self.config.max_price_queries;

        for item in &types.types {
            if !item.is_on_sale() || !self.config.allows_gpu(&item.name) {
                continue;
            }

            // 一次库存检查覆盖该型号下所有 (GPU, CPU, 内存) 组合
            let capacity_req = CapacityRequest {
                gpu_type: item.name.clone(),
                machine_type: self.config.machine_type.clone(),
                cpu_platform: self.config.cpu_platform.clone(),
                image_id: self.config.image_id.clone(),
                charge_type: self.config.charge_type.clone(),
                disk_type: self.config.disk_type.clone(),
                disk_size_gb: self.config.disk_size_gb,
            };
            // 库存查询失败不应让整个列举失败：用「未知库存」继续，
            // 让上层仍能看到价格与规格（真实创建时还会再查一次）
            let capacity = self.client.check_capacity(&capacity_req).ok();

            for size in &item.machine_sizes {
                if size.gpu == 0 || size.gpu > self.config.max_gpu_count {
                    continue;
                }
                // 选该卡数下第一个满足内存下限的组合（最小可用档，够用即可）
                let combo = size.collection.iter().find_map(|c| {
                    c.memory_gb
                        .iter()
                        .filter(|m| **m >= self.config.min_memory_gb)
                        .min()
                        .map(|m| (c.cpu, *m))
                });
                let Some((cpu, memory_gb)) = combo else {
                    continue;
                };

                let stock = capacity.as_ref().map(|cap| {
                    if cap.is_enough(size.gpu, cpu, memory_gb) {
                        1u32
                    } else {
                        0u32
                    }
                });

                let price = if price_budget > 0 {
                    price_budget -= 1;
                    self.price_detail(&item.name, size.gpu, cpu, memory_gb)
                        .ok()
                        .flatten()
                } else {
                    None
                };

                if let Some(spec) = mapping::to_available_spec(
                    item,
                    size.gpu,
                    cpu,
                    memory_gb,
                    price.as_ref(),
                    &self.config.image_id,
                    stock,
                ) {
                    out.push(spec);
                }
            }
        }
        Ok(out)
    }

    fn create(&self, spec_id: &str, name: Option<&str>) -> MnResult<Instance> {
        let (gpu_type, gpu, cpu, memory_gb) = mapping::parse_spec_id(spec_id)?;
        if !self.config.allows_gpu(&gpu_type) {
            return Err(MnError::bad_request(format!(
                "GPU 型号 `{gpu_type}` 不在配置允许列表内（gpu_types = {:?}）",
                self.config.gpu_types
            )));
        }

        // 1. 可用区与镜像兼容性（避免走到创建才被 226603 拒绝）
        //    查不到可用区信息时不阻断——交由后续真实校验兜底。
        self.check_zone_image_compat()?;

        // 2. 库存预检（可按配置关闭）
        //
        // 优云的库存接口是模拟调度：连续批量查询会全判无货，而同一时刻
        // 单独查或直接创建都成功。关掉后由真实创建失败（226604）兜底，
        // 编排器据此换下一个候选——语义一致，但不会误杀可用节点。
        if self.config.require_stock {
            self.ensure_capacity(&gpu_type, gpu, cpu, memory_gb)?;
        }

        // 3. 提交创建
        let req = CreateRequest {
            gpu_type: gpu_type.clone(),
            gpu,
            cpu,
            memory_mb: CompshareConfig::memory_mb(memory_gb),
            image_id: self.config.image_id.clone(),
            machine_type: self.config.machine_type.clone(),
            cpu_platform: self.config.cpu_platform.clone(),
            charge_type: self.config.charge_type.clone(),
            disk_type: self.config.disk_type.clone(),
            disk_size_gb: self.config.disk_size_gb,
            name: Some(self.config.instance_name(name.unwrap_or(""))),
            remark: Some(self.config.remark.clone()),
            quantity: None,
        };
        let resp = self.client.create_instance(&req)?;
        let id = resp
            .uhost_ids
            .first()
            .cloned()
            .ok_or_else(|| MnError::Provider {
                provider: self.name.clone(),
                code: "missing_instance_id".to_string(),
                message: "创建接口返回成功但未给出实例 ID".to_string(),
            })?;

        // 4. 补齐详情。失败也要返回带 ID 的占位实例——
        //    否则编排器拿不到 ID，回滚就无从谈起，实例会持续计费。
        match self.describe_raw(&id) {
            Ok(u) => Ok(mapping::to_instance(&self.name, &u, &self.config.image_id)),
            Err(_) => Ok(self.placeholder_instance(&id, &gpu_type, gpu, cpu, memory_gb, name)),
        }
    }

    fn start(&self, instance_id: &str) -> MnResult<Instance> {
        let current = self.describe(instance_id)?;
        match current.state {
            // 幂等：已在运行/启动中就直接返回现状
            InstanceState::Running | InstanceState::Starting => return Ok(current),
            InstanceState::Destroyed => {
                return Err(MnError::InvalidTransition {
                    instance: instance_id.to_string(),
                    from: InstanceState::Destroyed.as_str().to_string(),
                    action: "start".to_string(),
                })
            }
            _ => {}
        }

        self.client.start_instance(instance_id, None)?;
        // 启动是异步的：先给出「启动中」，由编排器轮询到 Running
        match self.describe_raw(instance_id) {
            Ok(u) => Ok(mapping::to_instance(&self.name, &u, &self.config.image_id)),
            Err(_) => {
                let mut inst = current;
                inst.state = InstanceState::Starting;
                Ok(inst)
            }
        }
    }

    fn stop(&self, instance_id: &str) -> MnResult<Instance> {
        let current = self.describe(instance_id)?;
        match current.state {
            InstanceState::Stopped | InstanceState::Stopping => return Ok(current),
            InstanceState::Destroyed => {
                return Err(MnError::InvalidTransition {
                    instance: instance_id.to_string(),
                    from: InstanceState::Destroyed.as_str().to_string(),
                    action: "stop".to_string(),
                })
            }
            _ => {}
        }

        self.client.stop_instance(instance_id)?;
        match self.describe_raw(instance_id) {
            Ok(u) => Ok(mapping::to_instance(&self.name, &u, &self.config.image_id)),
            Err(_) => {
                let mut inst = current;
                inst.state = InstanceState::Stopping;
                Ok(inst)
            }
        }
    }

    /// 销毁实例。
    ///
    /// 厂商要求「仅 `Stopped` 可删除」，所以这里自动完成
    /// 关机 → 等待 Stopped → 删除 的完整流程。
    /// 这是本驱动最重要的一个业务补偿：编排器回滚时直接调 `destroy`，
    /// 若这里不做补偿，运行中的实例永远删不掉，用户会一直被计费。
    fn destroy(&self, instance_id: &str) -> MnResult<()> {
        let current = match self.describe(instance_id) {
            Ok(i) => i,
            Err(e) => {
                // 查不到实例：可能是已删除（幂等场景），也可能地域配错。
                // 保守做法是再试一次删除，仍失败则如实报错。
                return match self
                    .client
                    .terminate_instance(instance_id, self.config.release_data_disk)
                {
                    Ok(_) => Ok(()),
                    Err(term_err) => {
                        if is_not_found(&term_err) {
                            Ok(())
                        } else {
                            Err(e)
                        }
                    }
                };
            }
        };

        if current.state == InstanceState::Destroyed {
            return Ok(());
        }

        // 1. 确保处于 Stopped
        if current.state != InstanceState::Stopped {
            self.client.stop_instance(instance_id)?;
            self.wait_until(instance_id, &[InstanceState::Stopped])?;
        }

        // 2. 删除
        self.client
            .terminate_instance(instance_id, self.config.release_data_disk)?;
        Ok(())
    }

    fn describe(&self, instance_id: &str) -> MnResult<Instance> {
        let raw = self.describe_raw(instance_id)?;
        Ok(mapping::to_instance(
            &self.name,
            &raw,
            &self.config.image_id,
        ))
    }

    fn list(&self) -> MnResult<Vec<Instance>> {
        let mut out = Vec::new();
        let mut offset = 0u32;
        for _ in 0..MAX_PAGES {
            let page = self.client.describe_instances(None, PAGE_LIMIT, offset)?;
            let got = page.uhost_set.len() as u32;
            out.extend(
                page.uhost_set
                    .iter()
                    .map(|u| mapping::to_instance(&self.name, u, &self.config.image_id)),
            );
            if got < PAGE_LIMIT || out.len() as u32 >= page.total_count {
                break;
            }
            offset = offset.saturating_add(PAGE_LIMIT);
        }
        Ok(out)
    }

    /// 查询实例实时监控。
    ///
    /// ## 为什么先查实例、再查监控
    ///
    /// `GetCompShareInstanceMonitor` 的 `Region` **必填且必须与实例所在地域一致**
    /// （这是这条链路最容易出错的地方）。如果直接用客户端配置的地域去查，
    /// 而实例其实建在别的地域，厂商会对该实例静默返回空 `List`——
    /// 表现为「有实例但永远没有监控数据」，极难排查。
    ///
    /// 因此这里先不限地域查出实例真实所在地域，再用该地域派生客户端发监控请求。
    fn reset_root_password(&self, instance_id: &str, password: &str) -> MnResult<()> {
        // 厂商要求密码同时含大写、小写、数字与特殊字符，且长度有限。
        // 这里先自检：把「密码不合规」在本地就报出来，比等厂商回 8314
        // 再翻译成人话要快得多。
        let has_upper = password.chars().any(|c| c.is_ascii_uppercase());
        let has_lower = password.chars().any(|c| c.is_ascii_lowercase());
        let has_digit = password.chars().any(|c| c.is_ascii_digit());
        let has_special = password.chars().any(|c| !c.is_ascii_alphanumeric());
        if !(has_upper && has_lower && has_digit && has_special) {
            return Err(MnError::config(
                "cloud.ssh_password",
                "密码必须同时包含大写字母、小写字母、数字与特殊字符（优云智算的强制要求）",
            ));
        }

        let r = self.client.reset_instance_password(instance_id, password)?;
        if r.ret_code != 0 {
            return Err(MnError::Provider {
                provider: self.name.clone(),
                code: r.ret_code.to_string(),
                message: format!("设置实例密码失败：{}", r.message),
            });
        }
        Ok(())
    }

    fn instance_metrics(&self, instance_id: &str) -> MnResult<Option<InstanceMetrics>> {
        // 1. 定位实例真实地域（不限地域查询，避免误判）
        let raw = match self.describe_raw_any_scope(instance_id) {
            Ok(u) => u,
            Err(e) if is_not_found(&e) => return Ok(None),
            Err(e) => return Err(e),
        };

        // 2. 用实例自己的 region/zone 派生客户端。厂商没回填这两个字段时
        //    退回配置值（否则会漏注入必填的 Region）。
        let region = if raw.region.trim().is_empty() {
            self.config.region.clone()
        } else {
            raw.region.clone()
        };
        let zone = if raw.zone.trim().is_empty() {
            self.config.zone.clone()
        } else {
            raw.zone.clone()
        };
        // `transport()` 返回的引用生命周期只到本条语句，必须先绑定到局部变量，
        // 否则派生客户端会持有已释放的临时值。
        let transport = self.client.transport();
        let scoped = self.client.with_scope(transport, region, zone)?;

        // 3. 查监控。实例刚开机的一两分钟厂商可能还没有数据点，
        //    此时返回空 List 属正常，不是错误。
        let ids = [instance_id.to_string()];
        let resp = scoped.instance_monitor(&ids)?;
        let from_series = resp.find(instance_id).map(mapping::to_instance_metrics);

        // 4. 时间序列为空时用 `MonitorMessages` 快照兜底。
        //
        //    实测：实例关机（或刚开机）时监控接口会返回全部指标键，
        //    但 `Results` 都是空数组。此时若直接返回空快照，设备页
        //    会显示「无数据」，用户无法区分「厂商没数据」与「我们没接好」。
        //    第 1 步查到的 `raw` 里就带着当前快照，正好补上。
        match from_series {
            Some(m) if m.has_any() => Ok(Some(m)),
            _ => {
                let snap = mapping::to_metrics_from_snapshot(&raw);
                if snap.has_any() {
                    Ok(Some(snap))
                } else {
                    // 两者都空：实例存在但确实没有指标（例如已关机）。
                    // 返回空快照而不是错误，让上层显示「暂无监控数据」。
                    Ok(from_series.or(Some(InstanceMetrics {
                        instance_id: instance_id.to_string(),
                        ..Default::default()
                    })))
                }
            }
        }
    }
}

/// 判断错误是否是「实例不存在」。
///
/// 厂商文案中英混用（`instance not found` / `实例不存在`），且不一定给稳定错误码，
/// 因此三种形态都要覆盖：错误码含 `not_found`、英文文案、中文文案。
fn is_not_found(err: &MnError) -> bool {
    match err {
        MnError::Provider { code, message, .. } => {
            let code_lower = code.to_ascii_lowercase();
            let msg_lower = message.to_ascii_lowercase();
            code_lower.contains("not_found")
                || msg_lower.contains("not found")
                || msg_lower.contains("notfound")
                || msg_lower.contains("does not exist")
                || message.contains("不存在")
        }
        _ => false,
    }
}

/// 在错误消息后追加回滚结果。
fn augment_cleanup(base: MnError, id: &str, cleanup: MnResult<()>) -> MnError {
    let suffix = match cleanup {
        Ok(()) => format!("\n已自动删除临时实例 {id}，未产生持续费用。"),
        Err(e) => format!(
            "\n⚠ 自动删除实例 {id} 失败：{e}\n\
             请立即到优云智算控制台手工释放，避免持续计费。"
        ),
    };
    MnError::Provider {
        provider: PROVIDER_NAME.to_string(),
        code: "provision_rollback".to_string(),
        message: format!("{base}{suffix}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::MockTransport;

    fn cfg() -> CompshareConfig {
        CompshareConfig {
            image_id: "img-1".to_string(),
            // 配置校验要求轮询间隔 ≥1 秒（避免打爆厂商接口），
            // 这里取最小合法值，把测试等待控制在 1 秒内
            poll_interval_secs: 1,
            operation_timeout_secs: 1,
            max_price_queries: 2,
            ..Default::default()
        }
    }

    /// 用默认配置 + 指定传输构造驱动。
    ///
    /// 泛型是为了同时支持两种测试传输：
    /// `MockTransport`（固定序列）与 `FakeVendor`（按 Action 路由）。
    fn driver<T: Transport>(transport: T) -> CompshareDriver<T> {
        CompshareDriver::new(
            "compshare",
            cfg(),
            Credential::new("pk", "sk").unwrap(),
            transport,
        )
        .unwrap()
    }

    /// 常用响应体片段
    const TYPES: &str = r#"{"RetCode":0,"AvailableInstanceTypes":[
        {"Name":"4090","Zone":"cn-wlcb-01","Status":"Normal",
         "MachineSizes":[{"Gpu":1,"Collection":[{"Cpu":16,"Memory":[64,94]}]},
                         {"Gpu":2,"Collection":[{"Cpu":32,"Memory":[128]}]}],
         "GraphicsMemory":{"Value":24,"Rate":3}},
        {"Name":"A100","Zone":"cn-wlcb-01","Status":"SoldOut",
         "MachineSizes":[{"Gpu":1,"Collection":[{"Cpu":16,"Memory":[64]}]}],
         "GraphicsMemory":{"Value":80,"Rate":5}}
    ]}"#;

    const CAPACITY_OK: &str =
        r#"{"RetCode":0,"Specs":[{"Gpu":1,"Cpu":16,"Mem":64,"ResourceEnough":true}]}"#;

    const PRICE: &str = r#"{"RetCode":0,"PriceDetails":[{"ChargeType":"Postpay","Instance":2.5,"SystemDisks":0.0}]}"#;

    const INSTANCE_RUNNING: &str = r#"{"RetCode":0,"TotalCount":1,"UHostSet":[{
        "UHostId":"uhost-1","Name":"mangosteen-11435","State":"Running",
        "CPU":16,"Memory":65536,"GPU":1,"GpuType":"4090","ChargeType":"Postpay",
        "CreateTime":1712563200,"CompShareImageId":"img-1",
        "SshLoginCommand":"ssh root@203.0.113.10 -p 60022",
        "IPSet":[{"IP":"203.0.113.10","Type":"Bgp"}],
        "TotalDiskSpace":100,"InstancePrice":2.5,"DiskPrice":0.0
    }]}"#;

    /// 构造指定 ID/状态的单实例 describe 响应。
    ///
    /// 比固定字符串更贴近真实协议：厂商只会返回被查询的那个实例，
    /// 用固定 ID 会让「按 ID 精确匹配」这条逻辑测不到。
    fn instance_json(id: &str, state: &str) -> String {
        format!(
            r#"{{"RetCode":0,"TotalCount":1,"UHostSet":[{{
                "UHostId":"{id}","Name":"mangosteen-{id}","State":"{state}",
                "CPU":16,"Memory":65536,"GPU":1,"GpuType":"4090","ChargeType":"Postpay",
                "CreateTime":1712563200,"CompShareImageId":"img-1",
                "SshLoginCommand":"ssh root@203.0.113.10 -p 60022",
                "IPSet":[{{"IP":"203.0.113.10","Type":"Bgp"}}],
                "TotalDiskSpace":100,"InstancePrice":2.5,"DiskPrice":0.0
            }}]}}"#
        )
    }

    /// 可用区响应：当前配置的 cn-wlcb-01 是虚机可用区且支持系统镜像。
    const ZONES_OK: &str = r#"{"RetCode":0,"ZoneInfo":[
        {"Region":"cn-wlcb","RegionId":1000039,"Zone":"cn-wlcb-01","ZoneId":10027,
         "Describe":"华北二A","IsPod":false,"UnsupportedImageTypes":[]}
    ]}"#;

    /// 按 `Action` 路由的测试传输：每个接口返回各自的响应。
    ///
    /// ## 为什么不用固定响应序列
    ///
    /// `MockTransport::sequence` 依赖「调用顺序完全固定」。一旦驱动内部新增一次
    /// 前置查询（例如创建前先查可用区做镜像兼容检查），整个序列就会错位，
    /// 导致一堆与被测逻辑无关的失败。
    ///
    /// 按 Action 路由与调用顺序解耦：新增前置查询只需注册它的响应，
    /// 不影响其它用例——这也更贴近真实厂商（每个 Action 独立响应）。
    struct FakeVendor {
        routes: std::collections::BTreeMap<&'static str, String>,
        recorded: std::sync::Mutex<Vec<String>>,
    }

    impl FakeVendor {
        /// `overrides` 可按 Action 名覆盖默认响应。
        fn new(overrides: Vec<(&'static str, String)>) -> Self {
            let mut routes = std::collections::BTreeMap::new();
            // 默认：可用区查询返回「当前可用区兼容系统镜像」
            routes.insert("DescribeCompShareSupportZone", ZONES_OK.to_string());
            for (k, v) in overrides {
                routes.insert(k, v);
            }
            Self {
                routes,
                recorded: std::sync::Mutex::new(Vec::new()),
            }
        }

        /// 收到的请求体（按顺序）。
        fn calls(&self) -> Vec<String> {
            self.recorded.lock().map(|v| v.clone()).unwrap_or_default()
        }

        /// 某个 Action 被调用了多少次。
        ///
        /// 同时匹配两种请求体形态：
        /// * 表单：`Action=XXX&...`
        /// * JSON：`{"Action":"XXX",...}`
        ///
        /// 只匹配一种会让「走 JSON 通道的接口」在测试里永远显示「没被调用」，
        /// 从而漏测最关键的那几条（监控、退费、带宽、端口）。
        fn action_count(&self, action: &str) -> usize {
            let form = format!("Action={action}");
            let json = format!("\"Action\":\"{action}\"");
            self.calls()
                .iter()
                .filter(|c| c.contains(&form) || c.contains(&json))
                .count()
        }

        /// 是否调用过某 Action。
        fn called(&self, action: &str) -> bool {
            self.action_count(action) > 0
        }
    }

    /// 从请求体里提取 `Action`，兼容表单与 JSON 两种形态。
    fn extract_action(body: &str) -> String {
        let trimmed = body.trim_start();
        if trimmed.starts_with('{') {
            // JSON：交给 serde 解析，避免手写字符串匹配漏掉转义
            return serde_json::from_str::<serde_json::Value>(trimmed)
                .ok()
                .and_then(|v| {
                    v.get("Action")
                        .and_then(|a| a.as_str())
                        .map(|s| s.to_string())
                })
                .unwrap_or_default();
        }
        body.split('&')
            .find_map(|kv| kv.strip_prefix("Action="))
            .unwrap_or("")
            .to_string()
    }

    impl Transport for FakeVendor {
        fn execute(
            &self,
            req: &crate::transport::HttpRequest,
        ) -> MnResult<crate::transport::HttpResponse> {
            let body = req.body.clone().unwrap_or_default();
            // 从请求体里取出 Action。
            //
            // 两种形态都要支持：
            // * 表单：`Action=XXX&...`（key 已排序，Action 在首位）
            // * JSON：`{"Action":"XXX",...}`（带数组参数的接口走这条通道）
            //
            // 只解析表单会让「走 JSON 的接口」在测试里报「未注册 Action ``」，
            // 而那个空字符串很难让人想到是「解析方式不匹配」。
            let action = extract_action(&body);
            self.recorded
                .lock()
                .map_err(|_| MnError::io("vendor lock", &std::io::Error::other("poisoned")))?
                .push(body);
            let resp =
                self.routes
                    .get(action.as_str())
                    .cloned()
                    .ok_or_else(|| MnError::Provider {
                        provider: "fake".to_string(),
                        code: "unrouted".to_string(),
                        message: format!("测试未注册 Action `{action}` 的响应"),
                    })?;
            Ok(crate::transport::HttpResponse::new(200, resp))
        }
    }

    #[test]
    fn list_specs_aggregates_types_capacity_and_price() {
        let mock = MockTransport::sequence(vec![
            TYPES.to_string(),
            CAPACITY_OK.to_string(),
            PRICE.to_string(),
        ]);
        // 默认 max_gpu_count = 1（个人场景单卡），故只应得到 1 卡档
        let d = driver(mock);
        let specs = d.list_available_specs().unwrap();
        assert_eq!(specs.len(), 1, "{specs:?}");
        assert_eq!(specs[0].spec_id, "4090/1x16c/64g");
        assert_eq!(specs[0].price.per_hour_cny, 2.5);
        assert_eq!(specs[0].stock, Some(1));
        assert_eq!(specs[0].spec.gpu.model, "RTX 4090");
        // A100 售罄，不应出现
        assert!(specs.iter().all(|s| !s.spec.gpu.model.contains("A100")));
    }

    #[test]
    fn list_specs_includes_multi_gpu_when_allowed() {
        let mock = MockTransport::sequence(vec![
            TYPES.to_string(),
            CAPACITY_OK.to_string(),
            PRICE.to_string(),
            PRICE.to_string(),
        ]);
        let mut c = cfg();
        c.max_gpu_count = 2;
        c.max_price_queries = 4;
        let d = CompshareDriver::new("compshare", c, Credential::new("pk", "sk").unwrap(), mock)
            .unwrap();
        let specs = d.list_available_specs().unwrap();
        assert_eq!(specs.len(), 2, "{specs:?}");
        assert!(specs.iter().any(|s| s.spec_id == "4090/1x16c/64g"));
        assert!(specs.iter().any(|s| s.spec_id == "4090/2x32c/128g"));
    }

    #[test]
    fn list_specs_respects_price_query_budget() {
        let mock = MockTransport::sequence(vec![
            TYPES.to_string(),
            CAPACITY_OK.to_string(),
            PRICE.to_string(),
        ]);
        let mut c = cfg();
        c.max_gpu_count = 2;
        // 只允许查 1 次价格：第二档必须是「价格未知」
        c.max_price_queries = 1;
        let d = CompshareDriver::new("compshare", c, Credential::new("pk", "sk").unwrap(), mock)
            .unwrap();
        let specs = d.list_available_specs().unwrap();
        assert_eq!(specs.len(), 2);
        let unpriced = specs
            .iter()
            .filter(|s| s.price.per_hour_cny == f64::MAX)
            .count();
        assert_eq!(unpriced, 1, "超预算的规格应标记为价格未知：{specs:?}");
    }

    #[test]
    fn list_specs_skips_sold_out_and_unlisted_gpu() {
        let mock = MockTransport::sequence(vec![
            TYPES.to_string(),
            CAPACITY_OK.to_string(),
            PRICE.to_string(),
        ]);
        let mut c = cfg();
        c.gpu_types = vec!["4090".to_string()];
        let d = CompshareDriver::new("compshare", c, Credential::new("pk", "sk").unwrap(), mock)
            .unwrap();
        let specs = d.list_available_specs().unwrap();
        assert!(specs.iter().all(|s| s.spec.gpu.model.contains("4090")));
    }

    #[test]
    fn list_specs_survives_capacity_query_failure() {
        // 库存接口失败 → 仍返回规格（stock 未知），不阻断列举
        let mock = MockTransport::sequence(vec![
            TYPES.to_string(),
            r#"{"RetCode":8433,"Message":"service error"}"#.to_string(),
            PRICE.to_string(),
        ]);
        let d = driver(mock);
        let specs = d.list_available_specs().unwrap();
        assert!(!specs.is_empty());
        assert!(specs.iter().all(|s| s.stock.is_none()));
    }

    #[test]
    fn create_checks_capacity_first() {
        let vendor = FakeVendor::new(vec![
            ("CheckCompShareResourceCapacity", CAPACITY_OK.to_string()),
            (
                "CreateCompShareInstance",
                r#"{"RetCode":0,"UHostIds":["uhost-new"]}"#.to_string(),
            ),
            // 补查只应返回刚创建的实例
            (
                "DescribeCompShareInstance",
                instance_json("uhost-new", "Install"),
            ),
        ]);
        let d = driver(vendor);
        let inst = d.create("4090/1x16c/64g", Some("11435")).unwrap();
        assert_eq!(inst.id, "uhost-new");
        assert_eq!(inst.provider, "compshare");
        // Install → Creating：镜像还在部署，不可服务
        assert_eq!(inst.state, InstanceState::Creating);

        // 三个必要动作各调用一次
        assert_eq!(
            d.client()
                .transport()
                .action_count("DescribeCompShareSupportZone"),
            1
        );
        assert_eq!(
            d.client()
                .transport()
                .action_count("CheckCompShareResourceCapacity"),
            1
        );
        assert_eq!(
            d.client()
                .transport()
                .action_count("CreateCompShareInstance"),
            1
        );
        assert_eq!(
            d.client()
                .transport()
                .action_count("DescribeCompShareInstance"),
            1
        );

        // 顺序约束：库存检查必须早于创建（厂商要求，且避免无谓下单）
        let calls = d.client().transport().calls();
        let cap_pos = calls
            .iter()
            .position(|c| c.contains("Action=CheckCompShareResourceCapacity"))
            .unwrap();
        let create_pos = calls
            .iter()
            .position(|c| c.contains("Action=CreateCompShareInstance"))
            .unwrap();
        assert!(cap_pos < create_pos, "库存检查必须早于创建：{calls:?}");
    }

    #[test]
    fn create_rejects_incompatible_zone_before_creating() {
        // 可用区不支持系统镜像 → 创建前就拒绝，不发创建请求
        let vendor = FakeVendor::new(vec![(
            "DescribeCompShareSupportZone",
            r#"{"RetCode":0,"ZoneInfo":[{
                "Region":"cn-wlcb","Zone":"cn-wlcb-01","Describe":"华北二A",
                "IsPod":true,"UnsupportedImageTypes":["System","Other"]
            }]}"#
                .to_string(),
        )]);
        let d = driver(vendor);
        let e = d.create("4090/1x16c/64g", None).unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("不支持系统镜像"), "{msg}");
        // 关键：绝不能发创建请求（避免走到厂商才被 226603 拒绝）
        assert!(
            !d.client().transport().called("CreateCompShareInstance"),
            "镜像不兼容时不应发起创建"
        );
    }

    #[test]
    fn create_proceeds_when_zone_unknown() {
        // 可用区列表查不到当前可用区 → 不阻断，交由真实校验兜底
        let vendor = FakeVendor::new(vec![
            (
                "DescribeCompShareSupportZone",
                r#"{"RetCode":0,"ZoneInfo":[]}"#.to_string(),
            ),
            ("CheckCompShareResourceCapacity", CAPACITY_OK.to_string()),
            (
                "CreateCompShareInstance",
                r#"{"RetCode":0,"UHostIds":["uhost-x"]}"#.to_string(),
            ),
            (
                "DescribeCompShareInstance",
                instance_json("uhost-x", "Install"),
            ),
        ]);
        let d = driver(vendor);
        assert!(d.create("4090/1x16c/64g", None).is_ok());
        assert!(d.client().transport().called("CreateCompShareInstance"));
    }

    #[test]
    fn create_fails_fast_without_capacity() {
        let vendor = FakeVendor::new(vec![(
            "CheckCompShareResourceCapacity",
            r#"{"RetCode":0,"Specs":[{"Gpu":1,"Cpu":16,"Mem":64,"ResourceEnough":false}]}"#
                .to_string(),
        )]);
        let d = driver(vendor);
        let e = d.create("4090/1x16c/64g", None).unwrap_err();
        assert!(crate::client::is_capacity_shortage(&e), "{e}");
        // 无库存时绝不能发创建请求（否则会被厂商计费）
        assert!(
            !d.client().transport().called("CreateCompShareInstance"),
            "无库存时不应发起创建"
        );
    }

    #[test]
    fn create_keeps_id_even_when_describe_fails() {
        let vendor = FakeVendor::new(vec![
            ("CheckCompShareResourceCapacity", CAPACITY_OK.to_string()),
            (
                "CreateCompShareInstance",
                r#"{"RetCode":0,"UHostIds":["uhost-orphan"]}"#.to_string(),
            ),
            (
                "DescribeCompShareInstance",
                r#"{"RetCode":8433,"Message":"service error"}"#.to_string(),
            ),
        ]);
        let d = driver(vendor);
        let inst = d.create("4090/1x16c/64g", Some("11435")).unwrap();
        // 关键：ID 必须保留，编排器才能回滚
        assert_eq!(inst.id, "uhost-orphan");
        assert_eq!(inst.state, InstanceState::Creating);
        assert_eq!(inst.spec.gpu.model, "RTX 4090");
        assert_eq!(inst.spec.memory_bytes, 64 << 30);
    }

    #[test]
    fn create_rejects_gpu_outside_allowlist() {
        let mut c = cfg();
        c.gpu_types = vec!["5090".to_string()];
        let d = CompshareDriver::new(
            "compshare",
            c,
            Credential::new("pk", "sk").unwrap(),
            MockTransport::always(200, CAPACITY_OK),
        )
        .unwrap();
        let e = d.create("4090/1x16c/64g", None).unwrap_err();
        assert!(e.to_string().contains("不在配置允许列表"), "{e}");
    }

    #[test]
    fn create_rejects_bad_spec_id() {
        let d = driver(MockTransport::always(200, CAPACITY_OK));
        assert!(d.create("garbage", None).is_err());
    }

    #[test]
    fn start_is_idempotent_when_running() {
        let mock = MockTransport::always(200, INSTANCE_RUNNING);
        let d = driver(mock);
        let inst = d.start("uhost-1").unwrap();
        assert_eq!(inst.state, InstanceState::Running);
        // 已在运行 → 不应发 Start 请求
        assert_eq!(d.client().transport().call_count(), 1);
    }

    #[test]
    fn stop_is_idempotent_when_stopped() {
        let stopped = INSTANCE_RUNNING.replace("Running", "Stopped");
        let mock = MockTransport::always(200, stopped);
        let d = driver(mock);
        let inst = d.stop("uhost-1").unwrap();
        assert_eq!(inst.state, InstanceState::Stopped);
        assert_eq!(d.client().transport().call_count(), 1);
    }

    #[test]
    fn destroy_stops_before_terminate() {
        // describe(Running) → stop → describe(Stopped) → terminate
        let mock = MockTransport::sequence(vec![
            INSTANCE_RUNNING.to_string(),
            r#"{"RetCode":0,"UHostId":"uhost-1"}"#.to_string(),
            INSTANCE_RUNNING.replace("Running", "Stopped"),
            r#"{"RetCode":0,"UHostId":"uhost-1","InRecycle":"Yes"}"#.to_string(),
        ]);
        let d = driver(mock);
        d.destroy("uhost-1").unwrap();

        let bodies: Vec<String> = d
            .client()
            .transport()
            .requests()
            .iter()
            .filter_map(|r| r.body.clone())
            .collect();
        assert_eq!(bodies.len(), 4, "{bodies:?}");
        assert!(bodies[0].contains("DescribeCompShareInstance"));
        assert!(bodies[1].contains("StopCompShareInstance"), "{bodies:?}");
        assert!(bodies[2].contains("DescribeCompShareInstance"));
        // 只有确认 Stopped 之后才发 Terminate
        assert!(
            bodies[3].contains("TerminateCompShareInstance"),
            "{bodies:?}"
        );
        assert!(bodies[3].contains("ReleaseUDisk=true"), "{bodies:?}");
    }

    #[test]
    fn destroy_terminates_directly_when_already_stopped() {
        let mock = MockTransport::sequence(vec![
            INSTANCE_RUNNING.replace("Running", "Stopped"),
            r#"{"RetCode":0,"UHostId":"uhost-1"}"#.to_string(),
        ]);
        let d = driver(mock);
        d.destroy("uhost-1").unwrap();
        let bodies: Vec<String> = d
            .client()
            .transport()
            .requests()
            .iter()
            .filter_map(|r| r.body.clone())
            .collect();
        assert_eq!(bodies.len(), 2, "{bodies:?}");
        assert!(bodies[1].contains("TerminateCompShareInstance"));
        assert!(!bodies.iter().any(|b| b.contains("StopCompShareInstance")));
    }

    #[test]
    fn destroy_is_idempotent_when_instance_missing() {
        // describe 找不到 + terminate 也报不存在 → 视为已销毁
        let mock = MockTransport::sequence(vec![
            r#"{"RetCode":0,"UHostSet":[]}"#.to_string(),
            r#"{"RetCode":1,"Message":"instance not found"}"#.to_string(),
        ]);
        let d = driver(mock);
        assert!(d.destroy("uhost-gone").is_ok());
    }

    #[test]
    fn describe_reports_missing_instance() {
        let d = driver(MockTransport::always(200, r#"{"RetCode":0,"UHostSet":[]}"#));
        let e = d.describe("uhost-x").unwrap_err();
        assert!(e.to_string().contains("不存在"), "{e}");
    }

    #[test]
    fn list_paginates_until_total_reached() {
        // 第 1 页满页（100 条），TotalCount 声明还有更多 → 必须继续翻页
        let page1 = format!(
            r#"{{"RetCode":0,"TotalCount":101,"UHostSet":[{}]}}"#,
            (0..PAGE_LIMIT)
                .map(|i| format!(r#"{{"UHostId":"u-{i}","State":"Running"}}"#))
                .collect::<Vec<_>>()
                .join(",")
        );
        let page2 =
            r#"{"RetCode":0,"TotalCount":101,"UHostSet":[{"UHostId":"last","State":"Stopped"}]}"#;
        let mock = MockTransport::sequence(vec![page1, page2.to_string()]);
        let d = driver(mock);
        let all = d.list().unwrap();
        assert_eq!(all.len(), PAGE_LIMIT as usize + 1);
        assert_eq!(all.last().map(|i| i.id.as_str()), Some("last"));
    }

    #[test]
    fn list_stops_when_first_page_is_partial() {
        // 未满页 → 不应再发第二页请求（减少接口调用）
        let page =
            r#"{"RetCode":0,"TotalCount":1,"UHostSet":[{"UHostId":"only","State":"Running"}]}"#;
        let d = driver(MockTransport::always(200, page));
        let all = d.list().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(d.client().transport().call_count(), 1, "不应多翻一页");
    }

    #[test]
    fn capabilities_advertise_documented_gpu_models() {
        let d = driver(MockTransport::always(200, "{}"));
        let c = d.capabilities();
        assert!(c.on_demand_create);
        assert!(c.stop_start);
        assert!(c.price_query);
        assert!(c.gpu_models.contains(&"RTX 4090".to_string()));
        assert!(c.gpu_models.contains(&"A100".to_string()));
        assert_eq!(d.display_name(), "优云智算（Compshare）");
        assert_eq!(d.name(), "compshare");
    }

    #[test]
    fn provision_ready_rolls_back_on_timeout() {
        // create 成功但实例一直 Stopped（永不 Running）→ 超时 → 自动销毁
        let vendor = FakeVendor::new(vec![
            ("CheckCompShareResourceCapacity", CAPACITY_OK.to_string()),
            (
                "CreateCompShareInstance",
                r#"{"RetCode":0,"UHostIds":["uhost-1"]}"#.to_string(),
            ),
            // 轮询与销毁流程里都会 describe，始终返回 Stopped
            (
                "DescribeCompShareInstance",
                instance_json("uhost-1", "Stopped"),
            ),
            (
                "StopCompShareInstance",
                r#"{"RetCode":0,"UHostId":"uhost-1"}"#.to_string(),
            ),
            (
                "TerminateCompShareInstance",
                r#"{"RetCode":0,"UHostId":"uhost-1","InRecycle":"Yes"}"#.to_string(),
            ),
        ]);
        let d = driver(vendor);
        let e = d.provision_ready("4090/1x16c/64g", "11435").unwrap_err();
        let s = e.to_string();
        assert!(s.contains("未进入") || s.contains("自动删除"), "{s}");

        // 必须出现 Terminate（否则实例会持续计费）
        assert!(
            d.client().transport().called("TerminateCompShareInstance"),
            "回滚必须删除实例：{:?}",
            d.client().transport().calls()
        );
    }

    #[test]
    fn wait_until_returns_when_target_reached() {
        let mock = MockTransport::sequence(vec![
            INSTANCE_RUNNING.replace("Running", "Starting"),
            INSTANCE_RUNNING.to_string(),
        ]);
        let d = driver(mock);
        let inst = d.wait_until("uhost-1", &[InstanceState::Running]).unwrap();
        assert_eq!(inst.state, InstanceState::Running);
        assert_eq!(d.client().transport().call_count(), 2);
    }

    #[test]
    fn wait_until_errors_when_destroyed() {
        let mock = MockTransport::always(200, INSTANCE_RUNNING.replace("Running", "Unknown"));
        let d = driver(mock);
        // Unknown → Failed，不在目标集内，但也不是 Destroyed → 会超时
        let e = d
            .wait_until("uhost-1", &[InstanceState::Running])
            .unwrap_err();
        assert!(e.to_string().contains("未进入"), "{e}");
    }

    #[test]
    fn invalid_config_rejected_at_construction() {
        let bad = CompshareConfig {
            image_id: String::new(),
            ..Default::default()
        };
        assert!(CompshareDriver::new(
            "compshare",
            bad,
            Credential::new("pk", "sk").unwrap(),
            MockTransport::always(200, "{}")
        )
        .is_err());
    }

    #[test]
    fn not_found_detection() {
        let e = MnError::Provider {
            provider: "compshare".into(),
            code: "instance_not_found".into(),
            message: "x".into(),
        };
        assert!(is_not_found(&e));
        let e2 = MnError::Provider {
            provider: "compshare".into(),
            code: "1".into(),
            message: "实例 u-1 不存在".into(),
        };
        assert!(is_not_found(&e2));
        assert!(!is_not_found(&MnError::bad_request("x")));
    }
    // ============================================================
    // 实例配置管理接口（改名 / 密码 / 重装 / 升降配 / 计费 / 端口 / 定时关机）
    // ============================================================

    const OK_ACTION: &str = r#"{"RetCode":0,"UHostId":"uhost-1"}"#;

    #[test]
    fn rename_sends_name_and_truncates() {
        let v = FakeVendor::new(vec![("ModifyCompShareInstanceName", OK_ACTION.into())]);
        let d = driver(v);
        d.rename("uhost-1", "my-gpu").unwrap();
        let body = d.client().transport().calls().remove(0);
        assert!(
            body.contains("Action=ModifyCompShareInstanceName"),
            "{body}"
        );
        assert!(body.contains("UHostId=uhost-1"), "{body}");
        assert!(body.contains("Name=my-gpu"), "{body}");
    }

    /// 密码必须 Base64 编码后发送（厂商要求），且不能泄漏明文。
    #[test]
    fn reset_password_base64_encodes() {
        let v = FakeVendor::new(vec![
            (
                "DescribeCompShareInstance",
                instance_json("uhost-1", "Stopped"),
            ),
            ("ResetCompShareInstancePassword", OK_ACTION.into()),
        ]);
        let d = driver(v);
        d.reset_password("uhost-1", "Abc123!").unwrap();
        let calls = d.client().transport().calls();
        let body = calls
            .iter()
            .find(|c| c.contains("ResetCompShareInstancePassword"))
            .expect("应调用重置密码接口");
        // "Abc123!" 的 Base64 是 "QWJjMTIzIQ=="
        assert!(
            body.contains("Password=QWJjMTIzIQ%3D%3D") || body.contains("Password=QWJjMTIzIQ=="),
            "密码必须是 Base64：{body}"
        );
        // 明文绝不能出现在请求里
        assert!(!body.contains("Abc123"), "明文密码不得出现在请求体：{body}");
    }

    /// 虚机重置密码必须先关机；容器实例允许运行中。
    #[test]
    fn reset_password_requires_stopped_for_vm_only() {
        // 虚机 + Running → 拒绝，且不发出重置请求
        let v = FakeVendor::new(vec![(
            "DescribeCompShareInstance",
            instance_json("uhost-1", "Running"),
        )]);
        let d = driver(v);
        let e = d.reset_password("uhost-1", "Abc123!").unwrap_err();
        assert!(e.to_string().contains("必须先关机"), "{e}");
        assert!(
            !d.client()
                .transport()
                .called("ResetCompShareInstancePassword"),
            "状态不符时绝不能发出重置请求"
        );

        // 容器 + Running → 允许
        //
        // `instance_json` 默认不含 `InstanceType`（虚机场景），
        // 这里显式注入 `Container` 以验证「容器可运行中重置」这条分支。
        let container = instance_json("uhost-1", "Running").replace(
            r#""GpuType":"4090","#,
            r#""GpuType":"4090","InstanceType":"Container","#,
        );
        assert!(
            container.contains(r#""InstanceType":"Container""#),
            "测试数据构造失败：{}",
            container
        );
        let v2 = FakeVendor::new(vec![
            ("DescribeCompShareInstance", container),
            ("ResetCompShareInstancePassword", OK_ACTION.into()),
        ]);
        let d2 = driver(v2);
        d2.reset_password("uhost-1", "Abc123!").unwrap();
        assert!(d2
            .client()
            .transport()
            .called("ResetCompShareInstancePassword"));
    }

    #[test]
    fn reinstall_and_resize_require_stopped() {
        let v = FakeVendor::new(vec![(
            "DescribeCompShareInstance",
            instance_json("uhost-1", "Running"),
        )]);
        let d = driver(v);

        for (label, r) in [
            ("重装", d.reinstall("uhost-1", "img-2", None)),
            ("升降配", d.resize_compute("uhost-1", 32, 128, 2)),
            ("磁盘扩容", d.resize_disk("uhost-1", "udisk-1", 500)),
        ] {
            let e = r.expect_err(&format!("{label}在 Running 时必须拒绝"));
            assert!(e.to_string().contains("要求实例已关机"), "{label}: {e}");
        }
        // 三个动作都不得真的发出去
        assert!(!d.client().transport().called("ReinstallCompShareInstance"));
        assert!(!d.client().transport().called("ResizeCompShareInstance"));
    }

    /// 升降配的内存单位必须是 MB（GB × 1024），错用会差 1024 倍。
    #[test]
    fn resize_sends_memory_in_mb() {
        let v = FakeVendor::new(vec![
            (
                "DescribeCompShareInstance",
                instance_json("uhost-1", "Stopped"),
            ),
            ("ResizeCompShareInstance", OK_ACTION.into()),
        ]);
        let d = driver(v);
        d.resize_compute("uhost-1", 32, 128, 2).unwrap();
        let body = d
            .client()
            .transport()
            .calls()
            .into_iter()
            .find(|c| c.contains("ResizeCompShareInstance"))
            .unwrap();
        // 128 GB → 131072 MB
        assert!(body.contains("Memory=131072"), "内存必须换算成 MB：{body}");
        assert!(body.contains("Cpu=32"), "{body}");
        assert!(body.contains("Gpu=2"), "{body}");
    }

    /// 磁盘扩容与计算升降配是**二选一**：不能同时带 CPU 与 DiskId。
    #[test]
    fn disk_resize_does_not_send_compute_params() {
        let v = FakeVendor::new(vec![
            (
                "DescribeCompShareInstance",
                instance_json("uhost-1", "Stopped"),
            ),
            ("ResizeCompShareInstance", OK_ACTION.into()),
        ]);
        let d = driver(v);
        d.resize_disk("uhost-1", "udisk-1", 500).unwrap();
        let body = d
            .client()
            .transport()
            .calls()
            .into_iter()
            .find(|c| c.contains("ResizeCompShareInstance"))
            .unwrap();
        assert!(body.contains("DiskId=udisk-1"), "{body}");
        assert!(body.contains("DiskSpace=500"), "{body}");
        // 厂商限制：同时传计算参数会被拒
        assert!(!body.contains("Cpu="), "磁盘扩容不得带 CPU：{body}");
        assert!(!body.contains("Memory="), "磁盘扩容不得带 Memory：{body}");
    }

    #[test]
    fn upgrade_price_is_yuan_not_hourly() {
        let v = FakeVendor::new(vec![(
            "GetCompShareInstanceUpgradePrice",
            r#"{"RetCode":0,"Price":356.0,"OriginalPrice":400.0}"#.into(),
        )]);
        let d = driver(v);
        let p = d
            .upgrade_price("uhost-1", Some(32), Some(128), None)
            .unwrap();
        assert!((p.price - 356.0).abs() < 1e-9);
        assert!(!p.is_refund(), "正数是补差价");
        let body = d.client().transport().calls().remove(0);
        // 内存仍是 MB
        assert!(body.contains("Memory=131072"), "{body}");
        // 未传的 GPU 不应出现
        assert!(!body.contains("GPU="), "未指定的参数不得发送：{body}");
    }

    #[test]
    fn refund_price_uses_json_array_and_sums_refundable_only() {
        let v = FakeVendor::new(vec![(
            "GetCompShareRefundPrice",
            r#"{"RetCode":0,"RefundPriceSet":[
                {"UHostId":"u-1","Code":0,"RefundPrice":235.5},
                {"UHostId":"u-2","Code":7,"Message":"不可退","RefundPrice":99.0}
            ]}"#
            .into(),
        )]);
        let d = driver(v);
        let r = d
            .refund_price(&["u-1".to_string(), "u-2".to_string()])
            .unwrap();
        assert_eq!(r.items.len(), 2);
        // 只累加「可退费」的：99 元那条 Code≠0，不能算进去
        assert!((r.total_refundable() - 235.5).abs() < 1e-9);
        assert!(r.find("u-2").map(|i| !i.is_refundable()).unwrap_or(false));

        let body = d.client().transport().calls().remove(0);
        // 必须 JSON 数组，不能是扁平索引键
        assert!(body.contains(r#""UHostIds":["u-1","u-2"]"#), "{body}");
        assert!(!body.contains("UHostIds.0"), "{body}");
    }

    /// 定时关机：先查 ProjectId 并缓存，第二次不再重复查。
    #[test]
    fn schedule_stop_fetches_and_caches_project_id() {
        let v = FakeVendor::new(vec![
            (
                "GetProjectList",
                r#"{"RetCode":0,"ProjectSet":[
                    {"ProjectId":"org-a","ProjectName":"A","IsDefault":false},
                    {"ProjectId":"org-b","ProjectName":"B","IsDefault":true}
                ]}"#
                .into(),
            ),
            ("UpdateCompShareStopScheduler", OK_ACTION.into()),
        ]);
        let d = driver(v);

        let at = d.schedule_stop("uhost-1", 3600).unwrap();
        assert!(at > 0);
        // 第二次：不应再查项目
        d.schedule_stop("uhost-1", 3600).unwrap();
        assert_eq!(
            d.client().transport().action_count("GetProjectList"),
            1,
            "项目 ID 必须缓存，不能每次都查"
        );

        let body = d
            .client()
            .transport()
            .calls()
            .into_iter()
            .find(|c| c.contains("UpdateCompShareStopScheduler"))
            .unwrap();
        // 必须用标了 IsDefault 的那个
        assert!(body.contains("ProjectId=org-b"), "{body}");
        assert!(body.contains("SchedulerStopTime="), "{body}");
    }

    #[test]
    fn schedule_stop_rejects_too_soon() {
        let v = FakeVendor::new(vec![]);
        let d = driver(v);
        // 厂商要求 ≥ now + 300 秒
        let e = d.schedule_stop("uhost-1", 60).unwrap_err();
        assert!(e.to_string().contains("300"), "{e}");
        // 提前拦下，不该发出任何请求
        assert_eq!(d.client().transport().calls().len(), 0);
    }

    /// 端口管理只适用于容器实例。
    #[test]
    fn update_ports_rejects_vm_instances() {
        let v = FakeVendor::new(vec![(
            "DescribeCompShareInstance",
            instance_json("uhost-1", "Running"),
        )]);
        let d = driver(v);
        let e = d.update_ports("uhost-1", &[8888], &[23]).unwrap_err();
        assert!(e.to_string().contains("仅适用于容器"), "{e}");
        assert!(!d
            .client()
            .transport()
            .called("UpdateCompShareInstancePorts"));
    }

    /// 带宽切换：顶层 RetCode=0 但单个实例失败，必须能被识别。
    #[test]
    fn bandwidth_switch_reports_partial_failure() {
        let v = FakeVendor::new(vec![(
            "SwitchCompShareEIPShareBandwidth",
            r#"{"RetCode":0,"request_uuid":"r-1","Results":[
                {"UHostId":"u-1","Success":true,"CurrentScope":"Company"},
                {"UHostId":"u-2","Success":false,"Message":"no eip"}
            ]}"#
            .into(),
        )]);
        let d = driver(v);
        let r = d
            .switch_bandwidth(&["u-1".to_string(), "u-2".to_string()], "Company")
            .unwrap();
        assert_eq!(r.ret_code, 0, "顶层是成功的");
        assert!(!r.all_succeeded(), "但有实例失败，不能算全成功");
        let fails = r.failures();
        assert_eq!(fails.len(), 1);
        assert_eq!(fails[0].0, "u-2");
        assert!(fails[0].1.contains("no eip"));
    }

    #[test]
    fn software_ports_and_url() {
        let v = FakeVendor::new(vec![
            (
                "DescribeCompShareSoftwarePort",
                r#"{"RetCode":0,"SoftwarePort":[
                    {"Software":"JupyterLab","Port":8888},
                    {"Software":"SSH","Port":22}
                ]}"#
                .into(),
            ),
            (
                "GetSoftwareURL",
                r#"{"RetCode":0,"URL":"http://1.2.3.4:8888"}"#.into(),
            ),
        ]);
        let d = driver(v);
        let ports = d.software_ports().unwrap();
        assert_eq!(ports.port_of("ssh"), Some(22), "软件名不区分大小写");
        let url = d.software_url("uhost-1", "JupyterLab").unwrap();
        assert_eq!(url, "http://1.2.3.4:8888");
    }

    #[test]
    fn net_optimizer_per_region() {
        let v = FakeVendor::new(vec![(
            "CheckCompShareNetOptimizer",
            r#"{"RetCode":0,"Optimized":true,"Info":[
                {"Region":"cn-wlcb","Optimized":true},
                {"Region":"cn-sh2","Optimized":false}
            ]}"#
            .into(),
        )]);
        let d = driver(v);
        let r = d.net_optimizer().unwrap();
        assert!(r.optimized_in("cn-wlcb"));
        assert!(!r.optimized_in("cn-sh2"));
        // 未知地域回退到全局值
        assert!(r.optimized_in("cn-unknown"));
    }
}
