use crate::{can_transition_invocation, can_transition_turn};
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;
use yeux_protocol::{
    AgentResult, AgentRunId, EffectSet, Event, EventEnvelope, EventId, InvocationId,
    InvocationState, Item, ItemId, JobId, JobSpec, JobState, ModelEvent, ModelRequestId, Thread,
    ThreadId, ThreadStatus, Turn, TurnId, TurnState, Workspace, WorkspaceId, PROTOCOL_VERSION,
};

#[derive(Clone, Debug, PartialEq)]
pub struct ProjectedInvocation {
    pub invocation_id: InvocationId,
    pub thread_id: ThreadId,
    pub turn_id: Option<TurnId>,
    pub tool_id: String,
    pub effects: EffectSet,
    pub state: InvocationState,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProjectedJob {
    pub spec: JobSpec,
    pub state: JobState,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Projection {
    pub workspaces: BTreeMap<WorkspaceId, Workspace>,
    pub threads: BTreeMap<ThreadId, Thread>,
    pub turns: BTreeMap<TurnId, Turn>,
    pub items: BTreeMap<ItemId, Item>,
    pub invocations: BTreeMap<InvocationId, ProjectedInvocation>,
    pub jobs: BTreeMap<JobId, ProjectedJob>,
    pub model_events: BTreeMap<ModelRequestId, Vec<ModelEvent>>,
    pub agent_results: BTreeMap<AgentRunId, AgentResult>,
    pub steering: BTreeMap<TurnId, Vec<String>>,
    pub last_seq_by_thread: BTreeMap<ThreadId, u64>,
    seen_events: BTreeSet<EventId>,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ReplayError {
    #[error(
        "unsupported event schema {found_major}.{found_minor}; runtime supports {supported_major}.{supported_minor}"
    )]
    IncompatibleSchema {
        found_major: u16,
        found_minor: u16,
        supported_major: u16,
        supported_minor: u16,
    },
    #[error("duplicate event ID {0}")]
    DuplicateEvent(EventId),
    #[error("thread {thread_id} expected sequence {expected}, got {actual}")]
    SequenceGap {
        thread_id: ThreadId,
        expected: u64,
        actual: u64,
    },
    #[error("entity already exists: {kind} {id}")]
    DuplicateEntity { kind: &'static str, id: String },
    #[error("missing {kind} {id}")]
    MissingEntity { kind: &'static str, id: String },
    #[error("event envelope and payload disagree: {0}")]
    EnvelopeMismatch(&'static str),
    #[error("thread {thread_id} already has active turn {active_turn_id}")]
    ActiveTurnConflict {
        thread_id: ThreadId,
        active_turn_id: TurnId,
    },
    #[error("turn {turn_id} state mismatch: projected {projected:?}, event says {event_from:?}")]
    TurnStateMismatch {
        turn_id: TurnId,
        projected: TurnState,
        event_from: TurnState,
    },
    #[error("invalid turn transition from {from:?} to {to:?}")]
    InvalidTurnTransition { from: TurnState, to: TurnState },
    #[error(
        "invocation {invocation_id} state mismatch: projected {projected:?}, event says {event_from:?}"
    )]
    InvocationStateMismatch {
        invocation_id: InvocationId,
        projected: InvocationState,
        event_from: InvocationState,
    },
    #[error("invalid invocation transition from {from:?} to {to:?}")]
    InvalidInvocationTransition {
        from: InvocationState,
        to: InvocationState,
    },
    #[error("job {job_id} state mismatch: projected {projected:?}, event says {event_from:?}")]
    JobStateMismatch {
        job_id: JobId,
        projected: JobState,
        event_from: JobState,
    },
}

impl Projection {
    pub fn active_turn(&self, thread_id: ThreadId) -> Option<&Turn> {
        self.turns
            .values()
            .find(|turn| turn.thread_id == thread_id && !turn.state.is_terminal())
    }

    pub fn apply(&mut self, envelope: &EventEnvelope) -> Result<(), ReplayError> {
        if !PROTOCOL_VERSION.accepts(envelope.schema_version) {
            return Err(ReplayError::IncompatibleSchema {
                found_major: envelope.schema_version.major,
                found_minor: envelope.schema_version.minor,
                supported_major: PROTOCOL_VERSION.major,
                supported_minor: PROTOCOL_VERSION.minor,
            });
        }
        if self.seen_events.contains(&envelope.event_id) {
            return Err(ReplayError::DuplicateEvent(envelope.event_id));
        }
        let expected = self
            .last_seq_by_thread
            .get(&envelope.thread_id)
            .copied()
            .unwrap_or(0)
            + 1;
        if envelope.seq != expected {
            return Err(ReplayError::SequenceGap {
                thread_id: envelope.thread_id,
                expected,
                actual: envelope.seq,
            });
        }

        match &envelope.event {
            Event::WorkspaceOpened { workspace } => {
                if self.workspaces.contains_key(&workspace.id) {
                    return Err(ReplayError::DuplicateEntity {
                        kind: "workspace",
                        id: workspace.id.to_string(),
                    });
                }
                self.workspaces.insert(workspace.id, workspace.clone());
            }
            Event::WorkspaceTrustChanged {
                workspace_id,
                trust,
            } => {
                let workspace = self.workspaces.get_mut(workspace_id).ok_or_else(|| {
                    ReplayError::MissingEntity {
                        kind: "workspace",
                        id: workspace_id.to_string(),
                    }
                })?;
                workspace.trust = *trust;
            }
            Event::ThreadStarted { thread } | Event::ThreadForked { thread } => {
                if thread.id != envelope.thread_id {
                    return Err(ReplayError::EnvelopeMismatch("thread_id"));
                }
                if self.threads.contains_key(&thread.id) {
                    return Err(ReplayError::DuplicateEntity {
                        kind: "thread",
                        id: thread.id.to_string(),
                    });
                }
                if !self.workspaces.contains_key(&thread.workspace_id) {
                    return Err(ReplayError::MissingEntity {
                        kind: "workspace",
                        id: thread.workspace_id.to_string(),
                    });
                }
                if let Some(parent_id) = thread.parent_thread_id {
                    if !self.threads.contains_key(&parent_id) {
                        return Err(ReplayError::MissingEntity {
                            kind: "parent thread",
                            id: parent_id.to_string(),
                        });
                    }
                }
                self.threads.insert(thread.id, thread.clone());
            }
            Event::ThreadArchived { thread_id } => {
                if *thread_id != envelope.thread_id {
                    return Err(ReplayError::EnvelopeMismatch("thread_id"));
                }
                let thread =
                    self.threads
                        .get_mut(thread_id)
                        .ok_or_else(|| ReplayError::MissingEntity {
                            kind: "thread",
                            id: thread_id.to_string(),
                        })?;
                thread.status = ThreadStatus::Archived;
            }
            Event::TurnStarted { turn } => {
                if turn.thread_id != envelope.thread_id || envelope.turn_id != Some(turn.id) {
                    return Err(ReplayError::EnvelopeMismatch("turn_id"));
                }
                if turn.state != TurnState::Accepted {
                    return Err(ReplayError::EnvelopeMismatch("initial turn state"));
                }
                if !self.threads.contains_key(&turn.thread_id) {
                    return Err(ReplayError::MissingEntity {
                        kind: "thread",
                        id: turn.thread_id.to_string(),
                    });
                }
                if let Some(active) = self.active_turn(turn.thread_id) {
                    return Err(ReplayError::ActiveTurnConflict {
                        thread_id: turn.thread_id,
                        active_turn_id: active.id,
                    });
                }
                if self.turns.contains_key(&turn.id) {
                    return Err(ReplayError::DuplicateEntity {
                        kind: "turn",
                        id: turn.id.to_string(),
                    });
                }
                self.turns.insert(turn.id, turn.clone());
                if let Some(thread) = self.threads.get_mut(&turn.thread_id) {
                    thread.status = ThreadStatus::Active;
                }
            }
            Event::TurnStateChanged {
                turn_id,
                from,
                to,
                reason,
            } => {
                if envelope.turn_id != Some(*turn_id) {
                    return Err(ReplayError::EnvelopeMismatch("turn_id"));
                }
                let turn =
                    self.turns
                        .get_mut(turn_id)
                        .ok_or_else(|| ReplayError::MissingEntity {
                            kind: "turn",
                            id: turn_id.to_string(),
                        })?;
                if turn.state != *from {
                    return Err(ReplayError::TurnStateMismatch {
                        turn_id: *turn_id,
                        projected: turn.state,
                        event_from: *from,
                    });
                }
                if !can_transition_turn(*from, *to) {
                    return Err(ReplayError::InvalidTurnTransition {
                        from: *from,
                        to: *to,
                    });
                }
                turn.state = *to;
                if *to == TurnState::Failed {
                    turn.failure = reason.clone();
                }
                if to.is_terminal() {
                    turn.ended_at = Some(envelope.time);
                    if let Some(thread) = self.threads.get_mut(&turn.thread_id) {
                        thread.status = if *to == TurnState::Failed {
                            ThreadStatus::Failed
                        } else {
                            ThreadStatus::Idle
                        };
                    }
                }
            }
            Event::TurnSteered { turn_id, message } => {
                if !self.turns.contains_key(turn_id) {
                    return Err(ReplayError::MissingEntity {
                        kind: "turn",
                        id: turn_id.to_string(),
                    });
                }
                self.steering
                    .entry(*turn_id)
                    .or_default()
                    .push(message.clone());
            }
            Event::ItemAdded { item } => {
                if item.thread_id != envelope.thread_id || envelope.turn_id != Some(item.turn_id) {
                    return Err(ReplayError::EnvelopeMismatch("item parent"));
                }
                if !self.turns.contains_key(&item.turn_id) {
                    return Err(ReplayError::MissingEntity {
                        kind: "turn",
                        id: item.turn_id.to_string(),
                    });
                }
                if self.items.insert(item.id, item.clone()).is_some() {
                    return Err(ReplayError::DuplicateEntity {
                        kind: "item",
                        id: item.id.to_string(),
                    });
                }
            }
            Event::ModelRequested { request_id } => {
                self.model_events.entry(*request_id).or_default();
            }
            Event::ModelStreamEvent {
                request_id,
                model_event,
            } => {
                let events = self.model_events.get_mut(request_id).ok_or_else(|| {
                    ReplayError::MissingEntity {
                        kind: "model request",
                        id: request_id.to_string(),
                    }
                })?;
                events.push(model_event.clone());
            }
            Event::InvocationProposed {
                invocation_id,
                tool_id,
                effects,
            } => {
                if self.invocations.contains_key(invocation_id) {
                    return Err(ReplayError::DuplicateEntity {
                        kind: "invocation",
                        id: invocation_id.to_string(),
                    });
                }
                self.invocations.insert(
                    *invocation_id,
                    ProjectedInvocation {
                        invocation_id: *invocation_id,
                        thread_id: envelope.thread_id,
                        turn_id: envelope.turn_id,
                        tool_id: tool_id.clone(),
                        effects: effects.clone(),
                        state: InvocationState::Proposed,
                        reason: None,
                    },
                );
            }
            Event::InvocationStateChanged {
                invocation_id,
                from,
                to,
                reason,
            } => {
                let invocation = self.invocations.get_mut(invocation_id).ok_or_else(|| {
                    ReplayError::MissingEntity {
                        kind: "invocation",
                        id: invocation_id.to_string(),
                    }
                })?;
                if invocation.state != *from {
                    return Err(ReplayError::InvocationStateMismatch {
                        invocation_id: *invocation_id,
                        projected: invocation.state,
                        event_from: *from,
                    });
                }
                if !can_transition_invocation(*from, *to) {
                    return Err(ReplayError::InvalidInvocationTransition {
                        from: *from,
                        to: *to,
                    });
                }
                invocation.state = *to;
                invocation.reason = reason.clone();
            }
            Event::JobCreated { job } => {
                if self.jobs.contains_key(&job.id) {
                    return Err(ReplayError::DuplicateEntity {
                        kind: "job",
                        id: job.id.to_string(),
                    });
                }
                self.jobs.insert(
                    job.id,
                    ProjectedJob {
                        spec: job.clone(),
                        state: JobState::Active,
                    },
                );
            }
            Event::JobStateChanged { job_id, from, to } => {
                let job = self
                    .jobs
                    .get_mut(job_id)
                    .ok_or_else(|| ReplayError::MissingEntity {
                        kind: "job",
                        id: job_id.to_string(),
                    })?;
                if job.state != *from {
                    return Err(ReplayError::JobStateMismatch {
                        job_id: *job_id,
                        projected: job.state,
                        event_from: *from,
                    });
                }
                job.state = *to;
            }
            Event::AgentSpawned { spec: _ } => {}
            Event::AgentCompleted { result } => {
                self.agent_results.insert(result.run_id, result.clone());
            }
            Event::RuntimeDiagnostic { .. } => {}
        }

        self.seen_events.insert(envelope.event_id);
        self.last_seq_by_thread
            .insert(envelope.thread_id, envelope.seq);
        if let Some(thread) = self.threads.get_mut(&envelope.thread_id) {
            thread.last_seq = envelope.seq;
            thread.updated_at = envelope.time;
        }
        Ok(())
    }
}

/// Rebuilds projections from evidence only. The function intentionally accepts
/// no provider, tool executor, event store, or callback, making side effects
/// impossible during replay.
pub fn replay<'a>(
    events: impl IntoIterator<Item = &'a EventEnvelope>,
) -> Result<Projection, ReplayError> {
    let mut projection = Projection::default();
    for event in events {
        projection.apply(event)?;
    }
    Ok(projection)
}
