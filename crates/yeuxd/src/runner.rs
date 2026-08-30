//! Minimal read-only model turn runner.
//!
//! The daemon remains responsible for accepting a turn and recording its user
//! message. This module advances that accepted turn through one provider call.
//! Every externally visible update is persisted before it is broadcast.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, MutexGuard,
};

use serde_json::{json, Value};
use thiserror::Error;
use tokio::sync::broadcast;
use yeux_core::{Clock, IdError, IdGenerator, ModelEventSink, ModelProvider, PortError};
use yeux_protocol::{
    AgentId, CausationId, ContentBlock, Event, EventEnvelope, Item, ItemId, ItemKind, MessageRole,
    ModelEvent, ModelMessage, ModelRequest, ModelRequestId, StopReason, ThreadId, TokenBudget,
    TurnId, TurnState, PROTOCOL_VERSION,
};
use yeux_runtime::{CoreProjectionError, EventLedger, LedgerError, NewLedgerEvent};

/// One concrete provider/model selection for the read-only runner.
#[derive(Clone)]
pub struct ModelProviderConfig {
    pub provider: Arc<dyn ModelProvider>,
    pub model: String,
    pub budget: TokenBudget,
    pub metadata: Value,
}

impl std::fmt::Debug for ModelProviderConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ModelProviderConfig")
            .field("provider", &self.provider.provider_id())
            .field("model", &self.model)
            .field("budget", &self.budget)
            .field("metadata", &self.metadata)
            .finish()
    }
}

impl ModelProviderConfig {
    pub fn new(
        provider: Arc<dyn ModelProvider>,
        model: impl Into<String>,
        budget: TokenBudget,
    ) -> Self {
        Self {
            provider,
            model: model.into(),
            budget,
            metadata: Value::Null,
        }
    }
}

/// Cancellation is deliberately a small read-only port. The command handler
/// can back it with the per-turn flag it owns without coupling the runner to a
/// particular task registry.
pub trait CancellationCheck: Send + Sync {
    fn is_cancelled(&self) -> bool;
}

#[derive(Debug, Default)]
pub struct CancellationFlag {
    cancelled: AtomicBool,
}

impl CancellationFlag {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

impl CancellationCheck for CancellationFlag {
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NeverCancelled;

impl CancellationCheck for NeverCancelled {
    fn is_cancelled(&self) -> bool {
        false
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TurnRunSpec {
    pub thread_id: ThreadId,
    pub turn_id: TurnId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TurnRunResult {
    Completed {
        request_id: ModelRequestId,
        assistant_item_id: ItemId,
    },
    Failed {
        request_id: ModelRequestId,
        code: String,
    },
    Cancelled {
        request_id: ModelRequestId,
    },
}

#[derive(Debug, Error)]
pub enum TurnRunnerError {
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error(transparent)]
    Projection(#[from] CoreProjectionError),
    #[error(transparent)]
    Id(#[from] IdError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("turn {turn_id} does not exist")]
    MissingTurn { turn_id: TurnId },
    #[error("turn {turn_id} belongs to thread {actual}, not {expected}")]
    WrongThread {
        turn_id: TurnId,
        expected: ThreadId,
        actual: ThreadId,
    },
    #[error("turn {turn_id} is {actual:?}; expected {expected:?}")]
    UnexpectedState {
        turn_id: TurnId,
        expected: TurnState,
        actual: TurnState,
    },
    #[error("message item {item_id} has invalid content: {message}")]
    InvalidMessageItem { item_id: ItemId, message: String },
    #[error("thread {thread_id} does not exist while building model context")]
    MissingThread { thread_id: ThreadId },
    #[error("forked thread {thread_id} is missing its parent sequence")]
    MissingParentSequence { thread_id: ThreadId },
    #[error("the daemon mutation gate is poisoned")]
    MutationGatePoisoned,
}

/// Drives a single accepted turn. Clone it before spawning a background task.
#[derive(Clone)]
pub struct TurnRunner {
    ledger: Arc<EventLedger>,
    events: broadcast::Sender<EventEnvelope>,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGenerator>,
    provider: Option<ModelProviderConfig>,
    mutation_gate: Arc<Mutex<()>>,
}

impl std::fmt::Debug for TurnRunner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TurnRunner")
            .field("provider", &self.provider)
            .finish_non_exhaustive()
    }
}

impl TurnRunner {
    pub fn new(
        ledger: Arc<EventLedger>,
        events: broadcast::Sender<EventEnvelope>,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGenerator>,
        provider: Option<ModelProviderConfig>,
        mutation_gate: Arc<Mutex<()>>,
    ) -> Self {
        Self {
            ledger,
            events,
            clock,
            ids,
            provider,
            mutation_gate,
        }
    }

    /// Run exactly one provider request. Tool execution is intentionally not a
    /// capability of this type: a tool call fails the turn with an explicit
    /// diagnostic instead of being interpreted or executed.
    pub async fn run(
        &self,
        spec: TurnRunSpec,
        cancellation: &(dyn CancellationCheck + Send + Sync),
    ) -> Result<TurnRunResult, TurnRunnerError> {
        let request_id = ModelRequestId::from_uuid(self.ids.next_uuid()?);
        let context = RunContext::load(self, spec, request_id)?;

        if cancellation.is_cancelled() {
            self.cancel(&context, TurnState::Accepted)?;
            return Ok(TurnRunResult::Cancelled { request_id });
        }

        self.transition(
            &context,
            TurnState::Accepted,
            TurnState::BuildingContext,
            None,
        )?;
        let messages = match messages_from_lineage_events(&context.events) {
            Ok(messages) => messages,
            Err(error) => {
                return self.fail(
                    &context,
                    TurnState::BuildingContext,
                    "context_build_failed",
                    &error.to_string(),
                );
            }
        };

        if cancellation.is_cancelled() {
            self.cancel(&context, TurnState::BuildingContext)?;
            return Ok(TurnRunResult::Cancelled { request_id });
        }

        let Some(provider) = self.provider.clone() else {
            return self.fail(
                &context,
                TurnState::BuildingContext,
                "provider_unconfigured",
                "no model provider is configured for this daemon",
            );
        };
        if provider.model.trim().is_empty() {
            return self.fail(
                &context,
                TurnState::BuildingContext,
                "model_unconfigured",
                "the configured model name is empty",
            );
        }

        self.transition(
            &context,
            TurnState::BuildingContext,
            TurnState::RequestingModel,
            None,
        )?;
        self.persist(&context, Event::ModelRequested { request_id })?;

        if cancellation.is_cancelled() {
            self.cancel(&context, TurnState::RequestingModel)?;
            return Ok(TurnRunResult::Cancelled { request_id });
        }

        self.transition(
            &context,
            TurnState::RequestingModel,
            TurnState::Streaming,
            None,
        )?;
        let request = ModelRequest {
            request_id,
            turn_id: spec.turn_id,
            provider: provider.provider.provider_id().to_owned(),
            model: provider.model,
            messages,
            tools: Vec::new(),
            budget: provider.budget,
            metadata: provider.metadata,
        };
        let mut sink = PersistingModelSink::new(self, &context, cancellation);
        let provider_result = provider.provider.stream(request, &mut sink).await;

        if cancellation.is_cancelled() {
            self.cancel(&context, TurnState::Streaming)?;
            return Ok(TurnRunResult::Cancelled { request_id });
        }
        if let Err(error) = provider_result {
            return self.fail(&context, TurnState::Streaming, &error.code, &error.message);
        }
        if let Some((code, message)) = sink.model_failure.take() {
            return self.fail(&context, TurnState::Streaming, &code, &message);
        }
        if sink.tool_use {
            return self.fail(
                &context,
                TurnState::Streaming,
                "tool_use_unsupported",
                "the read-only v1 turn runner does not execute model tool calls",
            );
        }
        if sink.completion_count != 1 {
            return self.fail(
                &context,
                TurnState::Streaming,
                "provider_incomplete_stream",
                "the provider stream did not contain exactly one completion event",
            );
        }

        let item_id = ItemId::from_uuid(self.ids.next_uuid()?);
        let item = Item {
            id: item_id,
            thread_id: spec.thread_id,
            turn_id: spec.turn_id,
            agent_id: context.agent_id.clone(),
            kind: ItemKind::AssistantMessage,
            content: json!({ "content": sink.content }),
            created_at: self.clock.now(),
        };
        self.complete(&context, item, cancellation)
    }

    fn transition(
        &self,
        context: &RunContext,
        from: TurnState,
        to: TurnState,
        reason: Option<String>,
    ) -> Result<EventEnvelope, TurnRunnerError> {
        let _guard = self.lock_mutations()?;
        self.transition_locked(context, from, to, reason)
    }

    fn transition_locked(
        &self,
        context: &RunContext,
        from: TurnState,
        to: TurnState,
        reason: Option<String>,
    ) -> Result<EventEnvelope, TurnRunnerError> {
        let actual = self.current_state(context)?;
        if actual != from {
            return Err(TurnRunnerError::UnexpectedState {
                turn_id: context.spec.turn_id,
                expected: from,
                actual,
            });
        }
        self.persist_locked(
            context,
            Event::TurnStateChanged {
                turn_id: context.spec.turn_id,
                from,
                to,
                reason,
            },
        )
    }

    fn fail(
        &self,
        context: &RunContext,
        from: TurnState,
        code: &str,
        message: &str,
    ) -> Result<TurnRunResult, TurnRunnerError> {
        let code = normalize_code(code);
        let message = bounded_message(message);
        let _guard = self.lock_mutations()?;
        self.persist_locked(
            context,
            Event::RuntimeDiagnostic {
                code: code.clone(),
                message: message.clone(),
                recoverable: false,
            },
        )?;
        self.transition_locked(context, from, TurnState::Failed, Some(message))?;
        Ok(TurnRunResult::Failed {
            request_id: context.request_id,
            code,
        })
    }

    fn cancel(&self, context: &RunContext, from: TurnState) -> Result<(), TurnRunnerError> {
        let _guard = self.lock_mutations()?;
        self.cancel_locked(context, from)
    }

    fn cancel_locked(&self, context: &RunContext, from: TurnState) -> Result<(), TurnRunnerError> {
        let actual = self.current_state(context)?;
        if actual == TurnState::Cancelled {
            return Ok(());
        }
        if actual.is_terminal() {
            return Err(TurnRunnerError::UnexpectedState {
                turn_id: context.spec.turn_id,
                expected: from,
                actual,
            });
        }
        if actual == TurnState::Cancelling {
            self.transition_locked(
                context,
                TurnState::Cancelling,
                TurnState::Cancelled,
                Some("turn runner cancelled".into()),
            )?;
            return Ok(());
        }
        self.transition_locked(
            context,
            actual,
            TurnState::Cancelling,
            Some("turn runner cancellation requested".into()),
        )?;
        self.transition_locked(
            context,
            TurnState::Cancelling,
            TurnState::Cancelled,
            Some("turn runner cancelled".into()),
        )?;
        Ok(())
    }

    fn complete(
        &self,
        context: &RunContext,
        item: Item,
        cancellation: &(dyn CancellationCheck + Send + Sync),
    ) -> Result<TurnRunResult, TurnRunnerError> {
        let _guard = self.lock_mutations()?;
        if cancellation.is_cancelled() {
            self.cancel_locked(context, TurnState::Streaming)?;
            return Ok(TurnRunResult::Cancelled {
                request_id: context.request_id,
            });
        }
        let item_id = item.id;
        self.persist_locked(context, Event::ItemAdded { item })?;
        self.transition_locked(context, TurnState::Streaming, TurnState::Completed, None)?;
        Ok(TurnRunResult::Completed {
            request_id: context.request_id,
            assistant_item_id: item_id,
        })
    }

    fn current_state(&self, context: &RunContext) -> Result<TurnState, TurnRunnerError> {
        self.ledger
            .project_core()?
            .turns
            .get(&context.spec.turn_id)
            .map(|turn| turn.state)
            .ok_or(TurnRunnerError::MissingTurn {
                turn_id: context.spec.turn_id,
            })
    }

    fn persist(
        &self,
        context: &RunContext,
        event: Event,
    ) -> Result<EventEnvelope, TurnRunnerError> {
        let _guard = self.lock_mutations()?;
        self.persist_locked(context, event)
    }

    fn persist_locked(
        &self,
        context: &RunContext,
        event: Event,
    ) -> Result<EventEnvelope, TurnRunnerError> {
        let serialized = serde_json::to_value(event)?;
        let kind = serialized
            .get("kind")
            .and_then(Value::as_str)
            .expect("serialized protocol Event always has a kind")
            .to_owned();
        let payload = serialized.get("payload").cloned().unwrap_or(Value::Null);
        let persisted = self.ledger.append(NewLedgerEvent {
            schema_version: PROTOCOL_VERSION,
            event_id: self.ids.next_uuid()?.to_string(),
            thread_id: context.spec.thread_id.to_string(),
            turn_id: Some(context.spec.turn_id.to_string()),
            agent_id: context.agent_id.to_string(),
            time: self.clock.now(),
            causation_id: Some(CausationId::from(context.request_id).to_string()),
            kind,
            payload,
        })?;
        let envelope = EventEnvelope::try_from(persisted)?;
        let _ = self.events.send(envelope.clone());
        Ok(envelope)
    }

    fn lock_mutations(&self) -> Result<MutexGuard<'_, ()>, TurnRunnerError> {
        self.mutation_gate
            .lock()
            .map_err(|_| TurnRunnerError::MutationGatePoisoned)
    }
}

struct RunContext {
    spec: TurnRunSpec,
    request_id: ModelRequestId,
    agent_id: AgentId,
    events: Vec<EventEnvelope>,
}

impl RunContext {
    fn load(
        runner: &TurnRunner,
        spec: TurnRunSpec,
        request_id: ModelRequestId,
    ) -> Result<Self, TurnRunnerError> {
        let projection = runner.ledger.project_core()?;
        let turn = projection
            .turns
            .get(&spec.turn_id)
            .ok_or(TurnRunnerError::MissingTurn {
                turn_id: spec.turn_id,
            })?;
        if turn.thread_id != spec.thread_id {
            return Err(TurnRunnerError::WrongThread {
                turn_id: spec.turn_id,
                expected: spec.thread_id,
                actual: turn.thread_id,
            });
        }
        if turn.state != TurnState::Accepted {
            return Err(TurnRunnerError::UnexpectedState {
                turn_id: spec.turn_id,
                expected: TurnState::Accepted,
                actual: turn.state,
            });
        }
        let events = load_lineage_events(runner, &projection, spec.thread_id)?;
        Ok(Self {
            spec,
            request_id,
            agent_id: turn.agent_id.clone(),
            events,
        })
    }
}

fn load_lineage_events(
    runner: &TurnRunner,
    projection: &yeux_core::Projection,
    thread_id: ThreadId,
) -> Result<Vec<EventEnvelope>, TurnRunnerError> {
    let mut lineage = Vec::new();
    let mut current_id = thread_id;
    let mut through_seq = None;
    loop {
        let thread = projection
            .threads
            .get(&current_id)
            .ok_or(TurnRunnerError::MissingThread {
                thread_id: current_id,
            })?;
        lineage.push((current_id, through_seq));
        let Some(parent_id) = thread.parent_thread_id else {
            break;
        };
        current_id = parent_id;
        through_seq = Some(
            thread
                .parent_seq
                .ok_or(TurnRunnerError::MissingParentSequence {
                    thread_id: thread.id,
                })?,
        );
    }
    lineage.reverse();

    let mut events = Vec::new();
    for (lineage_thread_id, through_seq) in lineage {
        let thread_events = runner.ledger.replay(&lineage_thread_id.to_string(), 0)?;
        for event in thread_events {
            if through_seq.is_none_or(|limit| event.seq <= limit) {
                events.push(EventEnvelope::try_from(event)?);
            }
        }
    }
    Ok(events)
}

/// Build provider-neutral history in ledger sequence order. Only user and
/// assistant message items enter the model context.
pub fn messages_from_thread_events(
    events: &[EventEnvelope],
    thread_id: ThreadId,
) -> Result<Vec<ModelMessage>, TurnRunnerError> {
    messages_from_events(
        events
            .iter()
            .filter(|envelope| envelope.thread_id == thread_id),
    )
}

/// Build a model context from root-to-leaf fork lineage events. Callers must
/// provide each lineage segment in sequence order and cap ancestors at the
/// sequence recorded by their child fork.
pub fn messages_from_lineage_events(
    events: &[EventEnvelope],
) -> Result<Vec<ModelMessage>, TurnRunnerError> {
    messages_from_events(events.iter())
}

fn messages_from_events<'a>(
    events: impl IntoIterator<Item = &'a EventEnvelope>,
) -> Result<Vec<ModelMessage>, TurnRunnerError> {
    let mut messages = Vec::new();
    for envelope in events {
        let Event::ItemAdded { item } = &envelope.event else {
            continue;
        };
        let role = match item.kind {
            ItemKind::UserMessage => MessageRole::User,
            ItemKind::AssistantMessage => MessageRole::Assistant,
            _ => continue,
        };
        let content_value = item
            .content
            .get("content")
            .cloned()
            .unwrap_or_else(|| item.content.clone());
        let content: Vec<ContentBlock> =
            serde_json::from_value(content_value).map_err(|error| {
                TurnRunnerError::InvalidMessageItem {
                    item_id: item.id,
                    message: error.to_string(),
                }
            })?;
        messages.push(ModelMessage { role, content });
    }
    Ok(messages)
}

struct PersistingModelSink<'a> {
    runner: &'a TurnRunner,
    context: &'a RunContext,
    cancellation: &'a (dyn CancellationCheck + Send + Sync),
    content: Vec<ContentBlock>,
    completion_count: usize,
    tool_use: bool,
    model_failure: Option<(String, String)>,
}

impl<'a> PersistingModelSink<'a> {
    fn new(
        runner: &'a TurnRunner,
        context: &'a RunContext,
        cancellation: &'a (dyn CancellationCheck + Send + Sync),
    ) -> Self {
        Self {
            runner,
            context,
            cancellation,
            content: Vec::new(),
            completion_count: 0,
            tool_use: false,
            model_failure: None,
        }
    }

    fn collect_content(&mut self, event: &ModelEvent) {
        match event {
            ModelEvent::TextDelta { text } => append_text(&mut self.content, false, text),
            ModelEvent::ReasoningDelta { text } => append_text(&mut self.content, true, text),
            ModelEvent::ToolCallDelta { .. } => self.tool_use = true,
            ModelEvent::Completed { stop_reason } => {
                self.completion_count += 1;
                self.tool_use |= matches!(stop_reason, StopReason::ToolUse);
                if matches!(stop_reason, StopReason::Cancelled) {
                    self.model_failure = Some((
                        "provider_cancelled".into(),
                        "the provider cancelled the model stream".into(),
                    ));
                }
            }
            ModelEvent::Failed { code, message, .. } => {
                self.model_failure = Some((code.clone(), message.clone()));
            }
            ModelEvent::Usage { .. } => {}
        }
    }
}

impl ModelEventSink for PersistingModelSink<'_> {
    fn emit<'life0, 'async_trait>(
        &'life0 mut self,
        event: ModelEvent,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<(), PortError>> + Send + 'async_trait>,
    >
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            let _guard = self.runner.lock_mutations().map_err(|error| PortError {
                code: "event_persistence".into(),
                message: error.to_string(),
                retryable: false,
            })?;
            if self.cancellation.is_cancelled() {
                return Err(PortError {
                    code: "turn_cancelled".into(),
                    message: "turn was cancelled while the provider was streaming".into(),
                    retryable: false,
                });
            }
            self.runner
                .persist_locked(
                    self.context,
                    Event::ModelStreamEvent {
                        request_id: self.context.request_id,
                        model_event: event.clone(),
                    },
                )
                .map_err(|error| PortError {
                    code: "event_persistence".into(),
                    message: error.to_string(),
                    retryable: false,
                })?;
            self.collect_content(&event);
            Ok(())
        })
    }
}

fn append_text(content: &mut Vec<ContentBlock>, reasoning: bool, delta: &str) {
    match (content.last_mut(), reasoning) {
        (Some(ContentBlock::Text { text }), false)
        | (Some(ContentBlock::Reasoning { text }), true) => text.push_str(delta),
        (_, false) => content.push(ContentBlock::Text {
            text: delta.to_owned(),
        }),
        (_, true) => content.push(ContentBlock::Reasoning {
            text: delta.to_owned(),
        }),
    }
}

fn normalize_code(code: &str) -> String {
    let normalized: String = code
        .chars()
        .take(128)
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect();
    if normalized.is_empty() {
        "provider_error".into()
    } else {
        normalized
    }
}

fn bounded_message(message: &str) -> String {
    const MAX_CHARS: usize = 4_096;
    let mut result: String = message.chars().take(MAX_CHARS).collect();
    if message.chars().count() > MAX_CHARS {
        result.push_str(" [truncated]");
    }
    result
}

#[cfg(test)]
mod tests {
    use std::{future::Future, pin::Pin, sync::Mutex as TestMutex};

    use chrono::{DateTime, Utc};
    use uuid::Uuid;
    use yeux_core::{FixedClock, UuidV7Generator};
    use yeux_protocol::{
        ProviderCapabilities, Thread, ThreadStatus, Turn, Workspace, WorkspaceId,
        WorkspaceIdentity, WorkspaceTrust,
    };

    use super::*;

    #[derive(Clone)]
    struct FauxProvider {
        events: Vec<ModelEvent>,
        error: Option<PortError>,
        requests: Arc<TestMutex<Vec<ModelRequest>>>,
    }

    impl FauxProvider {
        fn succeeds(events: Vec<ModelEvent>) -> Self {
            Self {
                events,
                error: None,
                requests: Arc::new(TestMutex::new(Vec::new())),
            }
        }

        fn fails(error: PortError) -> Self {
            Self {
                events: Vec::new(),
                error: Some(error),
                requests: Arc::new(TestMutex::new(Vec::new())),
            }
        }
    }

    impl ModelProvider for FauxProvider {
        fn provider_id(&self) -> &str {
            "faux"
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities::default()
        }

        fn stream<'life0, 'life1, 'async_trait>(
            &'life0 self,
            request: ModelRequest,
            sink: &'life1 mut (dyn ModelEventSink + Send),
        ) -> Pin<Box<dyn Future<Output = Result<(), PortError>> + Send + 'async_trait>>
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async move {
                self.requests.lock().unwrap().push(request);
                for event in &self.events {
                    sink.emit(event.clone()).await?;
                }
                match &self.error {
                    Some(error) => Err(error.clone()),
                    None => Ok(()),
                }
            })
        }
    }

    struct Fixture {
        ledger: Arc<EventLedger>,
        runner: TurnRunner,
        thread_id: ThreadId,
        turn_id: TurnId,
    }

    fn fixture(provider: Option<ModelProviderConfig>) -> Fixture {
        let ledger = Arc::new(EventLedger::open_in_memory().unwrap());
        let (events, _) = broadcast::channel(64);
        let now = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let workspace_id = WorkspaceId::from_uuid(Uuid::now_v7());
        let thread_id = ThreadId::from_uuid(Uuid::now_v7());
        let turn_id = TurnId::from_uuid(Uuid::now_v7());
        let agent_id = AgentId::new("root");
        append_seed(
            &ledger,
            thread_id,
            None,
            &agent_id,
            now,
            Event::WorkspaceOpened {
                workspace: Workspace {
                    id: workspace_id,
                    root: "/workspace".into(),
                    identity: WorkspaceIdentity {
                        canonical_root: "/workspace".into(),
                        digest: "fixture".into(),
                        device: None,
                        inode: None,
                        git_common_dir: None,
                    },
                    trust: WorkspaceTrust::Trusted,
                    opened_at: now,
                },
            },
        );
        append_seed(
            &ledger,
            thread_id,
            None,
            &agent_id,
            now,
            Event::ThreadStarted {
                thread: Thread {
                    id: thread_id,
                    workspace_id,
                    parent_thread_id: None,
                    parent_seq: None,
                    title: None,
                    status: ThreadStatus::Idle,
                    created_at: now,
                    updated_at: now,
                    last_seq: 0,
                },
            },
        );
        append_seed(
            &ledger,
            thread_id,
            Some(turn_id),
            &agent_id,
            now,
            Event::TurnStarted {
                turn: Turn {
                    id: turn_id,
                    thread_id,
                    agent_id: agent_id.clone(),
                    state: TurnState::Accepted,
                    started_at: now,
                    ended_at: None,
                    failure: None,
                },
            },
        );
        append_seed(
            &ledger,
            thread_id,
            Some(turn_id),
            &agent_id,
            now,
            Event::ItemAdded {
                item: Item {
                    id: ItemId::from_uuid(Uuid::now_v7()),
                    thread_id,
                    turn_id,
                    agent_id: agent_id.clone(),
                    kind: ItemKind::UserMessage,
                    content: json!({
                        "content": [ContentBlock::Text { text: "hello".into() }]
                    }),
                    created_at: now,
                },
            },
        );
        let runner = TurnRunner::new(
            Arc::clone(&ledger),
            events,
            Arc::new(FixedClock::new(now)),
            Arc::new(UuidV7Generator),
            provider,
            Arc::new(Mutex::new(())),
        );
        Fixture {
            ledger,
            runner,
            thread_id,
            turn_id,
        }
    }

    fn append_seed(
        ledger: &EventLedger,
        thread_id: ThreadId,
        turn_id: Option<TurnId>,
        agent_id: &AgentId,
        time: DateTime<Utc>,
        event: Event,
    ) {
        let value = serde_json::to_value(event).unwrap();
        ledger
            .append(NewLedgerEvent {
                schema_version: PROTOCOL_VERSION,
                event_id: Uuid::now_v7().to_string(),
                thread_id: thread_id.to_string(),
                turn_id: turn_id.map(|id| id.to_string()),
                agent_id: agent_id.to_string(),
                time,
                causation_id: None,
                kind: value["kind"].as_str().unwrap().to_owned(),
                payload: value.get("payload").cloned().unwrap_or(Value::Null),
            })
            .unwrap();
    }

    fn provider_config(provider: Arc<dyn ModelProvider>) -> ModelProviderConfig {
        ModelProviderConfig::new(
            provider,
            "faux-model",
            TokenBudget {
                max_input_tokens: 1_024,
                max_output_tokens: 128,
            },
        )
    }

    #[tokio::test]
    async fn persists_complete_read_only_turn_and_ordered_context() {
        let provider = Arc::new(FauxProvider::succeeds(vec![
            ModelEvent::TextDelta {
                text: "answer".into(),
            },
            ModelEvent::Completed {
                stop_reason: StopReason::EndTurn,
            },
        ]));
        let fixture = fixture(Some(provider_config(provider.clone())));
        let mut subscriber = fixture.runner.events.subscribe();
        let result = fixture
            .runner
            .run(
                TurnRunSpec {
                    thread_id: fixture.thread_id,
                    turn_id: fixture.turn_id,
                },
                &NeverCancelled,
            )
            .await
            .unwrap();

        assert!(matches!(result, TurnRunResult::Completed { .. }));
        let projection = fixture.ledger.project_core().unwrap();
        assert_eq!(
            projection.turns[&fixture.turn_id].state,
            TurnState::Completed
        );
        let assistant = projection
            .items
            .values()
            .find(|item| item.kind == ItemKind::AssistantMessage)
            .unwrap();
        assert_eq!(assistant.content["content"][0]["text"], "answer");

        let request = provider.requests.lock().unwrap()[0].clone();
        assert_eq!(request.messages.len(), 1);
        assert_eq!(request.messages[0].role, MessageRole::User);
        assert_eq!(
            request.messages[0].content,
            vec![ContentBlock::Text {
                text: "hello".into()
            }]
        );

        let mut broadcast_count = 0;
        while subscriber.try_recv().is_ok() {
            broadcast_count += 1;
        }
        assert_eq!(
            broadcast_count,
            fixture
                .ledger
                .replay(&fixture.thread_id.to_string(), 4)
                .unwrap()
                .len()
        );
    }

    #[tokio::test]
    async fn provider_failure_is_diagnostic_and_terminal() {
        let provider = Arc::new(FauxProvider::fails(PortError {
            code: "provider_offline".into(),
            message: "faux provider is unavailable".into(),
            retryable: true,
        }));
        let fixture = fixture(Some(provider_config(provider)));
        let result = fixture
            .runner
            .run(
                TurnRunSpec {
                    thread_id: fixture.thread_id,
                    turn_id: fixture.turn_id,
                },
                &NeverCancelled,
            )
            .await
            .unwrap();

        assert_eq!(
            result,
            TurnRunResult::Failed {
                request_id: match result {
                    TurnRunResult::Failed { request_id, .. } => request_id,
                    _ => unreachable!(),
                },
                code: "provider_offline".into(),
            }
        );
        let projection = fixture.ledger.project_core().unwrap();
        let turn = &projection.turns[&fixture.turn_id];
        assert_eq!(turn.state, TurnState::Failed);
        assert_eq!(
            turn.failure.as_deref(),
            Some("faux provider is unavailable")
        );
        assert!(fixture
            .ledger
            .replay(&fixture.thread_id.to_string(), 0)
            .unwrap()
            .iter()
            .any(|event| event.kind == "runtime/diagnostic"
                && event.payload["code"] == "provider_offline"));
    }

    #[tokio::test]
    async fn missing_provider_fails_without_hanging_the_turn() {
        let fixture = fixture(None);
        let result = fixture
            .runner
            .run(
                TurnRunSpec {
                    thread_id: fixture.thread_id,
                    turn_id: fixture.turn_id,
                },
                &NeverCancelled,
            )
            .await
            .unwrap();
        assert!(matches!(
            result,
            TurnRunResult::Failed { ref code, .. } if code == "provider_unconfigured"
        ));
        assert_eq!(
            fixture.ledger.project_core().unwrap().turns[&fixture.turn_id].state,
            TurnState::Failed
        );
    }

    #[tokio::test]
    async fn multi_level_fork_context_inherits_each_ancestor_at_its_fork_sequence() {
        let ledger = Arc::new(EventLedger::open_in_memory().unwrap());
        let (event_tx, _) = broadcast::channel(64);
        let now = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let workspace_id = WorkspaceId::from_uuid(Uuid::now_v7());
        let root_id = ThreadId::from_uuid(Uuid::now_v7());
        let parent_id = ThreadId::from_uuid(Uuid::now_v7());
        let child_id = ThreadId::from_uuid(Uuid::now_v7());
        let root_turn = TurnId::from_uuid(Uuid::now_v7());
        let parent_turn = TurnId::from_uuid(Uuid::now_v7());
        let child_turn = TurnId::from_uuid(Uuid::now_v7());
        let agent_id = AgentId::new("root");

        append_seed(
            &ledger,
            ThreadId::from_uuid(workspace_id.into_uuid()),
            None,
            &agent_id,
            now,
            Event::WorkspaceOpened {
                workspace: Workspace {
                    id: workspace_id,
                    root: "/workspace".into(),
                    identity: WorkspaceIdentity {
                        canonical_root: "/workspace".into(),
                        digest: "fork-fixture".into(),
                        device: None,
                        inode: None,
                        git_common_dir: None,
                    },
                    trust: WorkspaceTrust::Trusted,
                    opened_at: now,
                },
            },
        );
        append_seed(
            &ledger,
            root_id,
            None,
            &agent_id,
            now,
            Event::ThreadStarted {
                thread: Thread {
                    id: root_id,
                    workspace_id,
                    parent_thread_id: None,
                    parent_seq: None,
                    title: Some("root".into()),
                    status: ThreadStatus::Idle,
                    created_at: now,
                    updated_at: now,
                    last_seq: 0,
                },
            },
        );
        append_seed(
            &ledger,
            root_id,
            Some(root_turn),
            &agent_id,
            now,
            Event::TurnStarted {
                turn: Turn {
                    id: root_turn,
                    thread_id: root_id,
                    agent_id: agent_id.clone(),
                    state: TurnState::Accepted,
                    started_at: now,
                    ended_at: None,
                    failure: None,
                },
            },
        );
        append_message(
            &ledger,
            root_id,
            root_turn,
            &agent_id,
            now,
            "root-before-fork",
        );

        append_seed(
            &ledger,
            parent_id,
            None,
            &agent_id,
            now,
            Event::ThreadForked {
                thread: Thread {
                    id: parent_id,
                    workspace_id,
                    parent_thread_id: Some(root_id),
                    parent_seq: Some(3),
                    title: Some("parent".into()),
                    status: ThreadStatus::Idle,
                    created_at: now,
                    updated_at: now,
                    last_seq: 0,
                },
            },
        );
        append_message(
            &ledger,
            root_id,
            root_turn,
            &agent_id,
            now,
            "root-after-fork",
        );
        append_seed(
            &ledger,
            parent_id,
            Some(parent_turn),
            &agent_id,
            now,
            Event::TurnStarted {
                turn: Turn {
                    id: parent_turn,
                    thread_id: parent_id,
                    agent_id: agent_id.clone(),
                    state: TurnState::Accepted,
                    started_at: now,
                    ended_at: None,
                    failure: None,
                },
            },
        );
        append_message(
            &ledger,
            parent_id,
            parent_turn,
            &agent_id,
            now,
            "parent-before-fork",
        );

        append_seed(
            &ledger,
            child_id,
            None,
            &agent_id,
            now,
            Event::ThreadForked {
                thread: Thread {
                    id: child_id,
                    workspace_id,
                    parent_thread_id: Some(parent_id),
                    parent_seq: Some(3),
                    title: Some("child".into()),
                    status: ThreadStatus::Idle,
                    created_at: now,
                    updated_at: now,
                    last_seq: 0,
                },
            },
        );
        append_message(
            &ledger,
            parent_id,
            parent_turn,
            &agent_id,
            now,
            "parent-after-fork",
        );
        append_seed(
            &ledger,
            child_id,
            Some(child_turn),
            &agent_id,
            now,
            Event::TurnStarted {
                turn: Turn {
                    id: child_turn,
                    thread_id: child_id,
                    agent_id: agent_id.clone(),
                    state: TurnState::Accepted,
                    started_at: now,
                    ended_at: None,
                    failure: None,
                },
            },
        );
        append_message(
            &ledger,
            child_id,
            child_turn,
            &agent_id,
            now,
            "child-message",
        );

        let provider = Arc::new(FauxProvider::succeeds(vec![ModelEvent::Completed {
            stop_reason: StopReason::EndTurn,
        }]));
        let runner = TurnRunner::new(
            Arc::clone(&ledger),
            event_tx,
            Arc::new(FixedClock::new(now)),
            Arc::new(UuidV7Generator),
            Some(provider_config(provider.clone())),
            Arc::new(Mutex::new(())),
        );
        runner
            .run(
                TurnRunSpec {
                    thread_id: child_id,
                    turn_id: child_turn,
                },
                &NeverCancelled,
            )
            .await
            .unwrap();

        let request = provider.requests.lock().unwrap()[0].clone();
        let text = request
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            text,
            ["root-before-fork", "parent-before-fork", "child-message"]
        );
    }

    #[tokio::test]
    async fn tool_use_is_persisted_but_never_executed() {
        let provider = Arc::new(FauxProvider::succeeds(vec![
            ModelEvent::ToolCallDelta {
                call_id: "call-1".into(),
                name: "shell".into(),
                json_delta: "{\"command\":\"false\"}".into(),
            },
            ModelEvent::Completed {
                stop_reason: StopReason::ToolUse,
            },
        ]));
        let fixture = fixture(Some(provider_config(provider)));
        let result = fixture
            .runner
            .run(
                TurnRunSpec {
                    thread_id: fixture.thread_id,
                    turn_id: fixture.turn_id,
                },
                &NeverCancelled,
            )
            .await
            .unwrap();

        assert!(matches!(
            result,
            TurnRunResult::Failed { ref code, .. } if code == "tool_use_unsupported"
        ));
        let events = fixture
            .ledger
            .replay(&fixture.thread_id.to_string(), 0)
            .unwrap();
        assert!(events.iter().any(|event| {
            event.kind == "model/event" && event.payload["model_event"]["type"] == "tool_call_delta"
        }));
        assert!(!events.iter().any(|event| event.kind.starts_with("tool/")));
    }

    fn append_message(
        ledger: &EventLedger,
        thread_id: ThreadId,
        turn_id: TurnId,
        agent_id: &AgentId,
        time: DateTime<Utc>,
        text: &str,
    ) {
        append_seed(
            ledger,
            thread_id,
            Some(turn_id),
            agent_id,
            time,
            Event::ItemAdded {
                item: Item {
                    id: ItemId::from_uuid(Uuid::now_v7()),
                    thread_id,
                    turn_id,
                    agent_id: agent_id.clone(),
                    kind: ItemKind::UserMessage,
                    content: json!({
                        "content": [ContentBlock::Text { text: text.into() }]
                    }),
                    created_at: time,
                },
            },
        );
    }
}
