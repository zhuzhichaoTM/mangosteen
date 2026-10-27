//! HTTP request parsing API.
//!
//! The concrete parser remains in `server.rs` during the mechanical split so
//! the public behavior and stream relay ordering stay unchanged.
pub use super::server::{parse_request, ParsedRequest};
