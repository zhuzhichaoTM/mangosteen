//! HTTP handler groups（第二层 · API Server 内部按职责分组的处理器）。
//!
//! 每个子模块持有 `impl GatewayServer` 的一组方法。Rust 允许同 crate
//! 多个 `impl` 块分散在不同模块，因此拆分不改变任何对外签名；同时
//! `GatewayServer` 的私有字段以 `pub(crate)` 暴露给 handler 组，
//! 仍然只有网关 crate 内部可见。
//!
//! | 模块 | 路由组 |
//! |------|--------|
//! | [`sessions`] | `/api/sessions*` |
//! | [`status`] | `/health`、`/api/status`、`/api/sysinfo`、`/api/doctor` |
//! | [`models`] | `/api/models*`、`/v1/models` |
//! | [`pull`] | `/api/models/pull`、`/api/models/jobs*` |
//! | [`deployment`] | `/api/deploy`、`/api/deployments*`、`/api/preview`、`/api/assess` |
//! | [`inference`] | `/v1/chat/completions`、`/v1/completions`、`/v1/embeddings` |
//! | [`attachments`] | `/api/extract`、`/api/fetch-url` |
//! | [`settings`] | `/api/settings` |

pub mod attachments;
pub mod deployment;
pub mod inference;
pub mod models;
pub mod pull;
pub mod sessions;
pub mod settings;
pub mod status;
