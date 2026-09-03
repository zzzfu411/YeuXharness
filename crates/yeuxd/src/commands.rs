use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use uuid::Version;
use yeux_protocol::{
    method, AcceptedResult, AgentId, CapabilityMode, ClientInfo, ContentBlock, Event,
    EventEnvelope, InitializeParams, InitializeResult, InvocationReconcileParams,
    InvocationReconcileResult, InvocationReconciliationEvidence, InvocationReconciliationOutcome,
    InvocationState, Item, ItemId, ItemKind, JobCreateParams, JobIdParams, JobListParams,
    JobListResult, JobResult, JobState, McpServerStatus, McpStatusParams, McpStatusResult,
    ModelDescriptor, ModelListParams, ModelListResult, PluginDescriptor, PluginListParams,
    PluginListResult, RpcError, ServerCapabilities, SkillDescriptor, SkillListParams,
    SkillListResult, Thread, ThreadArchiveParams, ThreadForkParams, ThreadId, ThreadListParams,
    ThreadListResult, ThreadReadParams, ThreadReadResult, ThreadResult, ThreadResumeParams,
    ThreadStartParams, ThreadStatus, ThreadSubscribeParams, ThreadSubscribeResult, Turn, TurnId,
    TurnInterruptParams, TurnResult, TurnStartParams, TurnState, TurnSteerParams, Workspace,
    WorkspaceId, WorkspaceIdentity, WorkspaceOpenParams, WorkspaceOpenResult,
    WorkspaceStatusParams, WorkspaceStatusResult, WorkspaceTrust, WorkspaceTrustParams,
    WorkspaceTrustResult, PROTOCOL_VERSION,
};
use yeux_runtime::{
    descriptors::DescriptorKind, NewCommandReceipt, NewInvocationOutcome, NewLedgerEvent,
    RegisteredDescriptor, SandboxBackend, SandboxRequirement, WorkspaceIdentitySnapshot,
};

use crate::runner::TurnRunSpec;
use crate::server::{
    CommandOutcome, Daemon, ReplayWindow, RpcFault, FEATURE_UNAVAILABLE, INVALID_STATE,
    MAX_PAGE_SIZE, NOT_FOUND,
};

/// Ingress budgets keep untrusted client content from growing the ledger or
/// provider context without a corresponding scheduler decision.  These are
/// hard ceilings; future configuration may only narrow them.
const MAX_TURN_CONTENT_BLOCKS: usize = 128;
const MAX_TURN_CONTENT_BYTES: usize = 256 * 1024;
const MAX_TURN_TEXT_BYTES: usize = 64 * 1024;
const MAX_TURN_STEER_BYTES: usize = 64 * 1024;
const MAX_THREAD_TITLE_BYTES: usize = 512;
const MAX_RECONCILIATION_SUMMARY_BYTES: usize = 4 * 1024;
const MAX_RECONCILIATION_ARTIFACT_URI_BYTES: usize = 2 * 1024;
const OPERATOR_RECONCILIATION_SOURCE: &str = "operator_review";

impl Daemon {
    pub(crate) fn dispatch(
        &self,
        method_name: &str,
        command_id: yeux_protocol::CommandId,
        params: Value,
        params_digest: &str,
    ) -> Result<CommandOutcome, RpcFault> {
        let result = match method_name {
            method::INITIALIZE => self.initialize(decode(params)?)?,
            method::WORKSPACE_OPEN => {
                self.workspace_open(command_id, params_digest, decode(params)?)?
            }
            method::WORKSPACE_TRUST => {
                self.workspace_trust(command_id, params_digest, decode(params)?)?
            }
            method::WORKSPACE_STATUS => self.workspace_status(decode(params)?)?,
            method::THREAD_START => {
                self.thread_start(command_id, params_digest, decode(params)?)?
            }
            method::THREAD_RESUME => self.thread_resume(decode(params)?)?,
            method::THREAD_FORK => self.thread_fork(command_id, params_digest, decode(params)?)?,
            method::THREAD_READ => self.thread_read(decode(params)?)?,
            method::THREAD_LIST => self.thread_list(decode(params)?)?,
            method::THREAD_ARCHIVE => {
                self.thread_archive(command_id, params_digest, decode(params)?)?
            }
            method::THREAD_COMPACT => {
                return Err(RpcFault::new(
                    FEATURE_UNAVAILABLE,
                    "thread/compact requires the M3 checkpoint summarizer",
                ));
            }
            method::THREAD_SUBSCRIBE => return self.thread_subscribe(decode(params)?),
            method::TURN_START => {
                let (result, turn_run) =
                    self.turn_start(command_id, params_digest, decode(params)?)?;
                return Ok(CommandOutcome {
                    result,
                    turn_run: Some(turn_run),
                    ..CommandOutcome::default()
                });
            }
            method::TURN_STEER => self.turn_steer(command_id, params_digest, decode(params)?)?,
            method::TURN_INTERRUPT => {
                self.turn_interrupt(command_id, params_digest, decode(params)?)?
            }
            method::INVOCATION_RECONCILE => {
                self.invocation_reconcile(command_id, params_digest, decode(params)?)?
            }
            method::MODEL_LIST => self.model_list(decode(params)?)?,
            method::SKILL_LIST => self.skill_list(decode(params)?)?,
            method::MCP_STATUS => self.mcp_status(decode(params)?)?,
            method::PLUGIN_LIST => self.plugin_list(decode(params)?)?,
            method::JOB_CREATE => self.job_create(command_id, params_digest, decode(params)?)?,
            method::JOB_LIST => self.job_list(decode(params)?)?,
            method::JOB_PAUSE => {
                self.job_transition(command_id, params_digest, decode(params)?, JobState::Paused)?
            }
            method::JOB_RESUME => {
                self.job_transition(command_id, params_digest, decode(params)?, JobState::Active)?
            }
            method::JOB_RUN => {
                return Err(RpcFault::new(
                    FEATURE_UNAVAILABLE,
                    "job/run requires the M4 scheduler",
                ));
            }
            _ => {
                return Err(RpcFault::new(
                    RpcError::METHOD_NOT_FOUND,
                    format!("unknown method: {method_name}"),
                ));
            }
        };
        Ok(CommandOutcome {
            result,
            ..CommandOutcome::default()
        })
    }

    fn initialize(&self, params: InitializeParams) -> Result<Value, RpcFault> {
        if !PROTOCOL_VERSION.accepts(params.protocol_version) {
            return Err(RpcFault::new(
                RpcError::INCOMPATIBLE_PROTOCOL,
                format!(
                    "protocol {}.{} is incompatible with server {}.{}",
                    params.protocol_version.major,
                    params.protocol_version.minor,
                    PROTOCOL_VERSION.major,
                    PROTOCOL_VERSION.minor
                ),
            ));
        }
        let sandbox = SandboxBackend::detect();
        // Keep mutation and arbitrary process capabilities independent. The
        // structured revision-bound writer does not spawn a child and only
        // needs filesystem/network policy evidence; a process tool additionally
        // requires strict descendant containment. On macOS Seatbelt can satisfy
        // the former while deliberately failing closed for the latter.
        let write_sandbox_ready = sandbox
            .ensure(SandboxRequirement {
                filesystem_isolation: true,
                process_isolation: false,
                network_isolation: true,
                allow_workspace_write: true,
                allow_network: false,
            })
            .is_ok();
        let process_sandbox_ready = sandbox
            .ensure(SandboxRequirement {
                filesystem_isolation: true,
                process_isolation: true,
                network_isolation: true,
                allow_workspace_write: false,
                allow_network: false,
            })
            .is_ok();
        let provider_tools_ready = self
            .inner
            .config
            .model_provider
            .as_ref()
            .is_some_and(|selection| selection.provider.capabilities().tool_calls);
        let write_tools_ready = write_sandbox_ready
            && provider_tools_ready
            && self.inner.config.host_ceiling != CapabilityMode::Observe;
        let process_tools_ready = process_sandbox_ready
            && provider_tools_ready
            && self.inner.config.host_ceiling != CapabilityMode::Observe;
        encode(InitializeResult {
            protocol_version: PROTOCOL_VERSION,
            server_info: ClientInfo {
                name: "yeuxd".to_owned(),
                version: env!("CARGO_PKG_VERSION").to_owned(),
            },
            capabilities: ServerCapabilities {
                unix_socket: cfg!(unix),
                // Job persistence (create/list/pause/resume) exists as a
                // descriptor surface, but the scheduler and `job/run` are
                // intentionally unavailable until M4.  Advertising the
                // capability before that authority path is live causes
                // clients to assume background execution is safe.
                jobs: false,
                subagents: false,
                // plugin-host is an experimental standalone process and is
                // not connected to the daemon registry/policy/ledger path.
                plugins: false,
                write_tools: write_tools_ready,
                process_tools: process_tools_ready,
                sandbox: Some(sandbox.name().to_owned()),
            },
            host_ceiling: self.inner.config.host_ceiling,
        })
    }

    fn workspace_open(
        &self,
        command_id: yeux_protocol::CommandId,
        params_digest: &str,
        params: WorkspaceOpenParams,
    ) -> Result<Value, RpcFault> {
        let runtime = yeux_runtime::Workspace::open(&params.path).map_err(RpcFault::internal)?;
        let root = runtime.root().to_string_lossy().into_owned();
        let projection = self.projection()?;
        if let Some(workspace) = projection.workspaces.values().find(|workspace| {
            workspace.root == root && workspace.identity.digest == runtime.identity()
        }) {
            return encode(WorkspaceOpenResult {
                workspace: workspace.clone(),
            });
        }

        let workspace_id = WorkspaceId::from_uuid(self.next_uuid()?);
        let now = self.inner.clock.now();
        // Keep every persisted identity field tied to the same filesystem
        // observation used by `Workspace::open`.  Re-statting `root` here
        // would create a mixed digest/device/inode tuple across a replacement
        // race and turn a safe conflict into an ambiguous record.
        let identity_snapshot = runtime.identity_snapshot();
        let workspace = Workspace {
            id: workspace_id,
            root,
            identity: workspace_identity(&identity_snapshot),
            trust: WorkspaceTrust::Untrusted,
            opened_at: now,
        };
        let response = encode(WorkspaceOpenResult {
            workspace: workspace.clone(),
        })?;
        let event = self.new_event(
            ThreadId::from_uuid(workspace_id.into_uuid()),
            None,
            AgentId::new("root"),
            command_id,
            now,
            Event::WorkspaceOpened { workspace },
        )?;
        self.commit_events(
            command_id,
            method::WORKSPACE_OPEN,
            params_digest,
            response,
            vec![event],
        )
    }

    fn workspace_trust(
        &self,
        command_id: yeux_protocol::CommandId,
        params_digest: &str,
        params: WorkspaceTrustParams,
    ) -> Result<Value, RpcFault> {
        let projection = self.projection()?;
        let workspace = projection
            .workspaces
            .get(&params.workspace_id)
            .ok_or_else(|| not_found("workspace", params.workspace_id))?;
        let runtime = yeux_runtime::Workspace::open(&workspace.root).map_err(RpcFault::internal)?;
        if params.identity_digest != workspace.identity.digest
            || runtime.identity() != workspace.identity.digest
        {
            return Err(RpcFault::new(
                RpcError::COMMAND_CONFLICT,
                "workspace identity changed; reopen it before changing trust",
            ));
        }
        let mut workspace = workspace.clone();
        if workspace.trust != params.trust {
            let now = self.inner.clock.now();
            workspace.trust = params.trust;
            let response = encode(WorkspaceTrustResult {
                workspace: workspace.clone(),
            })?;
            let event = self.new_event(
                ThreadId::from_uuid(params.workspace_id.into_uuid()),
                None,
                AgentId::new("root"),
                command_id,
                now,
                Event::WorkspaceTrustChanged {
                    workspace_id: params.workspace_id,
                    trust: params.trust,
                },
            )?;
            return self.commit_events(
                command_id,
                method::WORKSPACE_TRUST,
                params_digest,
                response,
                vec![event],
            );
        }
        encode(WorkspaceTrustResult { workspace })
    }

    fn workspace_status(&self, params: WorkspaceStatusParams) -> Result<Value, RpcFault> {
        let projection = self.projection()?;
        let workspace = projection
            .workspaces
            .get(&params.workspace_id)
            .cloned()
            .ok_or_else(|| not_found("workspace", params.workspace_id))?;
        let active_thread_id = projection
            .threads
            .values()
            .find(|thread| {
                thread.workspace_id == params.workspace_id && thread.status == ThreadStatus::Active
            })
            .map(|thread| thread.id);
        encode(WorkspaceStatusResult {
            workspace,
            active_thread_id,
        })
    }

    fn thread_start(
        &self,
        command_id: yeux_protocol::CommandId,
        params_digest: &str,
        params: ThreadStartParams,
    ) -> Result<Value, RpcFault> {
        validate_optional_bounded_text(
            params.title.as_deref(),
            "thread title",
            MAX_THREAD_TITLE_BYTES,
        )?;
        if !self
            .projection()?
            .workspaces
            .contains_key(&params.workspace_id)
        {
            return Err(not_found("workspace", params.workspace_id));
        }
        let now = self.inner.clock.now();
        let thread_id = ThreadId::from_uuid(self.next_uuid()?);
        let thread = Thread {
            id: thread_id,
            workspace_id: params.workspace_id,
            parent_thread_id: None,
            parent_seq: None,
            title: params.title,
            status: ThreadStatus::Idle,
            created_at: now,
            updated_at: now,
            last_seq: 0,
        };
        let response_thread = Thread {
            last_seq: 1,
            ..thread.clone()
        };
        let response = encode(ThreadResult {
            thread: response_thread,
        })?;
        let event = self.new_event(
            thread_id,
            None,
            params.agent_id.unwrap_or_else(|| AgentId::new("root")),
            command_id,
            now,
            Event::ThreadStarted { thread },
        )?;
        self.commit_events(
            command_id,
            method::THREAD_START,
            params_digest,
            response,
            vec![event],
        )
    }

    fn thread_resume(&self, params: ThreadResumeParams) -> Result<Value, RpcFault> {
        encode(self.read_thread(params.thread_id, params.after_seq, MAX_PAGE_SIZE)?)
    }

    fn thread_fork(
        &self,
        command_id: yeux_protocol::CommandId,
        params_digest: &str,
        params: ThreadForkParams,
    ) -> Result<Value, RpcFault> {
        validate_optional_bounded_text(
            params.title.as_deref(),
            "thread title",
            MAX_THREAD_TITLE_BYTES,
        )?;
        let projection = self.projection()?;
        let parent = projection
            .threads
            .get(&params.thread_id)
            .ok_or_else(|| not_found("thread", params.thread_id))?;
        if params.at_seq > parent.last_seq {
            return Err(RpcFault::new(
                RpcError::INVALID_PARAMS,
                "atSeq is beyond the parent thread",
            ));
        }
        let unresolved: Vec<String> = projection
            .invocations
            .values()
            .filter(|invocation| {
                invocation.thread_id == parent.id && invocation.state == InvocationState::Unknown
            })
            .map(|invocation| invocation.invocation_id.to_string())
            .collect();
        if !unresolved.is_empty() {
            return Err(RpcFault::new(
                INVALID_STATE,
                "reconcile unknown invocation outcomes before forking a thread",
            )
            .with_data(json!({
                "invocationIds": unresolved,
                "recoverable": true,
                "action": method::INVOCATION_RECONCILE,
            })));
        }
        let now = self.inner.clock.now();
        let child_id = ThreadId::from_uuid(self.next_uuid()?);
        let child = Thread {
            id: child_id,
            workspace_id: parent.workspace_id,
            parent_thread_id: Some(parent.id),
            parent_seq: Some(params.at_seq),
            title: params.title,
            status: ThreadStatus::Idle,
            created_at: now,
            updated_at: now,
            last_seq: 0,
        };
        let response_thread = Thread {
            last_seq: 1,
            ..child.clone()
        };
        let response = encode(ThreadResult {
            thread: response_thread,
        })?;
        let event = self.new_event(
            child_id,
            None,
            AgentId::new("root"),
            command_id,
            now,
            Event::ThreadForked { thread: child },
        )?;
        self.commit_events(
            command_id,
            method::THREAD_FORK,
            params_digest,
            response,
            vec![event],
        )
    }

    fn thread_read(&self, params: ThreadReadParams) -> Result<Value, RpcFault> {
        validate_limit(params.limit)?;
        encode(self.read_thread(params.thread_id, params.after_seq, params.limit)?)
    }

    fn thread_list(&self, params: ThreadListParams) -> Result<Value, RpcFault> {
        validate_limit(params.limit)?;
        let offset = params
            .cursor
            .as_deref()
            .unwrap_or("0")
            .parse::<usize>()
            .map_err(|_| RpcFault::new(RpcError::INVALID_PARAMS, "invalid cursor"))?;
        let projection = self.projection()?;
        let mut threads: Vec<_> = projection
            .threads
            .values()
            .filter(|thread| {
                params
                    .workspace_id
                    .is_none_or(|workspace_id| thread.workspace_id == workspace_id)
                    && (params.include_archived || thread.status != ThreadStatus::Archived)
            })
            .cloned()
            .collect();
        threads.sort_by(|left, right| {
            right
                .updated_at
                .cmp(&left.updated_at)
                .then_with(|| left.id.cmp(&right.id))
        });
        let has_more = offset.saturating_add(params.limit as usize) < threads.len();
        let threads = threads
            .into_iter()
            .skip(offset)
            .take(params.limit as usize)
            .collect();
        encode(ThreadListResult {
            threads,
            next_cursor: has_more.then(|| (offset + params.limit as usize).to_string()),
        })
    }

    fn thread_archive(
        &self,
        command_id: yeux_protocol::CommandId,
        params_digest: &str,
        params: ThreadArchiveParams,
    ) -> Result<Value, RpcFault> {
        let projection = self.projection()?;
        let thread = projection
            .threads
            .get(&params.thread_id)
            .ok_or_else(|| not_found("thread", params.thread_id))?;
        if projection.active_turn(params.thread_id).is_some() {
            return Err(RpcFault::new(
                INVALID_STATE,
                "interrupt the active turn before archiving the thread",
            ));
        }
        if thread.status != ThreadStatus::Archived {
            let now = self.inner.clock.now();
            let mut archived = thread.clone();
            archived.status = ThreadStatus::Archived;
            archived.updated_at = now;
            archived.last_seq += 1;
            let response = encode(ThreadResult { thread: archived })?;
            let event = self.new_event(
                params.thread_id,
                None,
                AgentId::new("root"),
                command_id,
                now,
                Event::ThreadArchived {
                    thread_id: params.thread_id,
                },
            )?;
            return self.commit_events(
                command_id,
                method::THREAD_ARCHIVE,
                params_digest,
                response,
                vec![event],
            );
        }
        encode(ThreadResult {
            thread: thread.clone(),
        })
    }

    fn thread_subscribe(&self, params: ThreadSubscribeParams) -> Result<CommandOutcome, RpcFault> {
        let thread = self.projected_thread(params.thread_id)?;
        if params.after_seq > thread.last_seq {
            return Err(RpcFault::new(
                RpcError::INVALID_PARAMS,
                "afterSeq is beyond the end of the thread",
            ));
        }
        let through = thread.last_seq;
        Ok(CommandOutcome {
            result: encode(ThreadSubscribeResult {
                subscription_id: self.next_uuid()?.to_string(),
                replayed_through_seq: through,
            })?,
            replay: Some(ReplayWindow {
                thread_id: params.thread_id,
                after_seq: params.after_seq,
                through_seq: through,
            }),
            subscription: Some((params.thread_id, through)),
            turn_run: None,
        })
    }

    fn turn_start(
        &self,
        command_id: yeux_protocol::CommandId,
        params_digest: &str,
        params: TurnStartParams,
    ) -> Result<(Value, TurnRunSpec), RpcFault> {
        validate_turn_content(&params.content)?;
        if let Some(override_grant) = &params.capability_override {
            if override_grant.mode > self.inner.config.host_ceiling {
                return Err(RpcFault::new(
                    RpcError::INVALID_PARAMS,
                    "capability override exceeds the daemon host ceiling",
                ));
            }
            // A client-provided override is a narrowing hint only.  It is
            // persisted as input evidence and is never treated as an
            // authority grant by the runner.
        }
        let projection = self.projection()?;
        let thread = projection
            .threads
            .get(&params.thread_id)
            .ok_or_else(|| not_found("thread", params.thread_id))?;
        if thread.status == ThreadStatus::Archived {
            return Err(RpcFault::new(INVALID_STATE, "thread is archived"));
        }
        if let Some(active) = projection.active_turn(params.thread_id) {
            return Err(RpcFault::new(
                INVALID_STATE,
                format!("thread already has active turn {}", active.id),
            ));
        }
        let unresolved: Vec<String> = projection
            .invocations
            .values()
            .filter(|invocation| {
                invocation.thread_id == params.thread_id
                    && invocation.state == InvocationState::Unknown
            })
            .map(|invocation| invocation.invocation_id.to_string())
            .collect();
        if !unresolved.is_empty() {
            return Err(RpcFault::new(
                INVALID_STATE,
                "thread has invocation outcomes requiring reconciliation",
            )
            .with_data(json!({
                "invocationIds": unresolved,
                "recoverable": true,
                "action": method::INVOCATION_RECONCILE,
            })));
        }

        let now = self.inner.clock.now();
        let agent_id = params.agent_id.unwrap_or_else(|| AgentId::new("root"));
        let turn_id = TurnId::from_uuid(self.next_uuid()?);
        let turn = Turn {
            id: turn_id,
            thread_id: params.thread_id,
            agent_id: agent_id.clone(),
            state: TurnState::Accepted,
            started_at: now,
            ended_at: None,
            failure: None,
        };
        let started = self.new_event(
            params.thread_id,
            Some(turn_id),
            agent_id.clone(),
            command_id,
            now,
            Event::TurnStarted { turn: turn.clone() },
        )?;
        let item = Item {
            id: ItemId::from_uuid(self.next_uuid()?),
            thread_id: params.thread_id,
            turn_id,
            agent_id: agent_id.clone(),
            kind: ItemKind::UserMessage,
            content: json!({
                "content": params.content,
                "capability_override": params.capability_override,
            }),
            created_at: now,
        };
        let added = self.new_event(
            params.thread_id,
            Some(turn_id),
            agent_id,
            command_id,
            now,
            Event::ItemAdded { item },
        )?;
        let response = encode(TurnResult { turn })?;
        let result = self.commit_events(
            command_id,
            method::TURN_START,
            params_digest,
            response,
            vec![started, added],
        )?;
        Ok((
            result,
            TurnRunSpec {
                thread_id: params.thread_id,
                turn_id,
            },
        ))
    }

    fn turn_steer(
        &self,
        command_id: yeux_protocol::CommandId,
        params_digest: &str,
        params: TurnSteerParams,
    ) -> Result<Value, RpcFault> {
        if params.message.len() > MAX_TURN_STEER_BYTES {
            return Err(RpcFault::new(
                RpcError::INVALID_PARAMS,
                format!("steer message exceeds {MAX_TURN_STEER_BYTES}-byte limit"),
            ));
        }
        let projection = self.projection()?;
        let turn = projection
            .turns
            .get(&params.turn_id)
            .ok_or_else(|| not_found("turn", params.turn_id))?;
        if turn.thread_id != params.thread_id || turn.state.is_terminal() {
            return Err(RpcFault::new(
                INVALID_STATE,
                "turn is not active on this thread",
            ));
        }
        let now = self.inner.clock.now();
        let event = self.new_event(
            params.thread_id,
            Some(params.turn_id),
            turn.agent_id.clone(),
            command_id,
            now,
            Event::TurnSteered {
                turn_id: params.turn_id,
                message: params.message,
            },
        )?;
        let response = encode(AcceptedResult { accepted: true })?;
        self.commit_events(
            command_id,
            method::TURN_STEER,
            params_digest,
            response,
            vec![event],
        )
    }

    fn turn_interrupt(
        &self,
        command_id: yeux_protocol::CommandId,
        params_digest: &str,
        params: TurnInterruptParams,
    ) -> Result<Value, RpcFault> {
        let projection = self.projection()?;
        let turn = projection
            .turns
            .get(&params.turn_id)
            .ok_or_else(|| not_found("turn", params.turn_id))?;
        if turn.thread_id != params.thread_id {
            return Err(RpcFault::new(
                INVALID_STATE,
                "turn belongs to another thread",
            ));
        }
        if turn.state.is_terminal() {
            return encode(AcceptedResult { accepted: false });
        }
        let agent_id = turn.agent_id.clone();
        let now = self.inner.clock.now();
        let mut events = Vec::with_capacity(2);
        if turn.state != TurnState::Cancelling {
            events.push(self.new_event(
                params.thread_id,
                Some(params.turn_id),
                agent_id.clone(),
                command_id,
                now,
                Event::TurnStateChanged {
                    turn_id: params.turn_id,
                    from: turn.state,
                    to: TurnState::Cancelling,
                    reason: params.reason.clone(),
                },
            )?);
        }
        if !self.inner.config.executes_turns() {
            // With no background runner there is no execution boundary that
            // could still be active, so the control-plane command can close
            // Cancelling -> Cancelled in the same receipt transaction.  In
            // normal daemon mode the runner must observe the durable
            // Cancelling state first; it will choose Cancelled only after
            // proving that no tool outcome is unknown.
            events.push(self.new_event(
                params.thread_id,
                Some(params.turn_id),
                agent_id,
                command_id,
                now,
                Event::TurnStateChanged {
                    turn_id: params.turn_id,
                    from: TurnState::Cancelling,
                    to: TurnState::Cancelled,
                    reason: params.reason,
                },
            )?);
        }
        let response = encode(AcceptedResult { accepted: true })?;
        if events.is_empty() {
            // A repeated interrupt while the runner is already observing
            // `Cancelling` has no new state event to append.  Keep the
            // command idempotent by recording a receipt-only response rather
            // than sending an empty event batch to the ledger.
            let receipt = self
                .inner
                .ledger
                .record_command_receipt(NewCommandReceipt {
                    command_id: command_id.to_string(),
                    method: method::TURN_INTERRUPT.to_owned(),
                    params_digest: params_digest.to_owned(),
                    response,
                    created_at: self.inner.clock.now(),
                })
                .map_err(RpcFault::internal)?;
            self.request_turn_cancel(params.turn_id);
            return Ok(receipt.response);
        }
        let result = self.commit_events(
            command_id,
            method::TURN_INTERRUPT,
            params_digest,
            response,
            events,
        )?;
        // Propagate cancellation only after its durable state transition and
        // receipt commit. The caller still holds the shared mutation gate, so
        // the provider sink cannot persist a residual delta in between.
        self.request_turn_cancel(params.turn_id);
        Ok(result)
    }

    /// Resolve a durable `Unknown` invocation using explicit operator
    /// evidence. This control-plane command never calls the original tool or
    /// provider again; it only appends a typed `tool/reconciled` event and its
    /// model-visible ToolResult in one ledger transaction.
    fn invocation_reconcile(
        &self,
        command_id: yeux_protocol::CommandId,
        params_digest: &str,
        params: InvocationReconcileParams,
    ) -> Result<Value, RpcFault> {
        validate_reconciliation_evidence(&params.evidence)?;
        // An artifact URI is evidence only when it names a durable, verified
        // object in this daemon's content-addressed store.  Do this check
        // before reading the projection or allocating event IDs so a bad
        // reference cannot partially mutate reconciliation state.
        if let Some(uri) = params.evidence.artifact_uri.as_deref() {
            self.inner.artifacts.verify_uri(uri).map_err(|_| {
                RpcFault::new(
                    RpcError::INVALID_PARAMS,
                    "reconciliation artifact is unavailable or corrupt",
                )
                .with_data(json!({ "code": "artifact_invalid" }))
            })?;
        }
        let projection = self.projection()?;
        let invocation = projection
            .invocations
            .get(&params.invocation_id)
            .ok_or_else(|| not_found("invocation", params.invocation_id))?;
        if invocation.thread_id != params.thread_id {
            return Err(RpcFault::new(
                RpcError::INVALID_PARAMS,
                "invocation does not belong to thread",
            ));
        }
        if invocation.state != InvocationState::Unknown {
            return Err(RpcFault::new(
                INVALID_STATE,
                format!(
                    "invocation is {:?}; only unknown invocations can be reconciled",
                    invocation.state
                ),
            ));
        }
        let turn = projection
            .turns
            .get(&invocation.turn_id)
            .ok_or_else(|| not_found("turn", invocation.turn_id))?;
        if !turn.state.is_terminal() {
            return Err(RpcFault::new(
                INVALID_STATE,
                "parent turn must settle before reconciling its invocation",
            ));
        }

        let now = self.inner.clock.now();
        let result_content = json!({
            "reconciled": true,
            "outcome": params.outcome,
            "evidence": params.evidence.clone(),
            "execution_retried": false,
        });
        let item = Item {
            id: ItemId::from_uuid(self.next_uuid()?),
            thread_id: invocation.thread_id,
            turn_id: invocation.turn_id,
            agent_id: invocation.agent_id.clone(),
            kind: ItemKind::ToolResult,
            content: json!({
                "content": [ContentBlock::ToolResult {
                    call_id: invocation.call_id.clone(),
                    content: result_content,
                    is_error: params.outcome == InvocationReconciliationOutcome::Failed,
                }],
                "invocation_id": invocation.invocation_id,
            }),
            created_at: now,
        };
        let tool_result = self.new_event(
            invocation.thread_id,
            Some(invocation.turn_id),
            invocation.agent_id.clone(),
            command_id,
            now,
            Event::ItemAdded { item },
        )?;
        let terminal_state = self.new_event(
            invocation.thread_id,
            Some(invocation.turn_id),
            invocation.agent_id.clone(),
            command_id,
            now,
            Event::InvocationReconciled {
                invocation_id: invocation.invocation_id,
                outcome: params.outcome,
                evidence: params.evidence.clone(),
            },
        )?;
        let response = encode(InvocationReconcileResult {
            thread_id: invocation.thread_id,
            invocation_id: invocation.invocation_id,
            state: params.outcome.state(),
            evidence: params.evidence,
        })?;
        let committed = self
            .inner
            .ledger
            .append_invocation_reconciliation_with_receipt(
                NewInvocationOutcome {
                    tool_result,
                    terminal_state,
                },
                NewCommandReceipt {
                    command_id: command_id.to_string(),
                    method: method::INVOCATION_RECONCILE.to_owned(),
                    params_digest: params_digest.to_owned(),
                    response,
                    created_at: now,
                },
            )
            .map_err(|error| match error {
                yeux_runtime::ledger::LedgerError::InvocationStateConflict { .. } => RpcFault::new(
                    INVALID_STATE,
                    "invocation changed before reconciliation could be committed",
                ),
                yeux_runtime::ledger::LedgerError::InvalidInvocationOutcome(message) => {
                    RpcFault::new(RpcError::INVALID_PARAMS, message)
                }
                other => RpcFault::internal(other),
            })?;
        for event in committed.events {
            let envelope = EventEnvelope::try_from(event).map_err(RpcFault::internal)?;
            let _ = self.inner.events.send(envelope);
        }
        Ok(committed.response)
    }

    fn model_list(&self, params: ModelListParams) -> Result<Value, RpcFault> {
        let mut models: Vec<ModelDescriptor> = self
            .descriptors(DescriptorKind::Provider)?
            .into_iter()
            .filter_map(|descriptor| serde_json::from_value(descriptor.descriptor).ok())
            .filter(|model: &ModelDescriptor| {
                params
                    .provider
                    .as_deref()
                    .is_none_or(|provider| model.provider == provider)
            })
            .collect();
        if let Some(configured) = self.inner.configured_model.clone().filter(|model| {
            params
                .provider
                .as_deref()
                .is_none_or(|provider| model.provider == provider)
        }) {
            if !models.iter().any(|model| {
                model.provider == configured.provider && model.model == configured.model
            }) {
                models.push(configured);
            }
        }
        models.sort_by(|left, right| {
            left.provider
                .cmp(&right.provider)
                .then_with(|| left.model.cmp(&right.model))
        });
        encode(ModelListResult { models })
    }

    fn skill_list(&self, _: SkillListParams) -> Result<Value, RpcFault> {
        let skills = self
            .descriptors(DescriptorKind::Skill)?
            .into_iter()
            .map(|descriptor| {
                let name =
                    string_field(&descriptor, "name").unwrap_or_else(|| descriptor.id.clone());
                let description = string_field(&descriptor, "description").unwrap_or_default();
                let source = string_field(&descriptor, "source").unwrap_or_default();
                SkillDescriptor {
                    id: descriptor.id,
                    name,
                    description,
                    source,
                    content_digest: descriptor.source_digest,
                    trusted: descriptor.enabled,
                }
            })
            .collect();
        encode(SkillListResult { skills })
    }

    fn mcp_status(&self, _: McpStatusParams) -> Result<Value, RpcFault> {
        let servers = self
            .descriptors(DescriptorKind::McpServer)?
            .into_iter()
            .map(|descriptor| {
                let transport =
                    string_field(&descriptor, "transport").unwrap_or_else(|| "unknown".to_owned());
                let discovered_tool_count = descriptor
                    .descriptor
                    .get("discoveredToolCount")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as u32;
                McpServerStatus {
                    id: descriptor.id,
                    transport,
                    state: if descriptor.enabled {
                        "idle"
                    } else {
                        "disabled"
                    }
                    .into(),
                    discovered_tool_count,
                }
            })
            .collect();
        encode(McpStatusResult { servers })
    }

    fn plugin_list(&self, _: PluginListParams) -> Result<Value, RpcFault> {
        let plugins = self
            .descriptors(DescriptorKind::Plugin)?
            .into_iter()
            .map(|descriptor| {
                let capabilities = descriptor
                    .descriptor
                    .get("capabilities")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect();
                PluginDescriptor {
                    id: descriptor.id,
                    version: descriptor.version,
                    content_digest: descriptor.source_digest,
                    state: if descriptor.enabled {
                        "available"
                    } else {
                        "disabled"
                    }
                    .into(),
                    capabilities,
                }
            })
            .collect();
        encode(PluginListResult { plugins })
    }

    fn job_create(
        &self,
        command_id: yeux_protocol::CommandId,
        params_digest: &str,
        params: JobCreateParams,
    ) -> Result<Value, RpcFault> {
        if params.job.id.into_uuid().get_version() != Some(Version::SortRand) {
            return Err(RpcFault::new(
                RpcError::INVALID_PARAMS,
                "job id must be UUIDv7",
            ));
        }
        let projection = self.projection()?;
        let workspace = projection
            .workspaces
            .get(&params.job.workspace_id)
            .ok_or_else(|| not_found("workspace", params.job.workspace_id))?;
        if projection.jobs.contains_key(&params.job.id) {
            return Err(RpcFault::new(
                RpcError::COMMAND_CONFLICT,
                "job already exists",
            ));
        }
        if params.job.grant.mode > self.inner.config.host_ceiling
            || (workspace.trust == WorkspaceTrust::Untrusted
                && params.job.grant.mode > CapabilityMode::Observe)
        {
            return Err(RpcFault::new(
                INVALID_STATE,
                "job grant exceeds the host ceiling or project trust",
            ));
        }
        let now = self.inner.clock.now();
        let response = encode(JobResult {
            job: params.job.clone(),
            state: JobState::Active,
        })?;
        let event = self.new_event(
            ThreadId::from_uuid(params.job.workspace_id.into_uuid()),
            None,
            AgentId::new("scheduler"),
            command_id,
            now,
            Event::JobCreated { job: params.job },
        )?;
        self.commit_events(
            command_id,
            method::JOB_CREATE,
            params_digest,
            response,
            vec![event],
        )
    }

    fn job_list(&self, params: JobListParams) -> Result<Value, RpcFault> {
        let projection = self.projection()?;
        let mut jobs: Vec<_> = projection
            .jobs
            .values()
            .filter(|job| {
                params
                    .workspace_id
                    .is_none_or(|workspace_id| job.spec.workspace_id == workspace_id)
            })
            .map(|job| JobResult {
                job: job.spec.clone(),
                state: job.state,
            })
            .collect();
        jobs.sort_by_key(|job| job.job.id);
        encode(JobListResult { jobs })
    }

    fn job_transition(
        &self,
        command_id: yeux_protocol::CommandId,
        params_digest: &str,
        params: JobIdParams,
        to: JobState,
    ) -> Result<Value, RpcFault> {
        let projection = self.projection()?;
        let job = projection
            .jobs
            .get(&params.job_id)
            .ok_or_else(|| not_found("job", params.job_id))?;
        if !matches!(
            (job.state, to),
            (JobState::Active, JobState::Paused) | (JobState::Paused, JobState::Active)
        ) {
            return Err(RpcFault::new(INVALID_STATE, "invalid job state transition"));
        }
        let spec = job.spec.clone();
        let now = self.inner.clock.now();
        let response = encode(JobResult {
            job: spec.clone(),
            state: to,
        })?;
        let event = self.new_event(
            ThreadId::from_uuid(spec.workspace_id.into_uuid()),
            None,
            AgentId::new("scheduler"),
            command_id,
            now,
            Event::JobStateChanged {
                job_id: params.job_id,
                from: job.state,
                to,
            },
        )?;
        let method_name = if to == JobState::Paused {
            method::JOB_PAUSE
        } else {
            method::JOB_RESUME
        };
        self.commit_events(
            command_id,
            method_name,
            params_digest,
            response,
            vec![event],
        )
    }

    fn new_event(
        &self,
        thread_id: ThreadId,
        turn_id: Option<TurnId>,
        agent_id: AgentId,
        command_id: yeux_protocol::CommandId,
        time: DateTime<Utc>,
        event: Event,
    ) -> Result<NewLedgerEvent, RpcFault> {
        let serialized = serde_json::to_value(&event).map_err(RpcFault::internal)?;
        let kind = serialized
            .get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcFault::internal("event kind is missing"))?
            .to_owned();
        let payload = serialized.get("payload").cloned().unwrap_or(Value::Null);
        Ok(NewLedgerEvent {
            schema_version: PROTOCOL_VERSION,
            event_id: self.next_uuid()?.to_string(),
            thread_id: thread_id.to_string(),
            turn_id: turn_id.map(|id| id.to_string()),
            agent_id: agent_id.to_string(),
            time,
            causation_id: Some(command_id.to_string()),
            kind,
            payload,
        })
    }

    fn commit_events(
        &self,
        command_id: yeux_protocol::CommandId,
        method_name: &str,
        params_digest: &str,
        response: Value,
        events: Vec<NewLedgerEvent>,
    ) -> Result<Value, RpcFault> {
        let committed = self
            .inner
            .ledger
            .append_batch_with_receipt(
                events,
                NewCommandReceipt {
                    command_id: command_id.to_string(),
                    method: method_name.to_owned(),
                    params_digest: params_digest.to_owned(),
                    response,
                    created_at: self.inner.clock.now(),
                },
            )
            .map_err(RpcFault::internal)?;
        for event in committed.events {
            let envelope = EventEnvelope::try_from(event).map_err(RpcFault::internal)?;
            let _ = self.inner.events.send(envelope);
        }
        Ok(committed.response)
    }

    pub(crate) fn projection(&self) -> Result<yeux_core::Projection, RpcFault> {
        self.inner.ledger.project_core().map_err(RpcFault::internal)
    }

    fn projected_thread(&self, thread_id: ThreadId) -> Result<Thread, RpcFault> {
        self.projection()?
            .threads
            .get(&thread_id)
            .cloned()
            .ok_or_else(|| not_found("thread", thread_id))
    }

    fn events_page_after(
        &self,
        thread_id: ThreadId,
        after_seq: u64,
        limit: usize,
    ) -> Result<Vec<EventEnvelope>, RpcFault> {
        self.inner
            .ledger
            .replay_page(&thread_id.to_string(), after_seq, limit)
            .map_err(RpcFault::internal)?
            .into_iter()
            .map(EventEnvelope::try_from)
            .collect::<Result<_, _>>()
            .map_err(RpcFault::internal)
    }

    fn read_thread(
        &self,
        thread_id: ThreadId,
        after_seq: u64,
        limit: u32,
    ) -> Result<ThreadReadResult, RpcFault> {
        let thread = self.projected_thread(thread_id)?;
        if after_seq > thread.last_seq {
            return Err(RpcFault::new(
                RpcError::INVALID_PARAMS,
                "afterSeq is beyond the end of the thread",
            ));
        }
        let query_limit = (limit as usize).saturating_add(1);
        let mut events = self.events_page_after(thread_id, after_seq, query_limit)?;
        let next_after_seq =
            (events.len() > limit as usize).then(|| events[limit as usize - 1].seq);
        events.truncate(limit as usize);
        Ok(ThreadReadResult {
            thread,
            events,
            next_after_seq,
        })
    }

    fn descriptors(&self, kind: DescriptorKind) -> Result<Vec<RegisteredDescriptor>, RpcFault> {
        self.inner
            .descriptors
            .descriptors(kind)
            .map_err(RpcFault::internal)
    }

    fn next_uuid(&self) -> Result<uuid::Uuid, RpcFault> {
        self.inner.ids.next_uuid().map_err(RpcFault::internal)
    }
}

fn validate_turn_content(content: &[yeux_protocol::ContentBlock]) -> Result<(), RpcFault> {
    if content.is_empty() {
        return Err(RpcFault::new(
            RpcError::INVALID_PARAMS,
            "turn content must contain at least one block",
        ));
    }
    if content.len() > MAX_TURN_CONTENT_BLOCKS {
        return Err(RpcFault::new(
            RpcError::INVALID_PARAMS,
            format!("turn content exceeds {MAX_TURN_CONTENT_BLOCKS}-block limit"),
        ));
    }
    let mut total = 0usize;
    for block in content {
        let (kind, bytes) = match block {
            yeux_protocol::ContentBlock::Text { text } => ("text", text.len()),
            yeux_protocol::ContentBlock::Image { media_type, source } => {
                let source_bytes = match source {
                    yeux_protocol::ImageSource::Url { url }
                    | yeux_protocol::ImageSource::Artifact { uri: url }
                    | yeux_protocol::ImageSource::Base64 { data: url } => url.len(),
                };
                ("image", media_type.len().saturating_add(source_bytes))
            }
            // Tool calls/results and reasoning are daemon-generated events;
            // accepting them from turn/start would let a client forge model
            // lineage or tool history.  They may still arrive from a
            // provider through the runner's validated path.
            yeux_protocol::ContentBlock::Reasoning { .. }
            | yeux_protocol::ContentBlock::ToolCall { .. }
            | yeux_protocol::ContentBlock::ToolResult { .. } => {
                return Err(RpcFault::new(
                    RpcError::INVALID_PARAMS,
                    "turn content may contain only text or image blocks",
                ));
            }
        };
        if bytes > MAX_TURN_TEXT_BYTES {
            return Err(RpcFault::new(
                RpcError::INVALID_PARAMS,
                format!("{kind} block exceeds {MAX_TURN_TEXT_BYTES}-byte limit"),
            ));
        }
        total = total.saturating_add(bytes);
        if total > MAX_TURN_CONTENT_BYTES {
            return Err(RpcFault::new(
                RpcError::INVALID_PARAMS,
                format!("turn content exceeds {MAX_TURN_CONTENT_BYTES}-byte limit"),
            ));
        }
    }
    Ok(())
}

fn validate_optional_bounded_text(
    value: Option<&str>,
    label: &str,
    limit: usize,
) -> Result<(), RpcFault> {
    if value.is_some_and(|text| text.len() > limit) {
        return Err(RpcFault::new(
            RpcError::INVALID_PARAMS,
            format!("{label} exceeds {limit}-byte limit"),
        ));
    }
    Ok(())
}

fn validate_reconciliation_evidence(
    evidence: &InvocationReconciliationEvidence,
) -> Result<(), RpcFault> {
    // The first public reconciliation surface is deliberately operator-only.
    // Runtime receipt lookups will get a separate authority path once they
    // can prove the external state; accepting arbitrary client-supplied source
    // labels would make an assertion look like machine-verified evidence.
    if evidence.source != OPERATOR_RECONCILIATION_SOURCE {
        return Err(RpcFault::new(
            RpcError::INVALID_PARAMS,
            format!("reconciliation evidence source must be {OPERATOR_RECONCILIATION_SOURCE}"),
        ));
    }
    if evidence.summary.trim().is_empty() {
        return Err(RpcFault::new(
            RpcError::INVALID_PARAMS,
            "reconciliation evidence summary must not be empty",
        ));
    }
    if evidence.summary.len() > MAX_RECONCILIATION_SUMMARY_BYTES {
        return Err(RpcFault::new(
            RpcError::INVALID_PARAMS,
            format!(
                "reconciliation evidence summary exceeds {MAX_RECONCILIATION_SUMMARY_BYTES}-byte limit"
            ),
        ));
    }
    if evidence
        .summary
        .chars()
        .any(|character| character == '\u{0000}' || character == '\u{007f}')
    {
        return Err(RpcFault::new(
            RpcError::INVALID_PARAMS,
            "reconciliation evidence summary contains a forbidden control character",
        ));
    }
    if let Some(uri) = evidence.artifact_uri.as_deref() {
        if uri.len() > MAX_RECONCILIATION_ARTIFACT_URI_BYTES {
            return Err(RpcFault::new(
                RpcError::INVALID_PARAMS,
                format!(
                    "reconciliation artifact URI exceeds {MAX_RECONCILIATION_ARTIFACT_URI_BYTES}-byte limit"
                ),
            ));
        }
        if !uri.starts_with("artifact://") {
            return Err(RpcFault::new(
                RpcError::INVALID_PARAMS,
                "reconciliation artifact URI must use the artifact:// scheme",
            ));
        }
    }
    Ok(())
}

fn decode<T: DeserializeOwned>(params: Value) -> Result<T, RpcFault> {
    let params = if params.is_null() { json!({}) } else { params };
    serde_json::from_value(params).map_err(|error| {
        RpcFault::new(RpcError::INVALID_PARAMS, "invalid method parameters")
            .with_data(json!({ "detail": error.to_string() }))
    })
}

fn encode(value: impl serde::Serialize) -> Result<Value, RpcFault> {
    serde_json::to_value(value).map_err(RpcFault::internal)
}

fn validate_limit(limit: u32) -> Result<(), RpcFault> {
    if limit == 0 || limit > MAX_PAGE_SIZE {
        return Err(RpcFault::new(
            RpcError::INVALID_PARAMS,
            format!("limit must be between 1 and {MAX_PAGE_SIZE}"),
        ));
    }
    Ok(())
}

fn not_found(kind: &str, id: impl std::fmt::Display) -> RpcFault {
    RpcFault::new(NOT_FOUND, format!("{kind} not found: {id}"))
}

fn workspace_identity(snapshot: &WorkspaceIdentitySnapshot) -> WorkspaceIdentity {
    let root = snapshot.canonical_root();
    let git_common_dir = root
        .join(".git")
        .canonicalize()
        .ok()
        .filter(|path| path.is_dir())
        .map(|path| path.to_string_lossy().into_owned());
    WorkspaceIdentity {
        canonical_root: root.to_string_lossy().into_owned(),
        digest: snapshot.digest().to_owned(),
        device: snapshot.device(),
        inode: snapshot.inode(),
        git_common_dir,
    }
}

fn string_field(descriptor: &RegisteredDescriptor, field: &str) -> Option<String> {
    descriptor
        .descriptor
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::{ConnectionState, DaemonConfig, NOT_INITIALIZED};
    use yeux_protocol::{
        ClientCapabilities, ContentBlock, EffectSet, Idempotency, InvocationId, Reversibility,
    };

    fn command(method_name: &str, params: Value) -> String {
        command_with_id(method_name, params, uuid::Uuid::now_v7())
    }

    fn command_with_id(method_name: &str, params: Value, command_id: uuid::Uuid) -> String {
        serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": method_name,
            "command_id": command_id,
            "method": method_name,
            "params": params,
        }))
        .unwrap()
    }

    fn result(response: &Value) -> &Value {
        response
            .get("result")
            .unwrap_or_else(|| panic!("RPC failed: {response}"))
    }

    #[test]
    fn requires_initialize_and_recovers_nonterminal_turn_from_the_ledger() {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let daemon = Daemon::open(DaemonConfig::in_directory(state.path())).unwrap();
        let mut connection = ConnectionState::default();

        let (response, _) =
            daemon.handle_line(&command(method::THREAD_LIST, json!({})), &mut connection);
        assert_eq!(response["error"]["code"], NOT_INITIALIZED);

        let initialize = serde_json::to_value(InitializeParams {
            protocol_version: PROTOCOL_VERSION,
            client_info: ClientInfo {
                name: "test".into(),
                version: "0".into(),
            },
            capabilities: ClientCapabilities::default(),
        })
        .unwrap();
        let (response, _) =
            daemon.handle_line(&command(method::INITIALIZE, initialize), &mut connection);
        assert_eq!(
            result(&response)["protocolVersion"]["major"],
            PROTOCOL_VERSION.major
        );

        let (response, _) = daemon.handle_line(
            &command(method::WORKSPACE_OPEN, json!({ "path": workspace.path() })),
            &mut connection,
        );
        let workspace_id = response["result"]["workspace"]["id"].clone();
        let (response, _) = daemon.handle_line(
            &command(
                method::THREAD_START,
                json!({ "workspaceId": workspace_id, "title": "replay" }),
            ),
            &mut connection,
        );
        let thread_id = response["result"]["thread"]["id"].clone();
        let (response, _) = daemon.handle_line(
            &command(
                method::TURN_START,
                json!({
                    "threadId": thread_id,
                    "content": [ContentBlock::Text { text: "hello".into() }]
                }),
            ),
            &mut connection,
        );
        assert_eq!(result(&response)["turn"]["state"], "accepted");
        let expected = daemon.projection().unwrap();
        drop(connection);
        drop(daemon);

        let reopened = Daemon::reopen(DaemonConfig::in_directory(state.path())).unwrap();
        let rebuilt = reopened.projection().unwrap();
        assert_eq!(rebuilt.workspaces, expected.workspaces);
        assert_eq!(rebuilt.items, expected.items);
        let recovered_turn = rebuilt.turns.values().next().unwrap();
        assert_eq!(recovered_turn.state, TurnState::Failed);
        assert_eq!(
            recovered_turn.failure.as_deref(),
            Some("daemon restarted before the turn completed")
        );
        let recovered_thread = rebuilt.threads.values().next().unwrap();
        assert_eq!(recovered_thread.status, ThreadStatus::Failed);
        assert_eq!(recovered_thread.last_seq, 5);
    }

    #[test]
    fn workspace_open_persists_identity_fields_from_one_runtime_snapshot() {
        let state = tempfile::tempdir().unwrap();
        let workspace_dir = tempfile::tempdir().unwrap();
        let runtime = yeux_runtime::Workspace::open(workspace_dir.path()).unwrap();
        let snapshot = runtime.identity_snapshot();
        let daemon = Daemon::open(DaemonConfig::in_directory(state.path())).unwrap();
        let mut connection = ConnectionState {
            initialized: true,
            ..ConnectionState::default()
        };

        let (response, _) = daemon.handle_line(
            &command(
                method::WORKSPACE_OPEN,
                json!({ "path": workspace_dir.path() }),
            ),
            &mut connection,
        );
        let persisted: Workspace = serde_json::from_value(
            result(&response)
                .get("workspace")
                .cloned()
                .expect("workspace/open result must include workspace"),
        )
        .unwrap();

        assert_eq!(
            persisted.identity.canonical_root,
            snapshot.canonical_root().to_string_lossy()
        );
        assert_eq!(persisted.identity.digest, snapshot.digest());
        assert_eq!(persisted.identity.device, snapshot.device());
        assert_eq!(persisted.identity.inode, snapshot.inode());

        let projected = daemon
            .projection()
            .unwrap()
            .workspaces
            .remove(&persisted.id)
            .expect("workspace/open must persist the workspace");
        assert_eq!(projected.identity, persisted.identity);
    }

    #[test]
    fn initialize_is_revalidated_instead_of_replaying_a_stale_receipt() {
        let state = tempfile::tempdir().unwrap();
        let daemon = Daemon::open(DaemonConfig::in_directory(state.path())).unwrap();
        let mut connection = ConnectionState::default();
        let command_id = uuid::Uuid::now_v7();
        let initialize = |minor| {
            serde_json::to_string(&json!({
                "jsonrpc": "2.0",
                "id": minor,
                "command_id": command_id,
                "method": "initialize",
                "params": {
                    "protocolVersion": { "major": 1, "minor": minor },
                    "clientInfo": { "name": "test", "version": "0" },
                    "capabilities": {}
                }
            }))
            .unwrap()
        };
        let (first, _) = daemon.handle_line(&initialize(0), &mut connection);
        let (second, _) = daemon.handle_line(&initialize(0), &mut connection);
        assert_eq!(first["result"], second["result"]);
        let (conflict, _) = daemon.handle_line(&initialize(1), &mut connection);
        assert_eq!(conflict["error"]["code"], RpcError::INCOMPATIBLE_PROTOCOL);
    }

    #[test]
    fn subscribe_replays_only_events_after_the_requested_sequence() {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let daemon = Daemon::open(DaemonConfig::in_directory(state.path())).unwrap();
        let mut connection = ConnectionState {
            initialized: true,
            ..ConnectionState::default()
        };
        let (opened, _) = daemon.handle_line(
            &command(method::WORKSPACE_OPEN, json!({ "path": workspace.path() })),
            &mut connection,
        );
        let workspace_id = opened["result"]["workspace"]["id"].clone();
        let (started, _) = daemon.handle_line(
            &command(method::THREAD_START, json!({ "workspaceId": workspace_id })),
            &mut connection,
        );
        let thread_id = started["result"]["thread"]["id"].clone();
        let (response, outcome) = daemon.handle_line(
            &command(
                method::THREAD_SUBSCRIBE,
                json!({ "threadId": thread_id, "afterSeq": 0 }),
            ),
            &mut connection,
        );
        assert_eq!(result(&response)["replayedThroughSeq"], 1);
        assert_eq!(
            outcome.unwrap().replay.unwrap(),
            ReplayWindow {
                thread_id: thread_id.as_str().unwrap().parse().unwrap(),
                after_seq: 0,
                through_seq: 1,
            }
        );
    }

    #[test]
    fn successful_command_is_deduplicated_after_daemon_restart() {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let daemon = Daemon::open(DaemonConfig::in_directory(state.path())).unwrap();
        let mut connection = ConnectionState {
            initialized: true,
            ..ConnectionState::default()
        };
        let (opened, _) = daemon.handle_line(
            &command(method::WORKSPACE_OPEN, json!({ "path": workspace.path() })),
            &mut connection,
        );
        let workspace_id = opened["result"]["workspace"]["id"].clone();
        let command_id = uuid::Uuid::now_v7();
        let start = command_with_id(
            method::THREAD_START,
            json!({ "workspaceId": workspace_id }),
            command_id,
        );
        let (first, _) = daemon.handle_line(&start, &mut connection);
        let event_count = daemon.inner.ledger.all_events().unwrap().len();
        drop(connection);
        drop(daemon);

        let reopened = Daemon::reopen(DaemonConfig::in_directory(state.path())).unwrap();
        let mut connection = ConnectionState {
            initialized: true,
            ..ConnectionState::default()
        };
        let (second, _) = reopened.handle_line(&start, &mut connection);
        assert_eq!(first["result"], second["result"]);
        assert_eq!(
            reopened.inner.ledger.all_events().unwrap().len(),
            event_count
        );
    }

    #[test]
    fn concurrent_turn_start_allows_only_one_active_turn() {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let daemon = Daemon::open(DaemonConfig::in_directory(state.path())).unwrap();
        let mut connection = ConnectionState {
            initialized: true,
            ..ConnectionState::default()
        };
        let (opened, _) = daemon.handle_line(
            &command(method::WORKSPACE_OPEN, json!({ "path": workspace.path() })),
            &mut connection,
        );
        let workspace_id = opened["result"]["workspace"]["id"].clone();
        let (started, _) = daemon.handle_line(
            &command(method::THREAD_START, json!({ "workspaceId": workspace_id })),
            &mut connection,
        );
        let thread_id = started["result"]["thread"]["id"].clone();

        let handles: Vec<_> = (0..2)
            .map(|index| {
                let daemon = daemon.clone();
                let thread_id = thread_id.clone();
                std::thread::spawn(move || {
                    let mut connection = ConnectionState {
                        initialized: true,
                        ..ConnectionState::default()
                    };
                    daemon
                        .handle_line(
                            &command(
                                method::TURN_START,
                                json!({
                                    "threadId": thread_id,
                                    "content": [{"type": "text", "text": format!("turn {index}")}]
                                }),
                            ),
                            &mut connection,
                        )
                        .0
                })
            })
            .collect();
        let responses: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();

        assert_eq!(
            responses
                .iter()
                .filter(|response| response.get("result").is_some())
                .count(),
            1
        );
        assert_eq!(
            responses
                .iter()
                .filter(|response| response["error"]["code"] == INVALID_STATE)
                .count(),
            1
        );
        let projection = daemon.projection().unwrap();
        assert_eq!(projection.turns.len(), 1);
        assert_eq!(projection.items.len(), 1);
    }

    #[test]
    fn repeated_interrupt_while_cancelling_records_a_receipt_only() {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let daemon = Daemon::open(DaemonConfig::in_directory(state.path())).unwrap();
        let mut connection = ConnectionState {
            initialized: true,
            ..ConnectionState::default()
        };
        let (opened, _) = daemon.handle_line(
            &command(method::WORKSPACE_OPEN, json!({ "path": workspace.path() })),
            &mut connection,
        );
        let workspace_id = opened["result"]["workspace"]["id"].clone();
        let (started, _) = daemon.handle_line(
            &command(method::THREAD_START, json!({ "workspaceId": workspace_id })),
            &mut connection,
        );
        let thread_id = started["result"]["thread"]["id"].clone();
        let (turn_started, _) = daemon.handle_line(
            &command(
                method::TURN_START,
                json!({
                    "threadId": thread_id,
                    "content": [{"type": "text", "text": "interrupt me"}]
                }),
            ),
            &mut connection,
        );
        let turn_id = turn_started["result"]["turn"]["id"].clone();
        let first = command(
            method::TURN_INTERRUPT,
            json!({"threadId": thread_id, "turnId": turn_id, "reason": "stop"}),
        );
        let (first_response, _) = daemon.handle_line(&first, &mut connection);
        assert_eq!(result(&first_response)["accepted"], true);
        assert_eq!(
            daemon
                .projection()
                .unwrap()
                .turns
                .values()
                .next()
                .unwrap()
                .state,
            TurnState::Cancelling
        );

        let second_id = uuid::Uuid::now_v7();
        let second = command_with_id(
            method::TURN_INTERRUPT,
            json!({"threadId": thread_id, "turnId": turn_id, "reason": "still stop"}),
            second_id,
        );
        let (second_response, _) = daemon.handle_line(&second, &mut connection);
        assert_eq!(result(&second_response)["accepted"], true);
        assert!(daemon
            .inner
            .ledger
            .command_receipt(&second_id.to_string())
            .unwrap()
            .is_some());
        assert_eq!(daemon.inner.ledger.all_events().unwrap().len(), 5);
    }

    #[test]
    fn retried_subscription_rebuilds_replay_and_cursor() {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let daemon = Daemon::open(DaemonConfig::in_directory(state.path())).unwrap();
        let mut connection = ConnectionState {
            initialized: true,
            ..ConnectionState::default()
        };
        let (opened, _) = daemon.handle_line(
            &command(method::WORKSPACE_OPEN, json!({ "path": workspace.path() })),
            &mut connection,
        );
        let workspace_id = opened["result"]["workspace"]["id"].clone();
        let (started, _) = daemon.handle_line(
            &command(method::THREAD_START, json!({ "workspaceId": workspace_id })),
            &mut connection,
        );
        let thread_id = started["result"]["thread"]["id"].clone();
        let subscribe_id = uuid::Uuid::now_v7();
        let subscribe = command_with_id(
            method::THREAD_SUBSCRIBE,
            json!({ "threadId": thread_id, "afterSeq": 0 }),
            subscribe_id,
        );

        let (first, first_outcome) = daemon.handle_line(&subscribe, &mut connection);
        assert_eq!(result(&first)["replayedThroughSeq"], 1);
        assert_eq!(first_outcome.unwrap().subscription.unwrap().1, 1);

        let _ = daemon.handle_line(
            &command(
                method::TURN_START,
                json!({
                    "threadId": thread_id,
                    "content": [{"type": "text", "text": "hello"}]
                }),
            ),
            &mut connection,
        );
        let (second, second_outcome) = daemon.handle_line(&subscribe, &mut connection);
        let second_outcome = second_outcome.unwrap();
        assert_eq!(result(&second)["replayedThroughSeq"], 3);
        assert_eq!(second_outcome.replay.unwrap().through_seq, 3);
        assert_eq!(second_outcome.subscription.unwrap().1, 3);

        drop(connection);
        drop(daemon);
        let reopened = Daemon::reopen(DaemonConfig::in_directory(state.path())).unwrap();
        let mut connection = ConnectionState {
            initialized: true,
            ..ConnectionState::default()
        };
        let (third, third_outcome) = reopened.handle_line(&subscribe, &mut connection);
        assert_eq!(result(&third)["replayedThroughSeq"], 5);
        assert_eq!(third_outcome.unwrap().subscription.unwrap().1, 5);
    }

    #[test]
    fn command_params_may_be_omitted_for_parameterless_methods() {
        let state = tempfile::tempdir().unwrap();
        let daemon = Daemon::open(DaemonConfig::in_directory(state.path())).unwrap();
        let mut connection = ConnectionState {
            initialized: true,
            ..ConnectionState::default()
        };
        let input = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": "skills",
            "command_id": uuid::Uuid::now_v7(),
            "method": method::SKILL_LIST,
        }))
        .unwrap();
        let (response, _) = daemon.handle_line(&input, &mut connection);
        assert_eq!(result(&response)["skills"], json!([]));
    }

    #[test]
    fn invocation_reconcile_is_evidence_only_idempotent_and_unblocks_thread() {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let daemon =
            Daemon::open(DaemonConfig::in_directory(state.path()).without_turn_execution())
                .unwrap();
        let evidence_artifact = daemon
            .inner
            .artifacts
            .put(b"operator receipt: no durable change", "text/plain")
            .unwrap();
        let evidence_artifact_uri =
            yeux_runtime::ArtifactStore::uri_for_digest(&evidence_artifact.digest).unwrap();
        let mut connection = ConnectionState {
            initialized: true,
            ..ConnectionState::default()
        };

        let (opened, _) = daemon.handle_line(
            &command(method::WORKSPACE_OPEN, json!({ "path": workspace.path() })),
            &mut connection,
        );
        let workspace_id: WorkspaceId =
            serde_json::from_value(opened["result"]["workspace"]["id"].clone()).unwrap();
        let (started, _) = daemon.handle_line(
            &command(method::THREAD_START, json!({ "workspaceId": workspace_id })),
            &mut connection,
        );
        let thread_id: ThreadId =
            serde_json::from_value(started["result"]["thread"]["id"].clone()).unwrap();
        let (turn_started, _) = daemon.handle_line(
            &command(
                method::TURN_START,
                json!({
                    "threadId": thread_id,
                    "content": [{"type": "text", "text": "fixture"}]
                }),
            ),
            &mut connection,
        );
        let turn_id: TurnId =
            serde_json::from_value(turn_started["result"]["turn"]["id"].clone()).unwrap();

        let invocation_id = InvocationId::from_uuid(uuid::Uuid::now_v7());
        let effects = EffectSet {
            idempotency: Idempotency::NonIdempotent,
            reversibility: Reversibility::Unknown,
            ..EffectSet::default()
        };
        let effect_digest = yeux_core::digest_value(&serde_json::to_value(&effects).unwrap());
        let proposal = daemon
            .new_event(
                thread_id,
                Some(turn_id),
                AgentId::new("root"),
                uuid::Uuid::now_v7().into(),
                daemon.inner.clock.now(),
                Event::InvocationProposed {
                    invocation_id,
                    call_id: "fixture-call".into(),
                    tool_id: "fixture.tool".into(),
                    tool_version: "1".into(),
                    normalized_arguments_digest: yeux_core::digest_value(&json!({"fixture": true})),
                    effects: effects.clone(),
                    effect_digest,
                    idempotency: effects.idempotency,
                },
            )
            .unwrap();
        let approved = daemon
            .new_event(
                thread_id,
                Some(turn_id),
                AgentId::new("root"),
                uuid::Uuid::now_v7().into(),
                daemon.inner.clock.now(),
                Event::InvocationStateChanged {
                    invocation_id,
                    from: InvocationState::Proposed,
                    to: InvocationState::Approved,
                    reason: None,
                },
            )
            .unwrap();
        let prepared = daemon
            .new_event(
                thread_id,
                Some(turn_id),
                AgentId::new("root"),
                uuid::Uuid::now_v7().into(),
                daemon.inner.clock.now(),
                Event::InvocationStateChanged {
                    invocation_id,
                    from: InvocationState::Approved,
                    to: InvocationState::Prepared,
                    reason: None,
                },
            )
            .unwrap();
        let executing = daemon
            .new_event(
                thread_id,
                Some(turn_id),
                AgentId::new("root"),
                uuid::Uuid::now_v7().into(),
                daemon.inner.clock.now(),
                Event::InvocationStateChanged {
                    invocation_id,
                    from: InvocationState::Prepared,
                    to: InvocationState::Started,
                    reason: None,
                },
            )
            .unwrap();
        let unknown = daemon
            .new_event(
                thread_id,
                Some(turn_id),
                AgentId::new("root"),
                uuid::Uuid::now_v7().into(),
                daemon.inner.clock.now(),
                Event::InvocationStateChanged {
                    invocation_id,
                    from: InvocationState::Started,
                    to: InvocationState::Unknown,
                    reason: Some("fixture outcome is not observable".into()),
                },
            )
            .unwrap();
        let turn_failed = daemon
            .new_event(
                thread_id,
                Some(turn_id),
                AgentId::new("root"),
                uuid::Uuid::now_v7().into(),
                daemon.inner.clock.now(),
                Event::TurnStateChanged {
                    turn_id,
                    from: TurnState::Accepted,
                    to: TurnState::Failed,
                    reason: Some("fixture requires reconciliation".into()),
                },
            )
            .unwrap();
        daemon
            .inner
            .ledger
            .append_batch(vec![
                proposal,
                approved,
                prepared,
                executing,
                unknown,
                turn_failed,
            ])
            .unwrap();

        let blocked = daemon.handle_line(
            &command(
                method::TURN_START,
                json!({
                    "threadId": thread_id,
                    "content": [{"type": "text", "text": "must wait"}]
                }),
            ),
            &mut connection,
        );
        assert_eq!(blocked.0["error"]["code"], INVALID_STATE);
        assert_eq!(
            blocked.0["error"]["data"]["invocationIds"][0],
            invocation_id.to_string()
        );

        let invalid_artifact = command(
            method::INVOCATION_RECONCILE,
            json!({
                "threadId": thread_id,
                "invocationId": invocation_id,
                "outcome": "failed",
                "evidence": {
                    "source": OPERATOR_RECONCILIATION_SOURCE,
                    "summary": "receipt is not available",
                    "artifactUri": "artifact://blake3/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                }
            }),
        );
        let (invalid_response, _) = daemon.handle_line(&invalid_artifact, &mut connection);
        assert_eq!(invalid_response["error"]["code"], RpcError::INVALID_PARAMS);
        assert_eq!(
            invalid_response["error"]["data"]["code"],
            "artifact_invalid"
        );
        assert_eq!(
            daemon
                .projection()
                .unwrap()
                .invocations
                .get(&invocation_id)
                .unwrap()
                .state,
            InvocationState::Unknown
        );

        let reconcile_id = uuid::Uuid::now_v7();
        let reconcile = command_with_id(
            method::INVOCATION_RECONCILE,
            json!({
                "threadId": thread_id,
                "invocationId": invocation_id,
                "outcome": "failed",
                "evidence": {
                    "source": OPERATOR_RECONCILIATION_SOURCE,
                    "summary": "operator verified that the fixture made no durable change",
                    "artifactUri": evidence_artifact_uri
                }
            }),
            reconcile_id,
        );
        let (response, _) = daemon.handle_line(&reconcile, &mut connection);
        assert_eq!(result(&response)["state"], "failed");
        let projected = daemon
            .projection()
            .unwrap()
            .invocations
            .get(&invocation_id)
            .cloned()
            .unwrap();
        assert_eq!(projected.state, InvocationState::Failed);
        assert_eq!(
            projected.reconciliation.unwrap().source,
            OPERATOR_RECONCILIATION_SOURCE
        );
        assert_eq!(
            result(&response)["evidence"]["artifactUri"],
            evidence_artifact_uri
        );
        assert!(daemon
            .inner
            .ledger
            .all_events()
            .unwrap()
            .iter()
            .any(|event| event.kind == "tool/reconciled"));

        let event_count = daemon.inner.ledger.all_events().unwrap().len();
        let (replayed, _) = daemon.handle_line(&reconcile, &mut connection);
        assert_eq!(replayed, response);
        assert_eq!(daemon.inner.ledger.all_events().unwrap().len(), event_count);

        let (next_turn, _) = daemon.handle_line(
            &command(
                method::TURN_START,
                json!({
                    "threadId": thread_id,
                    "content": [{"type": "text", "text": "now continue"}]
                }),
            ),
            &mut connection,
        );
        assert!(
            next_turn.get("result").is_some(),
            "reconciliation should unblock turns: {next_turn}"
        );
    }
}
