//! 优云智算驱动的配置模型。
//!
//! 归属：**算力执行层 · 云端后端管理**。
//!
//! ## 为什么配置以 `BTreeMap<String,String>` 为输入
//!
//! 工作区的配置驱动原则要求：服务商参数全部走配置文件、改配置不重编译。
//! 但 `mangosteen-core` 的 [`mangosteen_core::config::ProviderConfig`] 只能
//! 给出通用的 `options` 表（它不该认识 `image_id` 这类厂商参数）。
//!
//! 因此本模块负责把通用 `options` **解析并校验**成强类型的
//! [`CompshareConfig`]，未知键直接报错——这样用户在配置里写错
//! `regoin` 时能立刻看到提示，而不是默默用了默认地域跑出意外的账单。
//!
//! ## 凭证从哪来
//!
//! UCloud 签名需要一对密钥：
//!
//! | 参数 | 来源 | 是否敏感 |
//! |------|------|----------|
//! | `PublicKey`（AccessKey ID） | `options.public_key` | 否（公开标识） |
//! | `PrivateKey`（AccessKey Secret） | `secret`（推荐 `{ env = "..." }`） | **是** |
//!
//! 私钥必须走 `secret` 字段，从而复用工作区已有的「环境变量 / 文件 /
//! 内联」三种来源与脱敏展示能力，不会明文落在配置里。

use std::collections::BTreeMap;

use mangosteen_core::{MnError, MnResult};

use crate::client::DEFAULT_ENDPOINT;

/// 允许的计费模式（见厂商创建接口文档）。
pub const CHARGE_TYPES: &[&str] = &["Month", "Day", "Postpay", "Spot"];
/// 允许的磁盘类型。
pub const DISK_TYPES: &[&str] = &["CLOUD_SSD", "CLOUD_RSSD"];
/// 允许的 CPU 平台写法。
pub const CPU_PLATFORMS: &[&str] = &["Auto", "Intel/Auto", "Amd/Auto"];

/// 驱动配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompshareConfig {
    /// API 入口
    pub endpoint: String,
    /// 地域（如 `cn-wlcb`）
    pub region: String,
    /// 可用区（如 `cn-wlcb-01`）
    pub zone: String,
    /// 镜像 ID（必填，需处于 `Available` 状态）
    pub image_id: String,
    /// 机型，当前厂商仅支持 `G`
    pub machine_type: String,
    /// CPU 平台
    pub cpu_platform: String,
    /// 计费模式
    pub charge_type: String,
    /// 系统盘类型
    pub disk_type: String,
    /// 系统盘大小（GB）
    pub disk_size_gb: u32,
    /// 实例名前缀（后接部署标识）
    pub name_prefix: String,
    /// 写入实例 `Remark` 的归属标记，便于在控制台辨认
    pub remark: String,
    /// 允许的 GPU 型号白名单（空表示不限制，取全部可售型号）
    pub gpu_types: Vec<String>,
    /// 单实例 GPU 卡数上限（个人场景默认 1 卡）
    pub max_gpu_count: u32,
    /// 内存下限（GB）。低于此值的组合不列为候选，避免筛出一堆跑不动模型的规格
    pub min_memory_gb: u64,
    /// 单次列举规格时最多查询多少个价格（控制接口调用量）
    pub max_price_queries: u32,
    /// 生命周期操作（启停/销毁）等待超时（秒）
    pub operation_timeout_secs: u64,
    /// 状态轮询间隔（秒）
    pub poll_interval_secs: u64,
    /// 删除实例时是否同时删除数据盘
    pub release_data_disk: bool,
    /// 创建前是否强制做库存预检。
    ///
    /// ## 为什么需要关掉
    ///
    /// 优云的 `CheckCompShareResourceCapacity` 是**模拟调度**而非实时预占。
    /// 实测：短时间内连续查询（一次部署要为 10 多个候选各查一遍）会**全部**
    /// 返回 `ResourceEnough: false`，而同一时刻单独查同一个卡型、或直接调
    /// 创建接口都成功。结果就是「明明有机器，却报没有符合约束的规格」。
    ///
    /// 关掉后由真实创建失败兜底：厂商会用 226604 明确拒绝，编排器据此换下一个
    /// 候选，语义与「预检说没货」完全一致，但不会误杀可用节点。
    pub require_stock: bool,
}

impl Default for CompshareConfig {
    fn default() -> Self {
        Self {
            endpoint: DEFAULT_ENDPOINT.to_string(),
            region: "cn-wlcb".to_string(),
            zone: "cn-wlcb-01".to_string(),
            image_id: String::new(),
            machine_type: "G".to_string(),
            cpu_platform: "Auto".to_string(),
            // 个人场景按量计费：关机不收费，避免包月闲置
            charge_type: "Postpay".to_string(),
            disk_type: "CLOUD_SSD".to_string(),
            disk_size_gb: 100,
            name_prefix: "mangosteen".to_string(),
            remark: "managed-by-mangosteen".to_string(),
            gpu_types: Vec::new(),
            max_gpu_count: 1,
            min_memory_gb: 32,
            max_price_queries: 10,
            require_stock: true,
            operation_timeout_secs: 600,
            poll_interval_secs: 5,
            release_data_disk: true,
        }
    }
}

/// 配置允许出现的全部键（用于拼写检查与错误提示）。
pub const ALLOWED_KEYS: &[&str] = &[
    "endpoint",
    "region",
    "zone",
    "image_id",
    "machine_type",
    "cpu_platform",
    "charge_type",
    "disk_type",
    "disk_size_gb",
    "require_stock",
    "name_prefix",
    "remark",
    "gpu_types",
    "max_gpu_count",
    "min_memory_gb",
    "max_price_queries",
    "operation_timeout_secs",
    "poll_interval_secs",
    "release_data_disk",
    "public_key",
];

impl CompshareConfig {
    /// 从配置文件的 `options` 表解析。
    ///
    /// 未知键一律报错：厂商参数名写错会导致「用了默认值但没人发现」，
    /// 例如把 `region` 写成 `regoin` 后仍能跑通，却建到了错误地域。
    pub fn from_options(options: &BTreeMap<String, String>) -> MnResult<Self> {
        // 拼写检查
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
        if let Some(v) = get("region") {
            cfg.region = v;
        }
        if let Some(v) = get("zone") {
            cfg.zone = v;
        }
        if let Some(v) = get("image_id") {
            cfg.image_id = v;
        }
        if let Some(v) = get("require_stock") {
            cfg.require_stock = parse_bool("require_stock", &v)?;
        }
        if let Some(v) = get("machine_type") {
            cfg.machine_type = v;
        }
        if let Some(v) = get("cpu_platform") {
            cfg.cpu_platform = v;
        }
        if let Some(v) = get("charge_type") {
            cfg.charge_type = v;
        }
        if let Some(v) = get("disk_type") {
            cfg.disk_type = v;
        }
        if let Some(v) = get("name_prefix") {
            if !v.is_empty() {
                cfg.name_prefix = v;
            }
        }
        if let Some(v) = get("remark") {
            cfg.remark = v;
        }
        if let Some(v) = get("gpu_types") {
            cfg.gpu_types = v
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }

        if let Some(v) = get("disk_size_gb") {
            cfg.disk_size_gb = parse_num::<u32>("disk_size_gb", &v)?;
        }
        if let Some(v) = get("max_gpu_count") {
            cfg.max_gpu_count = parse_num::<u32>("max_gpu_count", &v)?;
        }
        if let Some(v) = get("min_memory_gb") {
            cfg.min_memory_gb = parse_num::<u64>("min_memory_gb", &v)?;
        }
        if let Some(v) = get("max_price_queries") {
            cfg.max_price_queries = parse_num::<u32>("max_price_queries", &v)?;
        }
        if let Some(v) = get("operation_timeout_secs") {
            cfg.operation_timeout_secs = parse_num::<u64>("operation_timeout_secs", &v)?;
        }
        if let Some(v) = get("poll_interval_secs") {
            cfg.poll_interval_secs = parse_num::<u64>("poll_interval_secs", &v)?;
        }
        if let Some(v) = get("release_data_disk") {
            cfg.release_data_disk = parse_bool("release_data_disk", &v)?;
        }

        cfg.validate()?;
        Ok(cfg)
    }

    /// 校验配置自洽性。
    pub fn validate(&self) -> MnResult<()> {
        if self.image_id.trim().is_empty() {
            return Err(MnError::config(
                "providers.<name>.options.image_id",
                "必须指定镜像 ID（可用 `mangosteen backend images` 查询）",
            ));
        }
        if self.region.trim().is_empty() {
            return Err(MnError::config(
                "providers.<name>.options.region",
                "必须指定地域，如 cn-wlcb",
            ));
        }
        if self.zone.trim().is_empty() {
            return Err(MnError::config(
                "providers.<name>.options.zone",
                "必须指定可用区，如 cn-wlcb-01",
            ));
        }
        if !CHARGE_TYPES.contains(&self.charge_type.as_str()) {
            return Err(MnError::config(
                "providers.<name>.options.charge_type",
                format!(
                    "`{}` 不是合法计费模式，可选：{}",
                    self.charge_type,
                    CHARGE_TYPES.join(" / ")
                ),
            ));
        }
        if !DISK_TYPES.contains(&self.disk_type.as_str()) {
            return Err(MnError::config(
                "providers.<name>.options.disk_type",
                format!(
                    "`{}` 不是合法磁盘类型，可选：{}（CLOUD_RSSD 仅 A800 支持）",
                    self.disk_type,
                    DISK_TYPES.join(" / ")
                ),
            ));
        }
        if !CPU_PLATFORMS.contains(&self.cpu_platform.as_str()) {
            return Err(MnError::config(
                "providers.<name>.options.cpu_platform",
                format!(
                    "`{}` 不是合法 CPU 平台，可选：{}",
                    self.cpu_platform,
                    CPU_PLATFORMS.join(" / ")
                ),
            ));
        }
        if self.machine_type != "G" {
            return Err(MnError::config(
                "providers.<name>.options.machine_type",
                "当前厂商仅支持机型 `G`（GPU 型）",
            ));
        }
        if self.disk_size_gb == 0 {
            return Err(MnError::config(
                "providers.<name>.options.disk_size_gb",
                "系统盘不能为 0 GB",
            ));
        }
        if self.max_gpu_count == 0 {
            return Err(MnError::config(
                "providers.<name>.options.max_gpu_count",
                "至少为 1",
            ));
        }
        if self.min_memory_gb == 0 {
            return Err(MnError::config(
                "providers.<name>.options.min_memory_gb",
                "至少为 1 GB",
            ));
        }
        if self.poll_interval_secs == 0 {
            return Err(MnError::config(
                "providers.<name>.options.poll_interval_secs",
                "轮询间隔不能为 0（会打爆厂商接口）",
            ));
        }
        if self.operation_timeout_secs == 0 {
            return Err(MnError::config(
                "providers.<name>.options.operation_timeout_secs",
                "超时不能为 0",
            ));
        }
        if self.name_prefix.chars().count() > 40 {
            return Err(MnError::config(
                "providers.<name>.options.name_prefix",
                "前缀过长：厂商实例名上限 63 字符，需为后缀留出空间",
            ));
        }
        Ok(())
    }

    /// 内存折算为厂商要求的 MB 单位。
    ///
    /// 厂商要求 `Memory` 必须是 1024 的整数倍，GB→MB 天然满足。
    pub fn memory_mb(memory_gb: u64) -> u64 {
        memory_gb.saturating_mul(1024)
    }

    /// 该 GPU 型号是否被配置允许。
    pub fn allows_gpu(&self, gpu_type: &str) -> bool {
        self.gpu_types.is_empty()
            || self
                .gpu_types
                .iter()
                .any(|g| g.eq_ignore_ascii_case(gpu_type.trim()))
    }

    /// 构造实例名。厂商限制 63 字符，这里按前缀 + 标识拼接后截断。
    pub fn instance_name(&self, tag: &str) -> String {
        let raw = if tag.is_empty() {
            self.name_prefix.clone()
        } else {
            format!("{}-{}", self.name_prefix, tag)
        };
        raw.chars().take(63).collect()
    }
}

/// 解析整数配置项。
fn parse_num<T: std::str::FromStr>(key: &str, raw: &str) -> MnResult<T> {
    raw.parse::<T>().map_err(|_| {
        MnError::config(
            format!("providers.<name>.options.{key}"),
            format!("`{raw}` 不是合法数字"),
        )
    })
}

/// 解析布尔配置项（接受 true/false/yes/no/1/0）。
fn parse_bool(key: &str, raw: &str) -> MnResult<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "1" | "on" => Ok(true),
        "false" | "no" | "0" | "off" => Ok(false),
        _ => Err(MnError::config(
            format!("providers.<name>.options.{key}"),
            format!("`{raw}` 不是合法布尔值（true / false）"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn defaults_require_only_image_id() {
        // 未给 image_id 时默认配置本身不合法——这是刻意的，缺镜像无法创建
        assert!(CompshareConfig::default().validate().is_err());

        let c = CompshareConfig::from_options(&opts(&[("image_id", "img-1")])).unwrap();
        assert_eq!(c.region, "cn-wlcb");
        assert_eq!(c.zone, "cn-wlcb-01");
        assert_eq!(c.charge_type, "Postpay");
        assert_eq!(c.disk_type, "CLOUD_SSD");
        assert_eq!(c.disk_size_gb, 100);
        assert_eq!(c.max_gpu_count, 1);
        assert!(c.release_data_disk);
        assert_eq!(c.endpoint, DEFAULT_ENDPOINT);
    }

    #[test]
    fn overrides_are_applied() {
        let c = CompshareConfig::from_options(&opts(&[
            ("image_id", "img-9"),
            ("region", "cn-sh2"),
            ("zone", "cn-sh2-02"),
            ("charge_type", "Spot"),
            ("disk_size_gb", "200"),
            ("max_gpu_count", "2"),
            ("min_memory_gb", "128"),
            ("gpu_types", "4090, 5090"),
            ("release_data_disk", "false"),
            ("poll_interval_secs", "3"),
        ]))
        .unwrap();
        assert_eq!(c.region, "cn-sh2");
        assert_eq!(c.charge_type, "Spot");
        assert_eq!(c.disk_size_gb, 200);
        assert_eq!(c.max_gpu_count, 2);
        assert_eq!(c.min_memory_gb, 128);
        assert_eq!(c.gpu_types, vec!["4090", "5090"]);
        assert!(!c.release_data_disk);
        assert_eq!(c.poll_interval_secs, 3);
    }

    #[test]
    fn unknown_option_is_rejected_with_hint() {
        let e = CompshareConfig::from_options(&opts(&[
            ("image_id", "img-1"),
            ("regoin", "cn-wlcb"), // 拼写错误
        ]))
        .unwrap_err();
        let s = e.to_string();
        assert!(s.contains("regoin"), "{s}");
        assert!(s.contains("region"), "应提示正确键名：{s}");
    }

    #[test]
    fn invalid_enum_values_are_rejected() {
        let base = [("image_id", "img-1")];

        let mut o = opts(&base);
        o.insert("charge_type".into(), "Weekly".into());
        assert!(CompshareConfig::from_options(&o).is_err());

        let mut o = opts(&base);
        o.insert("disk_type".into(), "HDD".into());
        assert!(CompshareConfig::from_options(&o).is_err());

        let mut o = opts(&base);
        o.insert("cpu_platform".into(), "Arm/Auto".into());
        assert!(CompshareConfig::from_options(&o).is_err());

        let mut o = opts(&base);
        o.insert("machine_type".into(), "C".into());
        assert!(CompshareConfig::from_options(&o).is_err());
    }

    #[test]
    fn numeric_and_bool_parsing_errors_are_reported() {
        let mut o = opts(&[("image_id", "img-1")]);
        o.insert("disk_size_gb".into(), "big".into());
        let e = CompshareConfig::from_options(&o).unwrap_err();
        assert!(e.to_string().contains("disk_size_gb"), "{e}");

        let mut o = opts(&[("image_id", "img-1")]);
        o.insert("release_data_disk".into(), "maybe".into());
        assert!(CompshareConfig::from_options(&o).is_err());
    }

    #[test]
    fn zero_values_rejected() {
        for (k, v) in [
            ("disk_size_gb", "0"),
            ("max_gpu_count", "0"),
            ("min_memory_gb", "0"),
            ("poll_interval_secs", "0"),
            ("operation_timeout_secs", "0"),
        ] {
            let mut o = opts(&[("image_id", "img-1")]);
            o.insert(k.to_string(), v.to_string());
            assert!(CompshareConfig::from_options(&o).is_err(), "应拒绝 {k}={v}");
        }
    }

    #[test]
    fn memory_conversion_is_multiple_of_1024() {
        // 厂商要求 Memory 必须是 1024 的整数倍
        for gb in [1u64, 32, 64, 94, 128, 512] {
            let mb = CompshareConfig::memory_mb(gb);
            assert_eq!(mb % 1024, 0, "{gb}GB → {mb}MB 不是 1024 的倍数");
            assert_eq!(mb, gb * 1024);
        }
        // 饱和不溢出
        assert_eq!(CompshareConfig::memory_mb(u64::MAX), u64::MAX);
    }

    #[test]
    fn gpu_allowlist_matching() {
        let mut c = CompshareConfig {
            image_id: "i".into(),
            ..Default::default()
        };
        assert!(c.allows_gpu("4090"), "空白名单表示不限制");
        assert!(c.allows_gpu("A100"));

        c.gpu_types = vec!["4090".into(), "5090".into()];
        assert!(c.allows_gpu("4090"));
        assert!(c.allows_gpu(" 5090 "));
        assert!(!c.allows_gpu("A100"));
    }

    #[test]
    fn instance_name_is_truncated_to_63() {
        let mut c = CompshareConfig {
            name_prefix: "x".repeat(60),
            ..Default::default()
        };
        let name = c.instance_name("11435");
        assert_eq!(name.chars().count(), 63);
        // 短名保持完整
        c.name_prefix = "mangosteen".into();
        assert_eq!(c.instance_name("11435"), "mangosteen-11435");
        assert_eq!(c.instance_name(""), "mangosteen");
    }

    #[test]
    fn prefix_length_is_validated() {
        let mut o = opts(&[("image_id", "img-1")]);
        o.insert("name_prefix".into(), "x".repeat(41));
        assert!(CompshareConfig::from_options(&o).is_err());
    }
}
