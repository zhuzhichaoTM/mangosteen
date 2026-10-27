//! Route dispatch boundary.
//!
//! Routing is kept on `GatewayServer` while handlers are moved incrementally;
//! this module is the stable internal seam for that migration.
pub use super::server::GatewayServer;
