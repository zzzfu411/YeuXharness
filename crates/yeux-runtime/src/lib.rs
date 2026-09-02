//! Local, side-effecting adapters for the YeuX Harness kernel.
//!
//! The crate deliberately keeps persistence, workspace I/O, process execution,
//! sandbox discovery, and provider HTTP calls behind small interfaces.  Pure
//! state-machine semantics belong in `yeux-core`; this crate never performs
//! side effects while rebuilding a projection from the ledger.

pub mod artifact;
pub mod credentials;
pub mod descriptors;
pub mod ledger;
pub mod process;
pub mod provider;
pub mod sandbox;
pub mod workspace;
pub mod workspace_tools;

pub use artifact::{Artifact, ArtifactError, ArtifactStore};
pub use credentials::{
    CredentialBroker, CredentialError, CredentialLease, InMemoryCredentialBroker,
    NoCredentials as NoCredentialBroker,
};
pub use descriptors::{
    AgentRecord, DescriptorError, DescriptorKind, DescriptorStore, JobRecord, RegisteredDescriptor,
};
pub use ledger::{
    CommandAppendResult, CommandBatchAppendResult, CommandReceipt, CoreProjectionError,
    EventLedger, LedgerError, LedgerEvent, NewCommandReceipt, NewInvocationOutcome,
    NewInvocationUnknown, NewInvocationUnknownOutcome, NewLedgerEvent, Projection, ProjectionItem,
    ProjectionThread, ProjectionTurn,
};
pub use process::{ProcessError, ProcessExecutor, ProcessOutput, ProcessRequest};
pub use provider::{
    BrokerCredentialSource, CredentialSource, NoCredentials, OpenAiCompatibleProvider,
    ProviderConfig, ProviderError, RuntimeModelProvider,
};
pub use sandbox::{
    SandboxBackend, SandboxCapabilities, SandboxError, SandboxRequirement, SandboxedCommand,
};
pub use workspace::{
    ApplyPatchError, ApplyPatchResult, FileRevisionSnapshot, RevisionedFile, Workspace,
    WorkspaceError, WorkspaceIdentitySnapshot,
};
pub use workspace_tools::{
    workspace_apply_patch_spec, workspace_list_spec, workspace_mutation_tool_specs,
    workspace_read_spec, workspace_search_spec, workspace_tool_specs, PreparedWorkspaceMutation,
    SearchCancellation, SearchControl, SearchOperationBudget, WorkspaceDiffSummary,
    WorkspaceSearchControl, WorkspaceToolError, WorkspaceToolLimits, WorkspaceTools,
    WORKSPACE_APPLY_PATCH_TOOL_ID, WORKSPACE_LIST_TOOL_ID, WORKSPACE_READ_TOOL_ID,
    WORKSPACE_SEARCH_DEFAULT_OPERATION_BUDGET, WORKSPACE_SEARCH_HARD_OPERATION_LIMIT,
    WORKSPACE_SEARCH_TOOL_ID, WORKSPACE_TOOL_HARD_LIMITS, WORKSPACE_TOOL_VERSION,
};

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
