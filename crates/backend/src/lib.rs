//! # Mangosteen Backend
//!
//! **算力执行层 · 云端后端管理**：优云智算（Compshare / UCloud）GPU 实例的
//! API 适配与自动创建。
//!
//! ## 定位
//!
//! 工作区已有 `mangosteen-cloud` 承载「多云统一适配」，但它假定厂商 API 是
//! `REST + Token 头` 形态。优云智算用的是 **UCloud RPC 形态**：
//!
//! | 维度 | REST 形态（autodl / 通用 profile） | UCloud 形态（本 crate） |
//! |------|-----------------------------------|--------------------------|
//! | 端点 | 每个动作一个 URL 路径 | 全部 `POST https://api.compshare.cn` |
//! | 动作区分 | HTTP 方法 + 路径 | 表单参数 `Action=...` |
//! | 认证 | 请求头 Token / Bearer | 参数 `PublicKey` + `Signature`（SHA1 签名） |
//! | 编码 | JSON body | 扁平化表单（`Disks.0.Size=100`） |
//! | 嵌套结构 | 原生 JSON | 数组展开为 `Key.0.Field` |
//! | 成功判定 | HTTP 2xx | HTTP 2xx **且** `RetCode == 0` |
//!
//! 把这些差异硬塞进通用 REST profile 会得到一份无法工作的配置
//! （签名无法用字段映射表达），所以按工作区文档给出的「代码路径」
//! 单独实现一个 `ProviderDriver`。
//!
//! ## 模块地图
//!
//! | 模块 | 职责 |
//! |------|------|
//! | [`sign`] | UCloud V1 签名（SHA1）与凭证 |
//! | [`sha1`] | SHA-1 摘要（签名所需，工作区只依赖 sha2） |
//! | [`params`] | 参数扁平化与表单编码（签名用原值、传输用转义值） |
//! | [`transport`] | HTTP 传输抽象 + curl 实现 + 测试替身 |
//! | [`model`] | 厂商响应结构（字段名与文档逐字一致） |
//! | [`mapping`] | 厂商表示 → 领域模型的唯一换算点（单位/状态/端口/价格） |
//! | [`retcode`] | 返回码语义归类与错误归一化 |
//! | [`client`] | 各接口的具体调用（创建/查询/启停/删除/价格/镜像/可用区） |
//! | [`config`] | 配置解析与校验（强类型化 `options`） |
//! | [`driver`] | [`ProviderDriver`] 实现（业务补偿：库存检查、先关机再删除） |
//!
//! ## 与其它层的关系
//!
//! ```text
//! mangosteen-gateway（调度决策器）
//!        │  只认识 ProviderDriver
//!        ▼
//! mangosteen-cloud（编排器 / 驱动注册表）
//!        │  driver = "compshare" 时转交本 crate
//!        ▼
//! mangosteen-backend（本 crate）
//!        │  签名 + 表单编码 + 字段映射 + 业务补偿
//!        ▼
//! https://api.compshare.cn
//! ```
//!
//! 本 crate **不依赖** `mangosteen-cloud`，也不被 `mangosteen-core` 依赖，
//! 依赖方向保持自上而下（见 `crates/mangosteen-cli/tests/layering.rs` 的 C5）。
//!
//! ## 完整创建流程（自动创建方案）
//!
//! ```text
//! 1. 查机型    DescribeAvailableCompShareInstanceTypes   → 卡型/CPU/内存合法组合、显存
//! 2. 查库存    CheckCompShareResourceCapacity            → ResourceEnough（厂商要求，必做）
//! 3. 查价格    GetCompShareInstancePrice                 → 分项明细，按计费模式折算时价
//! 4. 创建      CreateCompShareInstance                   → UHostIds[0]
//! 5. 补详情    DescribeCompShareInstance                 → 状态/IP/SSH 登录命令
//! 6. 等就绪    DescribeCompShareInstance（轮询）          → Running
//! 7. 销毁      StopCompShareInstance → Terminate         → 厂商要求先关机
//! ```
//!
//! 第 2 步失败直接返回「可降级」错误，让网关换节点；
//! 第 6 步失败会自动回滚（删除实例），避免孤儿实例持续计费。
//!
//! ## 配置示例
//!
//! ```toml
//! [providers.compshare]
//! driver = "compshare"
//! enabled = true
//! display_name = "优云智算"
//! secret = { env = "COMPSHARE_PRIVATE_KEY" }   # AccessKey Secret
//!
//! [providers.compshare.options]
//! public_key = "your-accesskey-id"
//! region = "cn-wlcb"
//! zone = "cn-wlcb-01"
//! image_id = "compshareImage-xxxx"             # 必填，可用 `mangosteen backend images` 查
//! charge_type = "Postpay"                      # 按量计费，关机不收费
//! gpu_types = "4090,5090"
//! max_gpu_count = "1"
//! ```

pub mod autodl;
pub mod base64;
pub mod capacity;
pub mod client;
pub mod config;
pub mod driver;
pub mod mapping;
pub mod model;
pub mod params;
pub mod retcode;
pub mod sha1;
pub mod sign;
pub mod transport;

pub use capacity::{
    precheck_stock, price_quotes, provision_cheapest, scan, scan_all, scan_zones, CapacityMatrix,
    Launched, PrecheckResult, PriceBoard, PricedOption, QuotableSpec, ScanOptions, ScanScope,
    ZoneGpuCapacity,
};
pub use client::{CompshareClient, DEFAULT_ENDPOINT, PROVIDER_NAME};
pub use config::CompshareConfig;
pub use driver::CompshareDriver;
pub use mapping::{
    is_linux_image, known_vram_gb, normalize_gpu_model, normalize_state, parse_spec_id,
    pick_linux_image, spec_id, units,
};
pub use model::*;
pub use params::Params;
pub use retcode::{classify, ApiFailure};
pub use sign::Credential;
pub use transport::{CurlTransport, HttpRequest, HttpResponse, MockTransport, Transport};

/// 驱动标识：与配置里 `driver = "compshare"` 对应。
pub const DRIVER_NAME: &str = "compshare";

/// 用共享传输构造驱动（供 `mangosteen-cloud` 的注册表动态创建）。
///
/// 传 `Arc<dyn Transport>` 而不是具体类型，让所有服务商共用一套 curl 调用设施。
pub fn make_driver(
    name: &str,
    config: CompshareConfig,
    credential: Credential,
    transport: std::sync::Arc<dyn Transport + Send + Sync>,
) -> mangosteen_core::MnResult<CompshareDriver<std::sync::Arc<dyn Transport + Send + Sync>>> {
    CompshareDriver::new(name, config, credential, transport)
}

/// 从通用 `options` + 私钥构造驱动（注册表最常用的入口）。
///
/// 这是本 crate 对外的**唯一推荐构造方式**：把 `secret` 的解析、
/// `options` 的强类型化、凭证校验一次做完，调用方不必重复处理。
pub fn build_from_options(
    name: &str,
    options: &std::collections::BTreeMap<String, String>,
    private_key: &str,
    transport: std::sync::Arc<dyn Transport + Send + Sync>,
) -> mangosteen_core::MnResult<CompshareDriver<std::sync::Arc<dyn Transport + Send + Sync>>> {
    use mangosteen_core::MnError;

    let config = CompshareConfig::from_options(options)?;
    let public_key = options
        .get("public_key")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            MnError::config(
                format!("providers.{name}.options.public_key"),
                "UCloud 签名需要公钥（AccessKey ID）。\
                 请填在 options.public_key，私钥填在 secret 字段",
            )
        })?;
    let credential = Credential::new(public_key, private_key)?;
    make_driver(name, config, credential, transport)
}

/// 该驱动名是否由本 crate 提供。
pub fn handles(driver: &str) -> bool {
    driver.trim().eq_ignore_ascii_case(DRIVER_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mangosteen_core::cloud::ProviderDriver;
    use std::collections::BTreeMap;

    fn transport() -> std::sync::Arc<dyn Transport + Send + Sync> {
        std::sync::Arc::new(MockTransport::always(200, "{}"))
    }

    fn options(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn handles_its_own_driver_name_only() {
        assert!(handles("compshare"));
        assert!(handles(" CompShare "));
        assert!(!handles("autodl"));
        assert!(!handles("ucloud"));
        assert!(!handles(""));
    }

    #[test]
    fn build_from_options_happy_path() {
        let d = build_from_options(
            "compshare",
            &options(&[("image_id", "img-1"), ("public_key", "pk-1")]),
            "sk-1",
            transport(),
        )
        .unwrap();
        assert_eq!(d.name(), "compshare");
        assert_eq!(d.config().image_id, "img-1");
    }

    #[test]
    fn missing_public_key_is_actionable() {
        let e = build_from_options(
            "compshare",
            &options(&[("image_id", "img-1")]),
            "sk-1",
            transport(),
        )
        .unwrap_err();
        let s = e.to_string();
        assert!(s.contains("public_key"), "{s}");
        assert!(s.contains("secret"), "应说明私钥该放哪：{s}");
    }

    #[test]
    fn missing_private_key_is_rejected() {
        let e = build_from_options(
            "compshare",
            &options(&[("image_id", "img-1"), ("public_key", "pk-1")]),
            "  ",
            transport(),
        )
        .unwrap_err();
        assert!(e.to_string().contains("私钥"), "{e}");
    }

    #[test]
    fn missing_image_id_is_rejected() {
        let e = build_from_options(
            "compshare",
            &options(&[("public_key", "pk-1")]),
            "sk-1",
            transport(),
        )
        .unwrap_err();
        assert!(e.to_string().contains("image_id"), "{e}");
    }

    #[test]
    fn driver_name_is_configurable_per_provider_entry() {
        // 同一个驱动可以挂多个服务商名（如 compshare-east / compshare-west）
        for name in ["compshare", "compshare-east", "compshare-west"] {
            let d = build_from_options(
                name,
                &options(&[("image_id", "img-1"), ("public_key", "pk")]),
                "sk",
                transport(),
            )
            .unwrap();
            assert_eq!(d.name(), name);
        }
    }
}
