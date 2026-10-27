//! 优云智算（Compshare）API 客户端。
//!
//! 归属：**算力执行层 · 云端后端管理**（厂商 API 调用层）。
//!
//! ## 职责边界
//!
//! 本模块只做三件事：**拼参数、签名、把响应解析成结构体**。
//! 它不决定「该不该创建实例」「失败要不要重试」——那是上层
//! （[`crate::compshare::driver`] 与网关的调度决策器）的事。
//!
//! 这样拆分的好处：协议层可以对着文档逐条核对，业务策略可以独立演进。
//!
//! ## 统一约定
//!
//! * 所有接口都是 `POST https://api.compshare.cn` + 表单编码，用 `Action` 区分；
//! * 业务成功判定看响应体里的 `RetCode`（`0` 为成功），**不是** HTTP 状态码；
//! * `Region` / `Zone` 由客户端配置注入（本 crate 一次只服务一个可用区）；
//! * 错误一律归一化为 [`MnError::Provider`]，并保留厂商原始返回码。
//!
//! ## 参考
//!
//! 接口语义见优云智算官方文档 GPU API 章节：
//! `https://www.compshare.cn/docs/gpus/instance/createcompshareinstance`

use mangosteen_core::{MnError, MnResult};
use serde::de::DeserializeOwned;

use crate::compshare::model::{
    ActionResponse, AvailableTypesResponse, BandwidthSwitchResponse, CapacityResponse,
    CreateInstanceResponse, DescribeInstancesResponse, ImagesResponse, InstancePortsResponse,
    MachineTypeFamiliesResponse, MonitorResponse, NetOptimizerResponse, PriceResponse,
    ProjectListResponse, RefundPriceResponse, SoftwarePortResponse, SoftwareUrlResponse,
    UpgradePriceResponse, UserPriceRequest, ZoneInfo, ZonesResponse,
};
use crate::compshare::params::Params;
use crate::compshare::retcode::{provider_error, ApiFailure};
use crate::compshare::sign::Credential;
use crate::compshare::transport::{HttpRequest, HttpResponse, Transport};

/// 厂商默认 API 入口。
pub const DEFAULT_ENDPOINT: &str = "https://api.compshare.cn";

/// 错误信息里使用的服务商标识。
pub const PROVIDER_NAME: &str = "compshare";

/// 参数作用域：决定自动注入哪些定位参数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// 不注入（仅查询类接口，如可用区列表）
    None,
    /// 只注入 `Region`
    Region,
    /// 注入 `Region` 与 `Zone`
    RegionAndZone,
}

/// 创建实例的请求参数。
#[derive(Debug, Clone)]
pub struct CreateRequest {
    /// GPU 型号代号（如 `4090`）
    pub gpu_type: String,
    /// GPU 卡数
    pub gpu: u32,
    /// CPU 核数
    pub cpu: u32,
    /// 内存，**单位 MB**（必须是 1024 的整数倍）
    pub memory_mb: u64,
    /// 镜像 ID
    pub image_id: String,
    /// 机型，固定 `G`
    pub machine_type: String,
    /// CPU 平台（`Auto` / `Intel/Auto` / `Amd/Auto`）
    pub cpu_platform: String,
    /// 计费模式
    pub charge_type: String,
    /// 系统盘类型
    pub disk_type: String,
    /// 系统盘大小（GB）
    pub disk_size_gb: u32,
    /// 实例名（最长 63 字符）
    pub name: Option<String>,
    /// 备注（可用于标记部署归属）
    pub remark: Option<String>,
    /// 购买时长（`Month` 为月数，`Day` 为天数）
    pub quantity: Option<u32>,
}

/// 库存检查请求。
#[derive(Debug, Clone)]
pub struct CapacityRequest {
    /// GPU 型号代号
    pub gpu_type: String,
    /// 机型
    pub machine_type: String,
    /// CPU 平台
    pub cpu_platform: String,
    /// 镜像 ID（库存模拟调度会校验镜像与卡型兼容性）
    pub image_id: String,
    /// 计费模式
    pub charge_type: String,
    /// 系统盘类型
    pub disk_type: String,
    /// 系统盘大小（GB）
    pub disk_size_gb: u32,
}

/// 价格查询请求。
#[derive(Debug, Clone)]
pub struct PriceRequest {
    /// GPU 型号代号
    pub gpu_type: String,
    /// GPU 卡数
    pub gpu: u32,
    /// CPU 核数
    pub cpu: u32,
    /// 内存，**单位 MB**
    pub memory_mb: u64,
    /// 计费模式；`None` 表示让厂商返回全部模式
    pub charge_type: Option<String>,
    /// 系统盘类型
    pub disk_type: String,
    /// 系统盘大小（GB）
    pub disk_size_gb: u32,
}

/// Compshare API 客户端。
pub struct CompshareClient<T: Transport> {
    endpoint: String,
    credential: Credential,
    transport: T,
    region: String,
    zone: String,
}

impl<T: Transport> std::fmt::Debug for CompshareClient<T> {
    /// 手写 `Debug`：`T` 未必实现 `Debug`，且不得输出凭证。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompshareClient")
            .field("endpoint", &self.endpoint)
            .field("credential", &self.credential)
            .field("region", &self.region)
            .field("zone", &self.zone)
            .finish()
    }
}

impl<T: Transport> CompshareClient<T> {
    /// 构造。
    ///
    /// `region` / `zone` **允许为空**，语义是「不在请求里注入该参数」。
    /// 这是必要的：`DescribeCompShareInstance` 等接口的 Region/Zone 本就是可选
    /// （不传即查询全部地域），若强制非空就会逼调用方编一个假值，
    /// 而厂商会直接回 `Params [Region] not available`。
    ///
    /// 「部署必须指定地域」这类业务约束由 [`super::CompshareConfig`] 负责，
    /// 协议层不应越界。
    pub fn new(
        endpoint: impl Into<String>,
        credential: Credential,
        transport: T,
        region: impl Into<String>,
        zone: impl Into<String>,
    ) -> MnResult<Self> {
        let endpoint = endpoint.into();
        let endpoint = endpoint.trim().trim_end_matches('/').to_string();
        if endpoint.is_empty() {
            return Err(MnError::config("compshare.endpoint", "API 入口不能为空"));
        }
        if !endpoint.starts_with("https://") {
            // 明文 HTTP 会把签名与公钥暴露在链路上
            return Err(MnError::config(
                "compshare.endpoint",
                "必须使用 https:// 入口（签名与凭证不能被明文传输）",
            ));
        }
        Ok(Self {
            endpoint,
            credential,
            transport,
            region: region.into().trim().to_string(),
            zone: zone.into().trim().to_string(),
        })
    }

    /// 是否限定了地域（空表示查全部地域）。
    pub fn has_region(&self) -> bool {
        !self.region.is_empty()
    }

    /// 是否限定了可用区。
    pub fn has_zone(&self) -> bool {
        !self.zone.is_empty()
    }

    /// 以相同凭证/传输、但换一组 region/zone 构造新客户端。
    ///
    /// 用途：跨可用区批量查询时需要「同一身份访问不同地域」。
    /// 不便用 `Clone`（`T` 未必可克隆），所以由调用方传入传输层引用。
    /// 这里选择要求 `T: Clone` 的替代方案——直接重造，见 [`Self::rebind`]。
    pub fn rebind(&self, region: impl Into<String>, zone: impl Into<String>) -> MnResult<Self>
    where
        T: Clone,
    {
        Self::new(
            self.endpoint.clone(),
            self.credential.clone(),
            self.transport.clone(),
            region,
            zone,
        )
    }

    /// 以相同凭证与**外部**传输引用换 region/zone 构造新客户端（不要求 `T: Clone`）。
    ///
    /// 与 [`Self::rebind`] 的区别：`rebind` 要求 `T: Clone`（`Arc<dyn Transport>`
    /// 之外的 dyn 对象无法满足），而本方法借用外部传输，适合跨可用区临时查询。
    pub fn with_scope<'a, U: Transport>(
        &self,
        transport: &'a U,
        region: impl Into<String>,
        zone: impl Into<String>,
    ) -> MnResult<CompshareClient<&'a U>> {
        CompshareClient::new(
            self.endpoint.clone(),
            self.credential.clone(),
            transport,
            region,
            zone,
        )
    }

    /// 凭证引用（供构造派生客户端）。
    pub fn credential(&self) -> &Credential {
        &self.credential
    }

    /// 接口入口。
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// 当前可用区（`region/zone`），用于展示与日志。
    pub fn scope_label(&self) -> String {
        if self.zone.trim().is_empty() {
            self.region.clone()
        } else {
            format!("{}/{}", self.region, self.zone)
        }
    }

    /// 传输层引用（测试用）。
    pub fn transport(&self) -> &T {
        &self.transport
    }

    // ------------------------------------------------------------ 底层调用

    /// 发起一次 Action 调用，返回原始 JSON。
    pub fn call(
        &self,
        action: &str,
        mut params: Params,
        scope: Scope,
    ) -> MnResult<serde_json::Value> {
        params.set("Action", action);
        match scope {
            Scope::None => {}
            Scope::Region => {
                if params.get("Region").is_none() && !self.region.trim().is_empty() {
                    params.set("Region", self.region.clone());
                }
            }
            Scope::RegionAndZone => {
                if params.get("Region").is_none() && !self.region.trim().is_empty() {
                    params.set("Region", self.region.clone());
                }
                if params.get("Zone").is_none() && !self.zone.trim().is_empty() {
                    params.set("Zone", self.zone.clone());
                }
            }
        }

        // 公钥必须作为参数一起发送：UCloud 要求服务端能取到 PublicKey 来复算签名。
        // `Credential::sign` 只在内部副本里补 PublicKey 用于算签名，
        // 这里必须显式写进真正要发送的参数集，否则鉴权必然失败。
        if params.get("PublicKey").is_none() {
            params.set("PublicKey", self.credential.public_key().to_string());
        }

        // 签名：作用于扁平化后的原始值（不含 Signature 自身）
        let signature = self.credential.sign(params.as_map());
        params.set("Signature", signature);

        let req = HttpRequest::form_post(&self.endpoint, params.to_form_body()).with_header(
            "U-Timestamp-Ms",
            mangosteen_core::time::now_unix_millis().to_string(),
        );

        let resp = self.transport.execute(&req)?;
        self.interpret(action, resp)
    }

    /// 以 **JSON 请求体**发起一次 Action 调用，返回原始 JSON。
    ///
    /// ## 为什么需要单独的 JSON 通道
    ///
    /// 优云智算绝大多数接口用表单编码，数组靠扁平索引键
    /// （`UHostIds.0=...`）。但 `GetCompShareInstanceMonitor` 不接受这种形式，
    /// 实测四种表单写法全部被拒：
    ///
    /// | 表单写法 | 厂商返回 |
    /// |----------|----------|
    /// | `UHostIds.0=uhost-x` | `210 Missing params [UHostIds]` |
    /// | `UHostIds=["uhost-x"]` | `230 Params [...UHostIds] not available` |
    /// | `UHostId=uhost-x` | `210 Missing params [UHostIds]` |
    /// | 不传 | `210 Missing params [UHostIds]` |
    ///
    /// 只有 `application/json` + 真正的 JSON 数组能成功，且签名时数组
    /// 按「元素直接拼接」计算（见 [`crate::compshare::sign::signature_of_json`]）。
    pub fn call_json(
        &self,
        action: &str,
        mut fields: serde_json::Map<String, serde_json::Value>,
        scope: Scope,
    ) -> MnResult<serde_json::Value> {
        fields.insert(
            "Action".to_string(),
            serde_json::Value::String(action.to_string()),
        );
        match scope {
            Scope::None => {}
            Scope::Region => {
                if !self.region.trim().is_empty() {
                    fields
                        .entry("Region".to_string())
                        .or_insert_with(|| serde_json::Value::String(self.region.clone()));
                }
            }
            Scope::RegionAndZone => {
                if !self.region.trim().is_empty() {
                    fields
                        .entry("Region".to_string())
                        .or_insert_with(|| serde_json::Value::String(self.region.clone()));
                }
                if !self.zone.trim().is_empty() {
                    fields
                        .entry("Zone".to_string())
                        .or_insert_with(|| serde_json::Value::String(self.zone.clone()));
                }
            }
        }
        fields
            .entry("PublicKey".to_string())
            .or_insert_with(|| serde_json::Value::String(self.credential.public_key().to_string()));

        let signature =
            crate::compshare::sign::signature_of_json(self.credential.private_key(), &fields);
        fields.insert(
            "Signature".to_string(),
            serde_json::Value::String(signature),
        );

        let body = serde_json::Value::Object(fields).to_string();
        let req = HttpRequest::json_post(&self.endpoint, body).with_header(
            "U-Timestamp-Ms",
            mangosteen_core::time::now_unix_millis().to_string(),
        );
        let resp = self.transport.execute(&req)?;
        self.interpret(action, resp)
    }

    /// 解释响应：HTTP 状态 → JSON → RetCode。
    fn interpret(&self, action: &str, resp: HttpResponse) -> MnResult<serde_json::Value> {
        if !resp.is_success() {
            // HTTP 层失败时，厂商仍可能在 body 里给出 Message
            let detail = serde_json::from_str::<serde_json::Value>(&resp.body)
                .ok()
                .and_then(|v| {
                    v.get("Message")
                        .and_then(|m| m.as_str())
                        .map(|s| s.to_string())
                })
                .unwrap_or_else(|| truncate(&resp.body, 200));
            return Err(MnError::Provider {
                provider: PROVIDER_NAME.to_string(),
                code: format!("http_{}", resp.status),
                message: format!("{action} 调用失败（HTTP {}）：{detail}", resp.status),
            });
        }

        let value: serde_json::Value = if resp.body.trim().is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(&resp.body).map_err(|e| MnError::Provider {
                provider: PROVIDER_NAME.to_string(),
                code: "invalid_json".to_string(),
                message: format!(
                    "{action} 返回非 JSON（凭证 {}）：{e}；响应前 120 字符：{}",
                    self.credential.redacted(),
                    truncate(&resp.body, 120)
                ),
            })?
        };

        // 业务层判定：HTTP 200 也可能是失败
        let ret_code = extract_ret_code(&value).unwrap_or(0);
        if ret_code != 0 {
            let message = value
                .get("Message")
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .to_string();
            return Err(provider_error(PROVIDER_NAME, ret_code, &message));
        }
        Ok(value)
    }

    /// 调用并反序列化为具体响应结构。
    fn call_typed<R: DeserializeOwned>(
        &self,
        action: &str,
        params: Params,
        scope: Scope,
    ) -> MnResult<R> {
        let value = self.call(action, params, scope)?;
        serde_json::from_value(value).map_err(|e| MnError::Provider {
            provider: PROVIDER_NAME.to_string(),
            code: "schema_mismatch".to_string(),
            message: format!("{action} 响应结构与预期不符：{e}"),
        })
    }

    // ------------------------------------------------------------ 查询类

    /// 查询支持的可用区列表。
    ///
    /// 不传 `Region` 时厂商返回**全部**支持的地域/可用区；
    /// 若客户端配置了 region，则自动带上作为筛选条件。
    pub fn zones(&self) -> MnResult<ZonesResponse> {
        self.zones_filtered(None)
    }

    /// 查询可用区列表，`region` 为 `None` 时不筛选（返回全部）。
    ///
    /// 三种情况要分清：
    /// * `None` → 沿用客户端配置的 region（有则筛选，无则查全部）；
    /// * `Some("")` / `Some("  ")` → **明确要求不筛选**，不能回退到客户端配置，
    ///   否则「查全部」会被静默变成「查某一个地域」；
    /// * `Some("cn-sh2")` → 按该地域筛选（可查客户端未绑定的地域）。
    pub fn zones_filtered(&self, region: Option<&str>) -> MnResult<ZonesResponse> {
        let mut p = Params::new();
        match region {
            // 未指定 → 允许 Scope::Region 用客户端配置兜底
            None => self.call_typed("DescribeCompShareSupportZone", p, Scope::Region),
            Some(r) => {
                let r = r.trim();
                if !r.is_empty() {
                    p.set("Region", r);
                }
                // 显式给了（哪怕是空白表示「查全部」）→ 一律用 Scope::None，
                // 避免被客户端配置兜底覆盖掉调用方的明确意图
                self.call_typed("DescribeCompShareSupportZone", p, Scope::None)
            }
        }
    }

    /// 查询某地域下**可部署**的可用区；无则回退。
    ///
    /// 三级回退，逐级放宽（宁可给出「次优但能用」的答案，也不要直接报错）：
    ///
    /// 1. 虚机 + 支持 `image_type` 镜像 → 最理想（能 SSH、镜像兼容）；
    /// 2. 任意虚机可用区 → 镜像可能不兼容，但至少能登录；
    /// 3. 该地域下任意可用区 → 容器区，调用方需自行判断能否用。
    pub fn pick_zone(&self, region: &str) -> MnResult<Option<ZoneInfo>> {
        self.pick_zone_for_image(region, "System")
    }

    /// 同 [`Self::pick_zone`]，但指定镜像类型（`System` / `App` / `Custom` / `Community`）。
    ///
    /// 为什么需要它：实测发现容器可用区把 `System`、`Other` 列为不支持，
    /// 若只按 `IsPod` 判断，会挑出一个「能建但镜像用不了」的可用区，
    /// 直到创建实例时才被厂商以 226603 拒绝。
    pub fn pick_zone_for_image(
        &self,
        region: &str,
        image_type: &str,
    ) -> MnResult<Option<ZoneInfo>> {
        let r = self.zones_filtered(Some(region))?;
        let in_region = r.zones_in_region(region);
        Ok(in_region
            .iter()
            .find(|z| z.is_ready_for_deploy(image_type))
            .or_else(|| in_region.iter().find(|z| z.is_deployable()))
            .or_else(|| in_region.first())
            .map(|z| (*z).clone()))
    }

    /// 查询可售机型（含 CPU/内存合法组合与显存）。
    pub fn available_types(&self) -> MnResult<AvailableTypesResponse> {
        self.call_typed(
            "DescribeAvailableCompShareInstanceTypes",
            Params::new(),
            Scope::RegionAndZone,
        )
    }

    /// 检查指定规格的资源可用性。
    ///
    /// 厂商要求：**创建前必须先调用本接口**判断库存，不能用 Describe 类接口代替。
    pub fn check_capacity(&self, req: &CapacityRequest) -> MnResult<CapacityResponse> {
        let mut p = Params::new();
        p.set("GpuType", req.gpu_type.clone())
            .set("MachineType", req.machine_type.clone())
            .set("MinimalCpuPlatform", req.cpu_platform.clone())
            .set("CompShareImageId", req.image_id.clone())
            .set("ChargeType", req.charge_type.clone());
        p.set_json(
            "Disks",
            &serde_json::json!([{
                "IsBoot": true,
                "Type": req.disk_type,
                "Size": req.disk_size_gb,
            }]),
        );
        self.call_typed("CheckCompShareResourceCapacity", p, Scope::RegionAndZone)
    }

    /// 查询创建价格（分项明细）。
    pub fn price(&self, req: &PriceRequest) -> MnResult<PriceResponse> {
        let mut p = Params::new();
        p.set("GpuType", req.gpu_type.clone())
            .set_i64("Gpu", i64::from(req.gpu))
            .set_i64("Cpu", i64::from(req.cpu))
            .set_i64("Memory", req.memory_mb as i64);
        if let Some(ct) = &req.charge_type {
            p.set("ChargeType", ct.clone());
        }
        p.set_json(
            "Disks",
            &serde_json::json!([{
                "IsBoot": true,
                "Type": req.disk_type,
                "Size": req.disk_size_gb,
            }]),
        );
        self.call_typed("GetCompShareInstancePrice", p, Scope::RegionAndZone)
    }

    /// 查询平台镜像列表。
    pub fn images(
        &self,
        image_type: Option<&str>,
        limit: u32,
        offset: u32,
    ) -> MnResult<ImagesResponse> {
        let mut p = Params::new();
        p.set_opt("ImageType", image_type)
            .set_i64("Limit", i64::from(limit))
            .set_i64("Offset", i64::from(offset));
        self.call_typed("DescribeCompShareImages", p, Scope::RegionAndZone)
    }

    /// 查询实例列表（分页）。
    pub fn describe_instances(
        &self,
        ids: Option<&[String]>,
        limit: u32,
        offset: u32,
    ) -> MnResult<DescribeInstancesResponse> {
        let mut p = Params::new();
        if let Some(ids) = ids {
            p.set_list("UHostIds", ids);
        }
        p.set_i64("Limit", i64::from(limit))
            .set_i64("Offset", i64::from(offset));
        self.call_typed("DescribeCompShareInstance", p, Scope::RegionAndZone)
    }

    /// 查询实例列表，**不注入地域/可用区**（即不限地域）。
    ///
    /// 用途：实例可能是用另一份配置（另一个地域）创建的。按当前配置的
    /// 地域过滤会「查不到」，从而把「存在但地域不同」误判成「已删除」——
    /// 这是销毁/监控链路上最危险的一类误判。
    ///
    /// ## 为什么用 JSON 通道
    ///
    /// 实测：带 `UHostIds` 数组时，表单编码会被厂商回一段**非标准 JSON**
    /// （解析报 `trailing characters`），而同样的参数走 JSON body 就正常。
    /// 这与 `GetCompShareInstanceMonitor` 的结论一致——**只要请求里带
    /// 数组，就必须走 JSON 通道**。不带数组的普通查询仍可走表单。
    pub fn describe_instances_any_scope(
        &self,
        ids: Option<&[String]>,
        limit: u32,
        offset: u32,
    ) -> MnResult<DescribeInstancesResponse> {
        let mut fields = serde_json::Map::new();
        if let Some(ids) = ids {
            if !ids.is_empty() {
                fields.insert(
                    "UHostIds".to_string(),
                    serde_json::Value::Array(
                        ids.iter()
                            .map(|s| serde_json::Value::String(s.clone()))
                            .collect(),
                    ),
                );
            }
        }
        fields.insert("Limit".to_string(), serde_json::json!(limit));
        fields.insert("Offset".to_string(), serde_json::json!(offset));

        let value = self.call_json("DescribeCompShareInstance", fields, Scope::None)?;
        serde_json::from_value(value).map_err(|e| MnError::Provider {
            provider: PROVIDER_NAME.to_string(),
            code: "schema_mismatch".to_string(),
            message: format!("DescribeCompShareInstance 响应结构与预期不符：{e}"),
        })
    }

    /// 查询实例监控（CPU / 内存 / GPU / 磁盘使用率）。
    ///
    /// `ids` 为空表示查询当前 `Region` 下全部实例的监控。
    ///
    /// ## 三个必须注意的点（都是真实调用验证出来的）
    ///
    /// 1. `Region` 是**必填**且必须与实例所在地域一致，否则厂商回
    ///    `Params [Region] not available`；因此这里用 `Scope::RegionAndZone`
    ///    注入客户端绑定的地域。
    /// 2. 参数名是 `UHostIds`（数组），**不是** `UHostId`。写成单数会被判参数错误。
    /// 3. **本接口走 JSON 请求体**，表单编码全部被拒（`210 Missing params`
    ///    或 `230 ... not available`），见 [`Self::call_json`] 的对照表。
    pub fn instance_monitor(&self, ids: &[String]) -> MnResult<MonitorResponse> {
        let mut fields = serde_json::Map::new();
        if !ids.is_empty() {
            fields.insert(
                "UHostIds".to_string(),
                serde_json::Value::Array(
                    ids.iter()
                        .map(|s| serde_json::Value::String(s.clone()))
                        .collect(),
                ),
            );
        }
        let value = self.call_json("GetCompShareInstanceMonitor", fields, Scope::RegionAndZone)?;
        serde_json::from_value(value).map_err(|e| MnError::Provider {
            provider: PROVIDER_NAME.to_string(),
            code: "schema_mismatch".to_string(),
            message: format!("GetCompShareInstanceMonitor 响应结构与预期不符：{e}"),
        })
    }

    // ------------------------------------------------------------ 变更类

    /// 创建实例。成功时返回实例短 ID 列表。
    pub fn create_instance(&self, req: &CreateRequest) -> MnResult<CreateInstanceResponse> {
        let mut p = Params::new();
        p.set("GpuType", req.gpu_type.clone())
            .set_i64("GPU", i64::from(req.gpu))
            .set_i64("CPU", i64::from(req.cpu))
            .set_i64("Memory", req.memory_mb as i64)
            .set("CompShareImageId", req.image_id.clone())
            .set("MachineType", req.machine_type.clone())
            .set("MinimalCpuPlatform", req.cpu_platform.clone())
            .set("ChargeType", req.charge_type.clone())
            .set_i64("MaxCount", 1);
        if let Some(n) = &req.name {
            p.set("Name", truncate(n, 63));
        }
        if let Some(r) = &req.remark {
            p.set("Remark", r.clone());
        }
        if let Some(q) = req.quantity {
            p.set_i64("Quantity", i64::from(q));
        }
        p.set_json(
            "Disks",
            &serde_json::json!([{
                "IsBoot": true,
                "Type": req.disk_type,
                "Size": req.disk_size_gb,
            }]),
        );
        self.call_typed("CreateCompShareInstance", p, Scope::RegionAndZone)
    }

    /// 启动实例（仅 `Stopped` 可用）。
    pub fn start_instance(
        &self,
        id: &str,
        without_gpu_spec: Option<&str>,
    ) -> MnResult<ActionResponse> {
        let mut p = Params::new();
        p.set("UHostId", id);
        p.set_opt("WithoutGpuSpec", without_gpu_spec);
        self.call_typed("StartCompShareInstance", p, Scope::RegionAndZone)
    }

    /// 关闭实例（仅 `Running` 可用）。
    pub fn stop_instance(&self, id: &str) -> MnResult<ActionResponse> {
        let mut p = Params::new();
        p.set("UHostId", id);
        self.call_typed("StopCompShareInstance", p, Scope::RegionAndZone)
    }

    /// 重启实例（仅 `Running` 可用）。
    pub fn reboot_instance(&self, id: &str) -> MnResult<ActionResponse> {
        let mut p = Params::new();
        p.set("UHostId", id);
        self.call_typed("RebootCompShareInstance", p, Scope::RegionAndZone)
    }

    /// 删除实例（**仅 `Stopped` 可用**，调用方须先关机）。
    pub fn terminate_instance(
        &self,
        id: &str,
        release_data_disk: bool,
    ) -> MnResult<ActionResponse> {
        let mut p = Params::new();
        p.set("UHostId", id)
            .set_bool("ReleaseUDisk", release_data_disk);
        self.call_typed("TerminateCompShareInstance", p, Scope::RegionAndZone)
    }

    // ------------------------------------------------------------ 配置类

    /// 修改实例名称（最长 63 字符）。
    ///
    /// 名称用于在控制台辨认归属。Mangosteen 用它标记「这台属于哪个部署」，
    /// 避免用户在一堆 `host` 里找不到自己开的那台。
    pub fn modify_instance_name(&self, id: &str, name: &str) -> MnResult<ActionResponse> {
        let mut p = Params::new();
        p.set("UHostId", id).set("Name", truncate(name, 63));
        self.call_typed("ModifyCompShareInstanceName", p, Scope::RegionAndZone)
    }

    /// 重置实例密码。
    ///
    /// ## 调用方传**明文**，本方法负责 Base64
    ///
    /// 厂商要求 `Password` 为 Base64 编码。若让调用方自己编码，
    /// 每个调用点都要记得——漏一处就是「密码明明给了却登录失败」。
    /// 编码集中在此处，调用方只面对明文。
    ///
    /// ## 状态限制
    ///
    /// 普通主机实例**仅 `Stopped` 可重置**（容器实例支持运行中）。
    /// 这里不预检状态：容器可用区的实例允许运行中调用，
    /// 预检会把合法调用误拒。状态错误由厂商返回，驱动层负责翻译。
    pub fn reset_instance_password(&self, id: &str, password: &str) -> MnResult<ActionResponse> {
        let mut p = Params::new();
        p.set("UHostId", id)
            .set("Password", base64_encode(password));
        self.call_typed("ResetCompShareInstancePassword", p, Scope::RegionAndZone)
    }

    /// 重装实例系统镜像（**仅 `Stopped` 可用**）。
    ///
    /// 系统盘数据会被清除，数据盘不受影响。`password` 传 `None` 表示保留原密码。
    pub fn reinstall_instance(
        &self,
        id: &str,
        image_id: &str,
        password: Option<&str>,
    ) -> MnResult<ActionResponse> {
        let mut p = Params::new();
        p.set("UHostId", id).set("CompShareImageId", image_id);
        if let Some(pw) = password {
            p.set("Password", base64_encode(pw));
        }
        self.call_typed("ReinstallCompShareInstance", p, Scope::RegionAndZone)
    }

    /// 升降配：调整 CPU / 内存 / GPU（**仅 `Stopped` 可用**）。
    ///
    /// `memory_mb` 单位是 **MB**（与创建接口一致），不是 GB。
    ///
    /// ## 与磁盘扩容二选一
    ///
    /// 厂商限制：单次请求只能「改计算规格」或「扩磁盘」，不能同时做。
    /// 因此拆成两个方法而不是一个带 `Option` 的，避免调用方误以为能一次做完。
    pub fn resize_instance_compute(
        &self,
        id: &str,
        cpu: u32,
        memory_mb: u64,
        gpu: u32,
    ) -> MnResult<ActionResponse> {
        let mut p = Params::new();
        p.set("UHostId", id)
            .set_i64("Cpu", i64::from(cpu))
            .set_i64("Memory", memory_mb as i64)
            .set_i64("Gpu", i64::from(gpu));
        self.call_typed("ResizeCompShareInstance", p, Scope::RegionAndZone)
    }

    /// 升降配：扩容指定磁盘（**仅 `Stopped` 可用**，只能扩不能缩）。
    pub fn resize_instance_disk(
        &self,
        id: &str,
        disk_id: &str,
        disk_space_gb: u32,
    ) -> MnResult<ActionResponse> {
        let mut p = Params::new();
        p.set("UHostId", id)
            .set("DiskId", disk_id)
            .set_i64("DiskSpace", i64::from(disk_space_gb));
        self.call_typed("ResizeCompShareInstance", p, Scope::RegionAndZone)
    }

    /// 变更计费方式（如按量 `Postpay` → 包月 `Month`）。
    ///
    /// 余额不足时厂商可能返回 `520`，由 `retcode` 模块归类。
    pub fn switch_charge_type(&self, id: &str, dest: &str) -> MnResult<ActionResponse> {
        let mut p = Params::new();
        p.set("UHostId", id).set("DestChargeType", dest);
        self.call_typed("SwitchChargeType", p, Scope::RegionAndZone)
    }

    // ------------------------------------------------------------ 价格类

    /// 查询升降配差价（**元**，不是时价）。
    ///
    /// 三个参数都是 `Option`：厂商允许只改其中一项，
    /// 未传的项保持当前值。
    pub fn upgrade_price(
        &self,
        id: &str,
        cpu: Option<u32>,
        memory_mb: Option<u64>,
        gpu: Option<u32>,
    ) -> MnResult<UpgradePriceResponse> {
        let mut p = Params::new();
        p.set("UHostId", id);
        if let Some(c) = cpu {
            p.set_i64("CPU", i64::from(c));
        }
        if let Some(m) = memory_mb {
            p.set_i64("Memory", m as i64);
        }
        if let Some(g) = gpu {
            p.set_i64("GPU", i64::from(g));
        }
        self.call_typed("GetCompShareInstanceUpgradePrice", p, Scope::RegionAndZone)
    }

    /// 查询某规格在**全部计费模式**下的价格。
    ///
    /// 与 [`Self::price`] 的区别：`price` 只查当前配置的那一种模式；
    /// 本方法不传 `ChargeType` 时厂商会同时返回 Month / Day / Postpay / Spot，
    /// 便于用户对比「按量 vs 包月哪个划算」。
    pub fn user_price(&self, req: &UserPriceRequest<'_>) -> MnResult<PriceResponse> {
        let mut p = Params::new();
        p.set("GpuType", req.gpu_type)
            .set_i64("GPU", i64::from(req.gpu))
            .set_i64("Cpu", i64::from(req.cpu))
            .set_i64("Memory", req.memory_mb as i64)
            .set_opt("CompShareImageId", req.image_id);
        p.set_json(
            "Disks",
            &serde_json::json!([{
                "IsBoot": true,
                "Type": req.disk_type,
                "Size": req.disk_size_gb,
            }]),
        );
        self.call_typed("GetCompShareInstanceUserPrice", p, Scope::RegionAndZone)
    }

    /// 查询删除实例后的可退金额（批量）。
    ///
    /// ## `UHostIds` 必须走 JSON 数组
    ///
    /// 厂商文档明确「直接传字符串数组，不要改写为 `UHostIds.0`」。
    /// 与 `GetCompShareInstanceMonitor` 同理，走 JSON 通道。
    pub fn refund_price(&self, ids: &[String]) -> MnResult<RefundPriceResponse> {
        let mut fields = serde_json::Map::new();
        if !ids.is_empty() {
            fields.insert(
                "UHostIds".to_string(),
                serde_json::Value::Array(
                    ids.iter()
                        .map(|s| serde_json::Value::String(s.clone()))
                        .collect(),
                ),
            );
        }
        let value = self.call_json("GetCompShareRefundPrice", fields, Scope::RegionAndZone)?;
        serde_json::from_value(value).map_err(|e| MnError::Provider {
            provider: PROVIDER_NAME.to_string(),
            code: "schema_mismatch".to_string(),
            message: format!("GetCompShareRefundPrice 响应结构与预期不符：{e}"),
        })
    }

    // ------------------------------------------------------------ 网络 / 端口

    /// 查询账号是否已开通网络加速（影响海外模型下载速度）。
    pub fn check_net_optimizer(&self) -> MnResult<NetOptimizerResponse> {
        self.call_typed("CheckCompShareNetOptimizer", Params::new(), Scope::Region)
    }

    /// 查询平台支持的软件及其端口（JupyterLab / SSH / FileBrowser 等）。
    pub fn software_ports(&self) -> MnResult<SoftwarePortResponse> {
        self.call_typed(
            "DescribeCompShareSoftwarePort",
            Params::new(),
            Scope::Region,
        )
    }

    /// 查询实例上某软件的访问地址。
    ///
    /// Action 名是 `GetSoftwareURL`（不是 `GetCompShareSoftwareURL`）。
    pub fn software_url(&self, id: &str, software: &str) -> MnResult<SoftwareUrlResponse> {
        let mut p = Params::new();
        p.set("UHostId", id).set("Software", software);
        self.call_typed("GetSoftwareURL", p, Scope::Region)
    }

    /// 切换实例公网带宽归属（独享 ⇄ 平台共享）。
    ///
    /// ## 顶层 `RetCode=0` 不代表每个实例都成功
    ///
    /// 厂商逐实例处理，部分失败时顶层仍是 0。调用方必须检查
    /// [`BandwidthSwitchResponse::all_succeeded`] 或 [`BandwidthSwitchResponse::failures`]。
    ///
    /// `UHostIds` 同样是 JSON 数组。
    pub fn switch_bandwidth(
        &self,
        ids: &[String],
        target_scope: &str,
    ) -> MnResult<BandwidthSwitchResponse> {
        let mut fields = serde_json::Map::new();
        fields.insert(
            "UHostIds".to_string(),
            serde_json::Value::Array(
                ids.iter()
                    .map(|s| serde_json::Value::String(s.clone()))
                    .collect(),
            ),
        );
        fields.insert(
            "TargetScope".to_string(),
            serde_json::Value::String(target_scope.to_string()),
        );
        let value = self.call_json("SwitchCompShareEIPShareBandwidth", fields, Scope::Region)?;
        serde_json::from_value(value).map_err(|e| MnError::Provider {
            provider: PROVIDER_NAME.to_string(),
            code: "schema_mismatch".to_string(),
            message: format!("SwitchCompShareEIPShareBandwidth 响应结构与预期不符：{e}"),
        })
    }

    /// 更新**容器实例**的对外端口（全量替换语义）。
    ///
    /// ## 三个必须注意的点
    ///
    /// 1. **仅容器可用区可用**：传统 UHost 虚机不支持，调用会失败。
    /// 2. **全量替换**：未出现在请求里的端口会被**删除**，
    ///    所以每次都要传完整列表，不能只传新增的。
    /// 3. 服务端会自动补 `HTTP 8888` 与 `TCP 23`（各协议上限 10 个）。
    ///
    /// 端口数组走 JSON（`HttpPorts` / `TcpPorts`），与文档一致。
    pub fn update_instance_ports(
        &self,
        id: &str,
        http_ports: &[u16],
        tcp_ports: &[u16],
    ) -> MnResult<InstancePortsResponse> {
        let mut fields = serde_json::Map::new();
        fields.insert(
            "UHostId".to_string(),
            serde_json::Value::String(id.to_string()),
        );
        if !http_ports.is_empty() {
            fields.insert(
                "HttpPorts".to_string(),
                serde_json::Value::Array(http_ports.iter().map(|p| serde_json::json!(p)).collect()),
            );
        }
        if !tcp_ports.is_empty() {
            fields.insert(
                "TcpPorts".to_string(),
                serde_json::Value::Array(tcp_ports.iter().map(|p| serde_json::json!(p)).collect()),
            );
        }
        let value = self.call_json("UpdateCompShareInstancePorts", fields, Scope::RegionAndZone)?;
        serde_json::from_value(value).map_err(|e| MnError::Provider {
            provider: PROVIDER_NAME.to_string(),
            code: "schema_mismatch".to_string(),
            message: format!("UpdateCompShareInstancePorts 响应结构与预期不符：{e}"),
        })
    }

    // ------------------------------------------------------------ 定时关机

    /// 设置/更新定时关机。
    ///
    /// ## 为什么必须传 `ProjectId`
    ///
    /// 该接口把 `ProjectId` 列为必填，而它不在实例信息里。
    /// 驱动层会先调 [`Self::project_list`] 取默认项目并缓存。
    ///
    /// `stop_time` 是 Unix 秒，且必须 **≥ 当前时间 + 300 秒**（厂商限制）。
    pub fn update_stop_scheduler(
        &self,
        id: &str,
        project_id: &str,
        stop_time: i64,
    ) -> MnResult<ActionResponse> {
        let mut p = Params::new();
        p.set("UHostId", id)
            .set("ProjectId", project_id)
            .set_i64("SchedulerStopTime", stop_time);
        self.call_typed("UpdateCompShareStopScheduler", p, Scope::RegionAndZone)
    }

    /// 取消定时关机（同样需要 `ProjectId`）。
    pub fn delete_stop_scheduler(&self, id: &str, project_id: &str) -> MnResult<ActionResponse> {
        let mut p = Params::new();
        p.set("UHostId", id).set("ProjectId", project_id);
        self.call_typed("DeleteCompShareStopScheduler", p, Scope::Region)
    }

    /// 查询项目列表（用于取定时关机所需的 `ProjectId`）。
    pub fn project_list(&self) -> MnResult<ProjectListResponse> {
        self.call_typed("GetProjectList", Params::new(), Scope::None)
    }

    // ------------------------------------------------------------ 机型

    /// 查询机型族列表（按可用区分组的完整机型信息）。
    ///
    /// 与 `DescribeAvailableCompShareInstanceTypes` 的区别：
    /// 后者是「当前可售 + 合法组合」，本接口是**全量机型目录**
    /// （含未上架的），适合做型号说明与前端下拉。
    pub fn machine_type_families(&self) -> MnResult<MachineTypeFamiliesResponse> {
        self.call_typed(
            "DescribeCompShareMachineTypeFamilies",
            Params::new(),
            Scope::Region,
        )
    }
}

/// 把明文密码编码为厂商要求的 Base64 形式。
///
/// 集中在这里而不是每个调用点各写一遍：漏一处就是
/// 「密码明明给了却登录失败」，而且错得很隐蔽（厂商只回「参数非法」）。
fn base64_encode(plain: &str) -> String {
    crate::compshare::base64::encode(plain.as_bytes())
}

/// 从响应里取 `RetCode`，同时兼容数字与字符串两种形式。
fn extract_ret_code(v: &serde_json::Value) -> Option<i64> {
    match v.get("RetCode")? {
        serde_json::Value::Number(n) => n.as_i64(),
        serde_json::Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// 截断过长文本（避免把整段响应塞进错误信息）。
fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    format!("{}…（已截断）", s.chars().take(max).collect::<String>())
}

/// 判断错误是否属于「换个节点可能成功」的类别。
///
/// 供上层在**同一次部署**内决定是否继续尝试其它规格/服务商。
pub fn is_capacity_shortage(err: &MnError) -> bool {
    match err {
        MnError::Provider { code, message, .. } => {
            let parsed = code.parse::<i64>().unwrap_or(0);
            matches!(
                crate::compshare::retcode::classify(parsed, message),
                ApiFailure::ResourceNotEnough
            )
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compshare::transport::MockTransport;

    fn cred() -> Credential {
        Credential::new("test-public-key", "test-private-key").unwrap()
    }

    fn client(mock: MockTransport) -> CompshareClient<MockTransport> {
        CompshareClient::new(DEFAULT_ENDPOINT, cred(), mock, "cn-wlcb", "cn-wlcb-01").unwrap()
    }

    #[test]
    fn rejects_insecure_endpoint() {
        let e = CompshareClient::new(
            "http://api.compshare.cn",
            cred(),
            MockTransport::always(200, "{}"),
            "cn-wlcb",
            "cn-wlcb-01",
        )
        .unwrap_err();
        assert!(e.to_string().contains("https"), "{e}");
    }

    #[test]
    fn empty_region_means_do_not_inject() {
        // Region 是可选参数：空表示「查全部地域」，不能因此报错，
        // 也不能把空串/占位值塞进请求（厂商会回 Params [Region] not available）
        let c = CompshareClient::new(
            DEFAULT_ENDPOINT,
            cred(),
            MockTransport::always(200, r#"{"RetCode":0,"ZoneInfo":[]}"#),
            "",
            "",
        )
        .unwrap();
        assert!(!c.has_region());
        c.zones().unwrap();
        let body = c.transport().requests()[0].body.clone().unwrap();
        assert!(!body.contains("Region="), "不应注入空 Region：{body}");
        assert!(!body.contains("Zone="), "不应注入空 Zone：{body}");
    }

    #[test]
    fn region_is_trimmed() {
        let c = CompshareClient::new(
            DEFAULT_ENDPOINT,
            cred(),
            MockTransport::always(200, r#"{"RetCode":0,"ZoneInfo":[]}"#),
            "  cn-wlcb  ",
            "  ",
        )
        .unwrap();
        assert_eq!(c.scope_label(), "cn-wlcb");
        assert!(!c.has_zone());
    }

    #[test]
    fn scope_label_reflects_config() {
        let c = client(MockTransport::always(200, "{}"));
        assert_eq!(c.scope_label(), "cn-wlcb/cn-wlcb-01");
    }

    #[test]
    fn zones_filter_sends_region_when_given() {
        let mock = MockTransport::always(200, r#"{"RetCode":0,"ZoneInfo":[]}"#);
        let c = client(mock);
        c.zones_filtered(Some("cn-sh2")).unwrap();
        let body = c.transport().requests()[0].body.clone().unwrap();
        assert!(
            body.contains("Action=DescribeCompShareSupportZone"),
            "{body}"
        );
        assert!(body.contains("Region=cn-sh2"), "{body}");
        // 本接口不接受 Zone：不能把客户端配置的 zone 塞进去
        assert!(!body.contains("Zone="), "{body}");
    }

    #[test]
    fn zones_filter_ignores_blank_region() {
        let mock = MockTransport::always(200, r#"{"RetCode":0,"ZoneInfo":[]}"#);
        let c = client(mock);
        c.zones_filtered(Some("   ")).unwrap();
        let body = c.transport().requests()[0].body.clone().unwrap();
        // 空白等于不筛选 → 不注入 Region（否则厂商会判非法参数）
        assert!(!body.contains("Region="), "{body}");
    }

    #[test]
    fn zones_uses_client_region_as_default_filter() {
        let mock = MockTransport::always(200, r#"{"RetCode":0,"ZoneInfo":[]}"#);
        let c = client(mock); // 客户端 region = cn-wlcb
        c.zones().unwrap();
        let body = c.transport().requests()[0].body.clone().unwrap();
        assert!(body.contains("Region=cn-wlcb"), "{body}");
    }

    const ZONES_DOC: &str = r#"{"RetCode":0,"request_uuid":"u-1","ZoneInfo":[
        {"Region":"cn-wlcb","RegionId":1000039,"Zone":"cn-wlcb-01","ZoneId":10027,"Describe":"华北二A","IsPod":false},
        {"Region":"cn-sh2","RegionId":1000009,"Zone":"cn-sh2-02","ZoneId":8200,"Describe":"上海二B","IsPod":false},
        {"Region":"cn-bj2","RegionId":1000001,"Zone":"cn-bj2-03","ZoneId":5001,"Describe":"华北一C","IsPod":true}
    ]}"#;

    #[test]
    fn pick_zone_prefers_vm_zone() {
        let c = client(MockTransport::always(200, ZONES_DOC));
        let z = c.pick_zone("cn-wlcb").unwrap().expect("应有可用区");
        assert_eq!(z.zone, "cn-wlcb-01");
        assert!(z.is_vm());
    }

    #[test]
    fn pick_zone_falls_back_to_container_zone() {
        // cn-bj2 只有容器可用区：无可部署区时应回退到第一个，而不是返回 None
        let c = client(MockTransport::always(200, ZONES_DOC));
        let z = c.pick_zone("cn-bj2").unwrap().expect("应回退到容器可用区");
        assert_eq!(z.zone, "cn-bj2-03");
        assert!(z.is_pod);
    }

    #[test]
    fn pick_zone_returns_none_for_unknown_region() {
        let c = client(MockTransport::always(200, ZONES_DOC));
        assert!(c.pick_zone("mars-1").unwrap().is_none());
    }

    #[test]
    fn zone_request_carries_action_region_zone_and_signature() {
        let mock = MockTransport::always(200, r#"{"RetCode":0,"ZoneInfo":[]}"#);
        let c = client(mock);
        c.zones().unwrap();
        let reqs = c.transport().requests();
        assert_eq!(reqs.len(), 1);
        let body = reqs[0].body.as_deref().unwrap();
        assert!(
            body.contains("Action=DescribeCompShareSupportZone"),
            "{body}"
        );
        assert!(body.contains("Region=cn-wlcb"), "{body}");
        assert!(body.contains("PublicKey=test-public-key"), "{body}");
        assert!(body.contains("Signature="), "{body}");
        // 表单编码
        assert_eq!(
            reqs[0].header("Content-Type"),
            Some("application/x-www-form-urlencoded")
        );
    }

    #[test]
    fn zone_not_injected_for_region_only_scope() {
        let mock = MockTransport::always(200, r#"{"RetCode":0,"ZoneInfo":[]}"#);
        let c = client(mock);
        c.zones().unwrap();
        let body = c.transport().requests()[0].body.clone().unwrap();
        // DescribeCompShareSupportZone 只接受 Region
        assert!(!body.contains("Zone="), "{body}");
    }

    #[test]
    fn zone_injected_for_instance_actions() {
        let mock = MockTransport::always(200, r#"{"RetCode":0,"UHostId":"uhost-1"}"#);
        let c = client(mock);
        c.stop_instance("uhost-1").unwrap();
        let body = c.transport().requests()[0].body.clone().unwrap();
        assert!(body.contains("Action=StopCompShareInstance"), "{body}");
        assert!(body.contains("UHostId=uhost-1"), "{body}");
        assert!(body.contains("Zone=cn-wlcb-01"), "{body}");
    }

    #[test]
    fn business_failure_becomes_provider_error() {
        let mock =
            MockTransport::always(200, r#"{"RetCode":226604,"Message":"Resource not enough"}"#);
        let c = client(mock);
        let e = c.start_instance("u-1", None).unwrap_err();
        match &e {
            MnError::Provider { code, message, .. } => {
                assert_eq!(code, "226604");
                assert!(message.contains("无库存"), "{message}");
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(is_capacity_shortage(&e));
    }

    #[test]
    fn capacity_shortage_detection_ignores_other_errors() {
        let mock = MockTransport::always(200, r#"{"RetCode":226601,"Message":"arrears"}"#);
        let c = client(mock);
        let e = c.stop_instance("u-1").unwrap_err();
        assert!(!is_capacity_shortage(&e), "欠费不属于容量问题：{e}");
    }

    #[test]
    fn http_error_is_reported_with_status() {
        let mock = MockTransport::always(503, r#"{"Message":"gateway down"}"#);
        let c = client(mock);
        let e = c.available_types().unwrap_err();
        let s = e.to_string();
        assert!(s.contains("503"), "{s}");
        assert!(s.contains("gateway down"), "{s}");
    }

    #[test]
    fn non_json_body_is_reported_without_leaking_key() {
        let mock = MockTransport::always(200, "<html>oops</html>");
        let c = client(mock);
        let e = c.available_types().unwrap_err();
        let s = e.to_string();
        assert!(s.contains("非 JSON"), "{s}");
        assert!(!s.contains("test-private-key"), "私钥泄漏：{s}");
    }

    #[test]
    fn ret_code_accepts_string_form() {
        let mock = MockTransport::always(200, r#"{"RetCode":"0","ZoneInfo":[]}"#);
        let c = client(mock);
        assert!(c.zones().is_ok());
    }

    #[test]
    fn schema_mismatch_is_explained() {
        let mock = MockTransport::always(200, r#"{"RetCode":0,"AvailableInstanceTypes":"nope"}"#);
        let c = client(mock);
        let e = c.available_types().unwrap_err();
        assert!(e.to_string().contains("结构与预期不符"), "{e}");
    }

    #[test]
    fn create_sends_flat_disks_and_memory_in_mb() {
        let mock = MockTransport::always(200, r#"{"RetCode":0,"UHostIds":["uhost-new"]}"#);
        let c = client(mock);
        let req = CreateRequest {
            gpu_type: "4090".into(),
            gpu: 1,
            cpu: 16,
            memory_mb: 65536,
            image_id: "img-1".into(),
            machine_type: "G".into(),
            cpu_platform: "Auto".into(),
            charge_type: "Postpay".into(),
            disk_type: "CLOUD_SSD".into(),
            disk_size_gb: 100,
            name: Some("mangosteen-test".into()),
            remark: Some("managed-by-mangosteen".into()),
            quantity: None,
        };
        let r = c.create_instance(&req).unwrap();
        assert_eq!(r.uhost_ids, vec!["uhost-new"]);

        let body = c.transport().requests()[0].body.clone().unwrap();
        assert!(body.contains("Action=CreateCompShareInstance"), "{body}");
        assert!(body.contains("GpuType=4090"), "{body}");
        assert!(body.contains("GPU=1"), "{body}");
        assert!(body.contains("CPU=16"), "{body}");
        // 内存必须是 MB
        assert!(body.contains("Memory=65536"), "{body}");
        // 磁盘必须扁平为 Disks.0.*
        assert!(body.contains("Disks.0.IsBoot=true"), "{body}");
        assert!(body.contains("Disks.0.Type=CLOUD_SSD"), "{body}");
        assert!(body.contains("Disks.0.Size=100"), "{body}");
        assert!(body.contains("CompShareImageId=img-1"), "{body}");
        assert!(body.contains("Name=mangosteen-test"), "{body}");
    }

    #[test]
    fn terminate_sends_release_flag() {
        let mock = MockTransport::always(200, r#"{"RetCode":0,"UHostId":"u-1","InRecycle":"Yes"}"#);
        let c = client(mock);
        c.terminate_instance("u-1", true).unwrap();
        let body = c.transport().requests()[0].body.clone().unwrap();
        assert!(body.contains("Action=TerminateCompShareInstance"), "{body}");
        assert!(body.contains("ReleaseUDisk=true"), "{body}");
    }

    #[test]
    fn monitor_uses_json_body_with_array_and_region() {
        let mock = MockTransport::always(200, r#"{"RetCode":0,"Data":{"List":[]}}"#);
        let c = client(mock);
        c.instance_monitor(&["uhost-1".to_string(), "uhost-2".to_string()])
            .unwrap();
        let req = c.transport().requests().remove(0);
        // 必须走 JSON 请求体：表单编码会被厂商判 210/230（真实调用验证）
        assert_eq!(
            req.header("content-type"),
            Some("application/json"),
            "监控接口必须用 JSON 请求体"
        );
        let body: serde_json::Value = serde_json::from_str(req.body.as_deref().unwrap()).unwrap();
        assert_eq!(body["Action"], "GetCompShareInstanceMonitor");
        // UHostIds 必须是真正的 JSON 数组
        assert_eq!(body["UHostIds"][0], "uhost-1");
        assert_eq!(body["UHostIds"][1], "uhost-2");
        // Region 必填且由客户端注入
        assert_eq!(body["Region"], "cn-wlcb");
        // 公钥与签名都在体里
        assert_eq!(body["PublicKey"], "test-public-key");
        assert!(body["Signature"]
            .as_str()
            .map(|s| !s.is_empty())
            .unwrap_or(false));
    }

    #[test]
    fn monitor_signature_concatenates_array_elements() {
        // 用固定时钟无关的方式验证签名口径：直接调签名函数。
        let fields = serde_json::Map::from_iter([
            (
                "Action".to_string(),
                serde_json::Value::String("GetCompShareInstanceMonitor".to_string()),
            ),
            (
                "PublicKey".to_string(),
                serde_json::Value::String("pk".to_string()),
            ),
            (
                "Region".to_string(),
                serde_json::Value::String("cn-wlcb".to_string()),
            ),
            ("UHostIds".to_string(), serde_json::json!(["u-1", "u-2"])),
        ]);
        let got = crate::compshare::sign::signature_of_json("sk", &fields);
        // 手工按协议算：键有序 Action/PublicKey/Region/UHostIds，数组元素直接拼接
        let expect_src =
            "ActionGetCompShareInstanceMonitorPublicKeypkRegioncn-wlcbUHostIdsu-1u-2sk";
        assert_eq!(
            got,
            crate::compshare::sha1::hex_digest(expect_src.as_bytes())
        );

        // 对照：错误口径（逗号分隔 / JSON 字面量）必须得到不同结果，
        // 否则上面的断言无法证明「拼接方式」被正确实现
        let alt1 = got.clone();
        let mut wrong = fields.clone();
        wrong.insert(
            "UHostIds".to_string(),
            serde_json::Value::String("u-1,u-2".to_string()),
        );
        assert_ne!(
            alt1,
            crate::compshare::sign::signature_of_json("sk", &wrong)
        );
    }

    #[test]
    fn monitor_omits_ids_when_empty() {
        let mock = MockTransport::always(200, r#"{"RetCode":0,"Data":{"List":[]}}"#);
        let c = client(mock);
        c.instance_monitor(&[]).unwrap();
        let req = c.transport().requests().remove(0);
        let body: serde_json::Value = serde_json::from_str(req.body.as_deref().unwrap()).unwrap();
        // 空数组语义 = 查该地域全部实例，不应传个空数组让服务端判非法
        assert!(body.get("UHostIds").is_none(), "{body}");
    }

    #[test]
    fn monitor_parses_documented_response_sample() {
        // 文档「响应示例」原样
        let mock = MockTransport::always(
            200,
            r#"{
                "Action": "GetCompShareInstanceMonitorResponse",
                "RetCode": 0,
                "Data": {
                    "List": [{
                        "UHostId": "uhost-xxxx",
                        "Metrics": [{
                            "MetricKey": "uhost_cpu_used",
                            "Results": [{
                                "Values": [{"Timestamp": 1712563200, "Value": 35.2}]
                            }]
                        }]
                    }]
                }
            }"#,
        );
        let c = client(mock);
        let r = c.instance_monitor(&["uhost-xxxx".to_string()]).unwrap();
        assert_eq!(r.ret_code, 0);
        let inst = r.find("uhost-xxxx").unwrap();
        assert_eq!(inst.sampled_at(), 1_712_563_200);
        assert!((inst.value_of("uhost_cpu_used").unwrap() - 35.2).abs() < 1e-9);
        // 查不到的实例返回 None，而不是报错
        assert!(r.find("uhost-other").is_none());
    }

    #[test]
    fn monitor_tolerates_null_data() {
        // 实例刚开机时厂商可能返回 Data/List 为 null
        let mock = MockTransport::always(200, r#"{"RetCode":0,"Data":null}"#);
        let c = client(mock);
        let r = c.instance_monitor(&["uhost-1".to_string()]).unwrap();
        assert!(r.data.list.is_empty());
        assert!(r.find("uhost-1").is_none());
    }

    #[test]
    fn describe_ids_use_indexed_keys() {
        let mock = MockTransport::always(200, r#"{"RetCode":0,"UHostSet":[]}"#);
        let c = client(mock);
        c.describe_instances(Some(&["u-1".to_string(), "u-2".to_string()]), 20, 0)
            .unwrap();
        let body = c.transport().requests()[0].body.clone().unwrap();
        assert!(body.contains("UHostIds.0=u-1"), "{body}");
        assert!(body.contains("UHostIds.1=u-2"), "{body}");
    }

    #[test]
    fn capacity_request_includes_disk_and_image() {
        let mock = MockTransport::always(
            200,
            r#"{"RetCode":0,"Specs":[{"Gpu":1,"Cpu":16,"Mem":64,"ResourceEnough":true}]}"#,
        );
        let c = client(mock);
        let req = CapacityRequest {
            gpu_type: "4090".into(),
            machine_type: "G".into(),
            cpu_platform: "Auto".into(),
            image_id: "img-1".into(),
            charge_type: "Postpay".into(),
            disk_type: "CLOUD_SSD".into(),
            disk_size_gb: 100,
        };
        let r = c.check_capacity(&req).unwrap();
        assert!(r.is_enough(1, 16, 64));
        let body = c.transport().requests()[0].body.clone().unwrap();
        assert!(
            body.contains("Action=CheckCompShareResourceCapacity"),
            "{body}"
        );
        assert!(body.contains("CompShareImageId=img-1"), "{body}");
        assert!(body.contains("Disks.0.Size=100"), "{body}");
    }

    #[test]
    fn price_request_passes_memory_mb_and_optional_charge_type() {
        let mock = MockTransport::always(
            200,
            r#"{"RetCode":0,"PriceDetails":[{"ChargeType":"Postpay","Instance":2.5,"SystemDisks":0.0}]}"#,
        );
        let c = client(mock);
        let req = PriceRequest {
            gpu_type: "4090".into(),
            gpu: 1,
            cpu: 16,
            memory_mb: 65536,
            charge_type: Some("Postpay".into()),
            disk_type: "CLOUD_SSD".into(),
            disk_size_gb: 100,
        };
        let r = c.price(&req).unwrap();
        assert_eq!(r.price_details[0].total(), 2.5);
        let body = c.transport().requests()[0].body.clone().unwrap();
        assert!(body.contains("Memory=65536"), "{body}");
        assert!(body.contains("ChargeType=Postpay"), "{body}");
    }

    #[test]
    fn truncate_marks_ellipsis() {
        assert_eq!(truncate("abc", 10), "abc");
        assert!(truncate(&"x".repeat(50), 10).contains("已截断"));
    }
}
