//! 云端服务商驱动集合。
//!
//! 归属：**算力执行层 · 多云统一适配层**。
//!
//! ## 目录结构
//!
//! | 模块 | 说明 |
//! |------|------|
//! | [`transport`] | HTTP 传输抽象（驱动只依赖 trait，便于注入 mock） |
//! | [`rest`] | 配置驱动的通用 REST 驱动（覆盖绝大多数厂商） |
//! | [`profiles`] | 配置驱动 REST 模板（当前无内置模板） 的内置驱动配置 |
//!
//! ## 新增服务商的两条路径
//!
//! 1. **配置路径（首选）**：在 `config.toml` 的 `providers.<name>` 下写
//!    `profile` 段（端点点位、字段映射、状态映射），无需改代码、无需重编译。
//! 2. **代码路径（特殊场景）**：厂商需要签名算法、
//!    分页聚合、异步任务轮询等复杂逻辑时，新写一个 `impl ProviderDriver`，
//!    在 [`crate::registry`] 注册。
//!
//! 两条路径对上层完全等价——网关只认 [`mangosteen_core::cloud::ProviderDriver`]。

pub mod curl;
pub mod profiles;
pub mod rest;
pub mod transport;

pub use curl::CurlTransport;
pub use profiles::builtin_profile;
pub use rest::{
    AuthStyle, Credentials, EndpointProfile, FieldMap, RestDriver, RestDriverProfile, StateMap,
};
pub use transport::{HttpRequest, HttpResponse, MockTransport, Transport};
