use crate::{
    AgentId, AgentResult, AgentSpawnSpec, CausationId, EffectSet, EventId, Idempotency,
    InvocationId, InvocationReconciliationEvidence, InvocationReconciliationOutcome,
    InvocationState, Item, JobId, JobSpec, JobState, ModelEvent, ModelRequestId, ProtocolVersion,
    Thread, ThreadId, Turn, TurnId, TurnState, Workspace, WorkspaceId, WorkspaceTrust,
};
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct EventEnvelope {
    pub schema_version: ProtocolVersion,
    pub event_id: EventId,
    pub thread_id: ThreadId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<TurnId>,
    pub agent_id: AgentId,
    /// Strictly monotonic within one thread. The first event has sequence 1.
    pub seq: u64,
    pub time: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub causation_id: Option<CausationId>,
    #[serde(flatten)]
    pub event: Event,
}

impl EventEnvelope {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        schema_version: ProtocolVersion,
        event_id: EventId,
        thread_id: ThreadId,
        turn_id: Option<TurnId>,
        agent_id: AgentId,
        seq: u64,
        time: DateTime<Utc>,
        causation_id: Option<CausationId>,
        event: Event,
    ) -> Self {
        Self {
            schema_version,
            event_id,
            thread_id,
            turn_id,
            agent_id,
            seq,
            time,
            causation_id,
            event,
        }
    }
}

/// Canonical JSON-RPC notification emitted by both stdio and socket servers.
pub type EventNotification = crate::NotificationEnvelope<EventEnvelope>;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", content = "payload")]
pub enum Event {
    #[serde(rename = "workspace/opened")]
    WorkspaceOpened { workspace: Workspace },
    #[serde(rename = "workspace/trust_changed")]
    WorkspaceTrustChanged {
        workspace_id: WorkspaceId,
        trust: WorkspaceTrust,
    },
    #[serde(rename = "thread/started")]
    ThreadStarted { thread: Thread },
    #[serde(rename = "thread/forked")]
    ThreadForked { thread: Thread },
    #[serde(rename = "thread/archived")]
    ThreadArchived { thread_id: ThreadId },
    #[serde(rename = "turn/started")]
    TurnStarted { turn: Turn },
    #[serde(rename = "turn/state_changed")]
    TurnStateChanged {
        turn_id: TurnId,
        from: TurnState,
        to: TurnState,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    #[serde(rename = "turn/steered")]
    TurnSteered { turn_id: TurnId, message: String },
    #[serde(rename = "item/added")]
    ItemAdded { item: Item },
    #[serde(rename = "model/requested")]
    ModelRequested { request_id: ModelRequestId },
    #[serde(rename = "model/event")]
    ModelStreamEvent {
        request_id: ModelRequestId,
        model_event: ModelEvent,
    },
    #[serde(rename = "tool/proposed")]
    InvocationProposed {
        invocation_id: InvocationId,
        call_id: String,
        tool_id: String,
        tool_version: String,
        normalized_arguments_digest: String,
        effects: EffectSet,
        effect_digest: String,
        idempotency: Idempotency,
    },
    #[serde(rename = "tool/state_changed")]
    InvocationStateChanged {
        invocation_id: InvocationId,
        from: InvocationState,
        to: InvocationState,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// Explicitly resolves an invocation whose external outcome could not be
    /// proven after execution started. This event never authorizes a retry.
    #[serde(rename = "tool/reconciled")]
    InvocationReconciled {
        invocation_id: InvocationId,
        outcome: InvocationReconciliationOutcome,
        evidence: InvocationReconciliationEvidence,
    },
    #[serde(rename = "job/created")]
    JobCreated { job: JobSpec },
    #[serde(rename = "job/state_changed")]
    JobStateChanged {
        job_id: JobId,
        from: JobState,
        to: JobState,
    },
    #[serde(rename = "agent/spawned")]
    AgentSpawned { spec: AgentSpawnSpec },
    #[serde(rename = "agent/completed")]
    AgentCompleted { result: AgentResult },
    #[serde(rename = "runtime/diagnostic")]
    RuntimeDiagnostic {
        code: String,
        message: String,
        #[serde(default)]
        recoverable: bool,
    },
}
