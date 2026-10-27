//! # Mangosteen Core
//!
//! Mangosteen（山竹）领域层。这是四层架构中**唯一被所有层共享**的 crate，
//! 承载与操作系统、云厂商、传输协议均无关的纯领域模型：
//!
//! ```text
//! 用户交互层 (mangosteen-cli)
//!        │
//! API 网关层 (mangosteen-gateway) ── 资源评估引擎 / 调度决策器 / 统一协议适配层
//!        │
//! 算力执行层 (mangosteen-local | mangosteen-cloud)
//!        │
//! 基础能力层 (mangosteen-base) + 平台适配层 (mangosteen-platform)
//! ```
//!
//! ## 分层约束
//!
//! 本 crate **禁止**引入 `std::process` 的平台分支、`libc` / `windows-sys` 等
//! 平台专属依赖。所有 OS 差异必须收敛到 `mangosteen-platform`（平台适配层）。
//!
//! ## 模块地图
//!
//! | 模块 | 归属 | 职责 |
//! |------|------|------|
//! | [`model`] | 基础能力层 / 网关层 | GGUF 模型规格、量化格式、模型台账 |
//! | [`hardware`] | 网关层·资源评估引擎 | 硬件画像数据结构 |
//! | [`estimate`] | 网关层·资源评估引擎 | 显存/内存/算力需求计算与可行性判定 |
//! | [`plan`] | 网关层·调度决策器 | 部署方案（本地参数 / 云端参数） |
//! | [`cloud`] | 算力执行层·多云统一适配层 | 算力实例统一抽象、驱动接口 |
//! | [`policy`] | 算力执行层·后端节点选择器 | 多维加权打分与排序 |
//! | [`config`] | 基础能力层·配置管理 | 配置驱动模型（服务商/策略/硬件/存储） |
//! | [`gguf`] | 基础能力层·模型管理 | GGUF 头部与元数据解析 |
//! | [`openai`] | 网关层·统一协议适配层 | OpenAI 兼容协议数据结构 |
//! | [`net`] | 算力执行层·云端网络工具集 | 端口映射、隧道、存储映射描述 |
//! | [`error`] | 跨层 | 统一错误类型 |

pub mod cloud;
pub mod config;
pub mod error;
pub mod estimate;
pub mod gguf;
pub mod hardware;
pub mod model;
pub mod net;
pub mod openai;
pub mod plan;
pub mod policy;
pub mod source;
pub mod time;
pub mod transport;

pub use error::{MnError, MnResult};

/// 产品身份常量。用于 CLI banner、HTTP `Server` 头、配置目录名。
pub mod identity {
    /// 产品名
    pub const PRODUCT_NAME: &str = "Mangosteen";
    /// 产品中文名
    pub const PRODUCT_NAME_CN: &str = "山竹";
    /// 语义化版本
    pub const VERSION: &str = env!("CARGO_PKG_VERSION");
    /// OpenAI 兼容协议标识
    pub const OPENAI_COMPATIBILITY: &str = "OpenAI/v1";
    /// 配置目录名（xdg / Library / AppData 之下）
    pub const CONFIG_DIR: &str = "mangosteen";
    /// 环境变量前缀
    pub const ENV_PREFIX: &str = "MANGOSTEEN";
}

/// 统一 HTTP 服务默认端口。本地与云端实例的对外入口均由网关接管。
///
/// 刻意**避开 11434**：那是 Ollama 守护进程的固定端口，网关若占用它，
/// 会与 Ollama 引擎（`OllamaEngine::DEFAULT_PORT`）直接抢端口——
/// 表现为「Ollama 已启动但网关绑不上」或反过来。
/// 选 11516 是相对冷门的端口，与本地引擎区间 11435-11999 有重叠但
/// 分配器会 `is_port_bindable` 探测并跳过，不会真撞。
pub const DEFAULT_GATEWAY_PORT: u16 = 11_516;

/// 分页查询的默认与上限条数。
pub const DEFAULT_PAGE_LIMIT: usize = 100;
pub const MAX_PAGE_LIMIT: usize = 1000;
