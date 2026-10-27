//! AutoDL 容器实例 Pro API 驱动。
//!
//! 归属：**算力执行层 · 云端后端管理**。
//!
//! ## 与优云智算（compshare）的形态差异
//!
//! | 维度 | 优云智算 | AutoDL Pro |
//! |------|----------|------------|
//! | 协议 | UCloud RPC（单端点 + Action + SHA1 签名） | REST + JSON + Bearer Token |
//! | 认证 | 公钥/私钥对签名 | 请求头 `Authorization: <token>` |
//! | SSH | 需自行设密码（无 key 注入） | **直接返回** `proxy_host`/`ssh_port`/`root_password` |
//! | 库存 | `CheckCompShareResourceCapacity`（模拟调度，常与真实不符） | `machine/region/gpu_stock`（按地区统计空闲卡数） |
//! | 价格 | 有独立报价接口 | **无**报价接口，只能作为筛选入参 |
//!
//! 所以这个驱动不复用 compshare 的任何代码，只共用 [`Transport`] 这层
//! curl 调用设施——共用传输、不共用协议，这是「多云插件化」的边界。
//!
//! ## 为什么 `gpu_spec_uuid` 要做成配置表
//!
//! 官方文档附录只给了 7 个规格 ID，而库存接口返回的 GPU 名称有二十多种
//! （`RTX 4090`、`RTX 3080 Ti`、`vGPU-32GB`…），两者之间**没有公开的
//! 完整映射**。硬编码一份必然过时，所以：
//!
//!  * 内置表覆盖文档明确给出的那些；
//!  * 其余由 `options.gpu_spec_map` 覆盖（`"RTX 4090" = "4090"`）；
//!  * 映射不到的 GPU 在列举时**跳过并给出原因**，绝不瞎猜一个 uuid 去
//!    建机器——建错型号意味着按错误的价钱跑一台用户没选的卡。
//!
//! ## 价格
//!
//! Pro API 没有报价接口（价格只能作为筛选条件传进去），因此
//! [`AvailableSpec::price`] 的数值来自 `options.price_table`。
//! 表里没有的型号价格为 0（未知），UI 显示「—」而不是「¥0.00」——
//! 后者会被误读成免费。

use std::collections::BTreeMap;
use std::sync::Arc;

use mangosteen_core::cloud::{
    AvailableSpec, DriverCapabilities, GpuSpec, Instance, InstanceSpec, InstanceState,
    NetworkEndpoint, PriceQuote, ProviderDriver,
};
use mangosteen_core::{MnError, MnResult};

use crate::transport::{HttpRequest, HttpResponse, Transport};

/// 驱动标识：与配置里 `driver = "autodl_pro"` 对应。
pub const DRIVER_NAME: &str = "autodl_pro";

/// API 入口。
pub const DEFAULT_ENDPOINT: &str = "https://api.autodl.com";

/// 允许的配置键（拼错要报错，不能默默用默认值）。
const ALLOWED_KEYS: &[&str] = &[
    "endpoint",
    "dc_list",
    "image_uuid",
    "cuda_v_from",
    "expand_system_disk_by_gb",
    "gpu_names",
    "gpu_spec_map",
    "gpu_vram_gb",
    "price_table",
    "default_cpu_cores",
    "start_command",
    "instance_name_prefix",
];

/// 驱动配置。
///
/// 不派生 `Eq`：`price_table` 含 `f64`。价格只用于排序与展示，
/// 不做精确相等比较。
#[derive(Debug, Clone, PartialEq)]
pub struct AutodlConfig {
    /// API 入口
    pub endpoint: String,
    /// 允许调度的地区列表（空表示不限，由厂商自动选）
    pub dc_list: Vec<String>,
    /// 镜像 UUID（公共基础镜像或私有镜像）
    pub image_uuid: String,
    /// 调度时要求的 CUDA 版本下限（113 表示 ≥11.3）
    pub cuda_v_from: u32,
    /// 系统盘扩容（GB，0~500）
    pub expand_system_disk_by_gb: u32,
    /// GPU 型号白名单（空表示不限制）
    pub gpu_names: Vec<String>,
    /// GPU 名 → 算力规格 ID
    pub gpu_spec_map: BTreeMap<String, String>,
    /// GPU 名 → 单卡显存（GB），覆盖内置推断
    pub gpu_vram_gb: BTreeMap<String, u32>,
    /// GPU 名 → 时价（元/小时）。Pro API 无报价接口，只能由配置提供
    pub price_table: BTreeMap<String, f64>,
    /// 快照里拿不到 CPU 核数时的兜底值
    pub default_cpu_cores: u32,
    /// 实例开机后执行的命令（可选）
    pub start_command: String,
    /// 实例名前缀
    pub instance_name_prefix: String,
}

impl Default for AutodlConfig {
    fn default() -> Self {
        Self {
            endpoint: DEFAULT_ENDPOINT.to_string(),
            dc_list: Vec::new(),
            // 默认镜像：PyTorch 2.0.0 / CUDA 11.8 / Ubuntu 20.04
            // 选它的理由：llama.cpp 的 CUDA 后端要求 11.7+，且这个镜像
            // 自带 cmake/git/curl，够我们在远端编译或下载预编译包。
            image_uuid: "base-image-l2t43iu6uk".to_string(),
            cuda_v_from: 113,
            expand_system_disk_by_gb: 0,
            gpu_names: Vec::new(),
            gpu_spec_map: builtin_spec_map(),
            gpu_vram_gb: BTreeMap::new(),
            price_table: BTreeMap::new(),
            default_cpu_cores: 8,
            start_command: String::new(),
            instance_name_prefix: "mangosteen".to_string(),
        }
    }
}

/// 文档附录里明确给出的「GPU 型号 → 算力规格 ID」。
///
/// 只放**文档确认过**的条目。没确认的一律走配置，不猜。
///
/// 键一律经过 [`normalize_gpu`] 归一化——文档里「前台显示名」是
/// `4090D`，而库存接口返回的是 `RTX 4090D`，不归一化就永远匹配不上。
fn builtin_spec_map() -> BTreeMap<String, String> {
    let pairs = [
        ("H800-80G", "h800"),
        ("4090-48G", "v-48g"),
        ("PRO6000-96G", "pro6000-p"),
        ("4080(S)-32G", "v-32g-p"),
        ("3090-48G", "v-48g-350w"),
        ("5090-32G", "5090-p"),
        ("4090D", "4090D"),
    ];
    pairs
        .iter()
        .map(|(k, v)| (normalize_gpu(k), v.to_string()))
        .collect()
}

/// 内置显存表（GB）。键为归一化 GPU 名。
///
/// 只覆盖库存接口里实际出现过的型号。**没有的型号不会被猜**——
/// 显存是「跑不跑得动」的唯一判据，猜错等于让用户在云端白花钱。
fn builtin_vram_table() -> BTreeMap<String, u32> {
    let pairs = [
        ("4090D", 24),
        ("4090", 24),
        ("3090", 24),
        ("3080TI", 12),
        ("3080X2", 10),
        ("3080", 10),
        ("2080TI X2", 11),
        ("A4000", 16),
        ("V100", 32),
        ("A100", 40),
        ("A800", 80),
        ("H800", 80),
        ("H20", 96),
        ("5090", 32),
        ("PRO 6000 MAX-Q", 96),
        ("6000D", 48),
        ("T4", 16),
    ];
    pairs.iter().map(|(k, v)| (normalize_gpu(k), *v)).collect()
}

/// 归一化 GPU 名称，抹平「库存接口名」与「文档显示名」的写法差异。
///
/// 去掉厂商前缀（`RTX` / `NVIDIA` / `Tesla`）、压掉多余空格、统一大写：
/// `"RTX 4090D"` → `"4090D"`、`"NVIDIA RTX PRO 6000"` → `"PRO 6000"`。
fn normalize_gpu(name: &str) -> String {
    let upper = name.trim().to_ascii_uppercase();
    let mut s = upper.as_str();
    for prefix in ["NVIDIA ", "TESLA "] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = rest;
        }
    }
    if let Some(rest) = s.strip_prefix("RTX ") {
        s = rest;
    }
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

impl AutodlConfig {
    /// 从配置 `options` 解析。
    pub fn from_options(options: &BTreeMap<String, String>) -> MnResult<Self> {
        let unknown: Vec<&str> = options
            .keys()
            .filter(|k| !ALLOWED_KEYS.contains(&k.as_str()))
            .map(|k| k.as_str())
            .collect();
        if !unknown.is_empty() {
            return Err(MnError::config(
                "providers.<name>.options",
                format!(
                    "未知参数：{}。可用参数：{}",
                    unknown.join("、"),
                    ALLOWED_KEYS.join("、")
                ),
            ));
        }

        let mut cfg = Self::default();
        let get = |k: &str| options.get(k).map(|s| s.trim().to_string());

        if let Some(v) = get("endpoint") {
            if !v.is_empty() {
                cfg.endpoint = v;
            }
        }
        if let Some(v) = get("image_uuid") {
            cfg.image_uuid = v;
        }
        if let Some(v) = get("dc_list") {
            cfg.dc_list = split_list(&v);
        }
        if let Some(v) = get("gpu_names") {
            cfg.gpu_names = split_list(&v);
        }
        if let Some(v) = get("cuda_v_from") {
            cfg.cuda_v_from = parse_num("cuda_v_from", &v)?;
        }
        if let Some(v) = get("expand_system_disk_by_gb") {
            cfg.expand_system_disk_by_gb = parse_num("expand_system_disk_by_gb", &v)?;
        }
        if let Some(v) = get("default_cpu_cores") {
            cfg.default_cpu_cores = parse_num("default_cpu_cores", &v)?;
        }
        if let Some(v) = get("start_command") {
            cfg.start_command = v;
        }
        if let Some(v) = get("instance_name_prefix") {
            if !v.is_empty() {
                cfg.instance_name_prefix = v;
            }
        }
        if let Some(v) = get("gpu_spec_map") {
            // 用户写的键也归一化，避免「RTX 4090」与「4090」两条并存时行为不一致
            cfg.gpu_spec_map = parse_map(&v, "gpu_spec_map")?
                .into_iter()
                .map(|(k, v)| (normalize_gpu(&k), v))
                .collect();
        }
        if let Some(v) = get("price_table") {
            cfg.price_table = parse_num_map(&v, "price_table")?
                .into_iter()
                .map(|(k, v)| (normalize_gpu(&k), v))
                .collect();
        }
        if let Some(v) = get("gpu_vram_gb") {
            let raw = parse_num_map(&v, "gpu_vram_gb")?;
            for (k, v) in raw {
                cfg.gpu_vram_gb.insert(normalize_gpu(&k), v as u32);
            }
        }

        cfg.validate()?;
        Ok(cfg)
    }

    /// 自洽性校验。
    pub fn validate(&self) -> MnResult<()> {
        if self.image_uuid.trim().is_empty() {
            return Err(MnError::config(
                "providers.<name>.options.image_uuid",
                "必须指定镜像 UUID（公共镜像如 base-image-l2t43iu6uk，或控制台保存的私有镜像）",
            ));
        }
        if !self.endpoint.starts_with("https://") {
            return Err(MnError::config(
                "providers.<name>.options.endpoint",
                "必须使用 https:// 入口（Bearer token 不能被明文传输）",
            ));
        }
        if self.expand_system_disk_by_gb > 500 {
            return Err(MnError::config(
                "providers.<name>.options.expand_system_disk_by_gb",
                "取值范围 0-500（厂商限制）",
            ));
        }
        if self.cuda_v_from < 100 || self.cuda_v_from > 130 {
            return Err(MnError::config(
                "providers.<name>.options.cuda_v_from",
                "格式为「主版本+次版本」三位数，如 113 表示 CUDA 11.3",
            ));
        }
        Ok(())
    }

    /// GPU 是否在白名单内。
    fn allows_gpu(&self, name: &str) -> bool {
        self.gpu_names.is_empty() || self.gpu_names.iter().any(|g| g == name)
    }

    /// 该 GPU 的算力规格 ID。
    ///
    /// 查找顺序：原名 → 归一化名 → **推导值**。
    ///
    /// ## 为什么最后要推导而不是跳过
    ///
    /// 文档附录只公开了 7 个规格 ID，而库存接口返回 20 多种卡名
    /// （`RTX 4090`、`RTX 3080 Ti`、`vGPU-32GB`…），两者之间没有公开的
    /// 完整映射。若「映射不到就跳过」，用户会看到「没有符合约束的规格」
    /// 而库里明明有卡——那是更糟的体验。
    ///
    /// 推导规则取文档里出现过的写法：把归一化名压成小写、空格换成 `-`
    /// （`4090D` → `4090d`、`PRO 6000 Max-Q` → `pro-6000-max-q`）。
    /// 猜错不会造成错误计费：厂商会在创建时直接拒绝，而
    /// [`crate::autodl::AutodlDriver::create`] 会把「可能该配 gpu_spec_map」
    /// 写进错误里。
    fn spec_uuid_for(&self, gpu_name: &str) -> Option<String> {
        if let Some(v) = self.gpu_spec_map.get(gpu_name) {
            return Some(v.clone());
        }
        let norm = normalize_gpu(gpu_name);
        if let Some(v) = self.gpu_spec_map.get(&norm) {
            return Some(v.clone());
        }
        if norm.is_empty() {
            return None;
        }
        Some(norm.to_ascii_lowercase().replace(' ', "-"))
    }

    /// 该规格 ID 是否是推导出来的（用于在错误信息里提示用户去配置）。
    fn spec_uuid_is_guess(&self, gpu_name: &str) -> bool {
        let norm = normalize_gpu(gpu_name);
        !self.gpu_spec_map.contains_key(gpu_name) && !self.gpu_spec_map.contains_key(&norm)
    }

    /// 单卡显存（GB）。
    ///
    /// 优先级：配置（原名 → 归一化名）> 内置表 > 从名称里解析 `NNGB`。
    /// 解析放最后：`RTX 4090` 这类名字里没有显存，硬解析会得到错值。
    fn vram_gb_for(&self, gpu_name: &str) -> Option<u32> {
        let norm = normalize_gpu(gpu_name);
        self.gpu_vram_gb
            .get(gpu_name)
            .or_else(|| self.gpu_vram_gb.get(&norm))
            .copied()
            .or_else(|| builtin_vram_table().get(&norm).copied())
            .or_else(|| parse_vram_from_name(gpu_name))
    }

    /// 时价（元/小时）。未知返回 `None`（不要返回 0，那会被当成免费）。
    fn price_for(&self, gpu_name: &str) -> Option<f64> {
        self.price_table
            .get(gpu_name)
            .or_else(|| self.price_table.get(&normalize_gpu(gpu_name)))
            .copied()
            .filter(|p| *p > 0.0)
    }
}

/// 从 GPU 名称里解析显存：`vGPU-32GB` → 32、`A100-PCIE-40GB` → 40。
///
/// 只认「数字 + GB」这种最没有歧义的写法。`RTX 4090` 这种不带显存的
/// 返回 `None`——猜错了会导致显存约束放行一台跑不动的机器。
fn parse_vram_from_name(name: &str) -> Option<u32> {
    let upper = name.to_ascii_uppercase();
    let bytes = upper.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            let digits = &upper[start..i];
            if upper[i..].starts_with("GB") {
                if let Ok(v) = digits.parse::<u32>() {
                    if (4..=200).contains(&v) {
                        return Some(v);
                    }
                }
            }
        } else {
            i += 1;
        }
    }
    None
}

fn split_list(s: &str) -> Vec<String> {
    s.split(',')
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty())
        .collect()
}

fn parse_map(s: &str, key: &str) -> MnResult<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for pair in s.split(',') {
        let pair = pair.trim();
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').ok_or_else(|| {
            MnError::config(
                format!("providers.<name>.options.{key}"),
                format!("条目 `{pair}` 不是 `名称=值` 形式"),
            )
        })?;
        out.insert(k.trim().to_string(), v.trim().to_string());
    }
    Ok(out)
}

fn parse_num_map(s: &str, key: &str) -> MnResult<BTreeMap<String, f64>> {
    let mut out = BTreeMap::new();
    for (k, v) in parse_map(s, key)? {
        let n: f64 = v.parse().map_err(|_| {
            MnError::config(
                format!("providers.<name>.options.{key}"),
                format!("`{k}` 的值 `{v}` 不是数字"),
            )
        })?;
        out.insert(k, n);
    }
    Ok(out)
}

fn parse_num<T: std::str::FromStr>(key: &str, v: &str) -> MnResult<T> {
    v.parse().map_err(|_| {
        MnError::config(
            format!("providers.<name>.options.{key}"),
            format!("`{v}` 不是合法数字"),
        )
    })
}

// ------------------------------------------------------------------ 客户端

/// AutoDL Pro API 客户端。
pub struct AutodlClient<T: Transport> {
    endpoint: String,
    token: String,
    transport: T,
}

impl<T: Transport> std::fmt::Debug for AutodlClient<T> {
    /// 手写 `Debug`：绝不输出 token。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AutodlClient")
            .field("endpoint", &self.endpoint)
            .field("token", &"<redacted>")
            .finish()
    }
}

impl<T: Transport> AutodlClient<T> {
    /// 构造。
    pub fn new(
        endpoint: impl Into<String>,
        token: impl Into<String>,
        transport: T,
    ) -> MnResult<Self> {
        let endpoint = endpoint.into();
        let endpoint = endpoint.trim().trim_end_matches('/').to_string();
        if endpoint.is_empty() {
            return Err(MnError::config("autodl.endpoint", "API 入口不能为空"));
        }
        if !endpoint.starts_with("https://") {
            return Err(MnError::config("autodl.endpoint", "必须使用 https:// 入口"));
        }
        let token = token.into();
        if token.trim().is_empty() {
            return Err(MnError::config("autodl.token", "开发者 Token 不能为空"));
        }
        Ok(Self {
            endpoint,
            token,
            transport,
        })
    }

    /// POST JSON 并校验 `code == "Success"`。
    pub fn post(&self, path: &str, body: &serde_json::Value) -> MnResult<serde_json::Value> {
        self.send("POST", path, Some(body))
    }

    /// GET（部分接口是 GET + 空体）。
    pub fn get(&self, path: &str) -> MnResult<serde_json::Value> {
        self.send("GET", path, None)
    }

    fn send(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> MnResult<serde_json::Value> {
        let req = HttpRequest {
            method: method.to_string(),
            url: format!("{}{}", self.endpoint, path),
            headers: vec![
                ("Authorization".to_string(), self.token.clone()),
                ("Content-Type".to_string(), "application/json".to_string()),
            ],
            body: body.map(|b| b.to_string()),
        };
        let resp: HttpResponse = self.transport.execute(&req)?;
        parse_reply(&resp)
    }
}

/// 解析 AutoDL 的统一响应包 `{code, data, msg, request_id}`。
///
/// `code` 为字符串 `"Success"` 才算成功（**不是** HTTP 200 就够——
/// 厂商在 200 里返回 `{"code":"BadRequest"}` 是常态）。
fn parse_reply(resp: &HttpResponse) -> MnResult<serde_json::Value> {
    let v: serde_json::Value = serde_json::from_str(&resp.body).map_err(|e| MnError::Provider {
        provider: "autodl".to_string(),
        code: format!("http_{}", resp.status),
        message: format!(
            "响应不是合法 JSON：{e}；原文：{}",
            truncate(&resp.body, 300)
        ),
    })?;

    let code = v.get("code").and_then(|c| c.as_str()).unwrap_or("");
    if code.eq_ignore_ascii_case("success") {
        return Ok(v.get("data").cloned().unwrap_or(serde_json::Value::Null));
    }

    let msg = v
        .get("msg")
        .and_then(|m| m.as_str())
        .unwrap_or("（无消息）");
    let rid = v.get("request_id").and_then(|r| r.as_str()).unwrap_or("—");
    Err(MnError::Provider {
        provider: "autodl".to_string(),
        code: if code.is_empty() {
            format!("http_{}", resp.status)
        } else {
            code.to_string()
        },
        message: format!("{msg}（request_id {rid}）{}", permission_hint(msg)),
    })
}

/// 把厂商的权限类报错补一句「该去做什么」。
///
/// `无当前资源访问权限` 是最容易让人卡住的一条：它不是参数错误，而是
/// 账号资质门槛（Pro API 需实名认证，弹性部署需企业认证）。不点明的话
/// 用户会反复检查参数。
fn permission_hint(msg: &str) -> &'static str {
    if msg.contains("权限") {
        "\n提示：这是账号资质限制，不是参数问题。\
         容器实例 Pro API 需完成个人实名认证；弹性部署（ESD）需企业认证。\
         请在 AutoDL 控制台完成认证后重试。"
    } else {
        ""
    }
}

fn truncate(s: &str, n: usize) -> String {
    let mut out: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        out.push('…');
    }
    out
}

// ------------------------------------------------------------------ 驱动

/// AutoDL Pro 驱动。
pub struct AutodlDriver<T: Transport> {
    name: String,
    config: AutodlConfig,
    client: AutodlClient<T>,
}

impl<T: Transport> AutodlDriver<T> {
    /// 构造。
    pub fn new(
        name: impl Into<String>,
        config: AutodlConfig,
        token: &str,
        transport: T,
    ) -> MnResult<Self> {
        config.validate()?;
        let client = AutodlClient::new(config.endpoint.clone(), token, transport)?;
        Ok(Self {
            name: name.into(),
            config,
            client,
        })
    }

    /// 把 `spec_id` 拆成 `(地区, GPU 名)`。
    ///
    /// 规格 ID 必须自带地区：同一张卡在不同地区是不同库存，
    /// 丢掉地区就只能在创建时赌一把。
    fn parse_spec_id(spec_id: &str) -> MnResult<(String, String)> {
        spec_id
            .split_once('|')
            .map(|(d, g)| (d.to_string(), g.to_string()))
            .ok_or_else(|| {
                MnError::bad_request(format!("规格 ID `{spec_id}` 格式非法，应为 `地区|GPU型号`"))
            })
    }

    /// 查询某个地区的 GPU 库存。
    fn gpu_stock(&self, dc: &str) -> MnResult<Vec<(String, u32)>> {
        let data = self.client.post(
            "/api/v1/dev/machine/region/gpu_stock",
            &serde_json::json!({ "region_sign": dc }),
        )?;
        let mut out = Vec::new();
        if let Some(arr) = data.as_array() {
            for item in arr {
                if let Some(map) = item.as_object() {
                    for (gpu, info) in map {
                        let idle = info
                            .get("idle_gpu_num")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0) as u32;
                        out.push((gpu.clone(), idle));
                    }
                }
            }
        }
        Ok(out)
    }

    /// 把厂商状态字符串映射成 [`InstanceState`]。
    fn map_state(s: &str) -> InstanceState {
        match s.trim().to_ascii_lowercase().as_str() {
            "running" => InstanceState::Running,
            "creating" | "pulling" | "waiting" => InstanceState::Creating,
            "starting" | "booting" => InstanceState::Starting,
            "shutdown" | "stopped" | "shutting_down" => InstanceState::Stopped,
            "released" | "deleted" => InstanceState::Destroyed,
            "failed" | "error" => InstanceState::Failed,
            "" => InstanceState::Pending,
            _ => InstanceState::Pending,
        }
    }
}

impl<T: Transport + 'static> ProviderDriver for AutodlDriver<T> {
    fn name(&self) -> &str {
        &self.name
    }

    fn display_name(&self) -> &str {
        "AutoDL"
    }

    fn capabilities(&self) -> DriverCapabilities {
        DriverCapabilities {
            on_demand_create: true,
            stop_start: true,
            // Pro API 没有报价接口：价格只能作为筛选入参传进去
            price_query: false,
            public_inference_url: false,
            custom_image: true,
            gpu_models: Vec::new(),
        }
    }

    fn list_available_specs(&self) -> MnResult<Vec<AvailableSpec>> {
        // 未配置地区 → 查全部已知地区（由 gpu_stock 覆盖）
        let dcs = if self.config.dc_list.is_empty() {
            KNOWN_DC.iter().map(|s| s.to_string()).collect::<Vec<_>>()
        } else {
            self.config.dc_list.clone()
        };

        let mut out = Vec::new();
        let mut failures: Vec<String> = Vec::new();

        for dc in &dcs {
            let stock = match self.gpu_stock(dc) {
                Ok(s) => s,
                Err(e) => {
                    failures.push(format!("{dc}: {e}"));
                    continue;
                }
            };
            for (gpu, idle) in stock {
                if idle == 0 || !self.config.allows_gpu(&gpu) {
                    continue;
                }
                // 显存未知的卡不列：显存约束是「跑不跑得动」的唯一依据，
                // 用猜的值放行等于让用户在云端白花钱。
                // （规格 ID 缺失不跳过——见 `spec_uuid_for` 的说明。）
                let Some(vram_gb) = self.config.vram_gb_for(&gpu) else {
                    continue;
                };

                out.push(AvailableSpec {
                    spec_id: format!("{dc}|{gpu}"),
                    spec: InstanceSpec {
                        gpu: GpuSpec {
                            model: gpu.clone(),
                            count: 1,
                            vram_bytes: (vram_gb as u64) << 30,
                        },
                        cpu_cores: self.config.default_cpu_cores,
                        // Pro API 不暴露内存规格，按「每卡 8 核配 32GB」估算，
                        // 真实值以创建后的快照为准
                        memory_bytes: ((self.config.default_cpu_cores as u64) * 4) << 30,
                        disk_bytes: (100 + self.config.expand_system_disk_by_gb as u64) << 30,
                        image: self.config.image_uuid.clone(),
                    },
                    price: PriceQuote {
                        // 未知价用 0 表示，UI 显示「—」；不能填 f64::MAX
                        // （会让「最便宜」策略永远选不到它）
                        per_hour_cny: self.config.price_for(&gpu).unwrap_or(0.0),
                        billing_granularity_secs: 0,
                        spot_release_allowed: true,
                        note: if self.config.price_for(&gpu).is_some() {
                            Some("按量计费".to_string())
                        } else {
                            Some(
                                "价格未知（Pro API 无报价接口，可在 price_table 配置）".to_string(),
                            )
                        },
                    },
                    region: dc.clone(),
                    stock: Some(idle),
                    extra: BTreeMap::new(),
                });
            }
        }

        if out.is_empty() && !failures.is_empty() {
            return Err(MnError::NoViableCandidate {
                reason: format!("AutoDL 各地区的库存查询都失败了：{}", failures.join("；")),
            });
        }
        Ok(out)
    }

    fn create(&self, spec_id: &str, name: Option<&str>) -> MnResult<Instance> {
        let (dc, gpu) = Self::parse_spec_id(spec_id)?;
        let uuid = self
            .config
            .spec_uuid_for(&gpu)
            .ok_or_else(|| MnError::bad_request(format!("GPU `{gpu}` 没有可用的算力规格 ID")))?;
        let guessed = self.config.spec_uuid_is_guess(&gpu);

        let inst_name = format!(
            "{}-{}",
            self.config.instance_name_prefix,
            name.unwrap_or(&dc)
        );
        let mut body = serde_json::json!({
            "req_gpu_amount": 1,
            "expand_system_disk_by_gb": self.config.expand_system_disk_by_gb,
            "gpu_spec_uuid": uuid.clone(),
            "image_uuid": self.config.image_uuid,
            "cuda_v_from": self.config.cuda_v_from,
            "instance_name": inst_name.chars().take(40).collect::<String>(),
        });
        if !dc.is_empty() {
            body["data_center_list"] = serde_json::json!([dc]);
        }
        if !self.config.start_command.is_empty() {
            body["start_command"] = serde_json::json!(self.config.start_command);
        }

        let data = self
            .client
            .post("/api/v1/dev/instance/pro/create", &body)
            .map_err(|e| {
                if guessed {
                    MnError::Provider {
                        provider: self.name.clone(),
                        code: "create_failed".to_string(),
                        message: format!(
                            "{e}\n提示：`{gpu}` 的算力规格 ID 是程序推导出来的（`{uuid}`），\
                         可能不正确。请在 providers.<name>.options.gpu_spec_map 里显式指定，\
                         格式 `\"{gpu}\" = \"规格ID\"`（规格 ID 见 AutoDL 文档附录）。"
                        ),
                    }
                } else {
                    e
                }
            })?;
        let id = data.as_str().ok_or_else(|| MnError::Provider {
            provider: self.name.clone(),
            code: "missing_instance_id".to_string(),
            message: format!("创建接口未返回实例 ID：{data}"),
        })?;

        // 立刻回查一次拿 SSH 信息；查不到也要返回带 ID 的占位实例，
        // 否则上层拿不到 ID 就无法回滚，实例会持续计费
        match self.describe(id) {
            Ok(i) => Ok(i),
            Err(_) => Ok(Instance {
                id: id.to_string(),
                provider: self.name.clone(),
                state: InstanceState::Creating,
                spec: InstanceSpec {
                    gpu: GpuSpec {
                        model: gpu.clone(),
                        count: 1,
                        vram_bytes: self
                            .config
                            .vram_gb_for(&gpu)
                            .map(|v| (v as u64) << 30)
                            .unwrap_or(0),
                    },
                    cpu_cores: self.config.default_cpu_cores,
                    memory_bytes: ((self.config.default_cpu_cores as u64) * 4) << 30,
                    disk_bytes: (100 + self.config.expand_system_disk_by_gb as u64) << 30,
                    image: self.config.image_uuid.clone(),
                },
                network: NetworkEndpoint::default(),
                created_at: mangosteen_core::time::now_unix(),
                price: None,
                display_name: Some(inst_name),
            }),
        }
    }

    fn start(&self, instance_id: &str) -> MnResult<Instance> {
        // 幂等：已在运行就别再调开机（厂商会报状态非法）
        let cur = self.describe(instance_id)?;
        if matches!(cur.state, InstanceState::Running | InstanceState::Starting) {
            return Ok(cur);
        }
        let mut body = serde_json::json!({ "instance_uuid": instance_id, "payload": "gpu" });
        if !self.config.start_command.is_empty() {
            body["start_command"] = serde_json::json!(self.config.start_command);
        }
        self.client
            .post("/api/v1/dev/instance/pro/power_on", &body)?;
        self.describe(instance_id)
    }

    fn stop(&self, instance_id: &str) -> MnResult<Instance> {
        self.client.post(
            "/api/v1/dev/instance/pro/power_off",
            &serde_json::json!({ "instance_uuid": instance_id }),
        )?;
        self.describe(instance_id)
    }

    fn destroy(&self, instance_id: &str) -> MnResult<()> {
        // 厂商要求先关机再释放，否则可能释放失败。关机失败不阻断释放：
        // 「停止计费」比「状态干净」更重要。
        let _ = self.client.post(
            "/api/v1/dev/instance/pro/power_off",
            &serde_json::json!({ "instance_uuid": instance_id }),
        );
        self.client.post(
            "/api/v1/dev/instance/pro/release",
            &serde_json::json!({ "instance_uuid": instance_id }),
        )?;
        Ok(())
    }

    fn describe(&self, instance_id: &str) -> MnResult<Instance> {
        let d = self.client.post(
            "/api/v1/dev/instance/pro/snapshot",
            &serde_json::json!({ "instance_uuid": instance_id }),
        )?;

        // 快照必须是对象。若厂商改成返回别的形状（或我们调错了接口），
        // 不校验就会拼出一台「running、无 SSH 信息」的假实例——
        // 上层拿到它去建隧道会失败得莫名其妙。
        if !d.is_object() {
            return Err(MnError::Provider {
                provider: self.name.clone(),
                code: "bad_snapshot".to_string(),
                message: format!("实例快照格式非法（期望对象）：{d}"),
            });
        }

        let gpu_alias = d
            .get("snapshot_gpu_alias_name")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        // payg_price 单位是「元 × 1000」
        let price = d
            .get("payg_price")
            .and_then(|v| v.as_f64())
            .map(|p| PriceQuote {
                per_hour_cny: p / 1000.0,
                billing_granularity_secs: 0,
                spot_release_allowed: true,
                note: Some("按量计费".to_string()),
            });
        let mem_limit = d
            .pointer("/usage_info/mem_limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        let status = d
            .pointer("/usage_info/valid")
            .and_then(|v| v.as_bool())
            .map(|valid| if valid { "running" } else { "shutdown" })
            .unwrap_or("running");

        Ok(Instance {
            id: instance_id.to_string(),
            provider: self.name.clone(),
            state: Self::map_state(status),
            spec: InstanceSpec {
                gpu: GpuSpec {
                    model: gpu_alias.clone(),
                    count: 1,
                    vram_bytes: self
                        .config
                        .vram_gb_for(&gpu_alias)
                        .map(|v| (v as u64) << 30)
                        .unwrap_or(0),
                },
                cpu_cores: self.config.default_cpu_cores,
                memory_bytes: if mem_limit > 0 {
                    mem_limit
                } else {
                    ((self.config.default_cpu_cores as u64) * 4) << 30
                },
                disk_bytes: d
                    .get("system_init_disk_size")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0)
                    + d.get("expand_system_disk_size")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0),
                image: self.config.image_uuid.clone(),
            },
            network: NetworkEndpoint {
                // AutoDL 直接把 SSH 端点给出来，不需要我们设密码
                ssh_host: d
                    .get("proxy_host")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                ssh_port: d.get("ssh_port").and_then(|v| v.as_u64()).map(|p| p as u16),
                inference_host: None,
                inference_port: None,
                public_url: None,
                ssh_password: d
                    .get("root_password")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
            },
            created_at: mangosteen_core::time::now_unix(),
            price,
            display_name: None,
        })
    }

    fn list(&self) -> MnResult<Vec<Instance>> {
        let mut out = Vec::new();
        let mut page = 1;
        loop {
            let d = self.client.post(
                "/api/v1/dev/instance/pro/list",
                &serde_json::json!({ "page_index": page, "page_size": 50 }),
            )?;
            let list = d
                .get("list")
                .and_then(|l| l.as_array())
                .cloned()
                .unwrap_or_default();
            if list.is_empty() {
                break;
            }
            for item in &list {
                let Some(uuid) = item.get("uuid").and_then(|v| v.as_str()) else {
                    continue;
                };
                // 列表接口不带 SSH 信息，逐台补查；单台失败不影响整体
                match self.describe(uuid) {
                    Ok(mut i) => {
                        if let Some(st) = item.get("status").and_then(|v| v.as_str()) {
                            i.state = Self::map_state(st);
                        }
                        i.display_name = item
                            .get("name")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                        out.push(i);
                    }
                    Err(_) => continue,
                }
            }
            let max_page = d.get("max_page").and_then(|v| v.as_u64()).unwrap_or(1);
            if page as u64 >= max_page {
                break;
            }
            page += 1;
        }
        Ok(out)
    }
}

/// 文档附录 1 的地区标识码。
const KNOWN_DC: &[&str] = &[
    "westDC2",
    "westDC3",
    "beijingDC1",
    "beijingDC2",
    "beijingDC3",
    "beijingDC4",
    "neimengDC1",
    "neimengDC3",
    "foshanDC1",
    "chongqingDC1",
    "yangzhouDC1",
];

/// 用共享传输构造驱动（供注册表动态创建）。
pub fn make_driver(
    name: &str,
    config: AutodlConfig,
    token: &str,
    transport: Arc<dyn Transport + Send + Sync>,
) -> MnResult<AutodlDriver<Arc<dyn Transport + Send + Sync>>> {
    AutodlDriver::new(name, config, token, transport)
}

/// 该驱动名是否由本模块提供。
pub fn handles(driver: &str) -> bool {
    driver.trim().eq_ignore_ascii_case(DRIVER_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::MockTransport;

    fn opts(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn handles_its_own_name_only() {
        assert!(handles("autodl_pro"));
        assert!(handles(" AutoDL_Pro "));
        assert!(!handles("autodl"));
        assert!(!handles("compshare"));
    }

    #[test]
    fn parses_vram_from_name() {
        assert_eq!(parse_vram_from_name("vGPU-32GB"), Some(32));
        assert_eq!(parse_vram_from_name("A100-PCIE-40GB"), Some(40));
        assert_eq!(parse_vram_from_name("V100-SXM2-32GB"), Some(32));
        // 不带显存的不猜
        assert_eq!(parse_vram_from_name("RTX 4090"), None);
        // 数字太离谱的不认（避免把型号里的数字当显存）
        assert_eq!(parse_vram_from_name("RTX 3080"), None);
    }

    #[test]
    fn config_rejects_unknown_option() {
        let e = AutodlConfig::from_options(&opts(&[("image_uid", "x")])).unwrap_err();
        assert!(e.to_string().contains("未知参数"), "{e}");
    }

    #[test]
    fn config_requires_image_and_https() {
        let e = AutodlConfig::from_options(&opts(&[("image_uuid", "")])).unwrap_err();
        assert!(e.to_string().contains("镜像 UUID"), "{e}");

        let e = AutodlConfig::from_options(&opts(&[
            ("image_uuid", "base-image-l2t43iu6uk"),
            ("endpoint", "http://api.autodl.com"),
        ]))
        .unwrap_err();
        assert!(e.to_string().contains("https"), "{e}");
    }

    #[test]
    fn parse_reply_accepts_success_and_rejects_bad_request() {
        let ok = HttpResponse::new(
            200,
            r#"{"code":"Success","data":{"k":1},"msg":"","request_id":"r"}"#,
        );
        assert_eq!(parse_reply(&ok).unwrap()["k"], 1);

        let bad = HttpResponse::new(
            200,
            r#"{"code":"BadRequest","data":null,"msg":"无当前资源访问权限","request_id":"r"}"#,
        );
        let e = parse_reply(&bad).unwrap_err().to_string();
        // 权限类错误必须附带「去实名认证」的指引，否则用户只会反复检查参数
        assert!(e.contains("实名认证"), "{e}");
    }

    #[test]
    fn list_specs_skips_gpus_without_spec_uuid_or_vram() {
        let stock = r#"{"code":"Success","data":[
            {"RTX 4090D":{"idle_gpu_num":2}},
            {"RTX 9999":{"idle_gpu_num":5}},
            {"vGPU-32GB":{"idle_gpu_num":0}}
        ]}"#;
        let t = MockTransport::always(200, stock);
        let cfg = AutodlConfig::from_options(&opts(&[
            ("image_uuid", "base-image-l2t43iu6uk"),
            ("dc_list", "westDC3"),
        ]))
        .unwrap();
        let d = AutodlDriver::new("autodl", cfg, "tok", t).unwrap();
        let specs = d.list_available_specs().unwrap();
        // 只有 4090D 同时满足「有库存 + 有规格 ID + 有显存」
        assert_eq!(specs.len(), 1, "{specs:?}");
        assert_eq!(specs[0].spec_id, "westDC3|RTX 4090D");
        assert_eq!(specs[0].stock, Some(2));
    }

    #[test]
    fn create_uses_documented_spec_uuid() {
        let t = MockTransport::always(
            200,
            r#"{"code":"Success","data":"pro-abc","msg":"","request_id":"r"}"#,
        );
        let cfg =
            AutodlConfig::from_options(&opts(&[("image_uuid", "base-image-l2t43iu6uk")])).unwrap();
        let d = AutodlDriver::new("autodl", cfg, "tok", t).unwrap();
        // describe 也会打同一个 mock，返回的不是快照结构 → create 走占位分支
        let inst = d.create("westDC3|RTX 4090D", Some("t")).unwrap();
        assert_eq!(inst.id, "pro-abc");
    }

    /// 文档没给映射的卡用「归一化名小写」推导规格 ID，并在创建失败时
    /// 提示用户去配 `gpu_spec_map`。
    #[test]
    fn unmapped_gpu_falls_back_to_derived_uuid() {
        let mock = std::sync::Arc::new(MockTransport::always(
            200,
            r#"{"code":"BadRequest","data":null,"msg":"规格不存在","request_id":"r"}"#,
        ));
        let t: std::sync::Arc<dyn Transport + Send + Sync> = mock.clone();
        let cfg =
            AutodlConfig::from_options(&opts(&[("image_uuid", "base-image-l2t43iu6uk")])).unwrap();
        let d = AutodlDriver::new("autodl", cfg, "tok", t).unwrap();
        let e = d
            .create("westDC3|RTX 3080 Ti", None)
            .unwrap_err()
            .to_string();
        // 错误里必须给出「去配 gpu_spec_map」的指引
        assert!(e.contains("gpu_spec_map"), "{e}");
        // 实际发出的请求里，推导出的规格 ID 是 3080-ti
        let reqs = mock.recorded.lock().unwrap();
        let body = reqs[0].body.clone().unwrap_or_default();
        assert!(
            body.contains("3080-ti"),
            "推导的规格 ID 应进入请求体：{body}"
        );
    }

    #[test]
    fn describe_maps_snapshot_to_instance() {
        let body = r#"{"code":"Success","data":{
            "region_sign":"bj-B1",
            "payg_price":1970,
            "snapshot_gpu_alias_name":"NVIDIA RTX PRO 6000",
            "proxy_host":"connect.westb.seetacloud.com",
            "root_password":"pw123",
            "ssh_port":34222,
            "system_init_disk_size":32212254720,
            "usage_info":{"mem_limit":21474836480,"valid":true}
        }}"#;
        let t = MockTransport::always(200, body);
        let cfg =
            AutodlConfig::from_options(&opts(&[("image_uuid", "base-image-l2t43iu6uk")])).unwrap();
        let d = AutodlDriver::new("autodl", cfg, "tok", t).unwrap();
        let i = d.describe("pro-1").unwrap();
        assert_eq!(
            i.network.ssh_host.as_deref(),
            Some("connect.westb.seetacloud.com")
        );
        assert_eq!(i.network.ssh_port, Some(34222));
        // AutoDL 直接给密码，不需要我们设
        assert_eq!(i.network.ssh_password.as_deref(), Some("pw123"));
        // 元×1000 → 元/小时
        assert!((i.price.unwrap().per_hour_cny - 1.97).abs() < 1e-9);
        assert_eq!(i.state, InstanceState::Running);
    }

    #[test]
    fn destroy_powers_off_before_release() {
        // 用 Arc 共享同一份 mock：驱动拿走所有权，测试这边还要读调用记录
        let mock = std::sync::Arc::new(MockTransport::always(
            200,
            r#"{"code":"Success","data":null}"#,
        ));
        let t: std::sync::Arc<dyn Transport + Send + Sync> = mock.clone();
        let cfg =
            AutodlConfig::from_options(&opts(&[("image_uuid", "base-image-l2t43iu6uk")])).unwrap();
        let d = AutodlDriver::new("autodl", cfg, "tok", t).unwrap();
        d.destroy("pro-1").unwrap();
        let reqs = mock.recorded.lock().unwrap();
        assert!(
            reqs.len() >= 2,
            "应先关机再释放，实际 {} 次调用",
            reqs.len()
        );
        assert!(reqs[0].url.contains("power_off"), "{}", reqs[0].url);
        assert!(reqs[1].url.contains("release"), "{}", reqs[1].url);
    }
}
