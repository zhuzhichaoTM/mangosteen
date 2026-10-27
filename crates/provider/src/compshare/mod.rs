//! 优云智算（Compshare / UCloud）GPU 实例驱动。
//!
//! 归属：**第四层 · 云供应设备能力（provider 内部驱动）**。
//!
//! ## 为什么单独成一个子模块
//!
//! 优云智算用 **UCloud RPC 形态**：单端点 + `Action` 参数 + `PublicKey`/`Signature`
//! （SHA1）签名 + 扁平表单编码 + `RetCode == 0` 判成功。这套协议无法用通用
//! REST profile 的字段映射表达，因此作为一个完整驱动实现存在。
//!
//! | 维度 | 通用 REST 驱动 | 本驱动 |
//! |------|---------------|--------|
//! | 端点 | 每个动作一个 URL | 全部 `POST https://api.compshare.cn` |
//! | 动作区分 | HTTP 方法 + 路径 | 表单参数 `Action=...` |
//! | 认证 | Token 请求头 | `PublicKey` + `Signature`（SHA1） |
//! | 编码 | JSON body | 扁平表单（`Disks.0.Size=100`） |
//! | 成功判定 | HTTP 2xx | 2xx **且** `RetCode == 0` |
//!
//! ## 模块地图
//!
//! | 模块 | 职责 |
//! |------|------|
//! | [`sign`] | UCloud V1 签名与凭证 |
//! | [`sha1`] | SHA-1 摘要（签名所需，工作区只依赖 sha2） |
//! | [`params`] | 参数扁平化与表单编码 |
//! | [`transport`] | HTTP 传输抽象与测试替身 |
//! | [`model`] | 厂商响应结构（字段名与文档逐字一致） |
//! | [`mapping`] | 厂商表示 → 领域模型的唯一换算点 |
//! | [`retcode`] | 返回码语义归类与错误归一化 |
//! | [`client`] | 各接口的具体调用 |
//! | [`config`] | 配置解析与校验 |
//! | [`driver`] | [`ProviderDriver`] 实现（业务补偿） |
//! | [`capacity`] | 库存与价格扫描 |

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

/// 用共享传输构造驱动。
pub fn make_driver(
    name: &str,
    config: CompshareConfig,
    credential: Credential,
    transport: std::sync::Arc<dyn Transport + Send + Sync>,
) -> mangosteen_core::MnResult<CompshareDriver<std::sync::Arc<dyn Transport + Send + Sync>>> {
    CompshareDriver::new(name, config, credential, transport)
}

/// 从通用 `options` + 私钥构造驱动（注册表最常用的入口）。
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

/// 该驱动名是否由本驱动提供。
pub fn handles(driver: &str) -> bool {
    driver.trim().eq_ignore_ascii_case(DRIVER_NAME)
}
