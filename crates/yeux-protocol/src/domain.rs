use crate::{AgentId, ItemId, ThreadId, TurnId, WorkspaceId};
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceTrust {
    Untrusted,
    Trusted,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct WorkspaceIdentity {
    /// Canonical path at the moment the workspace was opened.
    pub canonical_root: String,
    /// Stable digest over the canonical path and available platform identity fields.
    pub digest: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inode: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_common_dir: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub root: String,
    pub identity: WorkspaceIdentity,
    pub trust: WorkspaceTrust,
    pub opened_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ThreadStatus {
    Active,
    Idle,
    Archived,
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Thread {
    pub id: ThreadId,
    pub workspace_id: WorkspaceId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_thread_id: Option<ThreadId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_seq: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub status: ThreadStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub last_seq: u64,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TurnState {
    Accepted,
    BuildingContext,
    RequestingModel,
    Streaming,
    ProposedTools,
    WaitingForApproval,
    Authorizing,
    Scheduling,
    Executing,
    IntegratingResults,
    WaitingForInput,
    Cancelling,
    Completed,
    Cancelled,
    Failed,
}

impl TurnState {
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Cancelled | Self::Failed)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Turn {
    pub id: TurnId,
    pub thread_id: ThreadId,
    pub agent_id: AgentId,
    pub state: TurnState,
    pub started_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    UserMessage,
    AssistantMessage,
    Reasoning,
    ToolCall,
    ToolResult,
    Checkpoint,
    Diagnostic,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Item {
    pub id: ItemId,
    pub thread_id: ThreadId,
    pub turn_id: TurnId,
    pub agent_id: AgentId,
    pub kind: ItemKind,
    pub content: Value,
    pub created_at: DateTime<Utc>,
}
