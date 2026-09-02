//! Authoritative daemon and JSON-RPC transports for YeuX Harness.

#![forbid(unsafe_code)]

mod commands;
mod grants;
pub mod pipeline;
pub mod runner;
pub mod server;
pub mod tool_calls;
pub mod tools;

pub use server::{Daemon, DaemonConfig, DaemonError};
