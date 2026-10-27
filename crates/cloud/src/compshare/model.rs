//! 优云智算（Compshare）API 的数据结构。
//!
//! 归属：**算力执行层 · 云端后端管理**（厂商契约层）。
//!
//! ## 设计约定
//!
//! 1. **字段名与厂商文档逐字一致**（PascalCase），用 `serde(rename)` 显式绑定。
//!    不靠 `rename_all` 猜，避免厂商新增字段时静默错位。
//! 2. **所有字段都 `#[serde(default)]`**：厂商会在不同地域/镜像下省略字段，
//!    缺失即取默认值，绝不让一个可选字段导致整条响应解析失败。
//! 3. 只建模本项目需要的字段。厂商新增字段会被 serde 忽略，不影响解析。
//!
//! ## 单位陷阱（本项目踩过的坑，务必按此实现）
//!
//! | 字段 | 接口 | 单位 |
//! |------|------|------|
//! | `Memory` | `CreateCompShareInstance` / `GetCompShareInstancePrice` | **MB** |
//! | `Memory` | `DescribeCompShareInstance` 响应 | **MB** |
//! | `Memory`（机型表 `Collection`） | `DescribeAvailableCompShareInstanceTypes` | **GB** |
//! | `Mem` | `CheckCompShareResourceCapacity` 响应 | **GB** |
//! | `Size` | `Disks`（创建/价格） | **GB** |
//! | `GraphicsMemory.Value` | 机型表 | **GB** |
//!
//! 把 MB 当 GB 用会让显存/内存评估差 1024 倍，直接把「本地优先」的
//! 降级判断带偏，因此这里每个字段都标注单位，并在 [`crate::compshare::model::units`] 集中换算。

use serde::{Deserialize, Deserializer};

/// 宽松解析数组字段：厂商在**空结果**时可能返回 `null` 而非 `[]`。
///
/// 这是实测踩到的坑：`DescribeCompShareImages` 在没有匹配镜像时返回
/// `"ImageSet": null`，而默认的 `Vec` 反序列化只接受 `[]`，会直接报
/// `invalid type: null, expected a sequence`，把「正常无数据」误判成协议错误。
///
/// 所有列表字段都应使用它，避免厂商某个接口漏返回数组就让整条调用失败。
fn de_vec_or_null<'de, D, T>(d: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    let v = Option::<Vec<T>>::deserialize(d)?;
    Ok(v.unwrap_or_default())
}

/// 宽松解析嵌套对象字段：厂商在**空结果**时可能返回 `null` 而非 `{}`。
///
/// 与 [`de_vec_or_null`] 同一类问题的对象版：`GetCompShareInstanceMonitor`
/// 在没有数据点时返回 `"Data": null`，默认反序列化会直接失败，
/// 把「正常无数据」误判成协议错误。
fn de_object_or_null<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: serde::Deserialize<'de> + Default,
{
    let v = Option::<T>::deserialize(d)?;
    Ok(v.unwrap_or_default())
}

/// 宽松解析 `RetCode`：厂商在不同接口上既可能返回数字 `0`，也可能返回字符串 `"0"`。
///
/// 用一个自定义反序列化器统一处理，避免每个响应结构各写一份兼容逻辑
/// （少写一处就会有个别接口解析失败）。
fn de_ret_code<'de, D>(d: D) -> Result<i64, D::Error>
where
    D: Deserializer<'de>,
{
    use serde_json::Value;
    let v = Option::<Value>::deserialize(d)?.unwrap_or(Value::Null);
    Ok(match v {
        Value::Number(n) => n.as_i64().unwrap_or(0),
        Value::String(s) => s.trim().parse().unwrap_or(0),
        _ => 0,
    })
}

/// 实例状态：厂商原始值（大小写敏感，按文档）。
pub mod state {
    /// 运行中
    pub const RUNNING: &str = "Running";
    /// 已关机
    pub const STOPPED: &str = "Stopped";
    /// 启动中
    pub const STARTING: &str = "Starting";
    /// 关机中
    pub const STOPPING: &str = "Stopping";
    /// 安装中（镜像部署阶段）
    pub const INSTALL: &str = "Install";
    /// 重启中
    pub const REBOOTING: &str = "Rebooting";
}

/// `CreateCompShareInstance` 响应。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CreateInstanceResponse {
    /// 返回码，0 为成功
    #[serde(rename = "RetCode", default, deserialize_with = "de_ret_code")]
    pub ret_code: i64,
    /// 错误信息
    #[serde(rename = "Message", default)]
    pub message: String,
    /// 创建成功的实例短 ID 列表
    #[serde(rename = "UHostIds", default, deserialize_with = "de_vec_or_null")]
    pub uhost_ids: Vec<String>,
    /// 警告信息（部分规格降级或库存紧张时返回）
    #[serde(rename = "Warning", default, deserialize_with = "de_vec_or_null")]
    pub warning: Vec<String>,
}

/// 单个 IP 信息。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct IpInfo {
    /// IP 地址
    #[serde(rename = "IP", default)]
    pub ip: String,
    /// 线路类型（`Bgp` 等）
    #[serde(rename = "Type", default)]
    pub kind: String,
    /// 协议族（`IPv4` / `IPv6`）
    #[serde(rename = "IPMode", default)]
    pub mode: String,
}

/// 实例详情（`DescribeCompShareInstance` 的 `UHostSet` 元素）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UHost {
    /// 实例 ID
    #[serde(rename = "UHostId", default)]
    pub id: String,
    /// 实例名称
    #[serde(rename = "Name", default)]
    pub name: String,
    /// 状态（见 [`state`]）
    #[serde(rename = "State", default)]
    pub state: String,
    /// 可用区
    #[serde(rename = "Zone", default)]
    pub zone: String,
    /// 地域
    #[serde(rename = "Region", default)]
    pub region: String,
    /// CPU 核数
    #[serde(rename = "CPU", default)]
    pub cpu: u32,
    /// 内存，**单位 MB**
    #[serde(rename = "Memory", default)]
    pub memory_mb: u64,
    /// GPU 卡数
    #[serde(rename = "GPU", default)]
    pub gpu: u32,
    /// GPU 型号代号（如 `4090`）
    #[serde(rename = "GpuType", default)]
    pub gpu_type: String,
    /// 计费模式
    #[serde(rename = "ChargeType", default)]
    pub charge_type: String,
    /// 创建时间（Unix 秒）
    #[serde(rename = "CreateTime", default)]
    pub create_time: i64,
    /// 镜像 ID
    #[serde(rename = "CompShareImageId", default)]
    pub image_id: String,
    /// 镜像名称
    #[serde(rename = "CompShareImageName", default)]
    pub image_name: String,
    /// 实例类型（`UHost` 普通主机 / `Container` 容器主机）
    #[serde(rename = "InstanceType", default)]
    pub instance_type: String,
    /// 操作系统类别
    #[serde(rename = "OsType", default)]
    pub os_type: String,
    /// SSH 登录命令，形如 `ssh root@1.2.3.4 -p 60022`
    #[serde(rename = "SshLoginCommand", default)]
    pub ssh_login_command: String,
    /// IP 列表
    #[serde(rename = "IPSet", default, deserialize_with = "de_vec_or_null")]
    pub ip_set: Vec<IpInfo>,
    /// 数据盘总空间，**单位 GB**
    #[serde(rename = "TotalDiskSpace", default)]
    pub total_disk_space_gb: u64,
    /// 实例价格（元）
    #[serde(rename = "InstancePrice", default)]
    pub instance_price: Option<f64>,
    /// 磁盘总价（元）
    #[serde(rename = "DiskPrice", default)]
    pub disk_price: Option<f64>,
    /// 是否为抢占式实例
    #[serde(rename = "IsSpot", default)]
    pub is_spot: bool,
    /// 备注（Mangosteen 用来标记部署归属）
    #[serde(rename = "Remark", default)]
    pub remark: String,
    /// 是否支持无卡开机
    #[serde(rename = "SupportWithoutGpuStart", default)]
    pub support_without_gpu_start: bool,
    /// 当前使用率快照（监控接口无数据时的兜底）
    #[serde(
        rename = "MonitorMessages",
        default,
        deserialize_with = "de_object_or_null"
    )]
    pub monitor_messages: MonitorMessages,
}

impl UHost {
    /// 取第一个可用 IP（优先 BGP 线路）。
    pub fn primary_ip(&self) -> Option<&str> {
        self.ip_set
            .iter()
            .find(|i| i.kind.eq_ignore_ascii_case("Bgp") && !i.ip.is_empty())
            .or_else(|| self.ip_set.iter().find(|i| !i.ip.is_empty()))
            .map(|i| i.ip.as_str())
    }
}

/// `DescribeCompShareInstance` 返回的**当前使用率快照**。
///
/// 与 `GetCompShareInstanceMonitor` 的区别：
///
/// | 来源 | 数据形态 | 用途 |
/// |------|----------|------|
/// | `GetCompShareInstanceMonitor` | 时间序列（多数据点） | 画趋势曲线 |
/// | `MonitorMessages`（本结构） | 单点当前值 | 只要「现在多少」 |
///
/// 厂商文档明确写了后者可作为前者的替代。这里把它做成**兜底**：
/// 监控接口返回空 `Results` 时（实例刚开机、或厂商侧数据未就绪），
/// 用这个快照补上 CPU/内存，避免设备页一片空白。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MonitorMessages {
    /// CPU 使用率（百分数字符串，如 `"35.2"`；空串表示无数据）
    #[serde(
        rename = "CpuUsageRate",
        default,
        deserialize_with = "de_opt_f64_lenient"
    )]
    pub cpu_usage_rate: Option<f64>,
    /// 内存使用率（百分数字符串）
    #[serde(
        rename = "MemUsageRate",
        default,
        deserialize_with = "de_opt_f64_lenient"
    )]
    pub mem_usage_rate: Option<f64>,
    /// GPU 信息。厂商未文档化其结构（实测关机时为 `null`），
    /// 因此原样保留，由 [`crate::compshare::mapping`] 做尽力提取。
    #[serde(rename = "GpuInfo", default)]
    pub gpu_info: Option<serde_json::Value>,
}

/// 宽松解析「字符串或数字」形式的可选浮点数。
///
/// 厂商的 `MonitorMessages` 用字符串承载数值（`"35.2"`），
/// 而监控接口的 `Value` 是数字。同一个概念两种承载方式，
/// 用一个反序列化器统一，避免每个字段各写一遍。
fn de_opt_f64_lenient<'de, D>(d: D) -> Result<Option<f64>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde_json::Value;
    let v = Option::<Value>::deserialize(d)?.unwrap_or(Value::Null);
    Ok(match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => {
            let t = s.trim();
            if t.is_empty() {
                None
            } else {
                t.parse().ok()
            }
        }
        _ => None,
    })
}

/// `DescribeCompShareInstance` 响应。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DescribeInstancesResponse {
    /// 返回码
    #[serde(rename = "RetCode", default, deserialize_with = "de_ret_code")]
    pub ret_code: i64,
    /// 错误信息
    #[serde(rename = "Message", default)]
    pub message: String,
    /// 满足条件的实例总数
    #[serde(rename = "TotalCount", default)]
    pub total_count: u32,
    /// 实例列表
    #[serde(rename = "UHostSet", default, deserialize_with = "de_vec_or_null")]
    pub uhost_set: Vec<UHost>,
}

/// 生命周期动作（启停/重启/删除）的通用响应。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ActionResponse {
    /// 返回码
    #[serde(rename = "RetCode", default, deserialize_with = "de_ret_code")]
    pub ret_code: i64,
    /// 错误信息
    #[serde(rename = "Message", default)]
    pub message: String,
    /// 操作的实例 ID
    #[serde(rename = "UHostId", default)]
    pub uhost_id: String,
    /// 删除时是否进入回收站（`Yes` / `No`）
    #[serde(rename = "InRecycle", default)]
    pub in_recycle: String,
}

/// 机型下某个 GPU 数量对应的可选 CPU/内存组合。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MachineCollection {
    /// CPU 核数
    #[serde(rename = "Cpu", default)]
    pub cpu: u32,
    /// 可选内存列表，**单位 GB**
    #[serde(rename = "Memory", default)]
    pub memory_gb: Vec<u64>,
    /// 该组合支持的 CPU 平台
    #[serde(
        rename = "MinimalCpuPlatform",
        default,
        deserialize_with = "de_vec_or_null"
    )]
    pub cpu_platforms: Vec<String>,
}

/// GPU 数量档位。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MachineSize {
    /// GPU 卡数
    #[serde(rename = "Gpu", default)]
    pub gpu: u32,
    /// 该卡数下的合法组合
    #[serde(rename = "Collection", default, deserialize_with = "de_vec_or_null")]
    pub collection: Vec<MachineCollection>,
}

/// 显存信息。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct GraphicsMemory {
    /// 显存大小，**单位 GB**
    #[serde(rename = "Value", default)]
    pub value_gb: u64,
    /// 显存评级
    #[serde(rename = "Rate", default)]
    pub rate: u32,
}

/// 可售机型。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AvailableInstanceType {
    /// 机型名称，即创建接口的 `GpuType`
    #[serde(rename = "Name", default)]
    pub name: String,
    /// 可用区
    #[serde(rename = "Zone", default)]
    pub zone: String,
    /// 售卖状态：`Normal` / `SoldOut`
    #[serde(rename = "Status", default)]
    pub status: String,
    /// 机型描述
    #[serde(rename = "Description", default)]
    pub description: String,
    /// GPU 数量与 CPU/内存组合
    #[serde(rename = "MachineSizes", default, deserialize_with = "de_vec_or_null")]
    pub machine_sizes: Vec<MachineSize>,
    /// 显存信息
    #[serde(rename = "GraphicsMemory", default)]
    pub graphics_memory: Option<GraphicsMemory>,
}

impl AvailableInstanceType {
    /// 是否可售。
    pub fn is_on_sale(&self) -> bool {
        self.status.eq_ignore_ascii_case("Normal")
    }
}

/// `DescribeAvailableCompShareInstanceTypes` 响应。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AvailableTypesResponse {
    /// 返回码
    #[serde(rename = "RetCode", default, deserialize_with = "de_ret_code")]
    pub ret_code: i64,
    /// 错误信息
    #[serde(rename = "Message", default)]
    pub message: String,
    /// 机型列表
    #[serde(
        rename = "AvailableInstanceTypes",
        default,
        deserialize_with = "de_vec_or_null"
    )]
    pub types: Vec<AvailableInstanceType>,
}

/// 单个规格组合的库存可用性。
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct CapacitySpec {
    /// GPU 卡数
    #[serde(rename = "Gpu", default)]
    pub gpu: u32,
    /// CPU 核数
    #[serde(rename = "Cpu", default)]
    pub cpu: u32,
    /// 内存，**单位 GB**
    #[serde(rename = "Mem", default)]
    pub mem_gb: u64,
    /// 资源是否充足
    #[serde(rename = "ResourceEnough", default)]
    pub resource_enough: bool,
}

/// `CheckCompShareResourceCapacity` 响应。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CapacityResponse {
    /// 返回码
    #[serde(rename = "RetCode", default, deserialize_with = "de_ret_code")]
    pub ret_code: i64,
    /// 错误信息
    #[serde(rename = "Message", default)]
    pub message: String,
    /// 各组合的可用性
    #[serde(rename = "Specs", default, deserialize_with = "de_vec_or_null")]
    pub specs: Vec<CapacitySpec>,
}

impl CapacityResponse {
    /// 查询某个组合是否可创建。未出现在结果里视为不可用（保守）。
    pub fn is_enough(&self, gpu: u32, cpu: u32, mem_gb: u64) -> bool {
        self.specs
            .iter()
            .any(|s| s.gpu == gpu && s.cpu == cpu && s.mem_gb == mem_gb && s.resource_enough)
    }
}

/// 价格明细。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PriceDetail {
    /// 计费模式
    #[serde(rename = "ChargeType", default)]
    pub charge_type: String,
    /// 实例价格（CPU + 内存 + GPU）
    #[serde(rename = "Instance", default)]
    pub instance: Option<f64>,
    /// 数据盘价格
    #[serde(rename = "Disks", default)]
    pub disks: Option<f64>,
    /// 系统盘价格
    #[serde(rename = "SystemDisks", default)]
    pub system_disks: Option<f64>,
    /// 镜像价格（仅付费社区镜像）
    #[serde(rename = "CompShareImage", default)]
    pub image: Option<f64>,
    /// 折扣类型
    #[serde(rename = "InstanceDiscountType", default)]
    pub discount_type: Option<i64>,
}

impl PriceDetail {
    /// 分项求和。
    pub fn total(&self) -> f64 {
        [self.instance, self.disks, self.system_disks, self.image]
            .iter()
            .flatten()
            .sum()
    }
}

/// 价格查询请求（按规格询价，不含实例 ID）。
///
/// 字段与厂商 `GetCompShareInstanceUserPrice` 入参一一对应。
/// 单独成结构而非长参数表：本接口是**全部计费模式**的横向对比入口，
/// 参数多且易错位（`gpu` 与 `cpu` 都是 u32，传反不会报错）。
#[derive(Debug, Clone)]
pub struct UserPriceRequest<'a> {
    /// 显卡型号，如 `"4090"`
    pub gpu_type: &'a str,
    /// 显卡数量
    pub gpu: u32,
    /// CPU 核数
    pub cpu: u32,
    /// 内存（MiB）
    pub memory_mb: u64,
    /// 镜像 ID
    pub image_id: Option<&'a str>,
    /// 系统盘大小（GiB）
    pub disk_size_gb: u32,
    /// 系统盘类型，如 `"CLOUD_SSD"`
    pub disk_type: &'a str,
}

/// `GetCompShareInstancePrice` 响应。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PriceResponse {
    /// 返回码
    #[serde(rename = "RetCode", default, deserialize_with = "de_ret_code")]
    pub ret_code: i64,
    /// 错误信息
    #[serde(rename = "Message", default)]
    pub message: String,
    /// 折后价格明细
    #[serde(rename = "PriceDetails", default, deserialize_with = "de_vec_or_null")]
    pub price_details: Vec<PriceDetail>,
}

/// 平台镜像。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ImageInfo {
    /// 镜像 ID
    #[serde(rename = "CompShareImageId", default)]
    pub id: String,
    /// 镜像名称
    #[serde(rename = "Name", default)]
    pub name: String,
    /// 镜像类型
    #[serde(rename = "ImageType", default)]
    pub image_type: String,
    /// 作者
    #[serde(rename = "Author", default)]
    pub author: String,
    /// 状态（`Available` 才可用于创建）
    #[serde(rename = "Status", default)]
    pub status: String,
    /// 标签
    #[serde(rename = "Tags", default, deserialize_with = "de_vec_or_null")]
    pub tags: Vec<String>,
}

/// `DescribeCompShareImages` 响应。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ImagesResponse {
    /// 返回码
    #[serde(rename = "RetCode", default, deserialize_with = "de_ret_code")]
    pub ret_code: i64,
    /// 错误信息
    #[serde(rename = "Message", default)]
    pub message: String,
    /// 镜像总数
    #[serde(rename = "TotalCount", default)]
    pub total_count: u32,
    /// 镜像列表
    #[serde(rename = "ImageSet", default, deserialize_with = "de_vec_or_null")]
    pub image_set: Vec<ImageInfo>,
}

/// 可用区信息（`DescribeCompShareSupportZone` 的 `ZoneInfo` 元素）。
///
/// ## 两套标识，别混用
///
/// | 字段 | 类型 | 用途 | 示例 |
/// |------|------|------|------|
/// | `Region` / `Zone` | **字符串** | 业务参数（创建/查询实例时传这两个） | `cn-wlcb` / `cn-wlcb-01` |
/// | `RegionId` / `ZoneId` | **整数** | 仅厂商内部标识，业务接口不用 | `1000039` / `10027` |
///
/// 本项目所有对外参数传的都是**字符串形式**（`Region`/`Zone`），
/// 整数 ID 只做展示与排障——传错一套会被厂商判为非法参数。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ZoneInfo {
    /// 地域名称，如 `cn-wlcb`（业务参数用这个）
    #[serde(rename = "Region", default)]
    pub region: String,
    /// 地域数字 ID，如 `1000039`（仅内部标识）
    #[serde(rename = "RegionId", default)]
    pub region_id: i64,
    /// 可用区名称，如 `cn-wlcb-01`（业务参数用这个）
    #[serde(rename = "Zone", default)]
    pub zone: String,
    /// 可用区数字 ID，如 `10027`（仅内部标识）
    #[serde(rename = "ZoneId", default)]
    pub zone_id: i64,
    /// 显示名，如 `华北二A`
    #[serde(rename = "Describe", default)]
    pub describe: String,
    /// 是否为容器可用区。
    ///
    /// `true` 表示该可用区只跑容器镜像（无独立虚机、不可 SSH 登录），
    /// 与「虚机可用区」在部署形态上完全不同。调度时应优先选 `false` 的可用区。
    #[serde(rename = "IsPod", default)]
    pub is_pod: bool,
    /// 该可用区**不支持**的镜像类型列表（实测字段，文档未列出）。
    ///
    /// 实测取值：容器可用区为 `["System", "Other"]`，上海二B 为 `["Community"]`。
    /// 含义是「用这些类型的镜像在该可用区创建实例会失败」（厂商错误码 226603
    /// `所选镜像不支持该卡型`）。
    ///
    /// 这条信息能**在选点阶段就排除**不兼容的组合，避免走到创建才失败——
    /// 失败创建虽不收费，但要多花一次下单往返。
    #[serde(
        rename = "UnsupportedImageTypes",
        default,
        deserialize_with = "de_vec_or_null"
    )]
    pub unsupported_image_types: Vec<String>,
    /// 是否禁用镜像同步（实测字段，文档未列出）。
    ///
    /// 仅供展示/排障：`true` 时该可用区不会同步其它地域的镜像，
    /// 可选的镜像范围更窄。
    #[serde(rename = "DisableImageSync", default)]
    pub disable_image_sync: bool,
}

impl ZoneInfo {
    /// 是否为虚机可用区（可 SSH 登录，能部署本地推理服务）。
    pub fn is_vm(&self) -> bool {
        !self.is_pod
    }

    /// 资源类型的中文描述（用于展示）。
    pub fn resource_kind(&self) -> &'static str {
        if self.is_pod {
            "容器"
        } else {
            "云主机"
        }
    }

    /// `region/zone` 简写，与 [`super::CompshareClient::scope_label`] 一致。
    pub fn scope_label(&self) -> String {
        format!("{}/{}", self.region, self.zone)
    }

    /// 是否属于指定地域（大小写与空白不敏感）。
    pub fn in_region(&self, region: &str) -> bool {
        self.region.eq_ignore_ascii_case(region.trim())
    }

    /// 是否可用于部署（字段完整且是虚机可用区）。
    pub fn is_deployable(&self) -> bool {
        !self.region.trim().is_empty() && !self.zone.trim().is_empty() && self.is_vm()
    }

    /// 该可用区是否支持指定类型的镜像（`System` / `App` / `Custom` / `Community`）。
    ///
    /// 用于在选点阶段排除「镜像与可用区不兼容」的组合，
    /// 避免走到创建接口才被厂商拒绝（错误码 226603）。
    pub fn supports_image_type(&self, image_type: &str) -> bool {
        let t = image_type.trim();
        if t.is_empty() {
            return true;
        }
        !self
            .unsupported_image_types
            .iter()
            .any(|u| u.eq_ignore_ascii_case(t))
    }

    /// 是否支持系统镜像（本项目默认镜像类型）。
    ///
    /// 容器可用区通常不支持系统镜像，因此这一项同时反映了「能否用默认镜像部署」。
    pub fn supports_system_image(&self) -> bool {
        self.supports_image_type("System")
    }

    /// 可用于本项目部署：虚机 + 支持系统镜像。
    ///
    /// 比 [`Self::is_deployable`] 更严格——后者只看是否容器，
    /// 本方法还把镜像兼容性算进来，是选点时的实际判据。
    pub fn is_ready_for_deploy(&self, image_type: &str) -> bool {
        self.is_deployable() && self.supports_image_type(image_type)
    }
}

/// `DescribeCompShareSupportZone` 响应。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ZonesResponse {
    /// 返回码
    #[serde(rename = "RetCode", default, deserialize_with = "de_ret_code")]
    pub ret_code: i64,
    /// 错误信息
    #[serde(rename = "Message", default)]
    pub message: String,
    /// 请求唯一标识（报障时厂商要这个，务必保留）
    #[serde(rename = "request_uuid", default)]
    pub request_uuid: String,
    /// 可用区列表
    #[serde(rename = "ZoneInfo", default, deserialize_with = "de_vec_or_null")]
    pub zones: Vec<ZoneInfo>,
}

impl ZonesResponse {
    /// 按地域筛选（不区分大小写）。
    pub fn zones_in_region(&self, region: &str) -> Vec<&ZoneInfo> {
        self.zones.iter().filter(|z| z.in_region(region)).collect()
    }

    /// 去重后的地域列表，保持厂商返回顺序（便于稳定展示）。
    pub fn regions(&self) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        for z in &self.zones {
            if !z.region.is_empty() && !out.contains(&z.region.as_str()) {
                out.push(z.region.as_str());
            }
        }
        out
    }

    /// 可部署（虚机）的可用区。
    pub fn deployable(&self) -> Vec<&ZoneInfo> {
        self.zones.iter().filter(|z| z.is_deployable()).collect()
    }
}

/// 监控指标时间序列里的一个数据点。
#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct MonitorValue {
    /// Unix 时间戳（秒）
    #[serde(rename = "Timestamp", default)]
    pub timestamp: i64,
    /// 指标值。注意：使用率类指标是 **0~100 的百分数**，不是比例
    #[serde(rename = "Value", default)]
    pub value: f64,
}

/// 一组指标数据（`Results[].Values[]`）。
///
/// 厂商把数据点包在 `Results` 数组里，而 `Results` 通常只有一个元素。
/// 保留数组形态（不拍扁成单值）：多卡场景下同一 `MetricKey` 可能按
/// `gpu_bus_id` 分多个 `Results`，拍扁会丢掉除第一张卡之外的数据。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MonitorResult {
    /// 时间序列数据点
    #[serde(rename = "Values", default, deserialize_with = "de_vec_or_null")]
    pub values: Vec<MonitorValue>,
}

/// 单个监控指标。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MonitorMetric {
    /// 指标名，见 [`metric`]
    #[serde(rename = "MetricKey", default)]
    pub key: String,
    /// 标签（如 `uuid`、`gpu_bus_id`），用于区分同一指标的不同卡
    #[serde(rename = "Tags", default)]
    pub tags: serde_json::Value,
    /// 指标数据组
    #[serde(rename = "Results", default, deserialize_with = "de_vec_or_null")]
    pub results: Vec<MonitorResult>,
}

impl MonitorMetric {
    /// 取**时间最新**的数据点。
    ///
    /// 不假设 `Values` 已按时间升序：厂商并未承诺顺序，
    /// 直接取 `last()` 可能拿到最早的点，导致界面显示陈旧数据。
    pub fn latest(&self) -> Option<MonitorValue> {
        self.results
            .iter()
            .flat_map(|r| r.values.iter())
            .max_by_key(|v| v.timestamp)
            .copied()
    }

    /// 全部数据点里最新的时间戳。
    pub fn latest_timestamp(&self) -> Option<i64> {
        self.latest().map(|v| v.timestamp)
    }

    /// 同一指标的最大值（多卡场景取最忙的一张卡）。
    pub fn max_value(&self) -> Option<f64> {
        self.results
            .iter()
            .filter_map(|r| r.values.iter().map(|v| v.value).reduce(f64::max))
            .reduce(f64::max)
    }
}

/// 单个实例的监控数据。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MonitorInstance {
    /// 实例 ID
    #[serde(rename = "UHostId", default)]
    pub id: String,
    /// 指标列表
    #[serde(rename = "Metrics", default, deserialize_with = "de_vec_or_null")]
    pub metrics: Vec<MonitorMetric>,
}

impl MonitorInstance {
    /// 按指标名查找。
    pub fn find(&self, key: &str) -> Option<&MonitorMetric> {
        self.metrics
            .iter()
            .find(|m| m.key.eq_ignore_ascii_case(key))
    }

    /// 指标最新值（原始百分数）。
    pub fn value_of(&self, key: &str) -> Option<f64> {
        self.find(key).and_then(|m| m.latest()).map(|v| v.value)
    }

    /// 指标最大值（原始百分数，多卡取最忙）。
    pub fn max_of(&self, key: &str) -> Option<f64> {
        self.find(key).and_then(|m| m.max_value())
    }

    /// 全部指标里最新的采样时间。
    pub fn sampled_at(&self) -> i64 {
        self.metrics
            .iter()
            .filter_map(|m| m.latest_timestamp())
            .max()
            .unwrap_or(0)
    }
}

/// `GetCompShareInstanceMonitor` 的 `Data` 段。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MonitorData {
    /// 实例监控数据列表
    #[serde(rename = "List", default, deserialize_with = "de_vec_or_null")]
    pub list: Vec<MonitorInstance>,
}

/// `GetCompShareInstanceMonitor` 响应。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MonitorResponse {
    /// 返回码
    #[serde(rename = "RetCode", default, deserialize_with = "de_ret_code")]
    pub ret_code: i64,
    /// 错误信息
    #[serde(rename = "Message", default)]
    pub message: String,
    /// 监控数据（厂商在无数据点时返回 `null`）
    #[serde(rename = "Data", default, deserialize_with = "de_object_or_null")]
    pub data: MonitorData,
}

impl MonitorResponse {
    /// 按实例 ID 取监控数据。
    pub fn find(&self, instance_id: &str) -> Option<&MonitorInstance> {
        self.data.list.iter().find(|i| i.id == instance_id)
    }
}

/// 常见监控指标名（厂商原文）。
///
/// 只列实测/文档确认过的；未列出的指标名不应被丢弃，
/// 归一化时放进 `InstanceMetrics::extra`。
pub mod metric {
    /// CPU 使用率（%）
    pub const CPU_USED: &str = "uhost_cpu_used";
    /// 内存使用率（%）
    pub const MEMORY_USAGE: &str = "cloudwatch_memory_usage";
    /// 系统盘使用率（%）
    pub const SYS_DISK_USED: &str = "cloudwatch_sys_disk_used_per";
    /// 数据盘使用率（%）
    pub const DATA_DISK_USED: &str = "cloudwatch_data_disk_used_per";
    /// GPU 使用率（%）
    pub const GPU_UTIL: &str = "cloudwatch_gpu_util";
    /// GPU 显存使用率（%）
    pub const GPU_MEMORY_USAGE: &str = "cloudwatch_gpu_memory_usage";
}

// ============================================================
// 实例管理扩展接口（升降配 / 计费 / 端口 / 定时关机 / 带宽）
// ============================================================

/// `GetCompShareInstanceUpgradePrice` 响应。
///
/// 单位是**元**，不是「元/小时」——这是升降配的**差价总额**
/// （按当前计费周期的剩余时长折算），与 `GetCompShareInstancePrice`
/// 的时价语义完全不同，混用会把「补 356 元」显示成「356 元/小时」。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UpgradePriceResponse {
    /// 返回码
    #[serde(rename = "RetCode", default, deserialize_with = "de_ret_code")]
    pub ret_code: i64,
    /// 错误信息
    #[serde(rename = "Message", default)]
    pub message: String,
    /// 升配差价（元）。正数为补差价，负数为退费
    #[serde(rename = "Price", default)]
    pub price: f64,
    /// 折前原价差额（元）
    #[serde(rename = "OriginalPrice", default)]
    pub original_price: f64,
}

impl UpgradePriceResponse {
    /// 是否为退费（差价为负）。
    pub fn is_refund(&self) -> bool {
        self.price < 0.0
    }
}

/// 单个实例的退费信息。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RefundPriceItem {
    /// 实例 ID
    #[serde(rename = "UHostId", default)]
    pub id: String,
    /// 状态码，`0` 表示可退费
    #[serde(rename = "Code", default)]
    pub code: i64,
    /// 状态说明
    #[serde(rename = "Message", default)]
    pub message: String,
    /// 可退金额（元）
    #[serde(rename = "RefundPrice", default)]
    pub refund_price: f64,
}

impl RefundPriceItem {
    /// 是否可退费。
    pub fn is_refundable(&self) -> bool {
        self.code == 0
    }
}

/// `GetCompShareRefundPrice` 响应。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RefundPriceResponse {
    /// 返回码
    #[serde(rename = "RetCode", default, deserialize_with = "de_ret_code")]
    pub ret_code: i64,
    /// 错误信息
    #[serde(rename = "Message", default)]
    pub message: String,
    /// 退费信息列表
    #[serde(
        rename = "RefundPriceSet",
        default,
        deserialize_with = "de_vec_or_null"
    )]
    pub items: Vec<RefundPriceItem>,
}

impl RefundPriceResponse {
    /// 全部实例的可退金额合计（元）。
    ///
    /// 只统计「可退费」的条目：不可退的条目金额无意义，
    /// 累加会把「退不了」算成「能退这么多」。
    pub fn total_refundable(&self) -> f64 {
        self.items
            .iter()
            .filter(|i| i.is_refundable())
            .map(|i| i.refund_price)
            .sum()
    }

    /// 按实例 ID 查。
    pub fn find(&self, id: &str) -> Option<&RefundPriceItem> {
        self.items.iter().find(|i| i.id == id)
    }
}

/// 软件 → 端口映射。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SoftwarePortItem {
    /// 软件名称（如 `JupyterLab` / `SSH` / `FileBrowser`）
    #[serde(rename = "Software", default)]
    pub software: String,
    /// 端口号
    #[serde(rename = "Port", default)]
    pub port: u16,
}

/// `DescribeCompShareSoftwarePort` 响应。
///
/// ## 实测与文档的差异
///
/// 文档示例里列了 `{"Software":"SSH","Port":22}`，但实测（2026-10）
/// 只返回 4 个 Web 工具：`SD-WebUI` / `FileBrowser` / `ComfyUI` / `JupyterLab`。
///
/// **SSH 不在这个列表里**，它走容器实例的 `TcpForwards`（内部端口 23）。
/// 因此不要用「列表里有没有 SSH」来判断实例能否 SSH 登录。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SoftwarePortResponse {
    /// 返回码
    #[serde(rename = "RetCode", default, deserialize_with = "de_ret_code")]
    pub ret_code: i64,
    /// 错误信息
    #[serde(rename = "Message", default)]
    pub message: String,
    /// 软件端口列表
    #[serde(rename = "SoftwarePort", default, deserialize_with = "de_vec_or_null")]
    pub items: Vec<SoftwarePortItem>,
}

impl SoftwarePortResponse {
    /// 按软件名查端口（不区分大小写）。
    pub fn port_of(&self, software: &str) -> Option<u16> {
        self.items
            .iter()
            .find(|i| i.software.eq_ignore_ascii_case(software))
            .map(|i| i.port)
    }
}

/// `GetSoftwareURL` 响应。
///
/// 注意 Action 名是 `GetSoftwareURL`（全大写 URL），
/// 不是 `GetCompShareSoftwareURL`——写错会被判「未知 Action」。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SoftwareUrlResponse {
    /// 返回码
    #[serde(rename = "RetCode", default, deserialize_with = "de_ret_code")]
    pub ret_code: i64,
    /// 错误信息
    #[serde(rename = "Message", default)]
    pub message: String,
    /// 访问地址（如 `http://x.x.x.x:8888`）
    #[serde(rename = "URL", default)]
    pub url: String,
}

/// 机型族条目。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MachineTypeFamily {
    /// 机型名（如 `4090`）
    #[serde(rename = "Name", default)]
    pub name: String,
    /// 描述（如 `NVIDIA GeForce RTX 4090`）
    #[serde(rename = "Description", default)]
    pub description: String,
}

/// `DescribeCompShareMachineTypeFamilies` 响应。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MachineTypeFamiliesResponse {
    /// 返回码
    #[serde(rename = "RetCode", default, deserialize_with = "de_ret_code")]
    pub ret_code: i64,
    /// 错误信息
    #[serde(rename = "Message", default)]
    pub message: String,
    /// 机型族列表
    #[serde(rename = "MachineTypes", default, deserialize_with = "de_vec_or_null")]
    pub families: Vec<MachineTypeFamily>,
}

/// 某地域的网络加速状态。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct NetOptimizerInfo {
    /// 地域
    #[serde(rename = "Region", default)]
    pub region: String,
    /// 该地域是否已开通加速
    #[serde(rename = "Optimized", default)]
    pub optimized: bool,
}

/// `CheckCompShareNetOptimizer` 响应。
///
/// 网络加速影响**海外模型下载速度**（HuggingFace 等）。
/// 未开通时下载 7B 级模型会明显变慢，值得在部署前提示用户。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct NetOptimizerResponse {
    /// 返回码
    #[serde(rename = "RetCode", default, deserialize_with = "de_ret_code")]
    pub ret_code: i64,
    /// 错误信息
    #[serde(rename = "Message", default)]
    pub message: String,
    /// 是否已开通加速（全地域汇总）
    #[serde(rename = "Optimized", default)]
    pub optimized: bool,
    /// 各地域明细
    #[serde(rename = "Info", default, deserialize_with = "de_vec_or_null")]
    pub info: Vec<NetOptimizerInfo>,
}

impl NetOptimizerResponse {
    /// 指定地域是否已加速（查不到该地域时回退到全局 `optimized`）。
    pub fn optimized_in(&self, region: &str) -> bool {
        self.info
            .iter()
            .find(|i| i.region == region)
            .map(|i| i.optimized)
            .unwrap_or(self.optimized)
    }
}

/// 项目（`GetProjectList` 的 `ProjectSet` 元素）。
///
/// 定时关机相关接口**必须**带 `ProjectId`，而配置里通常没有这项，
/// 因此需要先查默认项目。`IsDefault` 用来挑出默认项目。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProjectItem {
    /// 项目 ID（如 `org-xxxx`）
    #[serde(rename = "ProjectId", default)]
    pub id: String,
    /// 项目名
    #[serde(rename = "ProjectName", default)]
    pub name: String,
    /// 是否为默认项目
    #[serde(rename = "IsDefault", default)]
    pub is_default: bool,
}

/// `GetProjectList` 响应。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProjectListResponse {
    /// 返回码
    #[serde(rename = "RetCode", default, deserialize_with = "de_ret_code")]
    pub ret_code: i64,
    /// 错误信息
    #[serde(rename = "Message", default)]
    pub message: String,
    /// 项目列表
    #[serde(rename = "ProjectSet", default, deserialize_with = "de_vec_or_null")]
    pub projects: Vec<ProjectItem>,
}

impl ProjectListResponse {
    /// 默认项目 ID；没有标默认的则取第一个。
    pub fn default_project_id(&self) -> Option<&str> {
        self.projects
            .iter()
            .find(|p| p.is_default)
            .or_else(|| self.projects.first())
            .map(|p| p.id.as_str())
            .filter(|id| !id.is_empty())
    }
}

/// 单个实例的带宽切换结果。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BandwidthSwitchResult {
    /// 实例 ID
    #[serde(rename = "UHostId", default)]
    pub id: String,
    /// 绑定的 EIP ID（未找到 EIP 时可能缺失）
    #[serde(rename = "EIPId", default)]
    pub eip_id: String,
    /// 该实例是否切换成功
    #[serde(rename = "Success", default)]
    pub success: bool,
    /// 处理结果或失败原因
    #[serde(rename = "Message", default)]
    pub message: String,
    /// 处理后的带宽归属（`Company` / `Public` / `WithoutGPU`）
    #[serde(rename = "CurrentScope", default)]
    pub current_scope: String,
}

/// `SwitchCompShareEIPShareBandwidth` 响应。
///
/// ## 顶层 `RetCode` 不代表每个实例都成功
///
/// 厂商是**逐实例处理**：顶层 `RetCode=0` 只说明请求被处理了，
/// 单个实例仍可能失败（如没有 EIP、绑定多个 EIP）。
/// 必须逐条检查 [`BandwidthSwitchResult::success`]。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BandwidthSwitchResponse {
    /// 顶层返回码
    #[serde(rename = "RetCode", default, deserialize_with = "de_ret_code")]
    pub ret_code: i64,
    /// 顶层错误信息
    #[serde(rename = "Message", default)]
    pub message: String,
    /// 请求唯一标识（报障用）
    #[serde(rename = "request_uuid", default)]
    pub request_uuid: String,
    /// 逐实例结果（按输入顺序）
    #[serde(rename = "Results", default, deserialize_with = "de_vec_or_null")]
    pub results: Vec<BandwidthSwitchResult>,
}

impl BandwidthSwitchResponse {
    /// 是否全部实例都切换成功。
    pub fn all_succeeded(&self) -> bool {
        self.ret_code == 0 && !self.results.is_empty() && self.results.iter().all(|r| r.success)
    }

    /// 失败的实例（ID + 原因），供上层原样展示。
    pub fn failures(&self) -> Vec<(String, String)> {
        self.results
            .iter()
            .filter(|r| !r.success)
            .map(|r| (r.id.clone(), r.message.clone()))
            .collect()
    }
}

/// 容器实例的端口映射条目。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TcpForward {
    /// 容器内端口
    #[serde(rename = "InternalPort", default)]
    pub internal_port: u16,
    /// 对外端口
    #[serde(rename = "ExternalPort", default)]
    pub external_port: u16,
    /// 对外主机
    #[serde(rename = "ExternalHost", default)]
    pub external_host: String,
}

/// `UpdateCompShareInstancePorts` 响应。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct InstancePortsResponse {
    /// 返回码
    #[serde(rename = "RetCode", default, deserialize_with = "de_ret_code")]
    pub ret_code: i64,
    /// 错误信息
    #[serde(rename = "Message", default)]
    pub message: String,
    /// 实例 ID
    #[serde(rename = "UHostId", default)]
    pub uhost_id: String,
    /// TCP 转发规则
    #[serde(rename = "TcpForwards", default, deserialize_with = "de_vec_or_null")]
    pub tcp_forwards: Vec<TcpForward>,
}

impl InstancePortsResponse {
    /// 取 SSH 的对外地址（容器内端口为 23 的那条）。
    ///
    /// 文档明确：SSH 要连 `InternalPort=23` 对应的
    /// `ExternalHost` + `ExternalPort`，不是实例公网 IP + 22。
    pub fn ssh_endpoint(&self) -> Option<(String, u16)> {
        self.tcp_forwards
            .iter()
            .find(|f| f.internal_port == 23 && !f.external_host.is_empty())
            .map(|f| (f.external_host.clone(), f.external_port))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_documented_instance_sample() {
        // 文档「响应示例」原样
        let body = r#"{
            "Action": "DescribeCompShareInstanceResponse",
            "RetCode": 0,
            "TotalCount": 1,
            "UHostSet": [{
                "UHostId": "uhost-xxxx",
                "Name": "my-gpu-instance",
                "State": "Running",
                "Zone": "cn-wlcb-01",
                "Region": "cn-wlcb",
                "MachineType": "G",
                "CPU": 16,
                "Memory": 65536,
                "GPU": 1,
                "GpuType": "4090",
                "ChargeType": "Month",
                "CreateTime": 1712563200,
                "CompShareImageId": "compshareImage-xxxx",
                "CompShareImageName": "PyTorch 2.1",
                "InstanceType": "Container",
                "OsType": "Linux",
                "SshLoginCommand": "ssh root@x.x.x.x -p 60022",
                "IPSet": [{"IPId": "eip-xxxx", "IP": "203.0.113.10", "IPMode": "IPv4", "Type": "Bgp"}],
                "DiskSet": [],
                "SupportWithoutGpuStart": true
            }]
        }"#;
        let r: DescribeInstancesResponse = serde_json::from_str(body).unwrap();
        assert_eq!(r.ret_code, 0);
        assert_eq!(r.total_count, 1);
        let h = &r.uhost_set[0];
        assert_eq!(h.id, "uhost-xxxx");
        assert_eq!(h.state, "Running");
        assert_eq!(h.memory_mb, 65536);
        assert_eq!(h.gpu, 1);
        assert_eq!(h.primary_ip(), Some("203.0.113.10"));
        assert!(h.support_without_gpu_start);
    }

    #[test]
    fn null_lists_are_treated_as_empty() {
        // 实测发现：厂商在没有匹配结果时返回 `"ImageSet": null`，
        // 而不是 `[]`。默认 Vec 反序列化会报
        // `invalid type: null, expected a sequence`，把「无数据」误判成协议错误。
        let imgs: ImagesResponse =
            serde_json::from_str(r#"{"RetCode":0,"TotalCount":0,"ImageSet":null}"#).unwrap();
        assert!(imgs.image_set.is_empty());
        assert_eq!(imgs.total_count, 0);

        let inst: DescribeInstancesResponse =
            serde_json::from_str(r#"{"RetCode":0,"TotalCount":0,"UHostSet":null}"#).unwrap();
        assert!(inst.uhost_set.is_empty());

        let zones: ZonesResponse =
            serde_json::from_str(r#"{"RetCode":0,"ZoneInfo":null}"#).unwrap();
        assert!(zones.zones.is_empty());

        let types: AvailableTypesResponse =
            serde_json::from_str(r#"{"RetCode":0,"AvailableInstanceTypes":null}"#).unwrap();
        assert!(types.types.is_empty());

        let cap: CapacityResponse = serde_json::from_str(r#"{"RetCode":0,"Specs":null}"#).unwrap();
        assert!(cap.specs.is_empty());

        let price: PriceResponse =
            serde_json::from_str(r#"{"RetCode":0,"PriceDetails":null}"#).unwrap();
        assert!(price.price_details.is_empty());

        let create: CreateInstanceResponse =
            serde_json::from_str(r#"{"RetCode":0,"UHostIds":null,"Warning":null}"#).unwrap();
        assert!(create.uhost_ids.is_empty());
        assert!(create.warning.is_empty());
    }

    #[test]
    fn nested_nulls_do_not_break_instance_parsing() {
        // 实例内部的列表字段同样可能为 null
        let r: DescribeInstancesResponse =
            serde_json::from_str(r#"{"RetCode":0,"UHostSet":[{"UHostId":"u-1","IPSet":null}]}"#)
                .unwrap();
        assert_eq!(r.uhost_set[0].id, "u-1");
        assert!(r.uhost_set[0].primary_ip().is_none());

        // 机型表内的嵌套列表
        let t: AvailableTypesResponse = serde_json::from_str(
            r#"{"RetCode":0,"AvailableInstanceTypes":[
                {"Name":"4090","Status":"Normal","MachineSizes":null}
            ]}"#,
        )
        .unwrap();
        assert!(t.types[0].machine_sizes.is_empty());
    }

    #[test]
    fn parses_documented_zone_sample() {
        // 文档「响应示例」原样，逐字段核对
        let body = r#"{
            "Action": "DescribeCompShareSupportZoneResponse",
            "RetCode": 0,
            "request_uuid": "e897f547-bd2d-44dd-9de0-415017aa65ad",
            "ZoneInfo": [
                {"Region":"cn-wlcb","RegionId":1000039,"Zone":"cn-wlcb-01","ZoneId":10027,"Describe":"华北二A","IsPod":false},
                {"Region":"cn-sh2","RegionId":1000009,"Zone":"cn-sh2-02","ZoneId":8200,"Describe":"上海二B","IsPod":false},
                {"Region":"cn-bj2","RegionId":1000001,"Zone":"cn-bj2-03","ZoneId":5001,"Describe":"华北一C","IsPod":true},
                {"Region":"cn-sh2","RegionId":1000009,"Zone":"cn-sh2-01","ZoneId":8100,"Describe":"上海二A","IsPod":true},
                {"Region":"cn-sh2","RegionId":1000009,"Zone":"cn-sh2-03","ZoneId":8300,"Describe":"上海二C","IsPod":true}
            ]
        }"#;
        let r: ZonesResponse = serde_json::from_str(body).unwrap();
        assert_eq!(r.ret_code, 0);
        // request_uuid 必须保留：报障时厂商要它
        assert_eq!(r.request_uuid, "e897f547-bd2d-44dd-9de0-415017aa65ad");
        assert_eq!(r.zones.len(), 5);

        // 第一项逐字段核对
        let first = &r.zones[0];
        assert_eq!(first.region, "cn-wlcb");
        assert_eq!(first.region_id, 1_000_039);
        assert_eq!(first.zone, "cn-wlcb-01");
        assert_eq!(first.zone_id, 10_027);
        assert_eq!(first.describe, "华北二A");
        assert!(!first.is_pod);
    }

    #[test]
    fn zone_helpers_classify_vm_vs_container() {
        let body = r#"{"RetCode":0,"ZoneInfo":[
            {"Region":"cn-wlcb","Zone":"cn-wlcb-01","Describe":"华北二A","IsPod":false},
            {"Region":"cn-sh2","Zone":"cn-sh2-02","Describe":"上海二B","IsPod":false},
            {"Region":"cn-bj2","Zone":"cn-bj2-03","Describe":"华北一C","IsPod":true},
            {"Region":"cn-sh2","Zone":"cn-sh2-01","Describe":"上海二A","IsPod":true},
            {"Region":"cn-sh2","Zone":"cn-sh2-03","Describe":"上海二C","IsPod":true}
        ]}"#;
        let r: ZonesResponse = serde_json::from_str(body).unwrap();

        // 虚机可用区只有 2 个
        let vm = r.deployable();
        assert_eq!(vm.len(), 2);
        assert!(vm.iter().all(|z| z.is_vm()));
        assert_eq!(vm[0].describe, "华北二A");
        assert_eq!(vm[1].describe, "上海二B");

        // 地域去重，保持厂商返回顺序
        assert_eq!(r.regions(), vec!["cn-wlcb", "cn-sh2", "cn-bj2"]);

        // 按地域筛选
        let sh = r.zones_in_region("cn-sh2");
        assert_eq!(sh.len(), 3);
        assert!(sh.iter().all(|z| z.region == "cn-sh2"));
        // 大小写不敏感
        assert_eq!(r.zones_in_region("CN-WLCB").len(), 1);
        assert!(r.zones_in_region("nowhere").is_empty());
    }

    #[test]
    fn zone_resource_kind_and_label() {
        let vm = ZoneInfo {
            region: "cn-wlcb".into(),
            zone: "cn-wlcb-01".into(),
            is_pod: false,
            ..Default::default()
        };
        assert_eq!(vm.resource_kind(), "云主机");
        assert_eq!(vm.scope_label(), "cn-wlcb/cn-wlcb-01");
        assert!(vm.is_deployable());

        let pod = ZoneInfo {
            region: "cn-bj2".into(),
            zone: "cn-bj2-03".into(),
            is_pod: true,
            ..Default::default()
        };
        assert_eq!(pod.resource_kind(), "容器");
        // 容器可用区不可部署（无独立虚机、不能 SSH）
        assert!(!pod.is_deployable());
    }

    #[test]
    fn zone_without_ids_still_parses() {
        // 文档未承诺 ID 一定有值；缺失不应导致解析失败
        let r: ZonesResponse = serde_json::from_str(
            r#"{"RetCode":0,"ZoneInfo":[{"Region":"cn-wlcb","Zone":"cn-wlcb-01"}]}"#,
        )
        .unwrap();
        assert_eq!(r.zones[0].region_id, 0);
        assert_eq!(r.zones[0].zone_id, 0);
        assert_eq!(r.zones[0].describe, "");
        // 缺字段时仍可部署（虚机默认 false）
        assert!(r.zones[0].is_deployable());
    }

    #[test]
    fn zone_image_type_compatibility() {
        // 实测数据：容器可用区把 System/Other 列为不支持
        let container = ZoneInfo {
            region: "cn-bj2".into(),
            zone: "cn-bj2-03".into(),
            is_pod: true,
            unsupported_image_types: vec!["System".into(), "Other".into()],
            ..Default::default()
        };
        assert!(!container.supports_image_type("System"));
        assert!(!container.supports_image_type("system")); // 大小写不敏感
        assert!(container.supports_image_type("App"));
        assert!(!container.supports_system_image());
        // 容器区本身已不可部署
        assert!(!container.is_ready_for_deploy("System"));

        // 上海二B：支持 System，但不支持 Community
        let sh2 = ZoneInfo {
            region: "cn-sh2".into(),
            zone: "cn-sh2-02".into(),
            is_pod: false,
            unsupported_image_types: vec!["Community".into()],
            ..Default::default()
        };
        assert!(sh2.supports_system_image());
        assert!(!sh2.supports_image_type("Community"));
        // 系统镜像可用 → 可部署
        assert!(sh2.is_ready_for_deploy("System"));
        // 社区镜像不可用 → 不可部署
        assert!(!sh2.is_ready_for_deploy("Community"));

        // 空镜像类型视为不限制
        assert!(container.supports_image_type(""));
    }

    #[test]
    fn zone_parses_undocumented_fields() {
        // 实测响应含文档未列出的 DisableImageSync / UnsupportedImageTypes
        let body = r#"{"RetCode":0,"ZoneInfo":[{
            "Describe":"上海二B","DisableImageSync":false,"IsPod":false,
            "Region":"cn-sh2","RegionId":1000009,
            "UnsupportedImageTypes":["Community"],
            "Zone":"cn-sh2-02","ZoneId":8200
        }]}"#;
        let r: ZonesResponse = serde_json::from_str(body).unwrap();
        let z = &r.zones[0];
        assert_eq!(z.unsupported_image_types, vec!["Community"]);
        assert!(!z.disable_image_sync);
    }

    #[test]
    fn missing_optional_fields_do_not_fail_parsing() {
        let body = r#"{"RetCode":0,"UHostSet":[{"UHostId":"u-1"}]}"#;
        let r: DescribeInstancesResponse = serde_json::from_str(body).unwrap();
        assert_eq!(r.uhost_set[0].id, "u-1");
        assert_eq!(r.uhost_set[0].cpu, 0);
        assert!(r.uhost_set[0].primary_ip().is_none());
    }

    #[test]
    fn parses_available_types_sample() {
        let body = r#"{
            "RetCode": 0,
            "AvailableInstanceTypes": [{
                "Name": "4090",
                "Zone": "cn-wlcb-01",
                "Status": "Normal",
                "Description": "RTX40系",
                "MachineSizes": [
                    {"Gpu": 1, "Collection": [{"Cpu": 16, "Memory": [64, 94], "MinimalCpuPlatform": ["Amd/Epyc2"]}]},
                    {"Gpu": 2, "Collection": [{"Cpu": 32, "Memory": [128, 192]}]}
                ],
                "GraphicsMemory": {"Value": 24, "Rate": 3}
            }]
        }"#;
        let r: AvailableTypesResponse = serde_json::from_str(body).unwrap();
        let t = &r.types[0];
        assert!(t.is_on_sale());
        assert_eq!(t.graphics_memory.as_ref().map(|g| g.value_gb), Some(24));
        assert_eq!(t.machine_sizes.len(), 2);
        assert_eq!(t.machine_sizes[0].collection[0].memory_gb, vec![64, 94]);
    }

    #[test]
    fn sold_out_is_not_on_sale() {
        let mut t = AvailableInstanceType {
            status: "SoldOut".into(),
            ..Default::default()
        };
        assert!(!t.is_on_sale());
        t.status = "normal".into();
        assert!(t.is_on_sale());
    }

    #[test]
    fn capacity_lookup_is_conservative() {
        let body = r#"{"RetCode":0,"Specs":[
            {"Gpu":1,"Cpu":16,"Mem":32,"ResourceEnough":true},
            {"Gpu":2,"Cpu":32,"Mem":128,"ResourceEnough":false}
        ]}"#;
        let r: CapacityResponse = serde_json::from_str(body).unwrap();
        assert!(r.is_enough(1, 16, 32));
        assert!(!r.is_enough(2, 32, 128));
        // 未出现的组合一律视为不可用
        assert!(!r.is_enough(1, 16, 64));
    }

    #[test]
    fn price_detail_sums_all_components() {
        let body = r#"{"RetCode":0,"PriceDetails":[
            {"ChargeType":"Month","Instance":1068.0,"SystemDisks":120.0}
        ]}"#;
        let r: PriceResponse = serde_json::from_str(body).unwrap();
        assert_eq!(r.price_details[0].total(), 1188.0);

        let d = PriceDetail {
            instance: Some(1.5),
            disks: Some(0.5),
            system_disks: Some(0.2),
            image: Some(0.1),
            ..Default::default()
        };
        assert!((d.total() - 2.3).abs() < 1e-9);
    }

    #[test]
    fn price_detail_without_values_is_zero() {
        assert_eq!(PriceDetail::default().total(), 0.0);
    }

    #[test]
    fn parses_images_and_zones() {
        let imgs: ImagesResponse = serde_json::from_str(
            r#"{"RetCode":0,"TotalCount":1,"ImageSet":[{"CompShareImageId":"img-1","Name":"PyTorch 2.1","ImageType":"System","Author":"UCloud","Status":"Available","Tags":["深度学习"]}]}"#,
        )
        .unwrap();
        assert_eq!(imgs.image_set[0].id, "img-1");
        assert_eq!(imgs.image_set[0].tags.len(), 1);

        let zones: ZonesResponse = serde_json::from_str(
            r#"{"RetCode":0,"ZoneInfo":[{"Region":"cn-wlcb","Zone":"cn-wlcb-01","Describe":"华北二A","IsPod":false}]}"#,
        )
        .unwrap();
        assert_eq!(zones.zones[0].zone, "cn-wlcb-01");
        assert!(!zones.zones[0].is_pod);
    }

    #[test]
    fn create_response_collects_ids_and_warnings() {
        let r: CreateInstanceResponse = serde_json::from_str(
            r#"{"Action":"CreateCompShareInstanceResponse","RetCode":0,"UHostIds":["uhost-1"],"Warning":["库存紧张"]}"#,
        )
        .unwrap();
        assert_eq!(r.uhost_ids, vec!["uhost-1"]);
        assert_eq!(r.warning.len(), 1);
    }

    #[test]
    fn primary_ip_prefers_bgp() {
        let h = UHost {
            ip_set: vec![
                IpInfo {
                    ip: "10.0.0.1".into(),
                    kind: "Internal".into(),
                    mode: "IPv4".into(),
                },
                IpInfo {
                    ip: "1.2.3.4".into(),
                    kind: "Bgp".into(),
                    mode: "IPv4".into(),
                },
            ],
            ..Default::default()
        };
        assert_eq!(h.primary_ip(), Some("1.2.3.4"));

        // 无 BGP 时退回第一个非空
        let h2 = UHost {
            ip_set: vec![
                IpInfo {
                    ip: String::new(),
                    kind: "Bgp".into(),
                    mode: "IPv4".into(),
                },
                IpInfo {
                    ip: "5.6.7.8".into(),
                    kind: "Other".into(),
                    mode: "IPv4".into(),
                },
            ],
            ..Default::default()
        };
        assert_eq!(h2.primary_ip(), Some("5.6.7.8"));
    }
}
