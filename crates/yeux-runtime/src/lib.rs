//! Local, side-effecting adapters for the YeuX Harness kernel.
//!
//! The crate deliberately keeps persistence, workspace I/O, process execution,
//! sandbox discovery, and provider HTTP calls behind small interfaces.  Pure
//! state-machine semantics belong in `yeux-core`; this crate never performs
//! side effects while rebuilding a projection from the ledger.

pub mod artifact;
pub mod descriptors;
pub mod ledger;
pub mod policy;
pub mod process;
pub mod provider;
pub mod sandbox;
pub mod workspace;

pub use artifact::{Artifact, ArtifactError, ArtifactStore};
pub use descriptors::{
    AgentRecord, DescriptorError, DescriptorKind, DescriptorStore, JobRecord, RegisteredDescriptor,
};
pub use ledger::{
    CommandAppendResult, CommandBatchAppendResult, CommandReceipt, CoreProjectionError,
    EventLedger, LedgerError, LedgerEvent, NewCommandReceipt, NewLedgerEvent, Projection,
    ProjectionItem, ProjectionThread, ProjectionTurn,
};
pub use policy::{PolicyDecision, PolicyEvaluator, PolicyRequest};
pub use process::{ProcessError, ProcessExecutor, ProcessOutput, ProcessRequest};
pub use provider::{
    CredentialSource, NoCredentials, OpenAiCompatibleProvider, ProviderConfig, ProviderError,
    RuntimeModelProvider,
};
pub use sandbox::{
    SandboxBackend, SandboxCapabilities, SandboxError, SandboxRequirement, SandboxedCommand,
};
pub use workspace::{ApplyPatchError, ApplyPatchResult, RevisionedFile, Workspace, WorkspaceError};

/// Error shared by the concrete runtime adapters.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid runtime data: {0}")]
    InvalidData(String),
}

pub type Result<T, E = RuntimeError> = std::result::Result<T, E>;
