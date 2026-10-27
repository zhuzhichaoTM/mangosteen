//! 云端驱动共用的 HTTP 传输契约。
//!
//! DTO、trait 与 mock 统一由基础层提供；云驱动只保留 curl 实现。
pub use mangosteen_core::transport::{HttpRequest, HttpResponse, MockTransport, Transport};
