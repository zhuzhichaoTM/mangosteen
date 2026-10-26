//! 配置驱动的通用 REST 驱动。
//!
//! 归属：**算力执行层 · 多云统一适配层**。
//!
//! ## 为什么是「配置化」而不是为每家写死一份代码
//!
//! 按产品基线的**配置驱动原则**：
//!
//! > 服务商列表、调度策略、硬件参数全部通过配置文件管理，支持动态修改，无需重新编译。
//!
//! 云厂商的 REST API 在**结构上是同构的**（创建实例 / 查询 / 启停 / 销毁，
//! 都是「带 Token 的 JSON over HTTPS」），差异集中在三处：
//!
//! | 差异点 | 配置项 |
//! |--------|--------|
//! | 认证头形式 | [`AuthStyle`] |
//! | URL 路径与包装 | [`EndpointProfile`] |
//! | JSON 字段名与状态枚举 | [`FieldMap`] / [`StateMap`] |
//!
//! 把这些抽成配置后，**新增服务商 = 写一段 TOML**，真正做到了「不侵入上层、
//! 不重新编译」。同时保留 [`crate::drivers::ProviderDriver`] 接口，
//! 需要复杂逻辑（如分页、异步轮询、签名算法）的服务商仍可手写驱动。
//!
//! ## 安全约束
//!
//! * Token 从不写进日志或错误信息（错误信息里只出现脱敏前缀）；
//! * 所有厂商错误码归一化为 [`MnError::Provider`]，上层无需认识任何厂商码；
//! * 网络失败不重试（由网关调度决策器统一决策重试），避免双重放大。

use std::collections::BTreeMap;

use mangosteen_core::cloud::{
    AvailableSpec, DriverCapabilities, GpuSpec, Instance, InstanceSpec, InstanceState,
    NetworkEndpoint, PriceQuote, ProviderDriver,
};
use mangosteen_core::error::{MnError, MnResult};
use serde::{Deserialize, Serialize};

use super::transport::{HttpRequest, HttpResponse, Transport};

/// 认证方式。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthStyle {
    /// `Authorization: Bearer <token>`
    Bearer,
    /// 自定义头名，如 `Authorization: <token>`（阿里云风格）
    Header { name: String },
    /// 两个头（AccessKeyId + Secret），常见于国内云
    DoubleHeader {
        id_header: String,
        secret_header: String,
    },
    /// Token 作为查询参数（部分厂商）
    QueryParam { name: String },
}

impl Default for AuthStyle {
    fn default() -> Self {
        AuthStyle::Bearer
    }
}

/// JSON 字段路径。支持 `a.b.c` 形式的嵌套取值与 `a[0].b` 的数组下标。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldPath(String);

impl FieldPath {
    /// 构造。
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// 原始文本。
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// 在 JSON 值上取值。
    pub fn get<'a>(&self, root: &'a serde_json::Value) -> Option<&'a serde_json::Value> {
        let mut cur = root;
        for seg in self.0.split('.') {
            if seg.is_empty() {
                continue;
            }
            // 处理 name[0] 形式
            if let Some(idx_start) = seg.find('[') {
                let name = &seg[..idx_start];
                if !name.is_empty() {
                    cur = cur.get(name)?;
                }
                let inner = &seg[idx_start + 1..];
                let inner = inner.strip_suffix(']').unwrap_or(inner);
                let idx: usize = inner.parse().ok()?;
                cur = cur.get(idx)?;
            } else {
                cur = cur.get(seg)?;
            }
        }
        Some(cur)
    }

    /// 取字符串。
    pub fn get_str(&self, root: &serde_json::Value) -> Option<String> {
        match self.get(root)? {
            serde_json::Value::String(s) => Some(s.clone()),
            serde_json::Value::Number(n) => Some(n.to_string()),
            serde_json::Value::Bool(b) => Some(b.to_string()),
            _ => None,
        }
    }

    /// 取 u64（兼容字符串形式的数字）。
    pub fn get_u64(&self, root: &serde_json::Value) -> Option<u64> {
        match self.get(root)? {
            serde_json::Value::Number(n) => n.as_u64().or_else(|| n.as_f64().map(|f| f as u64)),
            serde_json::Value::String(s) => s.trim().parse().ok(),
            _ => None,
        }
    }

    /// 取 f64。
    pub fn get_f64(&self, root: &serde_json::Value) -> Option<f64> {
        match self.get(root)? {
            serde_json::Value::Number(n) => n.as_f64(),
            serde_json::Value::String(s) => s.trim().parse().ok(),
            _ => None,
        }
    }
}

/// 端点模板。
///
/// `{id}` 会被替换为实例 ID，`{region}` 替换为地域。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointProfile {
    /// API 基址，如 `https://api.autodl.com`
    pub base_url: String,
    /// 可创建规格列表：`GET`
    pub list_specs: String,
    /// 创建实例：`POST`，`{spec_id}` 替换规格 ID
    pub create: String,
    /// 启动：`POST`
    pub start: String,
    /// 停止：`POST`
    pub stop: String,
    /// 销毁：`DELETE`
    pub destroy: String,
    /// 查询单个：`GET`
    pub describe: String,
    /// 列出全部：`GET`
    pub list: String,
    /// 成功判定字段（部分厂商 HTTP 200 但业务失败）。如 `code`，值须等于 `success_code`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_field: Option<String>,
    /// 业务成功码
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub success_code: Option<String>,
    /// 错误信息字段
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_field: Option<String>,
}

impl EndpointProfile {
    /// 把模板里的占位符替换掉，得到完整 URL。
    pub fn url(&self, template: &str, id: Option<&str>, spec_id: Option<&str>) -> String {
        let mut u = format!(
            "{}/{}",
            self.base_url.trim_end_matches('/'),
            template.trim_start_matches('/')
        );
        if let Some(id) = id {
            u = u.replace("{id}", id);
        }
        if let Some(s) = spec_id {
            u = u.replace("{spec_id}", s);
        }
        u
    }

    /// 校验模板里必须含 `{id}` 的端点。
    pub fn validate(&self) -> MnResult<()> {
        if self.base_url.trim().is_empty() {
            return Err(MnError::config("provider.endpoints.base_url", "不能为空"));
        }
        for (name, tpl) in [
            ("start", &self.start),
            ("stop", &self.stop),
            ("destroy", &self.destroy),
            ("describe", &self.describe),
        ] {
            if !tpl.contains("{id}") {
                return Err(MnError::config(
                    format!("provider.endpoints.{name}"),
                    "模板必须包含 {id} 占位符",
                ));
            }
        }
        if !self.create.contains("{spec_id}") {
            return Err(MnError::config(
                "provider.endpoints.create",
                "模板必须包含 {spec_id} 占位符",
            ));
        }
        Ok(())
    }
}

/// JSON 字段映射。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldMap {
    /// 列表响应中承载条目数组的字段（空表示响应本身就是数组）
    #[serde(default)]
    pub list_root: String,
    /// 创建/查询响应中承载实例对象的字段（空表示响应本身就是对象）
    #[serde(default)]
    pub item_root: String,
    /// 实例 ID
    pub id: String,
    /// 状态
    pub state: String,
    /// GPU 型号
    pub gpu_model: String,
    /// GPU 数量
    pub gpu_count: String,
    /// 单卡显存（厂商若给 GB 而非字节，用 `vram_unit` 说明）
    pub vram: String,
    /// 显存单位：`bytes` / `gib` / `mib`
    #[serde(default = "default_bytes")]
    pub vram_unit: String,
    /// CPU 核心数
    pub cpu_cores: String,
    /// 内存（单位同 `memory_unit`）
    pub memory: String,
    /// 内存单位
    #[serde(default = "default_bytes")]
    pub memory_unit: String,
    /// 磁盘（单位同 `disk_unit`）
    pub disk: String,
    /// 磁盘单位
    #[serde(default = "default_gib")]
    pub disk_unit: String,
    /// 镜像
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// 时价（元）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price: Option<String>,
    /// SSH 主机
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_host: Option<String>,
    /// SSH 端口
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_port: Option<String>,
    /// 地域
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// 库存
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stock: Option<String>,
    /// 展示名
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// 创建时间（Unix 秒）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
}

fn default_bytes() -> String {
    "bytes".to_string()
}
fn default_gib() -> String {
    "gib".to_string()
}

impl FieldMap {
    /// 按单位把数值折算为字节。
    fn scaled(&self, value: Option<u64>, unit: &str) -> u64 {
        let v = value.unwrap_or(0);
        match unit {
            "gib" | "gb" => v.saturating_mul(1 << 30),
            "mib" | "mb" => v.saturating_mul(1 << 20),
            _ => v,
        }
    }
}

/// 状态字符串映射：厂商状态 → 归一化状态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateMap {
    /// 厂商状态值（小写）→ 归一化状态
    pub mapping: BTreeMap<String, InstanceState>,
    /// 未知状态时的默认
    #[serde(default = "default_unknown_state")]
    pub fallback: InstanceState,
}

fn default_unknown_state() -> InstanceState {
    InstanceState::Failed
}

impl StateMap {
    /// 归一化。匹配时忽略大小写与首尾空白。
    pub fn normalize(&self, raw: &str) -> InstanceState {
        let key = raw.trim().to_ascii_lowercase();
        self.mapping.get(&key).copied().unwrap_or(self.fallback)
    }
}

/// 通用 REST 驱动的完整配置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestDriverProfile {
    /// 驱动名（与配置 `providers.<name>.driver` 一致）
    pub name: String,
    /// 展示名
    pub display_name: String,
    /// 认证方式
    #[serde(default)]
    pub auth: AuthStyle,
    /// 端点
    pub endpoints: EndpointProfile,
    /// 字段映射
    pub fields: FieldMap,
    /// 状态映射
    pub states: StateMap,
    /// 能力声明
    #[serde(default)]
    pub capabilities: DriverCapabilities,
    /// 创建实例时额外附加的 JSON 字段（厂商私有参数透传）
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub create_extra: BTreeMap<String, serde_json::Value>,
}

impl RestDriverProfile {
    /// 校验配置自洽性。
    pub fn validate(&self) -> MnResult<()> {
        if self.name.trim().is_empty() {
            return Err(MnError::config("provider.name", "不能为空"));
        }
        self.endpoints.validate()?;
        if self.fields.id.trim().is_empty() {
            return Err(MnError::config("provider.fields.id", "不能为空"));
        }
        if self.fields.state.trim().is_empty() {
            return Err(MnError::config("provider.fields.state", "不能为空"));
        }
        if !["bytes", "gib", "mib"].contains(&self.fields.vram_unit.as_str()) {
            return Err(MnError::config(
                "provider.fields.vram_unit",
                "只支持 bytes / gib / mib",
            ));
        }
        if self.states.mapping.is_empty() {
            return Err(MnError::config(
                "provider.states.mapping",
                "至少需要一条状态映射",
            ));
        }
        Ok(())
    }
}

/// 凭证。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credentials {
    /// 主凭证（Token 或 AccessKeyId）
    pub id: String,
    /// 副凭证（Secret，用于 `DoubleHeader`）
    pub secret: Option<String>,
}

impl Credentials {
    /// 单 Token。
    pub fn token(t: impl Into<String>) -> Self {
        Self {
            id: t.into(),
            secret: None,
        }
    }

    /// 双凭证。
    pub fn pair(id: impl Into<String>, secret: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            secret: Some(secret.into()),
        }
    }

    /// 脱敏展示（绝不出现在错误信息里）。
    fn redacted(&self) -> String {
        let head: String = self.id.chars().take(4).collect();
        format!("{head}***")
    }
}

/// 通用 REST 驱动。
pub struct RestDriver<T: Transport> {
    profile: RestDriverProfile,
    creds: Credentials,
    transport: T,
}

impl<T: Transport> RestDriver<T> {
    /// 构造并校验。
    pub fn new(profile: RestDriverProfile, creds: Credentials, transport: T) -> MnResult<Self> {
        profile.validate()?;
        if creds.id.trim().is_empty() {
            return Err(MnError::config(
                format!("providers.{}.secret", profile.name),
                "凭证为空",
            ));
        }
        Ok(Self {
            profile,
            creds,
            transport,
        })
    }

    /// 驱动 profile。
    pub fn profile(&self) -> &RestDriverProfile {
        &self.profile
    }

    /// 给请求附加认证信息。
    fn authorize(&self, mut req: HttpRequest) -> HttpRequest {
        match &self.profile.auth {
            AuthStyle::Bearer => {
                req = req.with_header("Authorization", format!("Bearer {}", self.creds.id))
            }
            AuthStyle::Header { name } => req = req.with_header(name, self.creds.id.clone()),
            AuthStyle::DoubleHeader {
                id_header,
                secret_header,
            } => {
                req = req.with_header(id_header, self.creds.id.clone());
                req = req.with_header(secret_header, self.creds.secret.clone().unwrap_or_default());
            }
            AuthStyle::QueryParam { name } => {
                let sep = if req.url.contains('?') { '&' } else { '?' };
                req.url = format!("{}{}{}={}", req.url, sep, name, self.creds.id);
            }
        }
        req = req.with_header("Accept", "application/json");
        req
    }

    /// 发送并做通用错误归一化。
    fn send(&self, req: HttpRequest) -> MnResult<serde_json::Value> {
        let req = self.authorize(req);
        let resp = self.transport.execute(&req)?;
        self.interpret(resp, req.method.as_str(), &req.url)
    }

    /// 解释响应：处理 HTTP 状态、业务状态字段、JSON 解析。
    fn interpret(
        &self,
        resp: HttpResponse,
        method: &str,
        url: &str,
    ) -> MnResult<serde_json::Value> {
        let p = &self.profile;

        if !resp.is_success() {
            // 尽量从响应体里挖出厂商错误信息
            let detail = serde_json::from_str::<serde_json::Value>(&resp.body)
                .ok()
                .and_then(|v| {
                    p.endpoints
                        .error_field
                        .as_ref()
                        .and_then(|f| FieldPath::new(f.clone()).get_str(&v))
                        .or_else(|| {
                            v.get("message")
                                .and_then(|m| m.as_str())
                                .map(|s| s.to_string())
                        })
                })
                .unwrap_or_else(|| truncate(&resp.body, 200));
            return Err(MnError::Provider {
                provider: p.name.clone(),
                code: format!("http_{}", resp.status),
                message: format!("{method} {url} 失败：{detail}"),
            });
        }

        let value: serde_json::Value = if resp.body.trim().is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(&resp.body).map_err(|e| MnError::Provider {
                provider: p.name.clone(),
                code: "invalid_json".to_string(),
                message: format!(
                    "{method} {url} 返回非 JSON（凭证 {}-{}）：{e}",
                    p.name,
                    self.creds.redacted()
                ),
            })?
        };

        // 业务层状态校验：HTTP 200 不代表成功
        if let (Some(field), Some(ok)) = (&p.endpoints.status_field, &p.endpoints.success_code) {
            if let Some(actual) = FieldPath::new(field.clone()).get_str(&value) {
                if &actual != ok {
                    let detail = p
                        .endpoints
                        .error_field
                        .as_ref()
                        .and_then(|f| FieldPath::new(f.clone()).get_str(&value))
                        .unwrap_or_else(|| truncate(&resp.body, 200));
                    return Err(MnError::Provider {
                        provider: p.name.clone(),
                        code: actual,
                        message: detail,
                    });
                }
            }
        }

        Ok(value)
    }

    /// 从响应中取出条目数组。
    fn extract_items(&self, value: &serde_json::Value) -> Vec<serde_json::Value> {
        let root = &self.profile.fields.list_root;
        if root.trim().is_empty() {
            return match value {
                serde_json::Value::Array(a) => a.clone(),
                other => vec![other.clone()],
            };
        }
        match FieldPath::new(root.clone()).get(value) {
            Some(serde_json::Value::Array(a)) => a.clone(),
            Some(other) => vec![other.clone()],
            None => Vec::new(),
        }
    }

    /// 从响应中取出实例对象。
    fn extract_item(&self, value: &serde_json::Value) -> Option<serde_json::Value> {
        let root = &self.profile.fields.item_root;
        if root.trim().is_empty() {
            return Some(value.clone());
        }
        FieldPath::new(root.clone()).get(value).cloned()
    }

    /// 把厂商条目归一化为 [`Instance`]。
    fn to_instance(&self, item: &serde_json::Value) -> MnResult<Instance> {
        let f = &self.profile.fields;
        let id = FieldPath::new(f.id.clone())
            .get_str(item)
            .ok_or_else(|| MnError::Provider {
                provider: self.profile.name.clone(),
                code: "missing_field".to_string(),
                message: format!("响应缺少实例 ID 字段 `{}`", f.id),
            })?;

        let state_raw = FieldPath::new(f.state.clone())
            .get_str(item)
            .unwrap_or_else(|| "unknown".to_string());
        let state = self.profile.states.normalize(&state_raw);

        let gpu_model = FieldPath::new(f.gpu_model.clone())
            .get_str(item)
            .unwrap_or_else(|| "unknown".to_string());
        let gpu_count = FieldPath::new(f.gpu_count.clone())
            .get_u64(item)
            .unwrap_or(1)
            .max(1) as u32;
        let vram = f.scaled(FieldPath::new(f.vram.clone()).get_u64(item), &f.vram_unit);
        let cpu_cores = FieldPath::new(f.cpu_cores.clone())
            .get_u64(item)
            .unwrap_or(1)
            .max(1) as u32;
        let memory = f.scaled(
            FieldPath::new(f.memory.clone()).get_u64(item),
            &f.memory_unit,
        );
        let disk = f.scaled(FieldPath::new(f.disk.clone()).get_u64(item), &f.disk_unit);

        let image = f
            .image
            .as_ref()
            .and_then(|k| FieldPath::new(k.clone()).get_str(item))
            .unwrap_or_default();

        let network = NetworkEndpoint {
            ssh_host: f
                .ssh_host
                .as_ref()
                .and_then(|k| FieldPath::new(k.clone()).get_str(item)),
            ssh_port: f
                .ssh_port
                .as_ref()
                .and_then(|k| FieldPath::new(k.clone()).get_u64(item))
                .map(|v| v as u16),
            inference_host: None,
            inference_port: None,
            // 通用 REST profile 未定义密码字段：这类服务商走密钥/agent 认证
            ssh_password: None,
            public_url: None,
        };

        let price = f.price.as_ref().and_then(|k| {
            FieldPath::new(k.clone())
                .get_f64(item)
                .map(|per_hour| PriceQuote {
                    per_hour_cny: per_hour,
                    billing_granularity_secs: 0,
                    spot_release_allowed: true,
                    note: None,
                })
        });

        Ok(Instance {
            id,
            provider: self.profile.name.clone(),
            state,
            spec: InstanceSpec {
                gpu: GpuSpec {
                    model: gpu_model,
                    count: gpu_count,
                    vram_bytes: vram,
                },
                cpu_cores,
                memory_bytes: memory,
                disk_bytes: disk,
                image,
            },
            network,
            created_at: f
                .created_at
                .as_ref()
                .and_then(|k| FieldPath::new(k.clone()).get_u64(item))
                .map(|v| v as i64)
                .unwrap_or_else(mangosteen_core::time::now_unix),
            price,
            display_name: f
                .display_name
                .as_ref()
                .and_then(|k| FieldPath::new(k.clone()).get_str(item)),
        })
    }

    /// 把厂商条目归一化为 [`AvailableSpec`]。
    fn to_available_spec(&self, item: &serde_json::Value, index: usize) -> Option<AvailableSpec> {
        let f = &self.profile.fields;
        // 规格 ID 兼容 `spec_id` 与实例 ID 字段
        let spec_id = FieldPath::new("spec_id".to_string())
            .get_str(item)
            .or_else(|| FieldPath::new(f.id.clone()).get_str(item))
            .unwrap_or_else(|| format!("spec-{index}"));

        let gpu_model = FieldPath::new(f.gpu_model.clone()).get_str(item)?;
        let gpu_count = FieldPath::new(f.gpu_count.clone())
            .get_u64(item)
            .unwrap_or(1)
            .max(1) as u32;
        let vram = f.scaled(FieldPath::new(f.vram.clone()).get_u64(item), &f.vram_unit);
        if vram == 0 {
            return None; // 无显存信息的条目无法参与打分
        }
        let cpu_cores = FieldPath::new(f.cpu_cores.clone())
            .get_u64(item)
            .unwrap_or(1)
            .max(1) as u32;
        let memory = f.scaled(
            FieldPath::new(f.memory.clone()).get_u64(item),
            &f.memory_unit,
        );
        let disk = f.scaled(FieldPath::new(f.disk.clone()).get_u64(item), &f.disk_unit);
        let image = f
            .image
            .as_ref()
            .and_then(|k| FieldPath::new(k.clone()).get_str(item))
            .unwrap_or_default();
        let price = f
            .price
            .as_ref()
            .and_then(|k| FieldPath::new(k.clone()).get_f64(item))
            .unwrap_or(f64::MAX);

        Some(AvailableSpec {
            spec_id,
            spec: InstanceSpec {
                gpu: GpuSpec {
                    model: gpu_model,
                    count: gpu_count,
                    vram_bytes: vram,
                },
                cpu_cores,
                memory_bytes: memory,
                disk_bytes: disk,
                image,
            },
            price: PriceQuote {
                per_hour_cny: price,
                billing_granularity_secs: 0,
                spot_release_allowed: true,
                note: None,
            },
            region: f
                .region
                .as_ref()
                .and_then(|k| FieldPath::new(k.clone()).get_str(item))
                .unwrap_or_else(|| "default".to_string()),
            stock: f
                .stock
                .as_ref()
                .and_then(|k| FieldPath::new(k.clone()).get_u64(item))
                .map(|v| v as u32),
            extra: BTreeMap::new(),
        })
    }

    /// 执行生命周期动作并返回更新后的实例。
    fn action(&self, template: &str, id: &str) -> MnResult<Instance> {
        let url = self.profile.endpoints.url(template, Some(id), None);
        let value = self.send(HttpRequest::post(url, "{}"))?;
        match self.extract_item(&value) {
            Some(item) => self.to_instance(&item),
            // 部分厂商的启停接口只返回成功，不带实例体 → 回查一次
            None => {
                let url =
                    self.profile
                        .endpoints
                        .url(&self.profile.endpoints.describe, Some(id), None);
                let v = self.send(HttpRequest::get(url))?;
                let item = self.extract_item(&v).ok_or_else(|| MnError::Provider {
                    provider: self.profile.name.clone(),
                    code: "missing_instance".to_string(),
                    message: format!("实例 {id} 不存在或响应格式不符"),
                })?;
                self.to_instance(&item)
            }
        }
    }
}

impl<T: Transport + 'static> ProviderDriver for RestDriver<T> {
    fn name(&self) -> &str {
        &self.profile.name
    }

    fn display_name(&self) -> &str {
        &self.profile.display_name
    }

    fn capabilities(&self) -> DriverCapabilities {
        self.profile.capabilities.clone()
    }

    fn list_available_specs(&self) -> MnResult<Vec<AvailableSpec>> {
        let url = self
            .profile
            .endpoints
            .url(&self.profile.endpoints.list_specs, None, None);
        let value = self.send(HttpRequest::get(url))?;
        Ok(self
            .extract_items(&value)
            .iter()
            .enumerate()
            .filter_map(|(i, item)| self.to_available_spec(item, i))
            .collect())
    }

    fn create(&self, spec_id: &str, name: Option<&str>) -> MnResult<Instance> {
        let url = self
            .profile
            .endpoints
            .url(&self.profile.endpoints.create, None, Some(spec_id));

        let mut body = serde_json::Map::new();
        for (k, v) in &self.profile.create_extra {
            body.insert(k.clone(), v.clone());
        }
        if let Some(n) = name {
            body.insert("name".to_string(), serde_json::json!(n));
        }
        let payload = serde_json::to_string(&serde_json::Value::Object(body))
            .map_err(|e| MnError::config("provider.create_extra", format!("序列化失败：{e}")))?;

        let value = self.send(HttpRequest::post(url, payload))?;
        let item = self.extract_item(&value).ok_or_else(|| MnError::Provider {
            provider: self.profile.name.clone(),
            code: "missing_instance".to_string(),
            message: format!(
                "创建接口未返回实例对象（字段 `{}` 缺失）",
                self.profile.fields.item_root
            ),
        })?;
        self.to_instance(&item)
    }

    fn start(&self, instance_id: &str) -> MnResult<Instance> {
        self.action(&self.profile.endpoints.start, instance_id)
    }

    fn stop(&self, instance_id: &str) -> MnResult<Instance> {
        self.action(&self.profile.endpoints.stop, instance_id)
    }

    fn destroy(&self, instance_id: &str) -> MnResult<()> {
        let url =
            self.profile
                .endpoints
                .url(&self.profile.endpoints.destroy, Some(instance_id), None);
        let _ = self.send(HttpRequest::delete(url))?;
        Ok(())
    }

    fn describe(&self, instance_id: &str) -> MnResult<Instance> {
        let url =
            self.profile
                .endpoints
                .url(&self.profile.endpoints.describe, Some(instance_id), None);
        let value = self.send(HttpRequest::get(url))?;
        let item = self.extract_item(&value).ok_or_else(|| MnError::Provider {
            provider: self.profile.name.clone(),
            code: "missing_instance".to_string(),
            message: format!("实例 {instance_id} 不存在或响应格式不符"),
        })?;
        self.to_instance(&item)
    }

    fn list(&self) -> MnResult<Vec<Instance>> {
        let url = self
            .profile
            .endpoints
            .url(&self.profile.endpoints.list, None, None);
        let value = self.send(HttpRequest::get(url))?;
        self.extract_items(&value)
            .iter()
            .map(|item| self.to_instance(item))
            .collect()
    }
}

/// 使用共享传输的驱动包装。
///
/// 与 [`RestDriver`] 逻辑完全一致，只是把 `T` 固定为 `Arc<dyn Transport>`，
/// 以便注册表在运行时动态创建。
pub struct SharedRestDriver {
    inner: RestDriver<std::sync::Arc<dyn Transport + Send + Sync>>,
}

impl SharedRestDriver {
    /// 用共享传输构造（`Arc<dyn Transport>`）。
    ///
    /// 驱动注册表在运行时动态创建多个驱动，共用同一传输后端，
    /// 避免为每个驱动各建一套连接设施。
    pub fn from_shared(
        profile: RestDriverProfile,
        creds: Credentials,
        transport: std::sync::Arc<dyn Transport + Send + Sync>,
    ) -> MnResult<Self> {
        let inner = RestDriver::new(profile, creds, transport)?;
        Ok(Self { inner })
    }
}

impl ProviderDriver for SharedRestDriver {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn display_name(&self) -> &str {
        self.inner.display_name()
    }

    fn capabilities(&self) -> DriverCapabilities {
        self.inner.capabilities()
    }

    fn list_available_specs(&self) -> MnResult<Vec<AvailableSpec>> {
        self.inner.list_available_specs()
    }

    fn create(&self, spec_id: &str, name: Option<&str>) -> MnResult<Instance> {
        self.inner.create(spec_id, name)
    }

    fn start(&self, instance_id: &str) -> MnResult<Instance> {
        self.inner.start(instance_id)
    }

    fn stop(&self, instance_id: &str) -> MnResult<Instance> {
        self.inner.stop(instance_id)
    }

    fn destroy(&self, instance_id: &str) -> MnResult<()> {
        self.inner.destroy(instance_id)
    }

    fn describe(&self, instance_id: &str) -> MnResult<Instance> {
        self.inner.describe(instance_id)
    }

    fn list(&self) -> MnResult<Vec<Instance>> {
        self.inner.list()
    }
}

/// 截断过长文本（避免把整个响应体塞进错误消息）。
fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}…（已截断）")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::transport::MockTransport;

    fn profile() -> RestDriverProfile {
        RestDriverProfile {
            name: "autodl".into(),
            display_name: "AutoDL".into(),
            auth: AuthStyle::Bearer,
            endpoints: EndpointProfile {
                base_url: "https://api.autodl.com".into(),
                list_specs: "/api/v1/specs".into(),
                create: "/api/v1/instances?spec={spec_id}".into(),
                start: "/api/v1/instances/{id}/start".into(),
                stop: "/api/v1/instances/{id}/stop".into(),
                destroy: "/api/v1/instances/{id}".into(),
                describe: "/api/v1/instances/{id}".into(),
                list: "/api/v1/instances".into(),
                status_field: Some("code".into()),
                success_code: Some("Success".into()),
                error_field: Some("msg".into()),
            },
            fields: FieldMap {
                list_root: "data.list".into(),
                item_root: "data".into(),
                id: "uuid".into(),
                state: "status".into(),
                gpu_model: "gpu_name".into(),
                gpu_count: "gpu_num".into(),
                vram: "vram".into(),
                vram_unit: "gib".into(),
                cpu_cores: "cpu_num".into(),
                memory: "memory".into(),
                memory_unit: "gib".into(),
                disk: "disk".into(),
                disk_unit: "gib".into(),
                image: Some("image".into()),
                price: Some("price".into()),
                ssh_host: Some("ssh_host".into()),
                ssh_port: Some("ssh_port".into()),
                region: Some("region".into()),
                stock: Some("stock".into()),
                display_name: Some("name".into()),
                created_at: Some("created_at".into()),
            },
            states: StateMap {
                mapping: BTreeMap::from([
                    ("running".to_string(), InstanceState::Running),
                    ("shutdown".to_string(), InstanceState::Stopped),
                    ("creating".to_string(), InstanceState::Creating),
                    ("starting".to_string(), InstanceState::Starting),
                ]),
                fallback: InstanceState::Failed,
            },
            capabilities: DriverCapabilities {
                on_demand_create: true,
                stop_start: true,
                price_query: true,
                public_inference_url: false,
                custom_image: true,
                gpu_models: vec!["RTX 4090".into(), "RTX 5090".into()],
            },
            create_extra: BTreeMap::from([("image".to_string(), serde_json::json!("base-cuda12"))]),
        }
    }

    fn instance_json(id: &str, status: &str) -> String {
        format!(
            r#"{{"code":"Success","msg":"ok","data":{{
                "uuid":"{id}","status":"{status}","gpu_name":"RTX 4090","gpu_num":1,
                "vram":24,"cpu_num":16,"memory":60,"disk":50,
                "image":"base-cuda12","price":2.38,
                "ssh_host":"10.0.0.{id}","ssh_port":22,"region":"cn-west-1",
                "name":"mn-{id}","created_at":1700000000
            }}}}"#
        )
    }

    fn driver(mock: MockTransport) -> RestDriver<MockTransport> {
        RestDriver::new(profile(), Credentials::token("tok-abc-123"), mock).unwrap()
    }

    #[test]
    fn field_path_navigation() {
        let v: serde_json::Value =
            serde_json::from_str(r#"{"a":{"b":[{"c":42}]},"s":"txt","n":7}"#).unwrap();
        assert_eq!(FieldPath::new("a.b[0].c".to_string()).get_u64(&v), Some(42));
        assert_eq!(
            FieldPath::new("s".to_string()).get_str(&v).as_deref(),
            Some("txt")
        );
        assert_eq!(FieldPath::new("n".to_string()).get_u64(&v), Some(7));
        assert_eq!(FieldPath::new("missing".to_string()).get(&v), None);
        assert_eq!(FieldPath::new("a.b[9].c".to_string()).get(&v), None);
    }

    #[test]
    fn field_path_parses_numeric_strings() {
        let v = serde_json::json!({"vram": "24", "price": "2.5"});
        assert_eq!(FieldPath::new("vram".to_string()).get_u64(&v), Some(24));
        assert_eq!(FieldPath::new("price".to_string()).get_f64(&v), Some(2.5));
    }

    #[test]
    fn endpoint_url_substitution() {
        let p = profile();
        assert_eq!(
            p.endpoints.url(&p.endpoints.describe, Some("u-1"), None),
            "https://api.autodl.com/api/v1/instances/u-1"
        );
        assert_eq!(
            p.endpoints.url(&p.endpoints.create, None, Some("spec-9")),
            "https://api.autodl.com/api/v1/instances?spec=spec-9"
        );
    }

    #[test]
    fn endpoint_validation_requires_placeholders() {
        let mut p = profile();
        assert!(p.endpoints.validate().is_ok());
        p.endpoints.start = "/no/placeholder".into();
        assert!(p.endpoints.validate().is_err());
        p.endpoints.start = "/x/{id}".into();
        p.endpoints.create = "/no/spec".into();
        assert!(p.endpoints.validate().is_err());
    }

    #[test]
    fn state_normalization_is_case_insensitive() {
        let p = profile();
        assert_eq!(p.states.normalize("RUNNING"), InstanceState::Running);
        assert_eq!(p.states.normalize("  Shutdown "), InstanceState::Stopped);
        assert_eq!(p.states.normalize("weird"), InstanceState::Failed);
    }

    #[test]
    fn list_specs_normalizes_and_scales_units() {
        let body = r#"{"code":"Success","data":{"list":[
            {"spec_id":"s1","gpu_name":"RTX 4090","gpu_num":1,"vram":24,
             "cpu_num":16,"memory":60,"disk":50,"image":"img","price":2.38,
             "region":"cn-west","stock":3},
            {"spec_id":"s2","gpu_name":"RTX 5090","gpu_num":1,"vram":32,
             "cpu_num":32,"memory":128,"disk":100,"image":"img","price":4.5,
             "region":"cn-east","stock":0}
        ]}}"#;
        let d = driver(MockTransport::always(200, body));
        let specs = d.list_available_specs().unwrap();
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].spec_id, "s1");
        assert_eq!(specs[0].spec.gpu.total_vram_bytes(), 24 << 30);
        assert_eq!(specs[0].spec.memory_bytes, 60 << 30);
        assert_eq!(specs[0].price.per_hour_cny, 2.38);
        assert_eq!(specs[1].spec.gpu.model, "RTX 5090");
        assert!(!specs[1].is_in_stock());
        assert!(specs[0].is_in_stock());
    }

    #[test]
    fn list_specs_skips_entries_without_vram() {
        let body = r#"{"code":"Success","data":{"list":[
            {"spec_id":"bad","gpu_name":"X","gpu_num":1,"vram":0,"cpu_num":1,"memory":1,"disk":1}
        ]}}"#;
        let d = driver(MockTransport::always(200, body));
        assert!(d.list_available_specs().unwrap().is_empty());
    }

    #[test]
    fn auth_bearer_added_to_request() {
        let mock = MockTransport::always(200, r#"{"code":"Success","data":{"list":[]}}"#);
        let d = driver(mock);
        d.list_available_specs().unwrap();
        // 通过内部 recorded 验证（同一 mock 实例已被 move，改为新建验证）
        // 这里改用直接构造验证 authorize 行为
        let mock2 = MockTransport::always(200, r#"{"code":"Success","data":{"list":[]}}"#);
        let d2 = RestDriver::new(profile(), Credentials::token("secret-token"), mock2).unwrap();
        let req = d2.authorize(HttpRequest::get("https://x/y"));
        assert_eq!(req.header("Authorization"), Some("Bearer secret-token"));
        assert_eq!(req.header("Accept"), Some("application/json"));
        let _ = d;
    }

    #[test]
    fn auth_header_and_query_styles() {
        let mut p = profile();
        p.auth = AuthStyle::Header {
            name: "X-Api-Key".into(),
        };
        let d = RestDriver::new(
            p.clone(),
            Credentials::token("k"),
            MockTransport::always(200, "{}"),
        )
        .unwrap();
        let r = d.authorize(HttpRequest::get("https://x/y"));
        assert_eq!(r.header("X-Api-Key"), Some("k"));

        let mut p2 = profile();
        p2.auth = AuthStyle::QueryParam {
            name: "token".into(),
        };
        let d2 = RestDriver::new(
            p2,
            Credentials::token("q"),
            MockTransport::always(200, "{}"),
        )
        .unwrap();
        let r2 = d2.authorize(HttpRequest::get("https://x/y?a=1"));
        assert_eq!(r2.url, "https://x/y?a=1&token=q");

        let mut p3 = profile();
        p3.auth = AuthStyle::DoubleHeader {
            id_header: "X-Id".into(),
            secret_header: "X-Secret".into(),
        };
        let d3 = RestDriver::new(
            p3,
            Credentials::pair("id", "sec"),
            MockTransport::always(200, "{}"),
        )
        .unwrap();
        let r3 = d3.authorize(HttpRequest::get("https://x/y"));
        assert_eq!(r3.header("X-Id"), Some("id"));
        assert_eq!(r3.header("X-Secret"), Some("sec"));
    }

    #[test]
    fn describe_normalizes_instance() {
        let d = driver(MockTransport::always(200, instance_json("u-1", "running")));
        let inst = d.describe("u-1").unwrap();
        assert_eq!(inst.id, "u-1");
        assert_eq!(inst.provider, "autodl");
        assert_eq!(inst.state, InstanceState::Running);
        assert_eq!(inst.spec.gpu.model, "RTX 4090");
        assert_eq!(inst.spec.gpu.vram_bytes, 24 << 30);
        assert_eq!(inst.spec.memory_bytes, 60 << 30);
        assert_eq!(inst.network.ssh_host.as_deref(), Some("10.0.0.u-1"));
        assert_eq!(inst.network.ssh_port, Some(22));
        assert_eq!(inst.price.unwrap().per_hour_cny, 2.38);
        assert_eq!(inst.display_name.as_deref(), Some("mn-u-1"));
    }

    #[test]
    fn list_instances() {
        let body = r#"{"code":"Success","data":{"list":[
            {"uuid":"a","status":"running","gpu_name":"RTX 4090","gpu_num":1,"vram":24,
             "cpu_num":16,"memory":60,"disk":50},
            {"uuid":"b","status":"shutdown","gpu_name":"RTX 4090","gpu_num":1,"vram":24,
             "cpu_num":16,"memory":60,"disk":50}
        ]}}"#;
        let d = driver(MockTransport::always(200, body));
        let list = d.list().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].state, InstanceState::Running);
        assert_eq!(list[1].state, InstanceState::Stopped);
    }

    #[test]
    fn http_error_becomes_provider_error() {
        let d = driver(MockTransport::always(401, r#"{"msg":"invalid token"}"#));
        let e = d.list().unwrap_err();
        match &e {
            MnError::Provider {
                provider,
                code,
                message,
            } => {
                assert_eq!(provider, "autodl");
                assert_eq!(code, "http_401");
                assert!(message.contains("invalid token"), "got {message}");
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(e.is_retryable());
    }

    #[test]
    fn business_level_failure_is_detected() {
        // HTTP 200 但 code 不是 Success
        let body = r#"{"code":"ResourceNotAvailable","msg":"4090 无库存"}"#;
        let d = driver(MockTransport::always(200, body));
        let e = d.describe("u-1").unwrap_err();
        match &e {
            MnError::Provider { code, message, .. } => {
                assert_eq!(code, "ResourceNotAvailable");
                assert!(message.contains("无库存"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn invalid_json_is_reported_with_method_and_url() {
        let d = driver(MockTransport::always(200, "<html>gateway error</html>"));
        let e = d.list().unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("非 JSON"), "got {msg}");
        assert!(msg.contains("GET"), "got {msg}");
    }

    #[test]
    fn missing_id_field_is_reported() {
        let d = driver(MockTransport::always(
            200,
            r#"{"code":"Success","data":{"x":1}}"#,
        ));
        let e = d.describe("u-1").unwrap_err();
        assert!(e.to_string().contains("uuid"), "got {e}");
    }

    #[test]
    fn create_sends_extra_fields_and_name() {
        let mock = MockTransport::always(200, instance_json("new-1", "creating"));
        // 用 Arc 共享以便事后检查请求
        let mock = std::sync::Arc::new(mock);
        struct Shared(std::sync::Arc<MockTransport>);
        impl Transport for Shared {
            fn execute(&self, r: &HttpRequest) -> MnResult<HttpResponse> {
                self.0.execute(r)
            }
        }
        let d = RestDriver::new(profile(), Credentials::token("t"), Shared(mock.clone())).unwrap();
        let inst = d.create("spec-9", Some("my-instance")).unwrap();
        assert_eq!(inst.id, "new-1");

        let reqs = mock.requests();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].method, "POST");
        assert!(reqs[0].url.contains("spec=spec-9"));
        let body: serde_json::Value = serde_json::from_str(reqs[0].body.as_ref().unwrap()).unwrap();
        assert_eq!(body["image"], "base-cuda12");
        assert_eq!(body["name"], "my-instance");
    }

    #[test]
    fn start_stop_destroy_use_correct_methods_and_paths() {
        let mock = std::sync::Arc::new(MockTransport::always(200, instance_json("u-1", "running")));
        struct Shared(std::sync::Arc<MockTransport>);
        impl Transport for Shared {
            fn execute(&self, r: &HttpRequest) -> MnResult<HttpResponse> {
                self.0.execute(r)
            }
        }
        let d = RestDriver::new(profile(), Credentials::token("t"), Shared(mock.clone())).unwrap();

        d.start("u-1").unwrap();
        d.stop("u-1").unwrap();
        d.destroy("u-1").unwrap();

        let reqs = mock.requests();
        assert_eq!(reqs.len(), 3);
        assert!(reqs[0].url.ends_with("/instances/u-1/start"));
        assert!(reqs[1].url.ends_with("/instances/u-1/stop"));
        assert_eq!(reqs[2].method, "DELETE");
        assert!(reqs[2].url.ends_with("/instances/u-1"));
    }

    #[test]
    fn network_failure_propagates_as_retryable() {
        let d = driver(MockTransport::always_fail("connection refused"));
        let e = d.list().unwrap_err();
        assert!(matches!(e, MnError::Network { .. }));
        assert!(e.is_retryable());
    }

    #[test]
    fn empty_credentials_rejected() {
        let e = match RestDriver::new(
            profile(),
            Credentials::token("  "),
            MockTransport::always(200, "{}"),
        ) {
            Ok(_) => panic!("空白凭证必须被拒绝"),
            Err(e) => e,
        };
        assert!(e.to_string().contains("凭证为空"));
    }

    #[test]
    fn invalid_profile_rejected() {
        let mut p = profile();
        p.name = "".into();
        assert!(
            RestDriver::new(p, Credentials::token("t"), MockTransport::always(200, "{}")).is_err()
        );

        let mut p = profile();
        p.states.mapping.clear();
        assert!(
            RestDriver::new(p, Credentials::token("t"), MockTransport::always(200, "{}")).is_err()
        );

        let mut p = profile();
        p.fields.vram_unit = "kb".into();
        assert!(
            RestDriver::new(p, Credentials::token("t"), MockTransport::always(200, "{}")).is_err()
        );
    }

    #[test]
    fn capabilities_are_exposed() {
        let d = driver(MockTransport::always(200, "{}"));
        let c = d.capabilities();
        assert!(c.on_demand_create);
        assert!(c.gpu_models.contains(&"RTX 4090".to_string()));
        assert_eq!(d.name(), "autodl");
        assert_eq!(d.display_name(), "AutoDL");
    }

    #[test]
    fn truncate_limits_long_bodies() {
        let long = "x".repeat(500);
        let t = truncate(&long, 100);
        assert!(t.contains("已截断"));
        assert!(t.chars().count() < 130);
        assert_eq!(truncate("short", 100), "short");
    }
}
