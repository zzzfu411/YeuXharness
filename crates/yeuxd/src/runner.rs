//! Bounded read-only agent turn runner.
//!
//! The daemon remains responsible for accepting a turn and recording its user
//! message. This module can make multiple provider requests, execute only the
//! built-in structured workspace read tools, feed their results back to the
//! model, and persist every externally visible update before broadcasting it.

#![allow(clippy::result_large_err, clippy::large_enum_variant)]

use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, MutexGuard, Weak,
    },
};

use serde_json::{json, Value};
use thiserror::Error;
use tokio::{
    sync::{broadcast, OwnedSemaphorePermit, Semaphore},
    time::{timeout, Duration},
};
use yeux_core::{
    digest_value, Clock, IdError, IdGenerator, ModelEventSink, ModelProvider, PortError,
};
use yeux_protocol::{
    AgentId, ApprovalRequestParams, ApprovalRequestResult, CapabilityGrant, CapabilityMode,
    CausationId, ContentBlock, EffectSet, Event, EventEnvelope, InvocationId, InvocationState,
    Item, ItemId, ItemKind, MessageRole, ModelEvent, ModelMessage, ModelRequest, ModelRequestId,
    StopReason, ThreadId, TokenBudget, ToolSpec, TurnId, TurnState, WorkspaceId, WorkspaceIdentity,
    WorkspaceTrust, PROTOCOL_VERSION,
};
use yeux_runtime::{
    CoreProjectionError, EventLedger, LedgerError, LedgerEvent, NewInvocationOutcome,
    NewInvocationUnknown, NewInvocationUnknownOutcome, NewLedgerEvent, NoCredentialBroker,
    ProcessExecutor, SandboxBackend, SearchOperationBudget, Workspace as RuntimeWorkspace,
    WorkspaceSearchControl, WorkspaceTools, WORKSPACE_SEARCH_DEFAULT_OPERATION_BUDGET,
    WORKSPACE_SEARCH_HARD_OPERATION_LIMIT, WORKSPACE_SEARCH_TOOL_ID,
};

use crate::grants::resolve_grant_layers;
use crate::pipeline::{InvocationContext, InvocationPipeline, PipelineError, PipelineGrants};
use crate::tool_calls::{AssembledToolCall, ToolCallAssembler, ToolCallAssemblyError};
use crate::tools::{BuiltInToolRegistryConfig, ToolRegistry, ToolRegistryError};

/// The daemon transport implements this boundary for interactive approval.
/// A missing handler is deny-by-default; it is never treated as approval.
pub trait ApprovalHandler: Send + Sync {
    fn request<'a>(
        &'a self,
        params: ApprovalRequestParams,
    ) -> Pin<Box<dyn Future<Output = ApprovalRequestResult> + Send + 'a>>;
}

fn pipeline_error(error: PipelineError) -> ToolRegistryError {
    ToolRegistryError::Authority(error.to_string())
}

const DEFAULT_MAX_MODEL_ROUNDS: usize = 8;
const DEFAULT_MAX_TOOL_CALLS_PER_TURN: usize = 32;
const DEFAULT_MAX_TOOL_RESULT_BYTES_PER_TURN: usize = 4 * 1024 * 1024;
/// A turn may consume at most one full built-in search scan by default.  The
/// value is deliberately narrower than the 32-call turn budget; callers can
/// lower it further but cannot raise the runtime hard ceiling.
const DEFAULT_MAX_SEARCH_OPERATIONS_PER_TURN: u64 = WORKSPACE_SEARCH_DEFAULT_OPERATION_BUDGET;
/// Maximum number of blocking built-in tool workers allowed per daemon.
///
/// A model may request up to 32 calls in one response, but allowing all of
/// them to enter Tokio's blocking pool at once makes a single adversarial
/// search able to starve unrelated RPC work.  The semaphore is shared by all
/// turns and is deliberately independent from the per-turn call budget.
const DEFAULT_MAX_CONCURRENT_TOOL_WORKERS: usize = 4;
const MAX_CONTEXT_MESSAGES: usize = 4_096;
const MAX_CONTEXT_BLOCKS: usize = 16_384;
const MAX_CONTEXT_BYTES: usize = 8 * 1024 * 1024;
const MAX_CONTEXT_TEXT_BYTES_PER_BLOCK: usize = 256 * 1024;
const MAX_MODEL_ROUNDS_HARD: usize = 64;
const MAX_TOOL_CALLS_HARD: usize = 128;
const MAX_TOOL_RESULT_BYTES_HARD: usize = 64 * 1024 * 1024;
const MAX_INPUT_TOKENS_HARD: u64 = 2_000_000;
const MAX_OUTPUT_TOKENS_HARD: u64 = 256_000;
const MAX_WORKSPACE_SEARCH_GATES: usize = 256;
const MAX_CONCURRENT_SEARCHES_PER_WORKSPACE: usize = 1;

type WorkspaceSearchGateKey = (String, String);
type WorkspaceSearchGateMap = HashMap<WorkspaceSearchGateKey, Weak<Semaphore>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AgentLoopLimits {
    pub max_model_rounds: usize,
    pub max_tool_calls: usize,
    pub max_tool_result_bytes: usize,
    pub max_search_operations: u64,
}

impl Default for AgentLoopLimits {
    fn default() -> Self {
        Self {
            max_model_rounds: DEFAULT_MAX_MODEL_ROUNDS,
            max_tool_calls: DEFAULT_MAX_TOOL_CALLS_PER_TURN,
            max_tool_result_bytes: DEFAULT_MAX_TOOL_RESULT_BYTES_PER_TURN,
            max_search_operations: DEFAULT_MAX_SEARCH_OPERATIONS_PER_TURN,
        }
    }
}

impl AgentLoopLimits {
    /// Narrow the aggregate matcher allowance for one turn. Values above the
    /// runtime hard ceiling are rejected when the turn starts.
    pub const fn with_search_operation_budget(mut self, max_operations: u64) -> Self {
        self.max_search_operations = max_operations;
        self
    }
}

/// One concrete provider/model selection for the read-only runner.
#[derive(Clone)]
pub struct ModelProviderConfig {
    pub provider: Arc<dyn ModelProvider>,
    pub model: String,
    pub budget: TokenBudget,
    pub metadata: Value,
    pub loop_limits: AgentLoopLimits,
}

impl std::fmt::Debug for ModelProviderConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ModelProviderConfig")
            .field("provider", &self.provider.provider_id())
            .field("model", &self.model)
            .field("budget", &self.budget)
            .field("metadata", &self.metadata)
            .field("loop_limits", &self.loop_limits)
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
            loop_limits: AgentLoopLimits::default(),
        }
    }

    pub fn with_loop_limits(mut self, limits: AgentLoopLimits) -> Self {
        self.loop_limits = limits;
        self
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TurnSettlement {
    /// The cleanup pass did not mutate the parent turn.  This includes a
    /// normal terminal failure that intentionally retains `Unknown` child
    /// invocations for later reconciliation.
    Unchanged,
    /// The cleanup pass closed `Cancelling` after proving every child was
    /// settled before execution.
    Cancelled,
    /// The cleanup pass found an invocation whose external outcome cannot be
    /// proven and therefore closed the parent as reconciliation-required.
    FailedReconciliation,
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
    #[error("workspace {workspace_id} does not exist while building model context")]
    MissingWorkspace { workspace_id: WorkspaceId },
    #[error("forked thread {thread_id} is missing its parent sequence")]
    MissingParentSequence { thread_id: ThreadId },
    #[error("the daemon mutation gate is poisoned")]
    MutationGatePoisoned,
    #[error("the daemon workspace search gate is poisoned")]
    WorkspaceSearchGatePoisoned,
}

/// Drives a single accepted turn. Clone it before spawning a background task.
#[derive(Clone)]
pub struct TurnRunner {
    ledger: Arc<EventLedger>,
    events: broadcast::Sender<EventEnvelope>,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGenerator>,
    provider: Option<ModelProviderConfig>,
    host_ceiling: CapabilityMode,
    mutation_gate: Arc<Mutex<()>>,
    tool_workers: Arc<Semaphore>,
    workspace_search_gates: Arc<Mutex<WorkspaceSearchGateMap>>,
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
        Self::new_with_host_ceiling(
            ledger,
            events,
            clock,
            ids,
            provider,
            mutation_gate,
            CapabilityMode::Operate,
        )
    }

    pub fn new_with_host_ceiling(
        ledger: Arc<EventLedger>,
        events: broadcast::Sender<EventEnvelope>,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGenerator>,
        provider: Option<ModelProviderConfig>,
        mutation_gate: Arc<Mutex<()>>,
        host_ceiling: CapabilityMode,
    ) -> Self {
        Self {
            ledger,
            events,
            clock,
            ids,
            provider,
            host_ceiling,
            mutation_gate,
            tool_workers: Arc::new(Semaphore::new(DEFAULT_MAX_CONCURRENT_TOOL_WORKERS)),
            workspace_search_gates: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Run a bounded provider/tool loop.  The wrapper also performs a final
    /// cancellation settlement pass: if the control plane moved the durable
    /// turn to `Cancelling` while this task was between phases, every
    /// non-terminal invocation is resolved before the turn is closed.
    pub async fn run(
        &self,
        spec: TurnRunSpec,
        cancellation: &(dyn CancellationCheck + Send + Sync),
    ) -> Result<TurnRunResult, TurnRunnerError> {
        self.run_with_approval(spec, cancellation, None).await
    }

    pub async fn run_with_approval(
        &self,
        spec: TurnRunSpec,
        cancellation: &(dyn CancellationCheck + Send + Sync),
        approval: Option<Arc<dyn ApprovalHandler>>,
    ) -> Result<TurnRunResult, TurnRunnerError> {
        // Allocate the request ID at the wrapper boundary so a cancellation
        // race that aborts `run_inner` before it can return a result can still
        // be reported with the same stable ID after settlement.
        let first_request_id = ModelRequestId::from_uuid(self.ids.next_uuid()?);
        let result = self
            .run_inner(spec, first_request_id, cancellation, approval)
            .await;
        let settlement = self.settle_interrupted_turn(spec);
        match settlement {
            Ok(TurnSettlement::Unchanged) => result,
            Ok(TurnSettlement::Cancelled) => match result {
                // Settlement is authoritative.  A cancellation race can
                // make `run_inner` return an optimistic Completed/Failed
                // value while the durable parent is closed as Cancelled;
                // never report a result whose state no longer exists in the
                // ledger.
                Ok(TurnRunResult::Completed { request_id, .. })
                | Ok(TurnRunResult::Failed { request_id, .. })
                | Ok(TurnRunResult::Cancelled { request_id }) => {
                    Ok(TurnRunResult::Cancelled { request_id })
                }
                Err(TurnRunnerError::UnexpectedState { .. }) => Ok(TurnRunResult::Cancelled {
                    request_id: first_request_id,
                }),
                Err(error) => Err(error),
            },
            Ok(TurnSettlement::FailedReconciliation) => match result {
                // As above, the settlement pass owns the final durable
                // state.  Preserve only the request ID from any optimistic
                // result and expose the reconciliation-required failure.
                Ok(TurnRunResult::Completed { request_id, .. })
                | Ok(TurnRunResult::Failed { request_id, .. })
                | Ok(TurnRunResult::Cancelled { request_id }) => Ok(TurnRunResult::Failed {
                    request_id,
                    code: "turn_cancellation_requires_reconciliation".into(),
                }),
                Err(TurnRunnerError::UnexpectedState { .. }) => Ok(TurnRunResult::Failed {
                    request_id: first_request_id,
                    code: "turn_cancellation_requires_reconciliation".into(),
                }),
                Err(error) => Err(error),
            },
            Err(error) => Err(error),
        }
    }

    /// Execute the state machine itself.  Cleanup lives in [`Self::run`] so
    /// every early `?` path (including an interrupt race) gets the same
    /// invocation settlement semantics.
    async fn run_inner(
        &self,
        spec: TurnRunSpec,
        first_request_id: ModelRequestId,
        cancellation: &(dyn CancellationCheck + Send + Sync),
        approval: Option<Arc<dyn ApprovalHandler>>,
    ) -> Result<TurnRunResult, TurnRunnerError> {
        let mut context = RunContext::load(self, spec, first_request_id)?;

        let initial_state = self.current_state(&context)?;
        if cancellation.is_cancelled() || initial_state == TurnState::Cancelling {
            self.cancel(&context, TurnState::Accepted)?;
            return Ok(TurnRunResult::Cancelled {
                request_id: first_request_id,
            });
        }

        self.transition(
            &context,
            TurnState::Accepted,
            TurnState::BuildingContext,
            None,
        )?;
        if let Some(error) = context.turn_override_error.take() {
            return self.fail(
                &context,
                TurnState::BuildingContext,
                "invalid_capability_override",
                &error,
            );
        }
        if cancellation.is_cancelled() {
            self.cancel(&context, TurnState::BuildingContext)?;
            return Ok(TurnRunResult::Cancelled {
                request_id: first_request_id,
            });
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
        if provider.loop_limits.max_model_rounds == 0 {
            return self.fail(
                &context,
                TurnState::BuildingContext,
                "agent_loop_configuration",
                "max_model_rounds must be greater than zero",
            );
        }
        if provider.loop_limits.max_model_rounds > MAX_MODEL_ROUNDS_HARD
            || provider.loop_limits.max_tool_calls > MAX_TOOL_CALLS_HARD
            || provider.loop_limits.max_tool_result_bytes > MAX_TOOL_RESULT_BYTES_HARD
            || provider.loop_limits.max_search_operations > WORKSPACE_SEARCH_HARD_OPERATION_LIMIT
            || provider.budget.max_input_tokens > MAX_INPUT_TOKENS_HARD
            || provider.budget.max_output_tokens > MAX_OUTPUT_TOKENS_HARD
        {
            return self.fail(
                &context,
                TurnState::BuildingContext,
                "agent_loop_configuration",
                "configured model/tool budget exceeds the daemon hard ceiling",
            );
        }

        let provider_capabilities = provider.provider.capabilities();
        // The provider-visible tool list is derived from the daemon-owned
        // sealed registry.  Keeping registration, normalization and execution
        // behind this boundary prevents the runner from becoming a second
        // authority path as more built-ins are added.
        let (tool_registry, invocation_pipeline) = if provider_capabilities.tool_calls {
            match RuntimeWorkspace::open(&context.workspace_root) {
                Ok(workspace) => {
                    if let Err(error) =
                        validate_workspace_identity(&workspace, &context.workspace_identity)
                    {
                        return self.fail(
                            &context,
                            TurnState::BuildingContext,
                            "workspace_identity_changed",
                            &error,
                        );
                    }
                    let sandbox = SandboxBackend::detect();
                    let sandbox_ready = sandbox
                        .ensure(yeux_runtime::SandboxRequirement {
                            filesystem_isolation: true,
                            process_isolation: true,
                            network_isolation: true,
                            allow_workspace_write: true,
                            allow_network: false,
                        })
                        .is_ok();
                    let config = BuiltInToolRegistryConfig::read_only()
                        .with_hidden_workspace_mutations()
                        .with_hidden_process();
                    let config = if sandbox_ready && self.host_ceiling != CapabilityMode::Observe {
                        config
                            .with_advertised_workspace_mutations()
                            .with_advertised_process()
                    } else {
                        config
                    };
                    let runtime_tools = Arc::new(WorkspaceTools::new(workspace));
                    let process_executor = Arc::new(ProcessExecutor::new(sandbox.clone()));
                    match ToolRegistry::workspace_built_ins_with_config_and_process(
                        runtime_tools,
                        config,
                        Some(process_executor),
                    ) {
                        Ok(registry) => {
                            let registry = Arc::new(registry);
                            let grants = resolve_grant_layers(
                                self.host_ceiling,
                                context.workspace_trust,
                                None,
                                context.turn_override.as_ref(),
                            );
                            let pipeline = Arc::new(InvocationPipeline::new(
                                Arc::clone(&registry),
                                sandbox,
                                Arc::new(NoCredentialBroker),
                            ));
                            (Some(registry), Some((pipeline, grants)))
                        }
                        Err(error) => {
                            return self.fail(
                                &context,
                                TurnState::BuildingContext,
                                "tool_registry_unavailable",
                                &error.to_string(),
                            );
                        }
                    }
                }
                Err(error) => {
                    return self.fail(
                        &context,
                        TurnState::BuildingContext,
                        "workspace_tools_unavailable",
                        &error.to_string(),
                    );
                }
            }
        } else {
            (None, None)
        };
        let tool_specs = tool_registry
            .as_ref()
            .map(|registry| registry.advertised_specs().to_vec())
            .unwrap_or_default();

        self.transition(
            &context,
            TurnState::BuildingContext,
            TurnState::RequestingModel,
            None,
        )?;

        let mut model_rounds = 0usize;
        let mut tool_calls_used = 0usize;
        let mut tool_result_bytes = 0usize;
        // This counter lives for the whole run, not one model round. Every
        // search worker receives a borrowed view of the same atomic budget so
        // parallel calls and later rounds cannot multiply the allowance.
        let search_operation_budget = Arc::new(SearchOperationBudget::new(
            provider.loop_limits.max_search_operations,
        ));

        loop {
            if model_rounds >= provider.loop_limits.max_model_rounds {
                return self.fail(
                    &context,
                    TurnState::RequestingModel,
                    "agent_loop_round_limit",
                    "the turn exceeded its model-round budget",
                );
            }
            if model_rounds > 0 {
                context.request_id = ModelRequestId::from_uuid(self.ids.next_uuid()?);
            }
            model_rounds += 1;

            context.events = match self.reload_lineage_events(spec.thread_id) {
                Ok(events) => events,
                Err(error) => {
                    return self.fail(
                        &context,
                        TurnState::RequestingModel,
                        "context_build_failed",
                        &error.to_string(),
                    );
                }
            };
            let messages = match messages_from_lineage_events(&context.events) {
                Ok(messages) => messages,
                Err(error) => {
                    return self.fail(
                        &context,
                        TurnState::RequestingModel,
                        "context_build_failed",
                        &error.to_string(),
                    );
                }
            };
            if let Err(error) = validate_model_context(
                &messages,
                &provider.budget,
                provider_capabilities.max_context_tokens,
            ) {
                return self.fail(
                    &context,
                    TurnState::RequestingModel,
                    "context_budget_exceeded",
                    &error,
                );
            }

            self.persist_expected(
                &context,
                TurnState::RequestingModel,
                Event::ModelRequested {
                    request_id: context.request_id,
                },
            )?;
            if cancellation.is_cancelled() {
                self.cancel(&context, TurnState::RequestingModel)?;
                return Ok(TurnRunResult::Cancelled {
                    request_id: context.request_id,
                });
            }

            self.transition(
                &context,
                TurnState::RequestingModel,
                TurnState::Streaming,
                None,
            )?;
            let request = ModelRequest {
                request_id: context.request_id,
                turn_id: spec.turn_id,
                provider: provider.provider.provider_id().to_owned(),
                model: provider.model.clone(),
                messages,
                tools: tool_specs.clone(),
                budget: provider.budget.clone(),
                metadata: provider.metadata.clone(),
            };
            let mut sink = PersistingModelSink::new(self, &context, cancellation);
            let provider_result = provider.provider.stream(request, &mut sink).await;

            if cancellation.is_cancelled() {
                self.cancel(&context, TurnState::Streaming)?;
                return Ok(TurnRunResult::Cancelled {
                    request_id: context.request_id,
                });
            }
            if let Err(error) = provider_result {
                return self.fail(&context, TurnState::Streaming, &error.code, &error.message);
            }
            if let Some((code, message)) = sink.model_failure.take() {
                return self.fail(&context, TurnState::Streaming, &code, &message);
            }
            if sink.completion_count != 1 {
                return self.fail(
                    &context,
                    TurnState::Streaming,
                    "provider_incomplete_stream",
                    "the provider stream did not contain exactly one completion event",
                );
            }

            let stop_reason = sink.stop_reason.clone().unwrap_or(StopReason::EndTurn);
            let calls = match sink.finish_tool_calls() {
                Ok(calls) => calls,
                Err(error) => {
                    return self.fail(
                        &context,
                        TurnState::Streaming,
                        "provider_invalid_tool_call",
                        &error.to_string(),
                    );
                }
            };

            if calls.is_empty() {
                if matches!(stop_reason, StopReason::ToolUse) {
                    return self.fail(
                        &context,
                        TurnState::Streaming,
                        "provider_missing_tool_call",
                        "the provider stopped for tool use without emitting a tool call",
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
                return self.complete(&context, item, cancellation);
            }

            if !matches!(stop_reason, StopReason::ToolUse) {
                return self.fail(
                    &context,
                    TurnState::Streaming,
                    "provider_inconsistent_tool_stop",
                    "the provider emitted tool calls without a tool-use stop reason",
                );
            }
            let Some(tool_registry) = tool_registry.as_ref() else {
                return self.fail(
                    &context,
                    TurnState::Streaming,
                    "unadvertised_tool_use",
                    "the provider emitted tool calls although tool use was not negotiated",
                );
            };
            if tool_calls_used.saturating_add(calls.len()) > provider.loop_limits.max_tool_calls {
                return self.fail(
                    &context,
                    TurnState::Streaming,
                    "agent_loop_tool_limit",
                    "the turn exceeded its total tool-call budget",
                );
            }
            tool_calls_used += calls.len();

            self.transition(
                &context,
                TurnState::Streaming,
                TurnState::ProposedTools,
                None,
            )?;
            let mut invocations = self.persist_tool_proposals(
                &context,
                &calls,
                &tool_specs,
                tool_registry,
                invocation_pipeline.as_ref(),
                sink.content,
            )?;
            self.transition(
                &context,
                TurnState::ProposedTools,
                TurnState::Authorizing,
                None,
            )?;
            for invocation in &mut invocations {
                if invocation.preparation_failure.is_some() {
                    // Keep a preparation failure at Proposed until its
                    // model-visible ToolResult can be committed together with
                    // the terminal state.  Emitting the state transition here
                    // would reopen the crash window that
                    // `append_invocation_outcome` is designed to close.
                    continue;
                }
                if invocation.effects.is_read_only() {
                    // Read-only tools intentionally use the existing bound
                    // registry executor and do not retain a pipeline token.
                    // They are auto-approved by policy without crossing the
                    // interactive approval boundary.
                    self.persist_invocation_transition(
                        &context,
                        invocation.invocation_id,
                        InvocationState::Proposed,
                        InvocationState::Approved,
                        Some("structured read-only tool requires no interactive approval".into()),
                    )?;
                    self.persist_invocation_transition(
                        &context,
                        invocation.invocation_id,
                        InvocationState::Approved,
                        InvocationState::Prepared,
                        None,
                    )?;
                    continue;
                }
                let Some(prepared) = invocation.prepared.take() else {
                    invocation.preparation_failure = Some(ToolPreparationFailure {
                        output: json!({
                            "code": "tool_authority_unavailable",
                            "message": "the daemon did not produce a sealed invocation"
                        }),
                        reason: "the daemon did not produce a sealed invocation".into(),
                    });
                    continue;
                };
                if !prepared.effects.is_read_only() {
                    let Some((ref pipeline, _)) = invocation_pipeline else {
                        invocation.preparation_failure = Some(ToolPreparationFailure {
                            output: json!({
                                "code": "tool_authority_unavailable",
                                "message": "side-effecting tools require the daemon authority pipeline"
                            }),
                            reason: "side-effecting tools require the daemon authority pipeline"
                                .into(),
                        });
                        continue;
                    };
                    self.transition(
                        &context,
                        TurnState::Authorizing,
                        TurnState::WaitingForApproval,
                        None,
                    )?;
                    let response = if cancellation.is_cancelled() {
                        ApprovalRequestResult {
                            approved: false,
                            approval: None,
                        }
                    } else if let Some(handler) = approval.as_ref() {
                        handler
                            .request(pipeline.approval_request(
                                &prepared,
                                "side-effecting tool requires approval",
                            ))
                            .await
                    } else {
                        ApprovalRequestResult {
                            approved: false,
                            approval: None,
                        }
                    };
                    self.transition(
                        &context,
                        TurnState::WaitingForApproval,
                        TurnState::Authorizing,
                        None,
                    )?;
                    match pipeline.accept_approval_response(
                        prepared.clone(),
                        response.approved,
                        response.approval,
                    ) {
                        Ok(approved) => invocation.prepared = Some(approved),
                        Err(error) => {
                            let reason = bounded_message(&error.to_string());
                            invocation.preparation_failure = Some(ToolPreparationFailure {
                                output: json!({
                                    "code": error.code(),
                                    "message": reason.clone(),
                                }),
                                reason,
                            });
                            continue;
                        }
                    }
                }
                self.persist_invocation_transition(
                    &context,
                    invocation.invocation_id,
                    InvocationState::Proposed,
                    InvocationState::Approved,
                    Some("daemon approval binding minted".into()),
                )?;
                self.persist_invocation_transition(
                    &context,
                    invocation.invocation_id,
                    InvocationState::Approved,
                    InvocationState::Prepared,
                    None,
                )?;
            }
            self.transition(
                &context,
                TurnState::Authorizing,
                TurnState::Scheduling,
                None,
            )?;
            self.transition(&context, TurnState::Scheduling, TurnState::Executing, None)?;

            let mut handles = Vec::with_capacity(invocations.len());
            for invocation in &mut invocations {
                if invocation.preparation_failure.is_some() {
                    handles.push(None);
                    continue;
                }
                if cancellation.is_cancelled() {
                    // The invocation has been authorized/prepared but has
                    // not crossed the execution boundary.  Keep it out of
                    // the worker pool and let the integration pass commit a
                    // paired deterministic cancellation failure.
                    invocation.cancelled_before_start = true;
                    handles.push(None);
                    continue;
                }
                // Acquire a daemon-wide slot before crossing the execution
                // boundary.  A saturated scheduler fails this invocation
                // closed instead of flooding Tokio's blocking pool.
                let permit = match self.tool_workers.clone().try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => {
                        invocation.scheduler_rejected = true;
                        handles.push(None);
                        continue;
                    }
                };
                if cancellation.is_cancelled() {
                    invocation.cancelled_before_start = true;
                    drop(permit);
                    handles.push(None);
                    continue;
                }
                let registry = Arc::clone(tool_registry);
                let tool_id = invocation.call.name.clone();
                let tool_version = invocation.tool_version.clone();
                let expected_workspace_identity = context.workspace_identity.digest.clone();
                let expected_normalized_arguments = invocation.normalized_arguments.clone();
                let expected_effects = invocation.effects.clone();
                let worker_cancel = Arc::new(AtomicBool::new(false));
                let worker_cancel_for_task = Arc::clone(&worker_cancel);
                let search_budget_for_task = Arc::clone(&search_operation_budget);
                let timeout_duration = Duration::from_millis(invocation.timeout_ms.max(1));
                let workspace_search_gate = if tool_id == WORKSPACE_SEARCH_TOOL_ID {
                    match self.workspace_search_gate(&context.workspace_identity) {
                        Ok(Some(gate)) => match gate.try_acquire_owned() {
                            Ok(permit) => Some(permit),
                            Err(_) => {
                                invocation.scheduler_rejected = true;
                                drop(permit);
                                handles.push(None);
                                continue;
                            }
                        },
                        Ok(None) => {
                            invocation.scheduler_rejected = true;
                            handles.push(None);
                            continue;
                        }
                        Err(error) => {
                            drop(permit);
                            return Err(error);
                        }
                    }
                } else {
                    None
                };
                self.persist_invocation_transition(
                    &context,
                    invocation.invocation_id,
                    InvocationState::Prepared,
                    InvocationState::Started,
                    None,
                )?;
                invocation.started = true;
                let handle = if let Some(prepared) = invocation.prepared.clone() {
                    let Some((ref pipeline, _)) = invocation_pipeline else {
                        invocation.preparation_failure = Some(ToolPreparationFailure {
                            output: json!({
                                "code": "tool_authority_unavailable",
                                "message": "the daemon authority pipeline disappeared before execution"
                            }),
                            reason: "the daemon authority pipeline disappeared before execution"
                                .into(),
                        });
                        drop(permit);
                        handles.push(None);
                        continue;
                    };
                    let pipeline = Arc::clone(pipeline);
                    tokio::spawn(async move {
                        let _permit: OwnedSemaphorePermit = permit;
                        pipeline.execute(prepared).await.map_err(pipeline_error)
                    })
                } else {
                    tokio::task::spawn_blocking(move || {
                        let _permit: OwnedSemaphorePermit = permit;
                        let _workspace_search_permit = workspace_search_gate;
                        let control = WorkspaceSearchControl::new()
                            .with_cancellation(worker_cancel_for_task.as_ref())
                            .with_timeout(timeout_duration)
                            .with_shared_operation_budget(search_budget_for_task.as_ref());
                        registry.execute_read_only_bound_with_control(
                            &tool_id,
                            &tool_version,
                            &expected_workspace_identity,
                            &expected_normalized_arguments,
                            &expected_effects,
                            Some(&control),
                        )
                    })
                };
                handles.push(Some((handle, worker_cancel)));
            }

            // Keep cancellation handles separately from the JoinHandles.  The
            // result-integration loop consumes the latter, but an aggregate
            // result-budget failure may return early while sibling workers are
            // still queued/running.  Signalling these Arcs before dropping the
            // remaining JoinHandles gives cooperative tools a chance to stop
            // instead of leaving unobserved work running in the background.
            let worker_cancellations = handles
                .iter()
                .map(|worker| {
                    worker
                        .as_ref()
                        .map(|(_, cancellation)| Arc::clone(cancellation))
                })
                .collect::<Vec<_>>();

            let mut cancelled_during_tools = false;
            // A cancelled turn is only allowed to become `Cancelled` when no
            // execution crossed a boundary whose outcome is still unknown.
            // Keep this separate from the cancellation request itself: an
            // unproven blocking worker must surface a failed/reconciliation
            // diagnostic rather than being hidden behind a clean Cancelled
            // turn state.
            let mut requires_reconciliation = false;
            let mut unknown_outcome_seen = false;
            for (index, (invocation, worker)) in invocations.iter_mut().zip(handles).enumerate() {
                if cancellation.is_cancelled() {
                    cancelled_during_tools = true;
                    if invocation.started {
                        requires_reconciliation = true;
                        // A blocking worker may still be running after its
                        // JoinHandle is dropped.  Do not claim cancellation
                        // as a terminal fact; persist Unknown and require
                        // reconciliation on restart.
                        if let Some((_, worker_cancel)) = worker.as_ref() {
                            worker_cancel.store(true, Ordering::Release);
                        }
                        self.persist_invocation_unknown_outcome(
                            &context,
                            invocation,
                            json!({
                                "code": "tool_outcome_unknown",
                                "message": "tool execution may still be running; reconcile before retrying"
                            }),
                            true,
                            "turn cancelled while workspace tool outcome was unproven".into(),
                        )?;
                    } else if invocation.scheduler_rejected {
                        // No worker crossed the execution boundary, so this
                        // is a proven pre-execution failure and can be
                        // terminalized atomically even while the turn is
                        // being cancelled.
                        self.persist_invocation_outcome(
                            &context,
                            invocation,
                            InvocationState::Failed,
                            InvocationState::Prepared,
                            json!({
                                "code": "tool_scheduler_busy",
                                "message": "daemon tool worker budget is exhausted"
                            }),
                            true,
                            Some("daemon tool worker budget is exhausted".into()),
                        )?;
                    } else if let Some(failure) = &invocation.preparation_failure {
                        // No worker crossed the execution boundary.  Persist
                        // the deterministic preparation failure and its
                        // terminal state atomically, even when cancellation
                        // wins before the normal result-integrating pass.
                        self.persist_invocation_outcome(
                            &context,
                            invocation,
                            InvocationState::Failed,
                            InvocationState::Proposed,
                            failure.output.clone(),
                            true,
                            Some(failure.reason.clone()),
                        )?;
                    } else if invocation.cancelled_before_start {
                        self.persist_invocation_outcome(
                            &context,
                            invocation,
                            InvocationState::Failed,
                            InvocationState::Prepared,
                            json!({
                                "code": "tool_cancelled_before_start",
                                "message": "turn cancellation arrived before tool execution"
                            }),
                            true,
                            Some("turn cancellation arrived before tool execution".into()),
                        )?;
                    }
                    continue;
                }

                let mut unknown = false;
                let (output, is_error, terminal_state, from_state, reason) = if let Some(failure) =
                    &invocation.preparation_failure
                {
                    (
                        failure.output.clone(),
                        true,
                        Some(InvocationState::Failed),
                        InvocationState::Proposed,
                        Some(failure.reason.clone()),
                    )
                } else if invocation.scheduler_rejected {
                    (
                        json!({
                            "code": "tool_scheduler_busy",
                            "message": "daemon tool worker budget is exhausted"
                        }),
                        true,
                        Some(InvocationState::Failed),
                        InvocationState::Prepared,
                        Some("daemon tool worker budget is exhausted".into()),
                    )
                } else if invocation.cancelled_before_start {
                    (
                        json!({
                            "code": "tool_cancelled_before_start",
                            "message": "turn cancellation arrived before tool execution"
                        }),
                        true,
                        Some(InvocationState::Failed),
                        InvocationState::Prepared,
                        Some("turn cancellation arrived before tool execution".into()),
                    )
                } else {
                    let (handle, worker_cancel) =
                        worker.expect("started workspace tool has a worker");
                    match wait_for_tool_worker(
                        handle,
                        worker_cancel,
                        cancellation,
                        invocation.timeout_ms.max(1),
                    )
                    .await
                    {
                        ToolWorkerWait::Completed(Ok(Ok(output))) => (
                            output,
                            false,
                            Some(InvocationState::Completed),
                            InvocationState::Started,
                            None,
                        ),
                        ToolWorkerWait::Completed(Ok(Err(error))) => (
                            json!({
                                "code": error.provider_code(),
                                "message": bounded_message(&error.to_string()),
                            }),
                            true,
                            Some(InvocationState::Failed),
                            InvocationState::Started,
                            Some(bounded_message(&error.to_string())),
                        ),
                        ToolWorkerWait::Completed(Err(error)) => {
                            // A JoinError only proves that Tokio could not
                            // return the closure's value.  The closure may
                            // have panicked, been aborted, or completed an
                            // external read immediately before termination;
                            // none of those cases proves a Failed tool
                            // outcome.  Preserve the Started -> Unknown
                            // marker and pair it with a bounded diagnostic
                            // result so replay/reconciliation can decide
                            // whether a retry is safe.
                            unknown = true;
                            classify_tool_worker_join_error(&error)
                        }
                        ToolWorkerWait::Unknown => {
                            unknown = true;
                            (
                                json!({
                                    "code": "tool_timeout",
                                    "message": "tool exceeded its execution deadline; outcome requires reconciliation"
                                }),
                                true,
                                None,
                                InvocationState::Started,
                                Some("workspace tool exceeded its execution deadline".into()),
                            )
                        }
                    }
                };

                if cancellation.is_cancelled() && invocation.started && unknown {
                    cancelled_during_tools = true;
                    requires_reconciliation = true;
                    unknown_outcome_seen = true;
                    self.persist_invocation_unknown_outcome(
                        &context,
                        invocation,
                        json!({
                            "code": "tool_outcome_unknown",
                            "message": "tool execution may still be running; reconcile before retrying"
                        }),
                        true,
                        "turn cancelled while workspace tool outcome was unproven".into(),
                    )?;
                    continue;
                }

                if unknown {
                    // A timeout is not a proof that the blocking closure
                    // stopped.  Preserve the Unknown invocation marker even
                    // when the parent turn itself is allowed to continue.
                    unknown_outcome_seen = true;
                    self.persist_invocation_unknown_outcome(
                        &context,
                        invocation,
                        output,
                        is_error,
                        reason.unwrap_or_else(|| "workspace tool outcome was unproven".into()),
                    )?;
                    continue;
                }

                let output_bytes = serde_json::to_vec(&output)?.len();
                if tool_result_bytes.saturating_add(output_bytes)
                    > provider.loop_limits.max_tool_result_bytes
                {
                    let budget_output = json!({
                        "code": "agent_loop_tool_result_limit",
                        "message": "the turn exceeded its total tool-result byte budget"
                    });
                    // The invocation that crossed the aggregate budget is
                    // failed regardless of whether it was already started:
                    // Proposed/Prepared failures must not be left dangling
                    // when the turn is terminated below.
                    self.persist_invocation_outcome(
                        &context,
                        invocation,
                        InvocationState::Failed,
                        from_state,
                        budget_output.clone(),
                        true,
                        Some("turn tool-result byte budget exceeded".into()),
                    )?;

                    // Resolve every later invocation before making the parent
                    // turn terminal.  Started workers are indeterminate and
                    // receive the typed Unknown marker; workers that never
                    // crossed the execution boundary get an atomic Failed
                    // outcome with a bounded diagnostic result.
                    for (pending_index, pending) in
                        invocations.iter_mut().enumerate().skip(index + 1)
                    {
                        if let Some(Some(worker_cancel)) = worker_cancellations.get(pending_index) {
                            worker_cancel.store(true, Ordering::Release);
                        }
                        if pending.started {
                            self.persist_invocation_unknown_outcome(
                                &context,
                                pending,
                                budget_output.clone(),
                                true,
                                "a sibling tool exceeded the turn result budget".into(),
                            )?;
                        } else if pending.scheduler_rejected {
                            self.persist_invocation_outcome(
                                &context,
                                pending,
                                InvocationState::Failed,
                                InvocationState::Prepared,
                                budget_output.clone(),
                                true,
                                Some("a sibling tool exceeded the turn result budget".into()),
                            )?;
                        } else if let Some(failure) = &pending.preparation_failure {
                            self.persist_invocation_outcome(
                                &context,
                                pending,
                                InvocationState::Failed,
                                InvocationState::Proposed,
                                failure.output.clone(),
                                true,
                                Some(failure.reason.clone()),
                            )?;
                        } else if pending.cancelled_before_start {
                            self.persist_invocation_outcome(
                                &context,
                                pending,
                                InvocationState::Failed,
                                InvocationState::Prepared,
                                budget_output.clone(),
                                true,
                                Some("a sibling tool exceeded the turn result budget".into()),
                            )?;
                        }
                    }
                    return self.fail_current(
                        &context,
                        "agent_loop_tool_result_limit",
                        "the turn exceeded its total tool-result byte budget",
                    );
                }
                tool_result_bytes += output_bytes;
                if let Some(terminal_state) = terminal_state {
                    // Every proven terminal result, including failures before
                    // a worker starts, is committed as one ToolResult + state
                    // batch.  This keeps replay from observing a terminal
                    // invocation with no model-visible result.
                    self.persist_invocation_outcome(
                        &context,
                        invocation,
                        terminal_state,
                        from_state,
                        output,
                        is_error,
                        reason,
                    )?;
                } else {
                    // Unknown outcomes intentionally have no terminal state;
                    // the marker and diagnostic are committed atomically so
                    // recovery can distinguish indeterminate work without a
                    // half-written explanation.
                    self.persist_invocation_unknown_outcome(
                        &context,
                        invocation,
                        output,
                        is_error,
                        reason.unwrap_or_else(|| "workspace tool outcome was unproven".into()),
                    )?;
                }
            }

            if requires_reconciliation {
                return self.fail_current(
                    &context,
                    "turn_cancellation_requires_reconciliation",
                    "turn cancellation was requested while a tool outcome remained unknown; reconcile before retrying",
                );
            }
            if unknown_outcome_seen {
                // An unknown result is never fed into another provider round:
                // doing so could cause the model to retry an operation whose
                // external outcome is still unresolved.  Stop the turn at
                // this boundary and require explicit reconciliation.
                return self.fail_current(
                    &context,
                    "tool_outcome_unknown",
                    "a workspace tool outcome could not be proven stopped; reconcile before retrying",
                );
            }
            if cancelled_during_tools || cancellation.is_cancelled() {
                self.cancel(&context, TurnState::Executing)?;
                return Ok(TurnRunResult::Cancelled {
                    request_id: context.request_id,
                });
            }
            self.transition(
                &context,
                TurnState::Executing,
                TurnState::IntegratingResults,
                None,
            )?;
            if model_rounds >= provider.loop_limits.max_model_rounds {
                return self.fail(
                    &context,
                    TurnState::IntegratingResults,
                    "agent_loop_round_limit",
                    "the turn exhausted its model-round budget after tool execution",
                );
            }
            self.transition(
                &context,
                TurnState::IntegratingResults,
                TurnState::RequestingModel,
                None,
            )?;
        }
    }

    fn reload_lineage_events(
        &self,
        thread_id: ThreadId,
    ) -> Result<Vec<EventEnvelope>, TurnRunnerError> {
        let projection = self.ledger.project_core()?;
        load_lineage_events(self, &projection, thread_id)
    }

    fn persist_tool_proposals(
        &self,
        context: &RunContext,
        calls: &[AssembledToolCall],
        specs: &[ToolSpec],
        tool_registry: &ToolRegistry,
        invocation_pipeline: Option<&(Arc<InvocationPipeline>, crate::grants::GrantLayers)>,
        mut assistant_content: Vec<ContentBlock>,
    ) -> Result<Vec<PendingToolInvocation>, TurnRunnerError> {
        let mut invocations = Vec::with_capacity(calls.len());
        for call in calls {
            let invocation_id = InvocationId::from_uuid(self.ids.next_uuid()?);
            assistant_content.push(ContentBlock::ToolCall {
                call_id: call.call_id.clone(),
                name: call.name.clone(),
                arguments: call.arguments.clone(),
            });
            let registration = specs.iter().find(|spec| spec.id == call.name);
            let tool_version = registration
                .map(|spec| spec.version.clone())
                .or_else(|| {
                    tool_registry
                        .advertised_version(&call.name)
                        .map(str::to_owned)
                })
                .unwrap_or_else(|| "unregistered".into());
            let planned = registration
                .filter(|_| tool_version != "unregistered")
                .map(|spec| tool_registry.plan(&call.name, &spec.version, call.arguments.clone()))
                .unwrap_or_else(|| {
                    Err(ToolRegistryError::UnknownTool {
                        tool_id: call.name.clone(),
                        tool_version: tool_version.clone(),
                    })
                });
            let (normalized_arguments, effects, preparation_failure, prepared) = match planned {
                Ok(plan) => (
                    plan.normalized_arguments().clone(),
                    plan.effects().clone(),
                    None,
                    None,
                ),
                Err(error) => {
                    let reason = bounded_message(&error.to_string());
                    let effects = registration
                        .map(|spec| spec.effect_template.clone())
                        .unwrap_or_default();
                    (
                        call.arguments.clone(),
                        effects,
                        Some(ToolPreparationFailure {
                            output: json!({
                                "code": error.provider_code(),
                                "message": reason,
                            }),
                            reason,
                        }),
                        None,
                    )
                }
            };
            let (normalized_arguments, effects, preparation_failure, prepared) =
                if preparation_failure.is_none() {
                    if let Some((pipeline, grants)) = invocation_pipeline {
                        let pipeline_context = InvocationContext {
                            invocation_id,
                            workspace_id: context.workspace_id,
                            workspace_identity_digest: context.workspace_identity.digest.clone(),
                            thread_id: context.spec.thread_id,
                            turn_id: context.spec.turn_id,
                            agent_id: context.agent_id.clone(),
                            grants: PipelineGrants {
                                host_ceiling: grants.host_ceiling.clone(),
                                user_profile: grants.user_profile.clone(),
                                project_trust: grants.project_trust.clone(),
                                turn_override: grants.turn_override.clone(),
                            },
                            now: self.clock.now(),
                            preparation_ttl: chrono::Duration::seconds(
                                crate::pipeline::DEFAULT_PREPARATION_TTL_SECONDS,
                            ),
                        };
                        match pipeline.prepare(
                            &call.name,
                            &tool_version,
                            call.arguments.clone(),
                            &pipeline_context,
                        ) {
                            Ok(prepared) => {
                                let normalized = prepared.normalized_arguments.clone();
                                let effects = prepared.effects.clone();
                                let authority = if effects.is_read_only() {
                                    None
                                } else {
                                    Some(prepared)
                                };
                                (normalized, effects, None, authority)
                            }
                            Err(error) => {
                                let reason = bounded_message(&error.to_string());
                                (
                                    call.arguments.clone(),
                                    effects,
                                    Some(ToolPreparationFailure {
                                        output: json!({
                                            "code": error.code(),
                                            "message": reason.clone(),
                                        }),
                                        reason,
                                    }),
                                    None,
                                )
                            }
                        }
                    } else {
                        (normalized_arguments, effects, preparation_failure, prepared)
                    }
                } else {
                    (normalized_arguments, effects, preparation_failure, prepared)
                };
            invocations.push(PendingToolInvocation {
                invocation_id,
                call: call.clone(),
                tool_version,
                normalized_arguments,
                timeout_ms: registration.map_or(5_000, |spec| spec.timeout_ms),
                effects,
                prepared,
                preparation_failure,
                scheduler_rejected: false,
                cancelled_before_start: false,
                started: false,
            });
        }

        // The provider stream and the proposal write are separate phases, so
        // an interrupt can win the mutation gate in between them.  Re-check
        // the durable parent while holding that same gate immediately before
        // appending the ToolCall item/proposals; never append invocation
        // metadata to a turn that has already entered cancellation or reached
        // a terminal state.
        let _guard = self.lock_mutations()?;
        let actual = self.current_state(context)?;
        if actual != TurnState::ProposedTools {
            return Err(TurnRunnerError::UnexpectedState {
                turn_id: context.spec.turn_id,
                expected: TurnState::ProposedTools,
                actual,
            });
        }

        let item = Item {
            id: ItemId::from_uuid(self.ids.next_uuid()?),
            thread_id: context.spec.thread_id,
            turn_id: context.spec.turn_id,
            agent_id: context.agent_id.clone(),
            kind: ItemKind::ToolCall,
            content: json!({
                "content": assistant_content,
                "invocation_ids": invocations
                    .iter()
                    .map(|invocation| invocation.invocation_id.to_string())
                    .collect::<Vec<_>>(),
            }),
            created_at: self.clock.now(),
        };
        let mut events = Vec::with_capacity(invocations.len().saturating_add(1));
        events.push(Event::ItemAdded { item });
        for invocation in &invocations {
            let normalized_arguments_digest = digest_value(&invocation.normalized_arguments);
            let effect_digest = digest_value(&serde_json::to_value(&invocation.effects)?);
            events.push(Event::InvocationProposed {
                invocation_id: invocation.invocation_id,
                call_id: invocation.call.call_id.clone(),
                tool_id: invocation.call.name.clone(),
                tool_version: invocation.tool_version.clone(),
                normalized_arguments_digest,
                effects: invocation.effects.clone(),
                effect_digest,
                idempotency: invocation.effects.idempotency,
            });
        }
        // The assistant ToolCall item and all invocation proposals form one
        // logical model turn.  Append them in one transaction so a crash (or
        // an interrupt between phases) cannot leave a durable ToolCall item
        // whose invocation metadata is missing.
        let inputs = events
            .into_iter()
            .map(|event| self.new_ledger_event_locked(context, event))
            .collect::<Result<Vec<_>, _>>()?;
        self.persist_new_events_locked(inputs)?;
        Ok(invocations)
    }

    fn persist_invocation_transition(
        &self,
        context: &RunContext,
        invocation_id: InvocationId,
        from: InvocationState,
        to: InvocationState,
        reason: Option<String>,
    ) -> Result<EventEnvelope, TurnRunnerError> {
        // `Started -> Unknown` is a recovery-sensitive marker rather than a
        // normal state transition.  Route it through the ledger's typed API
        // so the exact transition and current-state precondition are checked
        // while the append transaction is held.  Other lifecycle transitions
        // retain the existing single-event path.
        let _guard = self.lock_mutations()?;
        if to == InvocationState::Started {
            // Crossing the execution boundary is only valid while the parent
            // turn is durably Executing.  The command gate is shared with
            // `turn/interrupt`, so this check and the transition append are
            // atomic with respect to a cancellation request: if Cancelling
            // won the race, leave the invocation Prepared for the settlement
            // pass instead of starting a worker after cancellation.
            let actual = self.current_state(context)?;
            if actual != TurnState::Executing {
                return Err(TurnRunnerError::UnexpectedState {
                    turn_id: context.spec.turn_id,
                    expected: TurnState::Executing,
                    actual,
                });
            }
        }
        self.ensure_turn_nonterminal_locked(context)?;
        if from == InvocationState::Started && to == InvocationState::Unknown {
            let input = self.new_ledger_event_locked(
                context,
                Event::InvocationStateChanged {
                    invocation_id,
                    from,
                    to,
                    reason,
                },
            )?;
            let persisted = self
                .ledger
                .append_invocation_unknown(NewInvocationUnknown { state: input })?;
            let envelope = EventEnvelope::try_from(persisted)?;
            let _ = self.events.send(envelope.clone());
            Ok(envelope)
        } else {
            self.persist_locked(
                context,
                Event::InvocationStateChanged {
                    invocation_id,
                    from,
                    to,
                    reason,
                },
            )
        }
    }

    /// Persist an indeterminate invocation marker and its bounded diagnostic
    /// as one ledger batch.  The marker is deliberately non-terminal; only an
    /// explicit reconciliation event may later conclude the invocation.
    fn persist_invocation_unknown_outcome(
        &self,
        context: &RunContext,
        invocation: &PendingToolInvocation,
        output: Value,
        is_error: bool,
        reason: String,
    ) -> Result<Vec<EventEnvelope>, TurnRunnerError> {
        let _guard = self.lock_mutations()?;
        self.ensure_turn_nonterminal_locked(context)?;
        let unknown_state = self.new_ledger_event_locked(
            context,
            Event::InvocationStateChanged {
                invocation_id: invocation.invocation_id,
                from: InvocationState::Started,
                to: InvocationState::Unknown,
                reason: Some(reason),
            },
        )?;
        let tool_result = self.new_ledger_event_locked(
            context,
            Event::ItemAdded {
                item: self.tool_result_item(context, invocation, output, is_error)?,
            },
        )?;
        let committed =
            self.ledger
                .append_invocation_unknown_outcome(NewInvocationUnknownOutcome {
                    unknown_state,
                    tool_result,
                })?;
        let mut envelopes = Vec::with_capacity(committed.len());
        for event in committed {
            let envelope = EventEnvelope::try_from(event)?;
            let _ = self.events.send(envelope.clone());
            envelopes.push(envelope);
        }
        Ok(envelopes)
    }

    /// Persist a terminal invocation transition and its model-visible result
    /// as one SQLite transaction.  This closes the crash window where a
    /// replay could otherwise observe `Completed`/`Failed` without a
    /// corresponding ToolResult.
    #[allow(clippy::too_many_arguments)]
    fn persist_invocation_outcome(
        &self,
        context: &RunContext,
        invocation: &PendingToolInvocation,
        terminal_state: InvocationState,
        from_state: InvocationState,
        output: Value,
        is_error: bool,
        reason: Option<String>,
    ) -> Result<Vec<EventEnvelope>, TurnRunnerError> {
        let _guard = self.lock_mutations()?;
        self.ensure_turn_nonterminal_locked(context)?;
        let tool_result = self.new_ledger_event_locked(
            context,
            Event::ItemAdded {
                item: self.tool_result_item(context, invocation, output, is_error)?,
            },
        )?;
        let terminal = self.new_ledger_event_locked(
            context,
            Event::InvocationStateChanged {
                invocation_id: invocation.invocation_id,
                from: from_state,
                to: terminal_state,
                reason,
            },
        )?;
        let committed = self
            .ledger
            .append_invocation_outcome(NewInvocationOutcome {
                tool_result,
                terminal_state: terminal,
            })?;
        let mut envelopes = Vec::with_capacity(committed.len());
        for event in committed {
            let envelope = EventEnvelope::try_from(event)?;
            let _ = self.events.send(envelope.clone());
            envelopes.push(envelope);
        }
        Ok(envelopes)
    }

    fn tool_result_item(
        &self,
        context: &RunContext,
        invocation: &PendingToolInvocation,
        output: Value,
        is_error: bool,
    ) -> Result<Item, TurnRunnerError> {
        Ok(Item {
            id: ItemId::from_uuid(self.ids.next_uuid()?),
            thread_id: context.spec.thread_id,
            turn_id: context.spec.turn_id,
            agent_id: context.agent_id.clone(),
            kind: ItemKind::ToolResult,
            content: json!({
                "content": [ContentBlock::ToolResult {
                    call_id: invocation.call.call_id.clone(),
                    content: output,
                    is_error,
                }],
                "invocation_id": invocation.invocation_id,
            }),
            created_at: self.clock.now(),
        })
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
        let actual = self.current_state(context)?;
        // `turn/interrupt` durably moves a live turn to `Cancelling` before
        // signalling the runner.  Any failure discovered in that narrow
        // window must still close the turn; writing a diagnostic and then
        // rejecting `Executing -> Failed` would strand it in Cancelling until
        // the next daemon restart.
        if actual != from && actual != TurnState::Cancelling {
            return Err(TurnRunnerError::UnexpectedState {
                turn_id: context.spec.turn_id,
                expected: from,
                actual,
            });
        }
        if actual == TurnState::Cancelling && self.has_pending_invocations(context)? {
            // Leave the turn in Cancelling for the wrapper's settlement pass;
            // terminalizing it here could strand Proposed/Prepared calls.
            return Err(TurnRunnerError::UnexpectedState {
                turn_id: context.spec.turn_id,
                expected: from,
                actual,
            });
        }
        self.persist_locked(
            context,
            Event::RuntimeDiagnostic {
                code: code.clone(),
                message: message.clone(),
                recoverable: false,
            },
        )?;
        self.transition_locked(context, actual, TurnState::Failed, Some(message))?;
        Ok(TurnRunResult::Failed {
            request_id: context.request_id,
            code,
        })
    }

    /// Fail from whatever non-terminal turn state is currently durable.  The
    /// interrupt command intentionally persists `Cancelling` before the
    /// runner observes it, so an unknown tool outcome must not assume the
    /// state is still `Executing` when emitting its reconciliation failure.
    fn fail_current(
        &self,
        context: &RunContext,
        code: &str,
        message: &str,
    ) -> Result<TurnRunResult, TurnRunnerError> {
        let code = normalize_code(code);
        let message = bounded_message(message);
        let _guard = self.lock_mutations()?;
        let actual = self.current_state(context)?;
        if actual.is_terminal() {
            return Err(TurnRunnerError::UnexpectedState {
                turn_id: context.spec.turn_id,
                expected: TurnState::Executing,
                actual,
            });
        }
        // `Unknown` is intentionally allowed here: it is the durable shape
        // for an execution whose external outcome cannot be proven and is
        // therefore paired with a Failed parent for later reconciliation.
        // Every earlier lifecycle state, however, still represents work that
        // has not been settled. Refuse to close the parent while any such
        // invocation remains, so an unexpected early return cannot orphan a
        // Proposed/Prepared/Started child behind a terminal turn.
        if self.has_unsettled_invocations(context)? {
            return Err(TurnRunnerError::UnexpectedState {
                turn_id: context.spec.turn_id,
                expected: TurnState::Executing,
                actual,
            });
        }
        self.persist_locked(
            context,
            Event::RuntimeDiagnostic {
                code: code.clone(),
                message: message.clone(),
                recoverable: false,
            },
        )?;
        self.transition_locked(context, actual, TurnState::Failed, Some(message))?;
        Ok(TurnRunResult::Failed {
            request_id: context.request_id,
            code,
        })
    }

    /// Resolve every invocation that belongs to an interrupted turn before
    /// choosing the turn's terminal state.  This is deliberately synchronous
    /// and read-only with respect to the workspace: it only appends durable
    /// evidence and never retries external work.
    fn settle_interrupted_turn(
        &self,
        spec: TurnRunSpec,
    ) -> Result<TurnSettlement, TurnRunnerError> {
        let _guard = self.lock_mutations()?;
        let projection = self.ledger.project_core()?;
        let turn =
            projection
                .turns
                .get(&spec.turn_id)
                .cloned()
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

        // Inspect the durable parent state before appending any invocation
        // evidence.  A normal result-budget/unknown-outcome path may already
        // have terminalized the turn as Failed while intentionally retaining
        // Unknown invocations for reconciliation; appending more events after
        // that terminal transition would violate the event-stream invariant.
        let current = turn.state;

        let pending = projection
            .invocations
            .values()
            .filter(|invocation| {
                invocation.thread_id == spec.thread_id
                    && invocation.turn_id == spec.turn_id
                    && !invocation.state.is_terminal()
            })
            .cloned()
            .collect::<Vec<_>>();

        if current != TurnState::Cancelling {
            if current.is_terminal() {
                // Failed + Unknown is the durable representation of an
                // indeterminate external outcome.  Completed/Cancelled may
                // never retain an unresolved invocation.
                if pending.is_empty()
                    || (current == TurnState::Failed
                        && pending
                            .iter()
                            .all(|invocation| invocation.state == InvocationState::Unknown))
                {
                    return Ok(TurnSettlement::Unchanged);
                }
                return Err(TurnRunnerError::UnexpectedState {
                    turn_id: spec.turn_id,
                    expected: TurnState::Cancelling,
                    actual: current,
                });
            }
            // A live turn that is not durably Cancelling is owned by another
            // runner (or failed before it acquired the cancellation boundary).
            // Do not mutate it from this cleanup pass; the next recovery pass
            // can inspect the exact non-terminal prefix instead.
            if !pending.is_empty() {
                return Err(TurnRunnerError::UnexpectedState {
                    turn_id: spec.turn_id,
                    expected: TurnState::Cancelling,
                    actual: current,
                });
            }
            return Ok(TurnSettlement::Unchanged);
        }

        let mut requires_reconciliation = false;
        for invocation in &pending {
            match invocation.state {
                InvocationState::Proposed
                | InvocationState::Approved
                | InvocationState::Prepared => {
                    self.persist_cancelled_invocation_locked(invocation)?;
                }
                InvocationState::Started => {
                    requires_reconciliation = true;
                    self.persist_unknown_invocation_locked(invocation)?;
                }
                InvocationState::Unknown => {
                    requires_reconciliation = true;
                }
                InvocationState::Completed
                | InvocationState::Failed
                | InvocationState::Cancelled => {}
            }
        }

        // The invocation batches above do not mutate the turn.  Re-read its
        // state under the same mutation gate in case this method is called by
        // a future recovery path after another durable transition.
        let current = self
            .ledger
            .project_core()?
            .turns
            .get(&spec.turn_id)
            .map(|turn| turn.state)
            .ok_or(TurnRunnerError::MissingTurn {
                turn_id: spec.turn_id,
            })?;
        if current != TurnState::Cancelling {
            // A normally completed/failed turn has nothing for this cleanup
            // pass to do.  Failed + Unknown is the expected reconciliation
            // shape; any other unresolved child is an inconsistency.
            if pending.is_empty()
                || (current == TurnState::Failed
                    && pending
                        .iter()
                        .all(|invocation| invocation.state == InvocationState::Unknown))
            {
                return Ok(TurnSettlement::Unchanged);
            }
            if current.is_terminal() {
                return Err(TurnRunnerError::UnexpectedState {
                    turn_id: spec.turn_id,
                    expected: TurnState::Cancelling,
                    actual: current,
                });
            }
            return Err(TurnRunnerError::UnexpectedState {
                turn_id: spec.turn_id,
                expected: TurnState::Cancelling,
                actual: current,
            });
        }

        let causation_id = format!("turn-cancel-settlement:{}", spec.turn_id);
        let mut events = Vec::with_capacity(if requires_reconciliation { 2 } else { 1 });
        if requires_reconciliation {
            events.push(self.new_scoped_ledger_event_locked(
                spec.thread_id,
                spec.turn_id,
                &turn.agent_id,
                &causation_id,
                Event::RuntimeDiagnostic {
                    code: "turn_cancellation_requires_reconciliation".into(),
                    message: "turn cancellation encountered an invocation whose external outcome was not provably stopped; reconcile before retrying".into(),
                    recoverable: false,
                },
            )?);
        }
        events.push(self.new_scoped_ledger_event_locked(
            spec.thread_id,
            spec.turn_id,
            &turn.agent_id,
            &causation_id,
            Event::TurnStateChanged {
                turn_id: spec.turn_id,
                from: TurnState::Cancelling,
                to: if requires_reconciliation {
                    TurnState::Failed
                } else {
                    TurnState::Cancelled
                },
                reason: Some(if requires_reconciliation {
                    "turn cancellation requires invocation reconciliation".into()
                } else {
                    "turn runner cancelled after settling all invocations".into()
                }),
            },
        )?);
        self.persist_new_events_locked(events)?;
        Ok(if requires_reconciliation {
            TurnSettlement::FailedReconciliation
        } else {
            TurnSettlement::Cancelled
        })
    }

    fn has_pending_invocations(&self, context: &RunContext) -> Result<bool, TurnRunnerError> {
        Ok(self
            .ledger
            .project_core()?
            .invocations
            .values()
            .any(|invocation| {
                invocation.thread_id == context.spec.thread_id
                    && invocation.turn_id == context.spec.turn_id
                    && !invocation.state.is_terminal()
            }))
    }

    /// Return whether any invocation is still in a lifecycle state that must
    /// be settled before the parent turn may become terminal. `Unknown` is
    /// deliberately excluded: a Failed parent may retain Unknown children as
    /// an explicit reconciliation obligation.
    fn has_unsettled_invocations(&self, context: &RunContext) -> Result<bool, TurnRunnerError> {
        Ok(self
            .ledger
            .project_core()?
            .invocations
            .values()
            .any(|invocation| {
                invocation.thread_id == context.spec.thread_id
                    && invocation.turn_id == context.spec.turn_id
                    && !invocation.state.is_terminal()
                    && invocation.state != InvocationState::Unknown
            }))
    }

    fn persist_cancelled_invocation_locked(
        &self,
        invocation: &yeux_core::ProjectedInvocation,
    ) -> Result<(), TurnRunnerError> {
        let causation_id = format!(
            "turn-cancel-invocation:{}:{}",
            invocation.turn_id, invocation.invocation_id
        );
        let output = json!({
            "code": "tool_cancelled_before_start",
            "message": "turn cancellation arrived before this tool crossed the execution boundary"
        });
        let tool_result = self.new_scoped_ledger_event_locked(
            invocation.thread_id,
            invocation.turn_id,
            &invocation.agent_id,
            &causation_id,
            Event::ItemAdded {
                item: Item {
                    id: ItemId::from_uuid(self.ids.next_uuid()?),
                    thread_id: invocation.thread_id,
                    turn_id: invocation.turn_id,
                    agent_id: invocation.agent_id.clone(),
                    kind: ItemKind::ToolResult,
                    content: json!({
                        "content": [ContentBlock::ToolResult {
                            call_id: invocation.call_id.clone(),
                            content: output,
                            is_error: true,
                        }],
                        "invocation_id": invocation.invocation_id,
                    }),
                    created_at: self.clock.now(),
                },
            },
        )?;
        let terminal_state = self.new_scoped_ledger_event_locked(
            invocation.thread_id,
            invocation.turn_id,
            &invocation.agent_id,
            &causation_id,
            Event::InvocationStateChanged {
                invocation_id: invocation.invocation_id,
                from: invocation.state,
                to: InvocationState::Failed,
                reason: Some("turn cancellation arrived before tool execution".into()),
            },
        )?;
        let committed = self
            .ledger
            .append_invocation_outcome(NewInvocationOutcome {
                tool_result,
                terminal_state,
            })?;
        self.broadcast_events(committed)?;
        Ok(())
    }

    fn persist_unknown_invocation_locked(
        &self,
        invocation: &yeux_core::ProjectedInvocation,
    ) -> Result<(), TurnRunnerError> {
        let causation_id = format!(
            "turn-cancel-invocation:{}:{}",
            invocation.turn_id, invocation.invocation_id
        );
        let output = json!({
            "code": "tool_outcome_unknown",
            "message": "tool execution may still be running; reconcile before retrying"
        });
        let unknown_state = self.new_scoped_ledger_event_locked(
            invocation.thread_id,
            invocation.turn_id,
            &invocation.agent_id,
            &causation_id,
            Event::InvocationStateChanged {
                invocation_id: invocation.invocation_id,
                from: InvocationState::Started,
                to: InvocationState::Unknown,
                reason: Some("turn cancellation could not prove tool termination".into()),
            },
        )?;
        let tool_result = self.new_scoped_ledger_event_locked(
            invocation.thread_id,
            invocation.turn_id,
            &invocation.agent_id,
            &causation_id,
            Event::ItemAdded {
                item: Item {
                    id: ItemId::from_uuid(self.ids.next_uuid()?),
                    thread_id: invocation.thread_id,
                    turn_id: invocation.turn_id,
                    agent_id: invocation.agent_id.clone(),
                    kind: ItemKind::ToolResult,
                    content: json!({
                        "content": [ContentBlock::ToolResult {
                            call_id: invocation.call_id.clone(),
                            content: output,
                            is_error: true,
                        }],
                        "invocation_id": invocation.invocation_id,
                    }),
                    created_at: self.clock.now(),
                },
            },
        )?;
        let committed =
            self.ledger
                .append_invocation_unknown_outcome(NewInvocationUnknownOutcome {
                    unknown_state,
                    tool_result,
                })?;
        self.broadcast_events(committed)?;
        Ok(())
    }

    fn cancel(&self, context: &RunContext, from: TurnState) -> Result<(), TurnRunnerError> {
        let _guard = self.lock_mutations()?;
        self.cancel_locked(context, from)
    }

    fn cancel_locked(&self, context: &RunContext, from: TurnState) -> Result<(), TurnRunnerError> {
        let actual = self.current_state(context)?;
        if actual == TurnState::Cancelled {
            if self.has_pending_invocations(context)? {
                return Err(TurnRunnerError::UnexpectedState {
                    turn_id: context.spec.turn_id,
                    expected: from,
                    actual,
                });
            }
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
            if self.has_pending_invocations(context)? {
                // Keep the durable state at Cancelling.  The outer `run`
                // wrapper will atomically settle every invocation before it
                // chooses Cancelled versus reconciliation-required Failed.
                // Returning success here lets the caller expose a normal
                // cancellation result while settlement finishes the child
                // invocations synchronously before the task exits.
                return Ok(());
            }
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
        if self.has_pending_invocations(context)? {
            // A cancellation request can race with a proposal/prepare write.
            // Do not close the parent turn while those invocations still need
            // a paired ToolResult/state outcome; settlement will handle them.
            return Ok(());
        }
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
        self.persist_locked_expected(context, TurnState::Streaming, Event::ItemAdded { item })?;
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

    /// Invocation evidence may be appended while a turn is live or
    /// Cancelling, but never after the parent has become terminal.  Keeping
    /// this check under the same mutation gate as the append prevents a
    /// control-plane interrupt from closing the parent between validation and
    /// the child event write.
    fn ensure_turn_nonterminal_locked(&self, context: &RunContext) -> Result<(), TurnRunnerError> {
        let actual = self.current_state(context)?;
        if actual.is_terminal() {
            return Err(TurnRunnerError::UnexpectedState {
                turn_id: context.spec.turn_id,
                expected: TurnState::Executing,
                actual,
            });
        }
        Ok(())
    }

    fn persist_expected(
        &self,
        context: &RunContext,
        expected: TurnState,
        event: Event,
    ) -> Result<EventEnvelope, TurnRunnerError> {
        let _guard = self.lock_mutations()?;
        self.persist_locked_expected(context, expected, event)
    }

    fn persist_locked_expected(
        &self,
        context: &RunContext,
        expected: TurnState,
        event: Event,
    ) -> Result<EventEnvelope, TurnRunnerError> {
        let actual = self.current_state(context)?;
        if actual != expected {
            return Err(TurnRunnerError::UnexpectedState {
                turn_id: context.spec.turn_id,
                expected,
                actual,
            });
        }
        self.persist_locked(context, event)
    }

    fn persist_locked(
        &self,
        context: &RunContext,
        event: Event,
    ) -> Result<EventEnvelope, TurnRunnerError> {
        // No ordinary runtime event may be appended after the parent turn is
        // terminal.  This final guard complements the phase-specific checks
        // above and closes any future post-terminal write path introduced by
        // a new sink or helper.
        let actual = self.current_state(context)?;
        if actual.is_terminal() {
            return Err(TurnRunnerError::UnexpectedState {
                turn_id: context.spec.turn_id,
                expected: TurnState::Executing,
                actual,
            });
        }
        let input = self.new_ledger_event_locked(context, event)?;
        let persisted = self.ledger.append(input)?;
        let envelope = EventEnvelope::try_from(persisted)?;
        let _ = self.events.send(envelope.clone());
        Ok(envelope)
    }

    /// Append a set of runtime events in one SQLite transaction and broadcast
    /// them only after the transaction commits.  Callers must already hold
    /// the daemon mutation gate when using this helper.
    fn persist_new_events_locked(
        &self,
        inputs: Vec<NewLedgerEvent>,
    ) -> Result<Vec<EventEnvelope>, TurnRunnerError> {
        let committed = self.ledger.append_batch(inputs)?;
        self.broadcast_events(committed)
    }

    /// Convert committed ledger rows back to protocol envelopes and publish
    /// them in append order.  The ledger is the source of truth; no event is
    /// broadcast before its transaction has committed.
    fn broadcast_events(
        &self,
        committed: Vec<LedgerEvent>,
    ) -> Result<Vec<EventEnvelope>, TurnRunnerError> {
        let mut envelopes = Vec::with_capacity(committed.len());
        for event in committed {
            let envelope = EventEnvelope::try_from(event)?;
            let _ = self.events.send(envelope.clone());
            envelopes.push(envelope);
        }
        Ok(envelopes)
    }

    /// Construct an event with an explicit protocol scope and causation ID.
    /// Settlement uses this instead of borrowing a transient RunContext so
    /// its cleanup events remain bound to the turn's durable owner.
    fn new_scoped_ledger_event_locked(
        &self,
        thread_id: ThreadId,
        turn_id: TurnId,
        agent_id: &AgentId,
        causation_id: &str,
        event: Event,
    ) -> Result<NewLedgerEvent, TurnRunnerError> {
        let serialized = serde_json::to_value(event)?;
        let kind = serialized
            .get("kind")
            .and_then(Value::as_str)
            .expect("serialized protocol Event always has a kind")
            .to_owned();
        let payload = serialized.get("payload").cloned().unwrap_or(Value::Null);
        Ok(NewLedgerEvent {
            schema_version: PROTOCOL_VERSION,
            event_id: self.ids.next_uuid()?.to_string(),
            thread_id: thread_id.to_string(),
            turn_id: Some(turn_id.to_string()),
            agent_id: agent_id.to_string(),
            time: self.clock.now(),
            causation_id: Some(causation_id.to_owned()),
            kind,
            payload,
        })
    }

    fn new_ledger_event_locked(
        &self,
        context: &RunContext,
        event: Event,
    ) -> Result<NewLedgerEvent, TurnRunnerError> {
        let causation_id = CausationId::from(context.request_id).to_string();
        self.new_scoped_ledger_event_locked(
            context.spec.thread_id,
            context.spec.turn_id,
            &context.agent_id,
            &causation_id,
            event,
        )
    }

    fn lock_mutations(&self) -> Result<MutexGuard<'_, ()>, TurnRunnerError> {
        self.mutation_gate
            .lock()
            .map_err(|_| TurnRunnerError::MutationGatePoisoned)
    }

    /// Return the bounded search gate for one canonical workspace identity.
    ///
    /// Gates are retained in a capped map owned by the daemon-wide runner, so
    /// cloned turn runners cannot accidentally create independent per-workspace
    /// slots.  Once the cap is reached, a new workspace fails closed at
    /// scheduling time instead of allowing an unbounded map allocation.
    fn workspace_search_gate(
        &self,
        identity: &WorkspaceIdentity,
    ) -> Result<Option<Arc<Semaphore>>, TurnRunnerError> {
        let key = (identity.canonical_root.clone(), identity.digest.clone());
        let mut gates = self
            .workspace_search_gates
            .lock()
            .map_err(|_| TurnRunnerError::WorkspaceSearchGatePoisoned)?;
        if let Some(gate) = gates.get(&key).and_then(Weak::upgrade) {
            return Ok(Some(gate));
        }
        // A weak entry whose last worker has exited is safe to discard. This
        // keeps the key map bounded in long-lived daemons that visit many
        // workspaces while retaining active gates across cloned runners.
        if gates.contains_key(&key) {
            gates.remove(&key);
        }
        gates.retain(|_, gate| gate.strong_count() > 0);
        if gates.len() >= MAX_WORKSPACE_SEARCH_GATES {
            return Ok(None);
        }
        let gate = Arc::new(Semaphore::new(MAX_CONCURRENT_SEARCHES_PER_WORKSPACE));
        gates.insert(key, Arc::downgrade(&gate));
        Ok(Some(gate))
    }
}

#[derive(Clone, Debug)]
struct PendingToolInvocation {
    invocation_id: InvocationId,
    call: AssembledToolCall,
    tool_version: String,
    normalized_arguments: Value,
    timeout_ms: u64,
    effects: EffectSet,
    prepared: Option<yeux_protocol::PreparedInvocation>,
    preparation_failure: Option<ToolPreparationFailure>,
    scheduler_rejected: bool,
    cancelled_before_start: bool,
    started: bool,
}

#[derive(Clone, Debug)]
struct ToolPreparationFailure {
    output: Value,
    reason: String,
}

struct RunContext {
    spec: TurnRunSpec,
    request_id: ModelRequestId,
    agent_id: AgentId,
    workspace_root: String,
    workspace_id: WorkspaceId,
    workspace_identity: WorkspaceIdentity,
    workspace_trust: WorkspaceTrust,
    turn_override: Option<CapabilityGrant>,
    turn_override_error: Option<String>,
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
        if !matches!(turn.state, TurnState::Accepted | TurnState::Cancelling) {
            return Err(TurnRunnerError::UnexpectedState {
                turn_id: spec.turn_id,
                expected: TurnState::Accepted,
                actual: turn.state,
            });
        }
        let thread =
            projection
                .threads
                .get(&spec.thread_id)
                .ok_or(TurnRunnerError::MissingThread {
                    thread_id: spec.thread_id,
                })?;
        let workspace = projection.workspaces.get(&thread.workspace_id).ok_or(
            TurnRunnerError::MissingWorkspace {
                workspace_id: thread.workspace_id,
            },
        )?;
        let events = load_lineage_events(runner, &projection, spec.thread_id)?;
        let turn_override_value = events
            .iter()
            .filter_map(|envelope| match &envelope.event {
                Event::ItemAdded { item }
                    if item.turn_id == spec.turn_id && item.kind == ItemKind::UserMessage =>
                {
                    item.content.get("capability_override").cloned()
                }
                _ => None,
            })
            .next()
            .filter(|value| !value.is_null());
        let (turn_override, turn_override_error) = match turn_override_value {
            Some(value) => match serde_json::from_value::<CapabilityGrant>(value) {
                Ok(grant) => (Some(grant), None),
                Err(error) => (None, Some(error.to_string())),
            },
            None => (None, None),
        };
        Ok(Self {
            spec,
            request_id,
            agent_id: turn.agent_id.clone(),
            workspace_root: workspace.root.clone(),
            workspace_id: workspace.id,
            workspace_identity: workspace.identity.clone(),
            workspace_trust: workspace.trust,
            turn_override,
            turn_override_error,
            events,
        })
    }
}

fn validate_workspace_identity(
    runtime: &RuntimeWorkspace,
    expected: &WorkspaceIdentity,
) -> Result<(), String> {
    let actual = runtime.identity_snapshot();
    let actual_root = actual.canonical_root().to_string_lossy();
    if actual_root != expected.canonical_root {
        return Err(format!(
            "workspace canonical root changed: expected {}, actual {}",
            expected.canonical_root, actual_root
        ));
    }
    if actual.digest() != expected.digest {
        return Err(format!(
            "workspace identity digest changed: expected {}, actual {}",
            expected.digest,
            actual.digest()
        ));
    }
    if expected.device.is_some() && actual.device() != expected.device {
        return Err(format!(
            "workspace device identity changed: expected {:?}, actual {:?}",
            expected.device,
            actual.device()
        ));
    }
    if expected.inode.is_some() && actual.inode() != expected.inode {
        return Err(format!(
            "workspace inode identity changed: expected {:?}, actual {:?}",
            expected.inode,
            actual.inode()
        ));
    }
    Ok(())
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
        if let Event::TurnSteered { message, .. } = &envelope.event {
            messages.push(ModelMessage {
                role: MessageRole::User,
                content: vec![ContentBlock::Text {
                    text: message.clone(),
                }],
            });
            continue;
        }
        let Event::ItemAdded { item } = &envelope.event else {
            continue;
        };
        let role = match item.kind {
            ItemKind::UserMessage => MessageRole::User,
            ItemKind::AssistantMessage | ItemKind::Reasoning | ItemKind::ToolCall => {
                MessageRole::Assistant
            }
            ItemKind::ToolResult => MessageRole::Tool,
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
        if item.kind == ItemKind::UserMessage
            && content.iter().any(|block| {
                matches!(
                    block,
                    ContentBlock::Reasoning { .. }
                        | ContentBlock::ToolCall { .. }
                        | ContentBlock::ToolResult { .. }
                )
            })
        {
            return Err(TurnRunnerError::InvalidMessageItem {
                item_id: item.id,
                message: "user messages may not contain reasoning or tool blocks".into(),
            });
        }
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
    stop_reason: Option<StopReason>,
    tool_calls: Option<ToolCallAssembler>,
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
            stop_reason: None,
            tool_calls: Some(ToolCallAssembler::default()),
            model_failure: None,
        }
    }

    fn collect_content(&mut self, event: &ModelEvent) -> Result<(), ToolCallAssemblyError> {
        match event {
            ModelEvent::TextDelta { text } => append_text(&mut self.content, false, text),
            ModelEvent::ReasoningDelta { text } => append_text(&mut self.content, true, text),
            ModelEvent::ToolCallDelta { .. } => self
                .tool_calls
                .as_mut()
                .expect("tool calls are finalized only after streaming")
                .push(event)?,
            ModelEvent::Completed { stop_reason } => {
                self.completion_count += 1;
                self.stop_reason = Some(stop_reason.clone());
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
        Ok(())
    }

    fn finish_tool_calls(&mut self) -> Result<Vec<AssembledToolCall>, ToolCallAssemblyError> {
        self.tool_calls
            .take()
            .expect("tool calls are finalized once")
            .finish()
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
                .persist_locked_expected(
                    self.context,
                    TurnState::Streaming,
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
            self.collect_content(&event).map_err(|error| PortError {
                code: "provider_invalid_tool_call".into(),
                message: error.to_string(),
                retryable: false,
            })?;
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

/// Validate the complete provider context before it is persisted as a model
/// request.  Ledger-backed lineage can grow across forks even when each
/// individual command is small, so enforce element, block, byte and rough
/// token ceilings at every round.  The estimate is deliberately conservative
/// (four UTF-8 bytes per token); providers remain responsible for their exact
/// tokenizer limits.
fn validate_model_context(
    messages: &[ModelMessage],
    budget: &TokenBudget,
    provider_context_tokens: u64,
) -> Result<(), String> {
    if messages.len() > MAX_CONTEXT_MESSAGES {
        return Err(format!(
            "model context contains too many messages ({}; maximum {})",
            messages.len(),
            MAX_CONTEXT_MESSAGES
        ));
    }
    let mut blocks = 0usize;
    for message in messages {
        blocks = blocks.saturating_add(message.content.len());
        if blocks > MAX_CONTEXT_BLOCKS {
            return Err(format!(
                "model context contains too many content blocks (maximum {})",
                MAX_CONTEXT_BLOCKS
            ));
        }
        for block in &message.content {
            let text_bytes = match block {
                ContentBlock::Text { text } | ContentBlock::Reasoning { text } => text.len(),
                ContentBlock::Image { media_type, source } => {
                    let source_bytes = match source {
                        yeux_protocol::ImageSource::Url { url }
                        | yeux_protocol::ImageSource::Artifact { uri: url }
                        | yeux_protocol::ImageSource::Base64 { data: url } => url.len(),
                    };
                    media_type.len().saturating_add(source_bytes)
                }
                ContentBlock::ToolCall {
                    call_id,
                    name,
                    arguments,
                } => call_id
                    .len()
                    .saturating_add(name.len())
                    .saturating_add(serde_json::to_vec(arguments).map_or(0, |bytes| bytes.len())),
                ContentBlock::ToolResult {
                    call_id, content, ..
                } => call_id
                    .len()
                    .saturating_add(serde_json::to_vec(content).map_or(0, |bytes| bytes.len())),
            };
            if text_bytes > MAX_CONTEXT_TEXT_BYTES_PER_BLOCK {
                return Err(format!(
                    "a model context block exceeds the {}-byte limit",
                    MAX_CONTEXT_TEXT_BYTES_PER_BLOCK
                ));
            }
        }
    }
    let encoded = serde_json::to_vec(messages).map_err(|error| error.to_string())?;
    if encoded.len() > MAX_CONTEXT_BYTES {
        return Err(format!(
            "serialized model context exceeds the {}-byte limit",
            MAX_CONTEXT_BYTES
        ));
    }
    let estimated_tokens = u64::try_from(encoded.len().saturating_add(3) / 4).unwrap_or(u64::MAX);
    if budget.max_input_tokens > 0 && estimated_tokens > budget.max_input_tokens {
        return Err(format!(
            "estimated model input tokens {} exceed the configured budget {}",
            estimated_tokens, budget.max_input_tokens
        ));
    }
    if provider_context_tokens > 0
        && estimated_tokens.saturating_add(budget.max_output_tokens) > provider_context_tokens
    {
        return Err(format!(
            "estimated input plus output tokens exceed provider context limit {}",
            provider_context_tokens
        ));
    }
    Ok(())
}

enum ToolWorkerWait {
    Completed(Result<Result<Value, ToolRegistryError>, tokio::task::JoinError>),
    Unknown,
}

/// Wait for a blocking workspace worker while giving cancellation a chance to
/// propagate into the runtime's cooperative search matcher.  Tokio cannot
/// forcibly abort a `spawn_blocking` closure; if the worker does not stop
/// within the bounded grace period (or its hard deadline expires), the caller
/// receives `Unknown` and must not claim a terminal external outcome.
async fn wait_for_tool_worker(
    mut handle: tokio::task::JoinHandle<Result<Value, ToolRegistryError>>,
    worker_cancel: Arc<AtomicBool>,
    cancellation: &(dyn CancellationCheck + Send + Sync),
    timeout_ms: u64,
) -> ToolWorkerWait {
    let deadline = tokio::time::sleep(Duration::from_millis(timeout_ms.max(1)));
    tokio::pin!(deadline);
    let mut poll = tokio::time::interval(Duration::from_millis(10));

    loop {
        tokio::select! {
            result = &mut handle => return ToolWorkerWait::Completed(result),
            _ = &mut deadline => {
                worker_cancel.store(true, Ordering::Release);
                return ToolWorkerWait::Unknown;
            }
            _ = poll.tick() => {
                if cancellation.is_cancelled() {
                    worker_cancel.store(true, Ordering::Release);
                    // A cooperative search normally observes the flag within
                    // one control interval. Give it a short, bounded grace
                    // window so known read outcomes can still be recorded.
                    return match timeout(Duration::from_millis(100), &mut handle).await {
                        Ok(result) => ToolWorkerWait::Completed(result),
                        Err(_) => ToolWorkerWait::Unknown,
                    };
                }
            }
        }
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

/// Convert a Tokio worker join failure into the conservative outcome shape
/// consumed by the result-integrating loop.
///
/// `JoinError` does not establish whether the blocking closure reached the
/// external workspace before it panicked/was aborted.  Treating it as a
/// terminal `Failed` invocation could therefore authorize an unsafe retry.
/// Keep both the model-visible diagnostic and the durable marker reason
/// bounded because panic payloads are attacker-controlled strings.
fn classify_tool_worker_join_error(
    error: &tokio::task::JoinError,
) -> (
    Value,
    bool,
    Option<InvocationState>,
    InvocationState,
    Option<String>,
) {
    let detail = bounded_message(&error.to_string());
    let reason = bounded_message(&format!(
        "workspace tool worker terminated before its outcome could be verified: {detail}"
    ));
    (
        json!({
            "code": "tool_outcome_unknown",
            "message": reason.clone(),
        }),
        true,
        None,
        InvocationState::Started,
        Some(reason),
    )
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque, future::Future, path::Path, pin::Pin, sync::Mutex as TestMutex,
    };

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
        capabilities: ProviderCapabilities,
    }

    impl FauxProvider {
        fn succeeds(events: Vec<ModelEvent>) -> Self {
            Self {
                events,
                error: None,
                requests: Arc::new(TestMutex::new(Vec::new())),
                capabilities: ProviderCapabilities::default(),
            }
        }

        fn fails(error: PortError) -> Self {
            Self {
                events: Vec::new(),
                error: Some(error),
                requests: Arc::new(TestMutex::new(Vec::new())),
                capabilities: ProviderCapabilities::default(),
            }
        }
    }

    impl ModelProvider for FauxProvider {
        fn provider_id(&self) -> &str {
            "faux"
        }

        fn capabilities(&self) -> ProviderCapabilities {
            self.capabilities.clone()
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

    #[derive(Clone)]
    struct ScriptedProvider {
        rounds: Arc<TestMutex<VecDeque<Vec<ModelEvent>>>>,
        requests: Arc<TestMutex<Vec<ModelRequest>>>,
    }

    impl ScriptedProvider {
        fn new(rounds: Vec<Vec<ModelEvent>>) -> Self {
            Self {
                rounds: Arc::new(TestMutex::new(rounds.into())),
                requests: Arc::new(TestMutex::new(Vec::new())),
            }
        }
    }

    impl ModelProvider for ScriptedProvider {
        fn provider_id(&self) -> &str {
            "scripted"
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                tool_calls: true,
                parallel_tool_calls: true,
                ..ProviderCapabilities::default()
            }
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
                let events = self.rounds.lock().unwrap().pop_front().ok_or(PortError {
                    code: "script_exhausted".into(),
                    message: "scripted provider has no response for this round".into(),
                    retryable: false,
                })?;
                for event in events {
                    sink.emit(event).await?;
                }
                Ok(())
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
        fixture_at(provider, Path::new("/workspace"))
    }

    fn fixture_at(provider: Option<ModelProviderConfig>, workspace_root: &Path) -> Fixture {
        let ledger = Arc::new(EventLedger::open_in_memory().unwrap());
        let (events, _) = broadcast::channel(64);
        let now = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let workspace_id = WorkspaceId::from_uuid(Uuid::now_v7());
        let thread_id = ThreadId::from_uuid(Uuid::now_v7());
        let turn_id = TurnId::from_uuid(Uuid::now_v7());
        let agent_id = AgentId::new("root");
        let fixture_identity = RuntimeWorkspace::open(workspace_root)
            .ok()
            .map(|runtime| {
                let snapshot = runtime.identity_snapshot();
                WorkspaceIdentity {
                    canonical_root: snapshot.canonical_root().to_string_lossy().into_owned(),
                    digest: snapshot.digest().to_owned(),
                    device: snapshot.device(),
                    inode: snapshot.inode(),
                    git_common_dir: None,
                }
            })
            .unwrap_or_else(|| WorkspaceIdentity {
                canonical_root: workspace_root.to_string_lossy().into_owned(),
                digest: "fixture".into(),
                device: None,
                inode: None,
                git_common_dir: None,
            });
        append_seed(
            &ledger,
            thread_id,
            None,
            &agent_id,
            now,
            Event::WorkspaceOpened {
                workspace: Workspace {
                    id: workspace_id,
                    root: workspace_root.to_string_lossy().into_owned(),
                    identity: fixture_identity,
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
    async fn panicking_tool_worker_join_is_classified_as_bounded_unknown() {
        // `spawn_blocking` catches a panic as JoinError.  The worker may have
        // touched the workspace before that panic, so the runner must not
        // turn this into a terminal Failed invocation.
        let handle = tokio::task::spawn_blocking(|| -> Result<Value, ToolRegistryError> {
            panic!("injected worker panic");
        });
        let waited = wait_for_tool_worker(
            handle,
            Arc::new(AtomicBool::new(false)),
            &NeverCancelled,
            1_000,
        )
        .await;
        let ToolWorkerWait::Completed(Err(join_error)) = waited else {
            panic!("expected a completed JoinError from the injected worker");
        };

        let (output, is_error, terminal_state, from_state, reason) =
            classify_tool_worker_join_error(&join_error);
        assert!(is_error);
        assert_eq!(terminal_state, None);
        assert_eq!(from_state, InvocationState::Started);
        assert_eq!(output["code"], "tool_outcome_unknown");
        assert!(output["message"].as_str().unwrap().len() <= 4_096);
        assert!(reason.unwrap().len() <= 4_096);
    }

    #[test]
    fn runner_workspace_identity_binding_rejects_a_changed_digest() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = RuntimeWorkspace::open(directory.path()).unwrap();
        let snapshot = runtime.identity_snapshot();
        let expected = WorkspaceIdentity {
            canonical_root: snapshot.canonical_root().to_string_lossy().into_owned(),
            digest: "stale-identity".into(),
            device: snapshot.device(),
            inode: snapshot.inode(),
            git_common_dir: None,
        };
        let error = validate_workspace_identity(&runtime, &expected).unwrap_err();
        assert!(error.contains("identity digest changed"));
    }

    #[test]
    fn workspace_search_gate_is_keyed_by_canonical_root_and_digest() {
        let fixture = fixture(None);
        let first = WorkspaceIdentity {
            canonical_root: "/workspace/project".into(),
            digest: "digest-a".into(),
            device: None,
            inode: None,
            git_common_dir: None,
        };
        let same = first.clone();
        let changed_digest = WorkspaceIdentity {
            digest: "digest-b".into(),
            ..first.clone()
        };

        let first_gate = fixture
            .runner
            .workspace_search_gate(&first)
            .unwrap()
            .unwrap();
        let held = first_gate.try_acquire_owned().unwrap();
        let same_gate = fixture
            .runner
            .workspace_search_gate(&same)
            .unwrap()
            .unwrap();
        assert!(same_gate.try_acquire().is_err());

        let distinct_gate = fixture
            .runner
            .workspace_search_gate(&changed_digest)
            .unwrap()
            .unwrap();
        assert!(distinct_gate.try_acquire().is_ok());
        drop(held);
        assert!(same_gate.try_acquire().is_ok());
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
    async fn runner_closes_a_durable_cancelling_turn_before_startup_work() {
        let fixture = fixture(None);
        append_seed(
            &fixture.ledger,
            fixture.thread_id,
            Some(fixture.turn_id),
            &AgentId::new("root"),
            DateTime::from_timestamp(1_700_000_001, 0).unwrap(),
            Event::TurnStateChanged {
                turn_id: fixture.turn_id,
                from: TurnState::Accepted,
                to: TurnState::Cancelling,
                reason: Some("interrupt arrived before runner launch".into()),
            },
        );

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
        assert!(matches!(result, TurnRunResult::Cancelled { .. }));
        assert_eq!(
            fixture.ledger.project_core().unwrap().turns[&fixture.turn_id].state,
            TurnState::Cancelled
        );
    }

    #[test]
    fn prepared_invocation_cannot_cross_execution_boundary_after_cancellation() {
        let fixture = fixture(None);
        let invocation_id = InvocationId::from_uuid(Uuid::now_v7());
        let agent_id = AgentId::new("root");
        let effects = EffectSet::default();
        let idempotency = effects.idempotency;
        let now = DateTime::from_timestamp(1_700_000_001, 0).unwrap();

        append_seed(
            &fixture.ledger,
            fixture.thread_id,
            Some(fixture.turn_id),
            &agent_id,
            now,
            Event::InvocationProposed {
                invocation_id,
                call_id: "prepared-cancel-call".into(),
                tool_id: "workspace.read".into(),
                tool_version: "1".into(),
                normalized_arguments_digest: digest_value(&json!({"path": "file.txt"})),
                effect_digest: digest_value(&serde_json::to_value(&effects).unwrap()),
                effects,
                idempotency,
            },
        );
        for (from, to) in [
            (InvocationState::Proposed, InvocationState::Approved),
            (InvocationState::Approved, InvocationState::Prepared),
        ] {
            append_seed(
                &fixture.ledger,
                fixture.thread_id,
                Some(fixture.turn_id),
                &agent_id,
                now,
                Event::InvocationStateChanged {
                    invocation_id,
                    from,
                    to,
                    reason: None,
                },
            );
        }
        append_seed(
            &fixture.ledger,
            fixture.thread_id,
            Some(fixture.turn_id),
            &agent_id,
            now,
            Event::TurnStateChanged {
                turn_id: fixture.turn_id,
                from: TurnState::Accepted,
                to: TurnState::Cancelling,
                reason: Some("interrupt won before worker start".into()),
            },
        );

        let context = RunContext::load(
            &fixture.runner,
            TurnRunSpec {
                thread_id: fixture.thread_id,
                turn_id: fixture.turn_id,
            },
            ModelRequestId::from_uuid(Uuid::now_v7()),
        )
        .unwrap();
        let error = fixture
            .runner
            .persist_invocation_transition(
                &context,
                invocation_id,
                InvocationState::Prepared,
                InvocationState::Started,
                None,
            )
            .unwrap_err();
        assert!(matches!(
            error,
            TurnRunnerError::UnexpectedState {
                expected: TurnState::Executing,
                actual: TurnState::Cancelling,
                ..
            }
        ));
        assert_eq!(
            fixture.ledger.project_core().unwrap().invocations[&invocation_id].state,
            InvocationState::Prepared
        );
    }

    #[test]
    fn fail_current_refuses_to_terminalize_with_started_invocation() {
        let fixture = fixture(None);
        let invocation_id = InvocationId::from_uuid(Uuid::now_v7());
        let agent_id = AgentId::new("root");
        let effects = EffectSet::default();
        let idempotency = effects.idempotency;
        let now = DateTime::from_timestamp(1_700_000_001, 0).unwrap();

        append_seed(
            &fixture.ledger,
            fixture.thread_id,
            Some(fixture.turn_id),
            &agent_id,
            now,
            Event::InvocationProposed {
                invocation_id,
                call_id: "started-fail-current-call".into(),
                tool_id: "workspace.read".into(),
                tool_version: "1".into(),
                normalized_arguments_digest: digest_value(&json!({"path": "file.txt"})),
                effect_digest: digest_value(&serde_json::to_value(&effects).unwrap()),
                effects,
                idempotency,
            },
        );
        for (from, to) in [
            (InvocationState::Proposed, InvocationState::Approved),
            (InvocationState::Approved, InvocationState::Prepared),
            (InvocationState::Prepared, InvocationState::Started),
        ] {
            append_seed(
                &fixture.ledger,
                fixture.thread_id,
                Some(fixture.turn_id),
                &agent_id,
                now,
                Event::InvocationStateChanged {
                    invocation_id,
                    from,
                    to,
                    reason: None,
                },
            );
        }
        append_seed(
            &fixture.ledger,
            fixture.thread_id,
            Some(fixture.turn_id),
            &agent_id,
            now,
            Event::TurnStateChanged {
                turn_id: fixture.turn_id,
                from: TurnState::Accepted,
                to: TurnState::Cancelling,
                reason: Some("interrupt while worker outcome was pending".into()),
            },
        );

        let context = RunContext::load(
            &fixture.runner,
            TurnRunSpec {
                thread_id: fixture.thread_id,
                turn_id: fixture.turn_id,
            },
            ModelRequestId::from_uuid(Uuid::now_v7()),
        )
        .unwrap();
        let error = fixture
            .runner
            .fail_current(
                &context,
                "injected_failure",
                "failure must not orphan started work",
            )
            .unwrap_err();
        assert!(matches!(
            error,
            TurnRunnerError::UnexpectedState {
                expected: TurnState::Executing,
                actual: TurnState::Cancelling,
                ..
            }
        ));
        let projection = fixture.ledger.project_core().unwrap();
        assert_eq!(
            projection.turns[&fixture.turn_id].state,
            TurnState::Cancelling
        );
        assert_eq!(
            projection.invocations[&invocation_id].state,
            InvocationState::Started
        );
        assert!(!fixture
            .ledger
            .replay(&fixture.thread_id.to_string(), 0)
            .unwrap()
            .iter()
            .any(|event| event.kind == "runtime/diagnostic"
                && event.payload["code"] == "injected_failure"));
    }

    #[tokio::test]
    async fn cancellation_settlement_pairs_prepared_invocation_before_cancelled_turn() {
        let fixture = fixture(None);
        let invocation_id = InvocationId::from_uuid(Uuid::now_v7());
        let agent_id = AgentId::new("root");
        let effects = EffectSet::default();
        let idempotency = effects.idempotency;
        append_seed(
            &fixture.ledger,
            fixture.thread_id,
            Some(fixture.turn_id),
            &agent_id,
            DateTime::from_timestamp(1_700_000_001, 0).unwrap(),
            Event::ItemAdded {
                item: Item {
                    id: ItemId::from_uuid(Uuid::now_v7()),
                    thread_id: fixture.thread_id,
                    turn_id: fixture.turn_id,
                    agent_id: agent_id.clone(),
                    kind: ItemKind::ToolCall,
                    content: json!({
                        "content": [ContentBlock::ToolCall {
                            call_id: "cancel-call".into(),
                            name: "workspace.read".into(),
                            arguments: json!({"path": "file.txt"}),
                        }],
                        "invocation_ids": [invocation_id.to_string()],
                    }),
                    created_at: DateTime::from_timestamp(1_700_000_001, 0).unwrap(),
                },
            },
        );
        append_seed(
            &fixture.ledger,
            fixture.thread_id,
            Some(fixture.turn_id),
            &agent_id,
            DateTime::from_timestamp(1_700_000_001, 0).unwrap(),
            Event::InvocationProposed {
                invocation_id,
                call_id: "cancel-call".into(),
                tool_id: "workspace.read".into(),
                tool_version: "1".into(),
                normalized_arguments_digest: digest_value(&json!({"path": "file.txt"})),
                effect_digest: digest_value(&serde_json::to_value(&effects).unwrap()),
                effects,
                idempotency,
            },
        );
        append_seed(
            &fixture.ledger,
            fixture.thread_id,
            Some(fixture.turn_id),
            &agent_id,
            DateTime::from_timestamp(1_700_000_002, 0).unwrap(),
            Event::TurnStateChanged {
                turn_id: fixture.turn_id,
                from: TurnState::Accepted,
                to: TurnState::Cancelling,
                reason: Some("race during proposal persistence".into()),
            },
        );

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
        assert!(matches!(result, TurnRunResult::Cancelled { .. }));

        let projection = fixture.ledger.project_core().unwrap();
        assert_eq!(
            projection.invocations[&invocation_id].state,
            InvocationState::Failed
        );
        assert!(projection.items.values().any(|item| {
            item.kind == ItemKind::ToolResult
                && item.content["invocation_id"] == invocation_id.to_string()
        }));
        assert_eq!(
            projection.turns[&fixture.turn_id].state,
            TurnState::Cancelled
        );
    }

    #[tokio::test]
    async fn cancellation_settlement_marks_started_invocation_unknown_and_fails_turn() {
        let fixture = fixture(None);
        let invocation_id = InvocationId::from_uuid(Uuid::now_v7());
        let agent_id = AgentId::new("root");
        let effects = EffectSet::default();
        let idempotency = effects.idempotency;
        append_seed(
            &fixture.ledger,
            fixture.thread_id,
            Some(fixture.turn_id),
            &agent_id,
            DateTime::from_timestamp(1_700_000_001, 0).unwrap(),
            Event::InvocationProposed {
                invocation_id,
                call_id: "started-call".into(),
                tool_id: "workspace.read".into(),
                tool_version: "1".into(),
                normalized_arguments_digest: digest_value(&json!({"path": "file.txt"})),
                effect_digest: digest_value(&serde_json::to_value(&effects).unwrap()),
                effects,
                idempotency,
            },
        );
        for (from, to) in [
            (InvocationState::Proposed, InvocationState::Approved),
            (InvocationState::Approved, InvocationState::Prepared),
            (InvocationState::Prepared, InvocationState::Started),
        ] {
            append_seed(
                &fixture.ledger,
                fixture.thread_id,
                Some(fixture.turn_id),
                &agent_id,
                DateTime::from_timestamp(1_700_000_001, 0).unwrap(),
                Event::InvocationStateChanged {
                    invocation_id,
                    from,
                    to,
                    reason: None,
                },
            );
        }
        append_seed(
            &fixture.ledger,
            fixture.thread_id,
            Some(fixture.turn_id),
            &agent_id,
            DateTime::from_timestamp(1_700_000_002, 0).unwrap(),
            Event::TurnStateChanged {
                turn_id: fixture.turn_id,
                from: TurnState::Accepted,
                to: TurnState::Cancelling,
                reason: Some("race after execution started".into()),
            },
        );

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
            TurnRunResult::Failed { ref code, .. }
                if code == "turn_cancellation_requires_reconciliation"
        ));

        let projection = fixture.ledger.project_core().unwrap();
        assert_eq!(
            projection.invocations[&invocation_id].state,
            InvocationState::Unknown
        );
        assert!(projection.items.values().any(|item| {
            item.kind == ItemKind::ToolResult
                && item.content["invocation_id"] == invocation_id.to_string()
        }));
        assert_eq!(projection.turns[&fixture.turn_id].state, TurnState::Failed);
        assert!(fixture
            .ledger
            .replay(&fixture.thread_id.to_string(), 0)
            .unwrap()
            .iter()
            .any(|event| event.kind == "runtime/diagnostic"
                && event.payload["code"] == "turn_cancellation_requires_reconciliation"));
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
    async fn unadvertised_tool_use_is_persisted_but_never_executed() {
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
            TurnRunResult::Failed { ref code, .. } if code == "unadvertised_tool_use"
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

    #[tokio::test]
    async fn executes_parallel_read_tools_and_integrates_results_in_model_order() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("a.txt"), "alpha").unwrap();
        std::fs::write(workspace.path().join("b.txt"), "beta").unwrap();
        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                ModelEvent::ToolCallDelta {
                    call_id: "call-b".into(),
                    name: "workspace.read".into(),
                    json_delta: "{\"path\":\"b.txt\"}".into(),
                },
                ModelEvent::ToolCallDelta {
                    call_id: "call-a".into(),
                    name: "workspace.read".into(),
                    json_delta: "{\"path\":\"a.txt\"}".into(),
                },
                ModelEvent::Completed {
                    stop_reason: StopReason::ToolUse,
                },
            ],
            vec![
                ModelEvent::TextDelta {
                    text: "read both files".into(),
                },
                ModelEvent::Completed {
                    stop_reason: StopReason::EndTurn,
                },
            ],
        ]));
        let fixture = fixture_at(Some(provider_config(provider.clone())), workspace.path());

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
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let advertised = requests[0]
            .tools
            .iter()
            .map(|tool| tool.id.as_str())
            .collect::<Vec<_>>();
        for tool in ["workspace.list", "workspace.read", "workspace.search"] {
            assert!(
                advertised.contains(&tool),
                "expected {tool} among advertised tools {advertised:?}"
            );
        }
        let tool_results = requests[1]
            .messages
            .iter()
            .filter(|message| message.role == MessageRole::Tool)
            .flat_map(|message| message.content.iter())
            .filter_map(|block| match block {
                ContentBlock::ToolResult {
                    call_id, content, ..
                } => Some((call_id.as_str(), content["content"].as_str().unwrap())),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(tool_results, [("call-b", "beta"), ("call-a", "alpha")]);

        let projection = fixture.ledger.project_core().unwrap();
        assert_eq!(projection.invocations.len(), 2);
        assert!(projection
            .invocations
            .values()
            .all(|invocation| invocation.state == InvocationState::Completed));
        let mut effect_paths = projection
            .invocations
            .values()
            .flat_map(|invocation| &invocation.effects.filesystem_read)
            .map(|scope| (scope.path.as_str(), scope.resolved))
            .collect::<Vec<_>>();
        effect_paths.sort_unstable();
        assert_eq!(effect_paths, [("a.txt", true), ("b.txt", true)]);
        assert_eq!(
            projection.turns[&fixture.turn_id].state,
            TurnState::Completed
        );
        assert!(projection.items.values().any(|item| {
            item.kind == ItemKind::AssistantMessage
                && item.content["content"][0]["text"] == "read both files"
        }));
    }

    #[tokio::test]
    async fn search_operation_budget_is_shared_across_model_rounds() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("file.txt"), "abc").unwrap();
        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                ModelEvent::ToolCallDelta {
                    call_id: "search-one".into(),
                    name: WORKSPACE_SEARCH_TOOL_ID.into(),
                    json_delta: "{\"query\":\"z\"}".into(),
                },
                ModelEvent::Completed {
                    stop_reason: StopReason::ToolUse,
                },
            ],
            vec![
                ModelEvent::ToolCallDelta {
                    call_id: "search-two".into(),
                    name: WORKSPACE_SEARCH_TOOL_ID.into(),
                    json_delta: "{\"query\":\"z\"}".into(),
                },
                ModelEvent::Completed {
                    stop_reason: StopReason::ToolUse,
                },
            ],
            vec![ModelEvent::Completed {
                stop_reason: StopReason::EndTurn,
            }],
        ]));
        let config = provider_config(provider.clone()).with_loop_limits(AgentLoopLimits {
            max_model_rounds: 3,
            max_tool_calls: 2,
            max_tool_result_bytes: 64 * 1024,
            // The first three-byte scan succeeds; the second round can only
            // charge two more operations and must return the stable budget
            // error instead of resetting at the model-round boundary.
            max_search_operations: 5,
        });
        let fixture = fixture_at(Some(config), workspace.path());

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
        let second = projection
            .invocations
            .values()
            .find(|invocation| invocation.call_id == "search-two")
            .unwrap();
        assert_eq!(second.state, InvocationState::Failed);
        assert!(projection.items.values().any(|item| {
            item.kind == ItemKind::ToolResult
                && item.content["content"][0]["content"]["code"]
                    == "workspace_search_budget_exceeded"
        }));
    }

    #[tokio::test]
    async fn mixed_valid_and_invalid_tools_preserve_result_order_without_starting_invalid_call() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("a.txt"), "alpha").unwrap();
        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                ModelEvent::ToolCallDelta {
                    call_id: "call-missing".into(),
                    name: "workspace.read".into(),
                    json_delta: "{\"path\":\"missing.txt\"}".into(),
                },
                ModelEvent::ToolCallDelta {
                    call_id: "call-a".into(),
                    name: "workspace.read".into(),
                    json_delta: "{\"path\":\"a.txt\"}".into(),
                },
                ModelEvent::Completed {
                    stop_reason: StopReason::ToolUse,
                },
            ],
            vec![ModelEvent::Completed {
                stop_reason: StopReason::EndTurn,
            }],
        ]));
        let fixture = fixture_at(Some(provider_config(provider.clone())), workspace.path());

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
        let requests = provider.requests.lock().unwrap();
        let results = requests[1]
            .messages
            .iter()
            .filter(|message| message.role == MessageRole::Tool)
            .flat_map(|message| message.content.iter())
            .filter_map(|block| match block {
                ContentBlock::ToolResult {
                    call_id,
                    content,
                    is_error,
                } => Some((call_id.as_str(), *is_error, content.clone())),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, "call-missing");
        assert!(results[0].1);
        assert_eq!(results[0].2["code"], "workspace_not_found");
        assert_eq!(results[1].0, "call-a");
        assert!(!results[1].1);
        assert_eq!(results[1].2["content"], "alpha");
        drop(requests);

        let projection = fixture.ledger.project_core().unwrap();
        let invalid_id = projection
            .items
            .values()
            .find(|item| item.kind == ItemKind::ToolCall)
            .and_then(|item| item.content["invocation_ids"][0].as_str())
            .unwrap()
            .parse::<InvocationId>()
            .unwrap();
        let invalid = &projection.invocations[&invalid_id];
        assert_eq!(invalid.state, InvocationState::Failed);
        assert_eq!(invalid.effects.filesystem_read.len(), 1);
        assert!(!invalid.effects.filesystem_read[0].resolved);
        let events = fixture
            .ledger
            .replay(&fixture.thread_id.to_string(), 0)
            .unwrap()
            .into_iter()
            .map(EventEnvelope::try_from)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(!events.iter().any(|envelope| matches!(
            envelope.event,
            Event::InvocationStateChanged {
                invocation_id,
                to: InvocationState::Approved | InvocationState::Prepared | InvocationState::Started,
                ..
            } if invocation_id == invalid_id
        )));
    }

    #[tokio::test]
    async fn tool_result_budget_failure_terminalizes_every_started_invocation() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("a.txt"), "alpha").unwrap();
        std::fs::write(workspace.path().join("b.txt"), "beta").unwrap();
        let provider = Arc::new(ScriptedProvider::new(vec![vec![
            ModelEvent::ToolCallDelta {
                call_id: "call-a".into(),
                name: "workspace.read".into(),
                json_delta: "{\"path\":\"a.txt\"}".into(),
            },
            ModelEvent::ToolCallDelta {
                call_id: "call-b".into(),
                name: "workspace.read".into(),
                json_delta: "{\"path\":\"b.txt\"}".into(),
            },
            ModelEvent::Completed {
                stop_reason: StopReason::ToolUse,
            },
        ]]));
        let config = provider_config(provider).with_loop_limits(AgentLoopLimits {
            max_model_rounds: 2,
            max_tool_calls: 2,
            max_tool_result_bytes: 1,
            max_search_operations: 1_000,
        });
        let fixture = fixture_at(Some(config), workspace.path());

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
            TurnRunResult::Failed { ref code, .. }
                if code == "agent_loop_tool_result_limit"
        ));
        let projection = fixture.ledger.project_core().unwrap();
        assert_eq!(projection.invocations.len(), 2);
        // The invocation whose result crossed the aggregate budget is a
        // durable Failed outcome; sibling workers that were already started
        // but whose result was intentionally not awaited are conservatively
        // Unknown rather than being misreported as Failed.
        assert!(projection.invocations.values().all(|invocation| matches!(
            invocation.state,
            InvocationState::Failed | InvocationState::Unknown
        )));
        assert_eq!(projection.turns[&fixture.turn_id].state, TurnState::Failed);
    }

    #[tokio::test]
    async fn tool_result_budget_failure_settles_preexecution_siblings() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("a.txt"), "alpha").unwrap();
        let provider = Arc::new(ScriptedProvider::new(vec![vec![
            ModelEvent::ToolCallDelta {
                call_id: "call-missing".into(),
                name: "workspace.read".into(),
                json_delta: "{\"path\":\"missing.txt\"}".into(),
            },
            ModelEvent::ToolCallDelta {
                call_id: "call-a".into(),
                name: "workspace.read".into(),
                json_delta: "{\"path\":\"a.txt\"}".into(),
            },
            ModelEvent::Completed {
                stop_reason: StopReason::ToolUse,
            },
        ]]));
        let config = provider_config(provider).with_loop_limits(AgentLoopLimits {
            max_model_rounds: 1,
            max_tool_calls: 2,
            max_tool_result_bytes: 1,
            max_search_operations: 1_000,
        });
        let fixture = fixture_at(Some(config), workspace.path());

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
            TurnRunResult::Failed { ref code, .. }
                if code == "agent_loop_tool_result_limit"
        ));

        let projection = fixture.ledger.project_core().unwrap();
        let missing = projection
            .invocations
            .values()
            .find(|invocation| invocation.call_id == "call-missing")
            .unwrap();
        assert_eq!(missing.state, InvocationState::Failed);
        assert!(projection.items.values().any(|item| {
            item.kind == ItemKind::ToolResult
                && item.content["invocation_id"] == missing.invocation_id.to_string()
        }));
        let valid = projection
            .invocations
            .values()
            .find(|invocation| invocation.call_id == "call-a")
            .unwrap();
        assert_eq!(valid.state, InvocationState::Unknown);
        assert_eq!(projection.turns[&fixture.turn_id].state, TurnState::Failed);
    }

    #[tokio::test]
    async fn unknown_tool_is_returned_as_an_error_without_side_effects() {
        let workspace = tempfile::tempdir().unwrap();
        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                ModelEvent::ToolCallDelta {
                    call_id: "call-shell".into(),
                    name: "shell".into(),
                    json_delta: "{\"command\":\"touch should-not-exist\"}".into(),
                },
                ModelEvent::Completed {
                    stop_reason: StopReason::ToolUse,
                },
            ],
            vec![ModelEvent::Completed {
                stop_reason: StopReason::EndTurn,
            }],
        ]));
        let fixture = fixture_at(Some(provider_config(provider.clone())), workspace.path());

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
        assert!(!workspace.path().join("should-not-exist").exists());
        let projection = fixture.ledger.project_core().unwrap();
        let invocation = projection.invocations.values().next().unwrap();
        assert_eq!(invocation.state, InvocationState::Failed);
        assert!(invocation.effects.filesystem_read.is_empty());
        let requests = provider.requests.lock().unwrap();
        let error = requests[1]
            .messages
            .iter()
            .flat_map(|message| message.content.iter())
            .find_map(|block| match block {
                ContentBlock::ToolResult {
                    content,
                    is_error: true,
                    ..
                } => Some(content),
                _ => None,
            })
            .unwrap();
        assert_eq!(error["code"], "workspace_unknown_tool");
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
