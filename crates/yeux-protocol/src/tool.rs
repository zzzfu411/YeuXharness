use crate::{AgentId, ApprovalBinding, InvocationId, ThreadId, TurnId, WorkspaceId};
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConcurrencyClass {
    StructuredReadOnly,
    SerialProcess,
    SerialMutation,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Idempotency {
    Idempotent,
    IdempotentWithKey,
    NonIdempotent,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Reversibility {
    Reversible,
    Compensatable,
    Irreversible,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PathScope {
    pub path: String,
    #[serde(default)]
    pub recursive: bool,
    /// Set after canonical path resolution. An unresolved scope must never be approved.
    #[serde(default)]
    pub resolved: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ProcessEffect {
    pub executable: String,
    #[serde(default)]
    pub argument_digest: Option<String>,
    #[serde(default)]
    pub may_spawn_children: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct NetworkEffect {
    pub scheme: String,
    pub host: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ExternalEffect {
    pub system: String,
    pub operation: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SecretHandle {
    pub name: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct EffectSet {
    #[serde(default)]
    pub filesystem_read: Vec<PathScope>,
    #[serde(default)]
    pub filesystem_write: Vec<PathScope>,
    #[serde(default)]
    pub filesystem_delete: Vec<PathScope>,
    #[serde(default)]
    pub processes: Vec<ProcessEffect>,
    #[serde(default)]
    pub network: Vec<NetworkEffect>,
    #[serde(default)]
    pub secrets: Vec<SecretHandle>,
    #[serde(default)]
    pub external_writes: Vec<ExternalEffect>,
    pub idempotency: Idempotency,
    pub reversibility: Reversibility,
}

impl Default for EffectSet {
    fn default() -> Self {
        Self {
            filesystem_read: Vec::new(),
            filesystem_write: Vec::new(),
            filesystem_delete: Vec::new(),
            processes: Vec::new(),
            network: Vec::new(),
            secrets: Vec::new(),
            external_writes: Vec::new(),
            idempotency: Idempotency::Idempotent,
            reversibility: Reversibility::Reversible,
        }
    }
}

impl EffectSet {
    pub fn is_read_only(&self) -> bool {
        self.filesystem_write.is_empty()
            && self.filesystem_delete.is_empty()
            && self.processes.is_empty()
            && self.network.is_empty()
            && self.secrets.is_empty()
            && self.external_writes.is_empty()
    }

    pub fn has_external_side_effects(&self) -> bool {
        !self.network.is_empty() || !self.external_writes.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ToolSpec {
    pub id: String,
    pub version: String,
    pub description: String,
    pub input_schema: Value,
    pub output_schema: Value,
    /// Registration-time upper bound; prepare may only narrow this set.
    pub effect_template: EffectSet,
    pub concurrency: ConcurrencyClass,
    pub timeout_ms: u64,
    pub inline_output_budget_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PreparedInvocation {
    pub invocation_id: InvocationId,
    pub tool_id: String,
    pub tool_version: String,
    pub workspace_id: WorkspaceId,
    pub workspace_identity_digest: String,
    pub thread_id: ThreadId,
    pub turn_id: TurnId,
    pub agent_id: AgentId,
    pub normalized_arguments: Value,
    pub normalized_arguments_digest: String,
    pub effects: EffectSet,
    pub effect_digest: String,
    /// Opaque, short-lived executor capability. Never include a credential in it.
    pub prepared_token: String,
    pub prepared_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval: Option<ApprovalBinding>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InvocationState {
    Proposed,
    Approved,
    Prepared,
    Started,
    Completed,
    Failed,
    Cancelled,
    Unknown,
}

impl InvocationState {
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

/// A conclusion reached without executing an invocation again after its
/// outcome became unknown.
///
/// Cancellation is intentionally absent: once execution may have produced a
/// side effect, reconciliation must determine whether that effect completed or
/// failed. It cannot retroactively claim that the invocation was cancelled.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InvocationReconciliationOutcome {
    Completed,
    Failed,
}

impl InvocationReconciliationOutcome {
    pub const fn state(self) -> InvocationState {
        match self {
            Self::Completed => InvocationState::Completed,
            Self::Failed => InvocationState::Failed,
        }
    }
}

/// Durable evidence explaining how an unknown invocation was reconciled.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct InvocationReconciliationEvidence {
    /// Stable identifier for the reconciliation mechanism, such as an
    /// executor receipt lookup or an operator review workflow.
    pub source: String,
    /// Bounded human-readable conclusion. Large receipts belong in the
    /// artifact store and are referenced by `artifact_uri`.
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact_uri: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ToolResult {
    pub invocation_id: InvocationId,
    pub output: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact_uri: Option<String>,
    #[serde(default)]
    pub truncated: bool,
}
