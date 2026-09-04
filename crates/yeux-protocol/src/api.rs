//! Typed parameters and results for every stable JSON-RPC method.

use crate::*;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceOpenParams {
    pub path: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceOpenResult {
    pub workspace: Workspace,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceTrustParams {
    pub workspace_id: WorkspaceId,
    pub trust: WorkspaceTrust,
    pub identity_digest: String,
}

pub type WorkspaceTrustResult = WorkspaceOpenResult;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceStatusParams {
    pub workspace_id: WorkspaceId,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceStatusResult {
    pub workspace: Workspace,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_thread_id: Option<ThreadId>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadStartParams {
    pub workspace_id: WorkspaceId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default)]
    pub agent_id: Option<AgentId>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadResult {
    pub thread: Thread,
}

pub type ThreadStartResult = ThreadResult;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadResumeParams {
    pub thread_id: ThreadId,
    #[serde(default)]
    pub after_seq: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadForkParams {
    pub thread_id: ThreadId,
    pub at_seq: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadReadParams {
    pub thread_id: ThreadId,
    #[serde(default)]
    pub after_seq: u64,
    #[serde(default = "default_page_size")]
    pub limit: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadReadResult {
    pub thread: Thread,
    pub events: Vec<EventEnvelope>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_after_seq: Option<u64>,
}

pub type ThreadResumeResult = ThreadReadResult;
pub type ThreadForkResult = ThreadResult;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadListParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<WorkspaceId>,
    #[serde(default)]
    pub include_archived: bool,
    #[serde(default = "default_page_size")]
    pub limit: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

fn default_page_size() -> u32 {
    100
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadListResult {
    pub threads: Vec<Thread>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadArchiveParams {
    pub thread_id: ThreadId,
}

pub type ThreadArchiveResult = ThreadResult;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadCompactParams {
    pub thread_id: ThreadId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub through_seq: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadCompactResult {
    pub checkpoint_item: Item,
    pub source_start_seq: u64,
    pub source_end_seq: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadSubscribeParams {
    pub thread_id: ThreadId,
    #[serde(default)]
    pub after_seq: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadSubscribeResult {
    pub subscription_id: String,
    pub replayed_through_seq: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartParams {
    pub thread_id: ThreadId,
    #[serde(default)]
    pub agent_id: Option<AgentId>,
    pub content: Vec<ContentBlock>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capability_override: Option<CapabilityGrant>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TurnResult {
    pub turn: Turn,
}

pub type TurnStartResult = TurnResult;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TurnSteerParams {
    pub thread_id: ThreadId,
    pub turn_id: TurnId,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TurnInterruptParams {
    pub thread_id: ThreadId,
    pub turn_id: TurnId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AcceptedResult {
    pub accepted: bool,
}

pub type TurnSteerResult = AcceptedResult;
pub type TurnInterruptResult = AcceptedResult;

/// Explicitly resolve an invocation whose external outcome could not be
/// proven after execution started. This request is evidence-only: it must not
/// cause the daemon to execute or retry the original tool invocation. The
/// current daemon accepts only `operator_review` evidence; machine-verified
/// receipt sources require a separate authority path.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InvocationReconcileParams {
    pub thread_id: ThreadId,
    pub invocation_id: InvocationId,
    pub outcome: InvocationReconciliationOutcome,
    pub evidence: InvocationReconciliationEvidence,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InvocationReconcileResult {
    pub thread_id: ThreadId,
    pub invocation_id: InvocationId,
    pub state: InvocationState,
    pub evidence: InvocationReconciliationEvidence,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModelListParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModelListResult {
    pub models: Vec<ModelDescriptor>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SkillDescriptor {
    pub id: String,
    pub name: String,
    pub description: String,
    pub source: String,
    pub content_digest: String,
    pub trusted: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SkillListParams {}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SkillListResult {
    pub skills: Vec<SkillDescriptor>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct McpStatusParams {}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct McpServerStatus {
    pub id: String,
    pub transport: String,
    pub state: String,
    pub discovered_tool_count: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct McpStatusResult {
    pub servers: Vec<McpServerStatus>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PluginListParams {}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PluginDescriptor {
    pub id: String,
    pub version: String,
    pub content_digest: String,
    pub state: String,
    pub capabilities: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PluginListResult {
    pub plugins: Vec<PluginDescriptor>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct JobCreateParams {
    pub job: JobSpec,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct JobResult {
    pub job: JobSpec,
    pub state: JobState,
}

pub type JobCreateResult = JobResult;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct JobListParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<WorkspaceId>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct JobListResult {
    pub jobs: Vec<JobResult>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct JobIdParams {
    pub job_id: JobId,
}

pub type JobPauseParams = JobIdParams;
pub type JobPauseResult = JobResult;
pub type JobResumeParams = JobIdParams;
pub type JobResumeResult = JobResult;
pub type JobRunParams = JobIdParams;
pub type JobRunResult = JobResult;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalRequestParams {
    pub invocation: PreparedInvocation,
    pub explanation: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unified_diff: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalRequestResult {
    pub approved: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval: Option<ApprovalBinding>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UserInputParams {
    pub thread_id: ThreadId,
    pub turn_id: TurnId,
    pub prompt: String,
    #[serde(default)]
    pub metadata: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UserInputResult {
    pub content: Vec<ContentBlock>,
}
