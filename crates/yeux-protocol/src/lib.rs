//! Stable, I/O-free protocol types shared by every YeuX client and runtime.
//!
//! The wire protocol is JSON-RPC 2.0. Stdio and Unix socket transports use
//! newline-delimited UTF-8 JSON; transport framing is intentionally not
//! implemented in this crate.

#![forbid(unsafe_code)]

pub mod api;
pub mod domain;
pub mod event;
pub mod ids;
pub mod job;
pub mod jsonrpc;
pub mod model;
pub mod policy;
pub mod schema;
pub mod tool;

pub use api::*;
pub use domain::*;
pub use event::*;
pub use ids::*;
pub use job::*;
pub use jsonrpc::*;
pub use model::*;
pub use policy::*;
pub use schema::*;
pub use tool::*;

/// Current stable wire protocol version.
///
/// P1 adds required invocation and approval evidence fields, so the wire and
/// persisted event vocabulary are intentionally incompatible with the P0
/// protocol rather than silently accepting history that cannot be authorized
/// or reconciled safely.
pub const PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion::new(2, 0);
