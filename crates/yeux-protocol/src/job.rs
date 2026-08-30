use crate::{AgentId, AgentRunId, CapabilityGrant, JobId, ThreadId, WorkspaceId};
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JobSchedule {
    At { at: DateTime<Utc> },
    Interval { every_seconds: u64 },
    Rrule { rrule: String, timezone: String },
    Manual,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RunBudget {
    pub max_tokens: u64,
    pub max_cost_micros: u64,
    pub max_duration_seconds: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct JobSpec {
    pub id: JobId,
    pub name: String,
    pub workspace_id: WorkspaceId,
    pub prompt: String,
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub tool_ids: Vec<String>,
    pub grant: CapabilityGrant,
    pub budget: RunBudget,
    pub schedule: JobSchedule,
    #[serde(default)]
    pub allow_reentry: bool,
    #[serde(default)]
    pub metadata: Value,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Active,
    Paused,
    Running,
    WaitingForApproval,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceIsolation {
    SharedReadOnly,
    GitWorktree,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AgentSpawnSpec {
    pub run_id: AgentRunId,
    pub parent_thread_id: ThreadId,
    pub parent_agent_id: AgentId,
    pub child_agent_id: AgentId,
    pub workspace_id: WorkspaceId,
    pub task: String,
    pub grant: CapabilityGrant,
    pub budget: RunBudget,
    pub isolation: WorkspaceIsolation,
    /// v1 permits only `1`; kept explicit so the kernel can reject escalation.
    pub depth: u8,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentResultState {
    Completed,
    Failed,
    Cancelled,
    BudgetExhausted,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AgentResult {
    pub run_id: AgentRunId,
    pub child_thread_id: ThreadId,
    pub state: AgentResultState,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff_artifact_uri: Option<String>,
    #[serde(default)]
    pub metadata: Value,
}
