//! HTTP handler groups.
//!
//! Handler methods currently remain on `GatewayServer` to preserve the
//! request/lock boundary; each group is an explicit future extraction seam.
