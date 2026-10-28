//! API Server implementation: HTTP parsing, routing, handlers, and upstream relay.

pub mod handlers;
pub mod helpers;
pub mod relay;
pub mod request;
pub mod response;
pub mod router;
pub mod server;
pub mod upstream;

// Public HTTP primitives live in their responsibility modules; re-export the
// stable API here for callers that should not depend on the server internals.
pub use request::{parse_request, ParsedRequest};
pub use response::Response;
pub use server::GatewayServer;
pub use upstream::{http_post_json, parse_http_response};
