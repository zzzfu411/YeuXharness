//! Deterministic, side-effect-free semantics for YeuX Harness.
//!
//! This crate owns state transitions, policy intersection, approval binding,
//! and event projection. It defines ports for runtime I/O but contains no I/O
//! implementation. In particular, [`projection::replay`] cannot call a model,
//! tool, network, or external system.

#![forbid(unsafe_code)]

pub mod agent;
pub mod approval;
pub mod clock;
pub mod digest;
pub mod ids;
pub mod invocation;
pub mod policy;
pub mod ports;
pub mod projection;

pub use agent::*;
pub use approval::*;
pub use clock::*;
pub use digest::*;
pub use ids::*;
pub use invocation::*;
pub use policy::*;
pub use ports::*;
pub use projection::*;
