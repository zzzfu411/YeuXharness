//! JSON-RPC service and transports.

use std::{
    collections::{BTreeMap, HashMap},
    fs::{File, OpenOptions},
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use fs2::FileExt;
use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader, BufWriter},
    sync::broadcast,
};
use uuid::Version;
use yeux_core::{
    digest_value, Clock, IdError, IdGenerator, ReplayError, SystemClock, UuidV7Generator,
};
use yeux_protocol::{
    method, AgentId, CapabilityMode, CommandEnvelope, ContentBlock, Event, EventEnvelope,
    InvocationId, InvocationState, Item, ItemId, ItemKind, ModelDescriptor, NotificationEnvelope,
    ResponseEnvelope, RpcError, RpcId, ThreadId, TurnId, TurnState, JSONRPC_VERSION,
    PROTOCOL_VERSION,
};
use yeux_runtime::{
    DescriptorStore, EventLedger, NewCommandReceipt, NewInvocationOutcome, NewInvocationUnknown,
    NewLedgerEvent,
};

use crate::runner::{
    CancellationFlag, ModelProviderConfig, TurnRunSpec, TurnRunner, TurnRunnerError,
};

const DEFAULT_MAX_LINE_BYTES: usize = 8 * 1024 * 1024;
const DEFAULT_EVENT_BUFFER: usize = 1_024;
const REPLAY_PAGE_SIZE: usize = 256;
pub(crate) const MAX_PAGE_SIZE: u32 = 1_000;
pub(crate) const NOT_INITIALIZED: i32 = -32_000;
pub(crate) const NOT_FOUND: i32 = -32_004;
pub(crate) const INVALID_STATE: i32 = -32_005;
pub(crate) const FEATURE_UNAVAILABLE: i32 = -32_006;

#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub state_dir: PathBuf,
    pub host_ceiling: CapabilityMode,
    pub max_line_bytes: usize,
    pub event_buffer: usize,
    pub model_provider: Option<ModelProviderConfig>,
    execute_turns: bool,
}

impl DaemonConfig {
    pub fn new(state_dir: Option<PathBuf>) -> Result<Self, DaemonError> {
        let state_dir = match state_dir {
            Some(path) => path,
            None => directories::ProjectDirs::from("dev", "YeuX", "Harness")
                .ok_or(DaemonError::StateDirectoryUnavailable)?
                .data_local_dir()
                .to_owned(),
        };
        Ok(Self {
            state_dir,
            host_ceiling: CapabilityMode::Operate,
            max_line_bytes: DEFAULT_MAX_LINE_BYTES,
            event_buffer: DEFAULT_EVENT_BUFFER,
            model_provider: None,
            execute_turns: true,
        })
    }

    pub fn in_directory(state_dir: impl Into<PathBuf>) -> Self {
        Self {
            state_dir: state_dir.into(),
            host_ceiling: CapabilityMode::Operate,
            max_line_bytes: DEFAULT_MAX_LINE_BYTES,
            event_buffer: DEFAULT_EVENT_BUFFER,
            model_provider: None,
            execute_turns: true,
        }
    }

    pub fn with_model_provider(mut self, provider: ModelProviderConfig) -> Self {
        self.model_provider = Some(provider);
        self
    }

    /// Disable asynchronous turn execution for deterministic protocol fixtures.
    #[doc(hidden)]
    pub fn without_turn_execution(mut self) -> Self {
        self.execute_turns = false;
        self
    }

    pub(crate) fn executes_turns(&self) -> bool {
        self.execute_turns
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error("could not determine a local state directory")]
    StateDirectoryUnavailable,
    #[error("another yeuxd process already owns {0}")]
    AlreadyRunning(PathBuf),
    #[error("refusing to replace a non-socket path: {0}")]
    InvalidSocketPath(PathBuf),
    #[error("a daemon is already accepting connections at {0}")]
    SocketInUse(PathBuf),
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("ledger error: {0}")]
    Ledger(#[from] yeux_runtime::ledger::LedgerError),
    #[error("descriptor error: {0}")]
    Descriptor(#[from] yeux_runtime::descriptors::DescriptorError),
    #[error("projection replay failed: {0}")]
    Replay(#[from] ReplayError),
    #[error("ID generation failed: {0}")]
    Id(#[from] IdError),
    #[error("JSON serialization failed: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Clone)]
pub struct Daemon {
    pub(crate) inner: Arc<DaemonInner>,
}

impl std::fmt::Debug for Daemon {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Daemon")
            .field("state_dir", &self.inner.config.state_dir)
            .finish_non_exhaustive()
    }
}

pub(crate) struct DaemonInner {
    pub(crate) config: DaemonConfig,
    pub(crate) ledger: Arc<EventLedger>,
    pub(crate) descriptors: DescriptorStore,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) ids: Arc<dyn IdGenerator>,
    pub(crate) events: broadcast::Sender<EventEnvelope>,
    pub(crate) configured_model: Option<ModelDescriptor>,
    turn_runner: TurnRunner,
    active_turns: Mutex<HashMap<TurnId, Arc<CancellationFlag>>>,
    command_gate: Arc<Mutex<()>>,
    _state_lock: File,
}

#[derive(Clone, Default)]
pub(crate) struct CommandOutcome {
    pub(crate) result: Value,
    pub(crate) replay: Option<ReplayWindow>,
    pub(crate) subscription: Option<(ThreadId, u64)>,
    pub(crate) turn_run: Option<TurnRunSpec>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ReplayWindow {
    pub(crate) thread_id: ThreadId,
    pub(crate) after_seq: u64,
    pub(crate) through_seq: u64,
}

#[derive(Default)]
pub(crate) struct ConnectionState {
    pub(crate) initialized: bool,
    pub(crate) subscriptions: BTreeMap<ThreadId, u64>,
}

#[derive(Debug)]
pub(crate) struct RpcFault {
    code: i32,
    message: String,
    data: Option<Value>,
}

impl RpcFault {
    pub(crate) fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub(crate) fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }

    pub(crate) fn internal(error: impl std::fmt::Display) -> Self {
        Self::new(RpcError::INTERNAL_ERROR, "internal daemon error")
            .with_data(json!({ "detail": error.to_string() }))
    }

    fn into_protocol(self) -> RpcError {
        RpcError {
            code: self.code,
            message: self.message,
            data: self.data,
        }
    }
}

impl Daemon {
    pub fn open(config: DaemonConfig) -> Result<Self, DaemonError> {
        Self::open_with(config, Arc::new(SystemClock), Arc::new(UuidV7Generator))
    }

    pub fn open_with(
        mut config: DaemonConfig,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGenerator>,
    ) -> Result<Self, DaemonError> {
        std::fs::create_dir_all(&config.state_dir)?;
        set_private_directory(&config.state_dir)?;
        config.state_dir = std::fs::canonicalize(&config.state_dir)?;

        let lock_path = config.state_dir.join("owner.lock");
        let state_lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)?;
        set_private_file(&lock_path)?;
        state_lock
            .try_lock_exclusive()
            .map_err(|_| DaemonError::AlreadyRunning(lock_path))?;

        let database = config.state_dir.join("state.sqlite3");
        let ledger = Arc::new(EventLedger::open(&database)?);
        recover_interrupted_turns(&ledger, clock.as_ref(), ids.as_ref())?;
        let descriptors = DescriptorStore::open(&database)?;
        let (events, _) = broadcast::channel(config.event_buffer.max(1));
        let command_gate = Arc::new(Mutex::new(()));
        let configured_model = config
            .model_provider
            .as_ref()
            .map(|selection| ModelDescriptor {
                provider: selection.provider.provider_id().to_owned(),
                model: selection.model.clone(),
                display_name: selection.model.clone(),
                capabilities: selection.provider.capabilities(),
            });
        let turn_runner = TurnRunner::new(
            Arc::clone(&ledger),
            events.clone(),
            Arc::clone(&clock),
            Arc::clone(&ids),
            config.model_provider.clone(),
            Arc::clone(&command_gate),
        );
        Ok(Self {
            inner: Arc::new(DaemonInner {
                config,
                ledger,
                descriptors,
                clock,
                ids,
                events,
                configured_model,
                turn_runner,
                active_turns: Mutex::new(HashMap::new()),
                command_gate,
                _state_lock: state_lock,
            }),
        })
    }

    pub async fn serve_stdio(self) -> Result<(), DaemonError> {
        self.serve_connection(
            BufReader::new(tokio::io::stdin()),
            BufWriter::new(tokio::io::stdout()),
        )
        .await
    }

    #[cfg(unix)]
    pub async fn serve_unix(self, path: PathBuf) -> Result<(), DaemonError> {
        use std::os::unix::fs::{FileTypeExt, PermissionsExt};
        use tokio::net::{UnixListener, UnixStream};

        let path = if path.is_absolute() {
            path
        } else {
            std::env::current_dir()?.join(path)
        };
        if let Some(parent) = path.parent() {
            ensure_socket_parent(parent)?;
        }
        if let Ok(metadata) = std::fs::symlink_metadata(&path) {
            if !metadata.file_type().is_socket() {
                return Err(DaemonError::InvalidSocketPath(path));
            }
            if UnixStream::connect(&path).await.is_ok() {
                return Err(DaemonError::SocketInUse(path));
            }
            std::fs::remove_file(&path)?;
        }

        let listener = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        let cleanup = SocketCleanup(path);
        let shutdown = tokio::signal::ctrl_c();
        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (stream, _) = accepted?;
                    let (read, write) = stream.into_split();
                    let daemon = self.clone();
                    tokio::spawn(async move {
                        let _ = daemon
                            .serve_connection(BufReader::new(read), BufWriter::new(write))
                            .await;
                    });
                }
                signal = &mut shutdown => {
                    signal?;
                    drop(cleanup);
                    return Ok(());
                }
            }
        }
    }

    async fn serve_connection<R, W>(&self, mut reader: R, mut writer: W) -> Result<(), DaemonError>
    where
        R: AsyncBufRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let mut state = ConnectionState::default();
        let mut receiver = self.inner.events.subscribe();
        let mut buffered = Vec::new();
        loop {
            tokio::select! {
                frame = read_frame(&mut reader, &mut buffered, self.inner.config.max_line_bytes) => {
                    let frame = match frame {
                        Ok(frame) => frame,
                        Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                            write_json(
                                &mut writer,
                                &raw_error(
                                    None,
                                    RpcError::INVALID_REQUEST,
                                    "JSON-RPC line is too large",
                                    None,
                                ),
                            ).await?;
                            writer.flush().await?;
                            return Ok(());
                        }
                        Err(error) => return Err(error.into()),
                    };
                    let Frame::Line(mut line) = frame else {
                        writer.flush().await?;
                        return Ok(());
                    };
                    if line.last() == Some(&b'\r') {
                        line.pop();
                    }
                    let input = match String::from_utf8(line) {
                        Ok(input) => input,
                        Err(error) => {
                            write_json(
                                &mut writer,
                                &raw_error(
                                    None,
                                    RpcError::PARSE_ERROR,
                                    "JSON-RPC input is not UTF-8",
                                    Some(json!({ "detail": error.to_string() })),
                                ),
                            ).await?;
                            writer.flush().await?;
                            continue;
                        }
                    };
                    if input.len() > self.inner.config.max_line_bytes {
                        write_json(
                            &mut writer,
                            &raw_error(None, RpcError::INVALID_REQUEST, "JSON-RPC line is too large", None),
                        ).await?;
                        return Ok(());
                    }
                    if input.trim().is_empty() {
                        continue;
                    }
                    let (response, mut outcome) = self.handle_line(&input, &mut state);
                    if self.inner.config.execute_turns {
                        if let Some(spec) = outcome.as_mut().and_then(|value| value.turn_run.take()) {
                            // The command and its receipt are already durable. Launch before writing
                            // the response so a disconnected client cannot strand an accepted turn.
                            self.launch_turn(spec);
                        }
                    }
                    write_json(&mut writer, &response).await?;
                    if let Some(outcome) = outcome {
                        if let Some((thread_id, through)) = outcome.subscription {
                            state.subscriptions.insert(thread_id, through);
                        }
                        if let Some(replay) = outcome.replay {
                            self.write_replay(&mut writer, replay).await?;
                        }
                    }
                    writer.flush().await?;
                }
                received = receiver.recv() => {
                    match received {
                        Ok(event) => {
                            if let Some(last_seq) = state.subscriptions.get_mut(&event.thread_id) {
                                if event.seq <= *last_seq {
                                    continue;
                                }
                                if event.seq != *last_seq + 1 {
                                    write_json(
                                        &mut writer,
                                        &json!({
                                            "jsonrpc": JSONRPC_VERSION,
                                            "method": "runtime/diagnostic",
                                            "params": {
                                                "code": "event_sequence_gap",
                                                "message": "reconnect with thread/subscribe and afterSeq",
                                                "recoverable": true,
                                                "thread_id": event.thread_id,
                                                "expected_seq": *last_seq + 1,
                                                "actual_seq": event.seq
                                            }
                                        }),
                                    ).await?;
                                    writer.flush().await?;
                                    return Ok(());
                                }
                                write_event(&mut writer, &event).await?;
                                writer.flush().await?;
                                *last_seq = event.seq;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            write_json(
                                &mut writer,
                                &json!({
                                    "jsonrpc": JSONRPC_VERSION,
                                    "method": "runtime/diagnostic",
                                    "params": {
                                        "code": "event_backpressure",
                                        "message": "client fell behind; reconnect from its last seq",
                                        "recoverable": true
                                    }
                                }),
                            ).await?;
                            writer.flush().await?;
                            return Ok(());
                        }
                        Err(broadcast::error::RecvError::Closed) => return Ok(()),
                    }
                }
            }
        }
    }

    pub(crate) fn handle_line(
        &self,
        input: &str,
        state: &mut ConnectionState,
    ) -> (Value, Option<CommandOutcome>) {
        let value: Value = match serde_json::from_str(input) {
            Ok(value) => value,
            Err(error) => {
                return (
                    raw_error(
                        None,
                        RpcError::PARSE_ERROR,
                        "invalid JSON",
                        Some(json!({ "detail": error.to_string() })),
                    ),
                    None,
                );
            }
        };
        let id = value.get("id").cloned();
        let command: CommandEnvelope<Value> = match serde_json::from_value(value) {
            Ok(command) => command,
            Err(error) => {
                return (
                    raw_error(
                        id,
                        RpcError::INVALID_REQUEST,
                        "invalid command envelope",
                        Some(json!({ "detail": error.to_string() })),
                    ),
                    None,
                );
            }
        };
        if command.jsonrpc != JSONRPC_VERSION {
            return (
                failure(
                    command.id,
                    RpcFault::new(RpcError::INVALID_REQUEST, "jsonrpc must be 2.0"),
                ),
                None,
            );
        }
        if command.command_id.into_uuid().get_version() != Some(Version::SortRand) {
            return (
                failure(
                    command.id,
                    RpcFault::new(RpcError::INVALID_REQUEST, "command_id must be UUIDv7"),
                ),
                None,
            );
        }
        if !state.initialized && command.method != method::INITIALIZE {
            return (
                failure(
                    command.id,
                    RpcFault::new(NOT_INITIALIZED, "initialize must be the first command"),
                ),
                None,
            );
        }

        let _command_guard = match self.inner.command_gate.lock() {
            Ok(guard) => guard,
            Err(_) => {
                return (
                    failure(command.id, RpcFault::internal("command gate is poisoned")),
                    None,
                );
            }
        };

        let params_digest = digest_value(&command.params);
        // `initialize` is pure and intentionally re-evaluated after every
        // reconnect. Persisting it across daemon upgrades could replay stale
        // protocol capabilities and bypass current version negotiation.
        let durable = if command.method == method::INITIALIZE {
            None
        } else {
            match self
                .inner
                .ledger
                .command_receipt(&command.command_id.to_string())
            {
                Ok(receipt) => receipt,
                Err(error) => {
                    return (failure(command.id, RpcFault::internal(error)), None);
                }
            }
        };
        if let Some(receipt) = durable {
            if receipt.method != command.method || receipt.params_digest != params_digest {
                return (
                    failure(
                        command.id,
                        RpcFault::new(
                            RpcError::COMMAND_CONFLICT,
                            "command_id was already used with different input",
                        ),
                    ),
                    None,
                );
            }
            let outcome = if command.method == method::THREAD_SUBSCRIBE {
                match self.dispatch(
                    &command.method,
                    command.command_id,
                    command.params.clone(),
                    &params_digest,
                ) {
                    Ok(recreated) => recreated,
                    Err(error) => return (failure(command.id, error), None),
                }
            } else {
                CommandOutcome {
                    result: receipt.response,
                    ..CommandOutcome::default()
                }
            };
            return (success(command.id, outcome.result.clone()), Some(outcome));
        }

        match self.dispatch(
            &command.method,
            command.command_id,
            command.params,
            &params_digest,
        ) {
            Ok(outcome) => {
                if command.method != method::INITIALIZE {
                    let receipt = NewCommandReceipt {
                        command_id: command.command_id.to_string(),
                        method: command.method.clone(),
                        params_digest,
                        response: outcome.result.clone(),
                        created_at: self.inner.clock.now(),
                    };
                    if let Err(error) = self.inner.ledger.record_command_receipt(receipt) {
                        return (failure(command.id, RpcFault::internal(error)), None);
                    }
                }
                if command.method == method::INITIALIZE {
                    state.initialized = true;
                }
                (success(command.id, outcome.result.clone()), Some(outcome))
            }
            Err(error) => (failure(command.id, error), None),
        }
    }

    fn launch_turn(&self, spec: TurnRunSpec) {
        let cancellation = Arc::new(CancellationFlag::default());
        if let Ok(mut active) = self.inner.active_turns.lock() {
            active.insert(spec.turn_id, Arc::clone(&cancellation));
        }
        let runner = self.inner.turn_runner.clone();
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            match runner.run(spec, cancellation.as_ref()).await {
                Err(TurnRunnerError::UnexpectedState { actual, .. }) if actual.is_terminal() => {}
                Err(error) => {
                    eprintln!("yeuxd: turn runner failed for {}: {error}", spec.turn_id);
                }
                Ok(_) => {}
            }
            if let Ok(mut active) = inner.active_turns.lock() {
                active.remove(&spec.turn_id);
            }
        });
    }

    pub(crate) fn request_turn_cancel(&self, turn_id: TurnId) {
        if let Ok(active) = self.inner.active_turns.lock() {
            if let Some(cancellation) = active.get(&turn_id) {
                cancellation.cancel();
            }
        }
    }

    async fn write_replay<W: AsyncWrite + Unpin>(
        &self,
        writer: &mut W,
        window: ReplayWindow,
    ) -> Result<(), DaemonError> {
        let mut after_seq = window.after_seq;
        while after_seq < window.through_seq {
            let remaining = (window.through_seq - after_seq).min(REPLAY_PAGE_SIZE as u64) as usize;
            let page = self.inner.ledger.replay_page(
                &window.thread_id.to_string(),
                after_seq,
                remaining,
            )?;
            if page.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "event replay ended before its advertised sequence",
                )
                .into());
            }
            for event in page {
                let envelope = EventEnvelope::try_from(event)?;
                after_seq = envelope.seq;
                write_event(writer, &envelope).await?;
            }
        }
        Ok(())
    }
}

fn recover_interrupted_turns(
    ledger: &EventLedger,
    clock: &dyn Clock,
    ids: &dyn IdGenerator,
) -> Result<(), DaemonError> {
    let projection = ledger.project_core().map_err(|error| match error {
        yeux_runtime::CoreProjectionError::Ledger(error) => DaemonError::Ledger(error),
        yeux_runtime::CoreProjectionError::Replay(error) => DaemonError::Replay(error),
    })?;

    // Invocation state events must retain the proposal's complete envelope.
    // In particular, an invocation may be attributed to a delegated agent
    // rather than the turn's agent, so deriving this from `Turn` would make a
    // replay disagree with the durable proposal.
    let mut invocation_envelopes: HashMap<InvocationId, (ThreadId, TurnId, AgentId)> =
        HashMap::new();
    for persisted in ledger.all_events()? {
        let envelope = EventEnvelope::try_from(persisted)?;
        if let Event::InvocationProposed { invocation_id, .. } = envelope.event {
            let turn_id = envelope.turn_id.ok_or_else(|| {
                DaemonError::Replay(ReplayError::MissingEntity {
                    kind: "invocation proposal turn",
                    id: invocation_id.to_string(),
                })
            })?;
            invocation_envelopes.insert(
                invocation_id,
                (envelope.thread_id, turn_id, envelope.agent_id),
            );
        }
    }

    let now = clock.now();
    let mut recovery = Vec::new();

    // Resolve every pre-execution invocation before terminating its parent
    // turn. Once Started has been persisted, the external outcome is
    // indeterminate: conservatively record Unknown and never replay it during
    // daemon startup, regardless of its idempotency classification.
    for invocation in projection
        .invocations
        .values()
        .filter(|invocation| !invocation.state.is_terminal())
    {
        let to = match invocation.state {
            InvocationState::Proposed | InvocationState::Approved | InvocationState::Prepared => {
                InvocationState::Failed
            }
            InvocationState::Started => InvocationState::Unknown,
            InvocationState::Unknown => continue,
            InvocationState::Completed | InvocationState::Failed | InvocationState::Cancelled => {
                unreachable!("terminal invocations were filtered")
            }
        };
        let (thread_id, turn_id, agent_id) = invocation_envelopes
            .get(&invocation.invocation_id)
            .cloned()
            .ok_or_else(|| {
                DaemonError::Replay(ReplayError::MissingEntity {
                    kind: "invocation proposal envelope",
                    id: invocation.invocation_id.to_string(),
                })
            })?;
        let reason = if to == InvocationState::Unknown {
            "daemon restarted after the invocation crossed the execution boundary; outcome requires reconciliation"
        } else {
            "daemon restarted before the invocation crossed the execution boundary"
        };
        let causation_id = format!("daemon-restart:{}", invocation.invocation_id);
        let state_event = new_recovery_event(
            ids,
            thread_id,
            Some(turn_id),
            agent_id.clone(),
            now,
            &causation_id,
            Event::InvocationStateChanged {
                invocation_id: invocation.invocation_id,
                from: invocation.state,
                to,
                reason: Some(reason.into()),
            },
        )?;
        if to == InvocationState::Unknown {
            // Unknown is intentionally a marker-only fact: there is no
            // proven external result to expose as a terminal ToolResult.  The
            // typed ledger API still enforces Started -> Unknown and checks
            // the projected state atomically, so a restart cannot append a
            // stale or divergent marker.
            ledger.append_invocation_unknown(NewInvocationUnknown { state: state_event })?;
        } else {
            // Pre-execution recovery is deterministic.  Persist a bounded
            // diagnostic ToolResult together with the Failed transition so a
            // replay can never observe a terminal invocation without its
            // model-visible result.
            let failure_message =
                "daemon restarted before the invocation crossed the execution boundary";
            let tool_result = new_recovery_event(
                ids,
                thread_id,
                Some(turn_id),
                agent_id.clone(),
                now,
                &causation_id,
                Event::ItemAdded {
                    item: Item {
                        id: ItemId::from_uuid(ids.next_uuid()?),
                        thread_id,
                        turn_id,
                        agent_id: agent_id.clone(),
                        kind: ItemKind::ToolResult,
                        content: json!({
                            "content": [ContentBlock::ToolResult {
                                call_id: invocation.call_id.clone(),
                                content: json!({
                                    "code": "tool_interrupted_by_restart",
                                    "message": failure_message,
                                }),
                                is_error: true,
                            }],
                            "invocation_id": invocation.invocation_id,
                        }),
                        created_at: now,
                    },
                },
            )?;
            ledger.append_invocation_outcome(NewInvocationOutcome {
                tool_result,
                terminal_state: state_event,
            })?;
        }
    }

    for turn in projection
        .turns
        .values()
        .filter(|turn| !turn.state.is_terminal())
    {
        let causation_id = format!("daemon-restart:{}", turn.id);
        recovery.push(new_recovery_event(
            ids,
            turn.thread_id,
            Some(turn.id),
            turn.agent_id.clone(),
            now,
            &causation_id,
            Event::RuntimeDiagnostic {
                code: "turn_interrupted_by_restart".into(),
                message: "daemon restarted before the turn reached a terminal state; external work was not replayed".into(),
                recoverable: false,
            },
        )?);
        recovery.push(new_recovery_event(
            ids,
            turn.thread_id,
            Some(turn.id),
            turn.agent_id.clone(),
            now,
            &causation_id,
            Event::TurnStateChanged {
                turn_id: turn.id,
                from: turn.state,
                to: TurnState::Failed,
                reason: Some("daemon restarted before the turn completed".into()),
            },
        )?);
    }
    if !recovery.is_empty() {
        ledger.append_batch(recovery)?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn new_recovery_event(
    ids: &dyn IdGenerator,
    thread_id: ThreadId,
    turn_id: Option<TurnId>,
    agent_id: AgentId,
    time: chrono::DateTime<chrono::Utc>,
    causation_id: &str,
    event: Event,
) -> Result<NewLedgerEvent, DaemonError> {
    let serialized = serde_json::to_value(event)?;
    let kind = serialized
        .get("kind")
        .and_then(Value::as_str)
        .expect("serialized protocol Event always has a kind")
        .to_owned();
    Ok(NewLedgerEvent {
        schema_version: PROTOCOL_VERSION,
        event_id: ids.next_uuid()?.to_string(),
        thread_id: thread_id.to_string(),
        turn_id: turn_id.map(|id| id.to_string()),
        agent_id: agent_id.to_string(),
        time,
        causation_id: Some(causation_id.to_owned()),
        kind,
        payload: serialized.get("payload").cloned().unwrap_or(Value::Null),
    })
}

enum Frame {
    Line(Vec<u8>),
    Eof,
}

/// Cancellation-safe line framing. Bytes are copied into caller-owned state
/// before the reader is consumed, so an event notification winning `select!`
/// cannot discard a partial command.
async fn read_frame<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    buffered: &mut Vec<u8>,
    max_line_bytes: usize,
) -> io::Result<Frame> {
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if buffered.is_empty() {
                Ok(Frame::Eof)
            } else {
                Ok(Frame::Line(std::mem::take(buffered)))
            };
        }
        if let Some(newline) = available.iter().position(|byte| *byte == b'\n') {
            if buffered.len().saturating_add(newline) > max_line_bytes {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "JSON-RPC line is too large",
                ));
            }
            buffered.extend_from_slice(&available[..newline]);
            reader.consume(newline + 1);
            return Ok(Frame::Line(std::mem::take(buffered)));
        }
        if buffered.len().saturating_add(available.len()) > max_line_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "JSON-RPC line is too large",
            ));
        }
        let consumed = available.len();
        buffered.extend_from_slice(available);
        reader.consume(consumed);
    }
}

fn success(id: RpcId, result: Value) -> Value {
    serde_json::to_value(ResponseEnvelope::success(id, result))
        .expect("JSON-RPC success response is serializable")
}

fn failure(id: RpcId, error: RpcFault) -> Value {
    serde_json::to_value(ResponseEnvelope::<Value>::failure(
        id,
        error.into_protocol(),
    ))
    .expect("JSON-RPC error response is serializable")
}

fn raw_error(id: Option<Value>, code: i32, message: &str, data: Option<Value>) -> Value {
    let mut error = json!({ "code": code, "message": message });
    if let Some(data) = data {
        error["data"] = data;
    }
    json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id.unwrap_or(Value::Null),
        "error": error,
    })
}

async fn write_json<W: AsyncWrite + Unpin>(writer: &mut W, value: &Value) -> io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    writer.write_all(&bytes).await?;
    writer.write_all(b"\n").await
}

async fn write_event<W: AsyncWrite + Unpin>(
    writer: &mut W,
    event: &EventEnvelope,
) -> io::Result<()> {
    let value = serde_json::to_value(NotificationEnvelope::new(method::EVENT, event))
        .map_err(io::Error::other)?;
    write_json(writer, &value).await
}

fn set_private_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn ensure_socket_parent(parent: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

        match std::fs::symlink_metadata(parent) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let mut builder = std::fs::DirBuilder::new();
                builder.recursive(true).mode(0o700).create(parent)?;
            }
            Err(error) => return Err(error),
        }

        let metadata = std::fs::symlink_metadata(parent)?;
        if !metadata.file_type().is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("socket parent is not a directory: {}", parent.display()),
            ));
        }
        let expected_uid = rustix::process::geteuid().as_raw();
        if metadata.uid() != expected_uid {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "socket parent must be owned by uid {expected_uid}: {}",
                    parent.display()
                ),
            ));
        }
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "socket parent must not be accessible by group or other users (mode {mode:o}): {}",
                    parent.display()
                ),
            ));
        }
        Ok(())
    }

    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(parent)
    }
}

fn set_private_file(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(unix)]
struct SocketCleanup(PathBuf);

#[cfg(unix)]
impl Drop for SocketCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        future::Future,
        pin::Pin,
        sync::atomic::{AtomicUsize, Ordering},
        task::{Context, Poll},
        time::Duration,
    };
    use tokio::{
        io::{duplex, split, AsyncWriteExt, DuplexStream, ReadHalf, WriteHalf},
        sync::Notify,
    };
    use yeux_core::{ModelEventSink, ModelProvider, PortError, SequenceIdGenerator, SystemClock};
    use yeux_protocol::{
        ClientCapabilities, ClientInfo, ContentBlock, EffectSet, Idempotency, InitializeParams,
        InvocationId, InvocationState, ModelEvent, ModelRequest, ProviderCapabilities, StopReason,
        TokenBudget, PROTOCOL_VERSION,
    };

    #[derive(Debug)]
    struct FauxProvider;

    impl ModelProvider for FauxProvider {
        fn provider_id(&self) -> &str {
            "faux"
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities::default()
        }

        fn stream<'life0, 'life1, 'async_trait>(
            &'life0 self,
            _request: ModelRequest,
            sink: &'life1 mut (dyn ModelEventSink + Send),
        ) -> Pin<Box<dyn Future<Output = Result<(), PortError>> + Send + 'async_trait>>
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async move {
                sink.emit(ModelEvent::TextDelta {
                    text: "faux answer".into(),
                })
                .await?;
                sink.emit(ModelEvent::Completed {
                    stop_reason: StopReason::EndTurn,
                })
                .await
            })
        }
    }

    #[derive(Debug, Default)]
    struct ToolLoopProvider {
        round: AtomicUsize,
        requests: Mutex<Vec<ModelRequest>>,
    }

    impl ModelProvider for ToolLoopProvider {
        fn provider_id(&self) -> &str {
            "tool-loop"
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
                match self.round.fetch_add(1, Ordering::SeqCst) {
                    0 => {
                        sink.emit(ModelEvent::ToolCallDelta {
                            call_id: "read-facts".into(),
                            name: "workspace.read".into(),
                            json_delta: "{\"path\":\"facts.txt\"}".into(),
                        })
                        .await?;
                        sink.emit(ModelEvent::Completed {
                            stop_reason: StopReason::ToolUse,
                        })
                        .await
                    }
                    1 => {
                        sink.emit(ModelEvent::TextDelta {
                            text: "facts integrated".into(),
                        })
                        .await?;
                        sink.emit(ModelEvent::Completed {
                            stop_reason: StopReason::EndTurn,
                        })
                        .await
                    }
                    _ => Err(PortError {
                        code: "unexpected_round".into(),
                        message: "tool-loop provider was called too many times".into(),
                        retryable: false,
                    }),
                }
            })
        }
    }

    #[derive(Debug)]
    struct ControlledProvider {
        started: Arc<Notify>,
        release: Arc<Notify>,
    }

    #[derive(Default)]
    struct FailAfterFirstFlush {
        flushes: usize,
    }

    impl AsyncWrite for FailAfterFirstFlush {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            buffer: &[u8],
        ) -> Poll<io::Result<usize>> {
            if self.flushes > 0 {
                Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "injected response disconnect",
                )))
            } else {
                Poll::Ready(Ok(buffer.len()))
            }
        }

        fn poll_flush(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            self.flushes += 1;
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    impl ModelProvider for ControlledProvider {
        fn provider_id(&self) -> &str {
            "controlled"
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities::default()
        }

        fn stream<'life0, 'life1, 'async_trait>(
            &'life0 self,
            _request: ModelRequest,
            sink: &'life1 mut (dyn ModelEventSink + Send),
        ) -> Pin<Box<dyn Future<Output = Result<(), PortError>> + Send + 'async_trait>>
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async move {
                sink.emit(ModelEvent::TextDelta {
                    text: "before interrupt".into(),
                })
                .await?;
                self.started.notify_one();
                self.release.notified().await;
                sink.emit(ModelEvent::TextDelta {
                    text: "residual delta".into(),
                })
                .await?;
                sink.emit(ModelEvent::Completed {
                    stop_reason: StopReason::EndTurn,
                })
                .await
            })
        }
    }

    async fn send_json(
        writer: &mut WriteHalf<DuplexStream>,
        reader: &mut BufReader<ReadHalf<DuplexStream>>,
        value: Value,
    ) -> Value {
        let mut bytes = serde_json::to_vec(&value).unwrap();
        bytes.push(b'\n');
        writer.write_all(&bytes).await.unwrap();
        writer.flush().await.unwrap();
        read_json(reader).await
    }

    async fn read_json(reader: &mut BufReader<ReadHalf<DuplexStream>>) -> Value {
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut line))
            .await
            .expect("runtime response timed out")
            .unwrap();
        serde_json::from_str(&line).unwrap()
    }

    fn wire_command(method_name: &str, params: Value) -> Value {
        json!({
            "jsonrpc": JSONRPC_VERSION,
            "id": method_name,
            "command_id": uuid::Uuid::now_v7(),
            "method": method_name,
            "params": params,
        })
    }

    fn direct_command(
        daemon: &Daemon,
        state: &mut ConnectionState,
        method_name: &str,
        params: Value,
    ) -> Value {
        daemon
            .handle_line(
                &serde_json::to_string(&wire_command(method_name, params)).unwrap(),
                state,
            )
            .0
    }

    fn append_test_event(
        ledger: &EventLedger,
        thread_id: ThreadId,
        turn_id: TurnId,
        agent_id: &str,
        event: Event,
    ) {
        let serialized = serde_json::to_value(event).unwrap();
        ledger
            .append(NewLedgerEvent {
                schema_version: PROTOCOL_VERSION,
                event_id: uuid::Uuid::now_v7().to_string(),
                thread_id: thread_id.to_string(),
                turn_id: Some(turn_id.to_string()),
                agent_id: agent_id.into(),
                time: chrono::Utc::now(),
                causation_id: Some("restart-fixture".into()),
                kind: serialized["kind"].as_str().unwrap().into(),
                payload: serialized["payload"].clone(),
            })
            .unwrap();
    }

    fn append_invocation_fixture(
        ledger: &EventLedger,
        thread_id: ThreadId,
        turn_id: TurnId,
        invocation_id: InvocationId,
        target_state: InvocationState,
        idempotency: Idempotency,
        agent_id: &str,
    ) {
        let effects = EffectSet {
            idempotency,
            ..EffectSet::default()
        };
        append_test_event(
            ledger,
            thread_id,
            turn_id,
            agent_id,
            Event::InvocationProposed {
                invocation_id,
                call_id: format!("fixture-{invocation_id}"),
                tool_id: "fixture.tool".into(),
                tool_version: "1".into(),
                normalized_arguments_digest: digest_value(&json!({"fixture": invocation_id})),
                effect_digest: digest_value(&serde_json::to_value(&effects).unwrap()),
                effects,
                idempotency,
            },
        );

        let transitions: &[InvocationState] = match target_state {
            InvocationState::Proposed => &[],
            InvocationState::Approved => &[InvocationState::Approved],
            InvocationState::Prepared => &[InvocationState::Approved, InvocationState::Prepared],
            InvocationState::Started => &[
                InvocationState::Approved,
                InvocationState::Prepared,
                InvocationState::Started,
            ],
            InvocationState::Completed => &[
                InvocationState::Approved,
                InvocationState::Prepared,
                InvocationState::Started,
                InvocationState::Completed,
            ],
            InvocationState::Failed => &[InvocationState::Failed],
            InvocationState::Cancelled => &[InvocationState::Cancelled],
            InvocationState::Unknown => &[
                InvocationState::Approved,
                InvocationState::Prepared,
                InvocationState::Started,
                InvocationState::Unknown,
            ],
        };
        let mut from = InvocationState::Proposed;
        for to in transitions {
            append_test_event(
                ledger,
                thread_id,
                turn_id,
                agent_id,
                Event::InvocationStateChanged {
                    invocation_id,
                    from,
                    to: *to,
                    reason: None,
                },
            );
            from = *to;
        }
    }

    #[test]
    fn wrong_thread_interrupt_does_not_cancel_the_real_turn() {
        let state_dir = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let daemon =
            Daemon::open(DaemonConfig::in_directory(state_dir.path()).without_turn_execution())
                .unwrap();
        let mut state = ConnectionState {
            initialized: true,
            ..ConnectionState::default()
        };
        let opened = direct_command(
            &daemon,
            &mut state,
            method::WORKSPACE_OPEN,
            json!({ "path": workspace.path() }),
        );
        let workspace_id = opened["result"]["workspace"]["id"].clone();
        let first = direct_command(
            &daemon,
            &mut state,
            method::THREAD_START,
            json!({ "workspaceId": workspace_id }),
        );
        let second = direct_command(
            &daemon,
            &mut state,
            method::THREAD_START,
            json!({ "workspaceId": workspace_id }),
        );
        let real_thread_id = first["result"]["thread"]["id"].clone();
        let wrong_thread_id = second["result"]["thread"]["id"].clone();
        let started = direct_command(
            &daemon,
            &mut state,
            method::TURN_START,
            json!({
                "threadId": real_thread_id,
                "content": [{"type": "text", "text": "keep running"}]
            }),
        );
        let turn_id = started["result"]["turn"]["id"]
            .as_str()
            .unwrap()
            .parse::<TurnId>()
            .unwrap();
        let cancellation = Arc::new(CancellationFlag::default());
        daemon
            .inner
            .active_turns
            .lock()
            .unwrap()
            .insert(turn_id, Arc::clone(&cancellation));

        let response = direct_command(
            &daemon,
            &mut state,
            method::TURN_INTERRUPT,
            json!({
                "threadId": wrong_thread_id,
                "turnId": turn_id,
                "reason": "wrong target"
            }),
        );

        assert_eq!(response["error"]["code"], INVALID_STATE);
        assert!(!crate::runner::CancellationCheck::is_cancelled(
            cancellation.as_ref()
        ));
        assert_eq!(
            daemon.projection().unwrap().turns[&turn_id].state,
            TurnState::Accepted
        );
    }

    #[test]
    fn failed_interrupt_commit_does_not_cancel_the_runner() {
        let state_dir = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let mut ids = (0..9).map(|_| uuid::Uuid::now_v7()).collect::<Vec<_>>();
        ids.push(ids[8]);
        let daemon = Daemon::open_with(
            DaemonConfig::in_directory(state_dir.path()).without_turn_execution(),
            Arc::new(SystemClock),
            Arc::new(SequenceIdGenerator::new(ids).unwrap()),
        )
        .unwrap();
        let mut state = ConnectionState {
            initialized: true,
            ..ConnectionState::default()
        };
        let opened = direct_command(
            &daemon,
            &mut state,
            method::WORKSPACE_OPEN,
            json!({ "path": workspace.path() }),
        );
        let created = direct_command(
            &daemon,
            &mut state,
            method::THREAD_START,
            json!({ "workspaceId": opened["result"]["workspace"]["id"] }),
        );
        let thread_id = created["result"]["thread"]["id"].clone();
        let started = direct_command(
            &daemon,
            &mut state,
            method::TURN_START,
            json!({
                "threadId": thread_id,
                "content": [{"type": "text", "text": "keep running"}]
            }),
        );
        let turn_id = started["result"]["turn"]["id"]
            .as_str()
            .unwrap()
            .parse::<TurnId>()
            .unwrap();
        let cancellation = Arc::new(CancellationFlag::default());
        daemon
            .inner
            .active_turns
            .lock()
            .unwrap()
            .insert(turn_id, Arc::clone(&cancellation));

        let response = direct_command(
            &daemon,
            &mut state,
            method::TURN_INTERRUPT,
            json!({
                "threadId": thread_id,
                "turnId": turn_id,
                "reason": "injected commit failure"
            }),
        );

        assert_eq!(response["error"]["code"], RpcError::INTERNAL_ERROR);
        assert!(!crate::runner::CancellationCheck::is_cancelled(
            cancellation.as_ref()
        ));
        assert_eq!(
            daemon.projection().unwrap().turns[&turn_id].state,
            TurnState::Accepted
        );
    }

    #[tokio::test]
    async fn committed_turn_runs_even_when_its_response_cannot_be_written() {
        let state_dir = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let daemon = Daemon::open(DaemonConfig::in_directory(state_dir.path())).unwrap();
        let mut control = ConnectionState {
            initialized: true,
            ..ConnectionState::default()
        };
        let opened = direct_command(
            &daemon,
            &mut control,
            method::WORKSPACE_OPEN,
            json!({ "path": workspace.path() }),
        );
        let created = direct_command(
            &daemon,
            &mut control,
            method::THREAD_START,
            json!({ "workspaceId": opened["result"]["workspace"]["id"] }),
        );
        let thread_id = created["result"]["thread"]["id"].clone();
        let initialize = wire_command(
            method::INITIALIZE,
            serde_json::to_value(InitializeParams {
                protocol_version: PROTOCOL_VERSION,
                client_info: ClientInfo {
                    name: "disconnect-test".into(),
                    version: "0".into(),
                },
                capabilities: ClientCapabilities::default(),
            })
            .unwrap(),
        );
        let turn = wire_command(
            method::TURN_START,
            json!({
                "threadId": thread_id,
                "content": [{"type": "text", "text": "disconnect now"}]
            }),
        );
        let input = format!(
            "{}\n{}\n",
            serde_json::to_string(&initialize).unwrap(),
            serde_json::to_string(&turn).unwrap()
        );
        let reader = BufReader::new(input.as_bytes());
        let mut writer = FailAfterFirstFlush::default();

        let error = daemon
            .serve_connection(reader, &mut writer)
            .await
            .unwrap_err();
        assert!(
            matches!(error, DaemonError::Io(ref error) if error.kind() == io::ErrorKind::BrokenPipe)
        );

        let recovered_turn = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(turn) = daemon
                    .projection()
                    .unwrap()
                    .turns
                    .values()
                    .find(|turn| turn.thread_id.to_string() == thread_id.as_str().unwrap())
                    .filter(|turn| turn.state.is_terminal())
                    .cloned()
                {
                    break turn;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("committed turn remained active after response disconnect");
        assert_eq!(recovered_turn.state, TurnState::Failed);
        assert_eq!(
            recovered_turn.failure.as_deref(),
            Some("no model provider is configured for this daemon")
        );
    }

    #[test]
    fn restart_fails_orphaned_turn_and_allows_a_new_turn() {
        let state_dir = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let (thread_id, orphaned_turn_id) = {
            let daemon =
                Daemon::open(DaemonConfig::in_directory(state_dir.path()).without_turn_execution())
                    .unwrap();
            let mut state = ConnectionState {
                initialized: true,
                ..ConnectionState::default()
            };
            let opened = direct_command(
                &daemon,
                &mut state,
                method::WORKSPACE_OPEN,
                json!({ "path": workspace.path() }),
            );
            let created = direct_command(
                &daemon,
                &mut state,
                method::THREAD_START,
                json!({ "workspaceId": opened["result"]["workspace"]["id"] }),
            );
            let thread_id = created["result"]["thread"]["id"].clone();
            let started = direct_command(
                &daemon,
                &mut state,
                method::TURN_START,
                json!({
                    "threadId": thread_id,
                    "content": [{"type": "text", "text": "crash fixture"}]
                }),
            );
            (
                thread_id,
                started["result"]["turn"]["id"]
                    .as_str()
                    .unwrap()
                    .parse::<TurnId>()
                    .unwrap(),
            )
        };

        let daemon =
            Daemon::open(DaemonConfig::in_directory(state_dir.path()).without_turn_execution())
                .unwrap();
        let projection = daemon.projection().unwrap();
        assert_eq!(projection.turns[&orphaned_turn_id].state, TurnState::Failed);
        assert_eq!(
            projection.turns[&orphaned_turn_id].failure.as_deref(),
            Some("daemon restarted before the turn completed")
        );
        assert!(daemon
            .inner
            .ledger
            .replay(thread_id.as_str().unwrap(), 0)
            .unwrap()
            .iter()
            .any(|event| event.kind == "runtime/diagnostic"
                && event.payload["code"] == "turn_interrupted_by_restart"));

        let mut state = ConnectionState {
            initialized: true,
            ..ConnectionState::default()
        };
        let next = direct_command(
            &daemon,
            &mut state,
            method::TURN_START,
            json!({
                "threadId": thread_id,
                "content": [{"type": "text", "text": "new turn"}]
            }),
        );
        assert!(next.get("error").is_none(), "{next}");
        assert_ne!(next["result"]["turn"]["id"], orphaned_turn_id.to_string());
    }

    #[test]
    fn restart_recovers_invocations_before_turn_without_replaying_started_work() {
        let state_dir = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let proposed = InvocationId::from_uuid(uuid::Uuid::now_v7());
        let approved = InvocationId::from_uuid(uuid::Uuid::now_v7());
        let prepared = InvocationId::from_uuid(uuid::Uuid::now_v7());
        let started_non_idempotent = InvocationId::from_uuid(uuid::Uuid::now_v7());
        let started_idempotent = InvocationId::from_uuid(uuid::Uuid::now_v7());
        let already_unknown = InvocationId::from_uuid(uuid::Uuid::now_v7());
        let completed = InvocationId::from_uuid(uuid::Uuid::now_v7());

        let (thread_id, turn_id) = {
            let daemon =
                Daemon::open(DaemonConfig::in_directory(state_dir.path()).without_turn_execution())
                    .unwrap();
            let mut state = ConnectionState {
                initialized: true,
                ..ConnectionState::default()
            };
            let opened = direct_command(
                &daemon,
                &mut state,
                method::WORKSPACE_OPEN,
                json!({ "path": workspace.path() }),
            );
            let created = direct_command(
                &daemon,
                &mut state,
                method::THREAD_START,
                json!({ "workspaceId": opened["result"]["workspace"]["id"] }),
            );
            let thread_id = created["result"]["thread"]["id"]
                .as_str()
                .unwrap()
                .parse::<ThreadId>()
                .unwrap();
            let started = direct_command(
                &daemon,
                &mut state,
                method::TURN_START,
                json!({
                    "threadId": thread_id,
                    "content": [{"type": "text", "text": "restart fixture"}]
                }),
            );
            let turn_id = started["result"]["turn"]["id"]
                .as_str()
                .unwrap()
                .parse::<TurnId>()
                .unwrap();

            for (invocation_id, target_state, idempotency, agent_id) in [
                (
                    proposed,
                    InvocationState::Proposed,
                    Idempotency::Idempotent,
                    "proposal-agent",
                ),
                (
                    approved,
                    InvocationState::Approved,
                    Idempotency::Idempotent,
                    "approved-agent",
                ),
                (
                    prepared,
                    InvocationState::Prepared,
                    Idempotency::IdempotentWithKey,
                    "prepared-agent",
                ),
                (
                    started_non_idempotent,
                    InvocationState::Started,
                    Idempotency::NonIdempotent,
                    "non-idempotent-agent",
                ),
                (
                    started_idempotent,
                    InvocationState::Started,
                    Idempotency::IdempotentWithKey,
                    "idempotent-agent",
                ),
                (
                    already_unknown,
                    InvocationState::Unknown,
                    Idempotency::Unknown,
                    "unknown-agent",
                ),
                (
                    completed,
                    InvocationState::Completed,
                    Idempotency::Idempotent,
                    "completed-agent",
                ),
            ] {
                append_invocation_fixture(
                    daemon.inner.ledger.as_ref(),
                    thread_id,
                    turn_id,
                    invocation_id,
                    target_state,
                    idempotency,
                    agent_id,
                );
            }
            (thread_id, turn_id)
        };

        let first_restart =
            Daemon::open(DaemonConfig::in_directory(state_dir.path()).without_turn_execution())
                .unwrap();
        let projection = first_restart.projection().unwrap();
        assert_eq!(
            projection.invocations[&proposed].state,
            InvocationState::Failed
        );
        assert_eq!(
            projection.invocations[&approved].state,
            InvocationState::Failed
        );
        assert_eq!(
            projection.invocations[&prepared].state,
            InvocationState::Failed
        );
        assert_eq!(
            projection.invocations[&started_non_idempotent].state,
            InvocationState::Unknown
        );
        assert_eq!(
            projection.invocations[&started_idempotent].state,
            InvocationState::Unknown
        );
        assert_eq!(
            projection.invocations[&already_unknown].state,
            InvocationState::Unknown
        );
        assert_eq!(
            projection.invocations[&completed].state,
            InvocationState::Completed
        );
        assert!(projection
            .invocations
            .values()
            .all(|invocation| invocation.state != InvocationState::Started));
        assert_eq!(projection.turns[&turn_id].state, TurnState::Failed);

        // Recovery of work that never crossed the execution boundary must be
        // a complete Failed outcome, not a bare state marker.  The typed
        // ledger path above commits one ToolResult with each of these three
        // transitions, while Started work remains marker-only Unknown.
        for invocation_id in [proposed, approved, prepared] {
            assert!(projection.items.values().any(|item| {
                item.kind == ItemKind::ToolResult
                    && item.content["invocation_id"] == invocation_id.to_string()
            }));
        }

        let events = first_restart
            .inner
            .ledger
            .replay(&thread_id.to_string(), 0)
            .unwrap();
        let recovery_events = events
            .iter()
            .filter(|event| {
                event
                    .causation_id
                    .as_deref()
                    .is_some_and(|id| id.starts_with("daemon-restart:"))
            })
            .collect::<Vec<_>>();
        let turn_failure_index = recovery_events
            .iter()
            .position(|event| event.kind == "turn/state_changed")
            .unwrap();
        assert!(
            recovery_events[..turn_failure_index]
                .iter()
                .filter(|event| event.kind == "tool/state_changed")
                .count()
                == 5
        );
        assert!(recovery_events[turn_failure_index + 1..]
            .iter()
            .all(|event| event.kind != "tool/state_changed"));

        let non_idempotent_recovery = recovery_events
            .iter()
            .find(|event| {
                event.kind == "tool/state_changed"
                    && event.payload["invocation_id"] == started_non_idempotent.to_string()
            })
            .unwrap();
        assert_eq!(non_idempotent_recovery.payload["from"], "started");
        assert_eq!(non_idempotent_recovery.payload["to"], "unknown");
        assert_eq!(non_idempotent_recovery.agent_id, "non-idempotent-agent");
        assert_eq!(
            non_idempotent_recovery.turn_id.as_deref(),
            Some(turn_id.to_string().as_str())
        );
        assert!(!recovery_events.iter().any(|event| {
            event.kind == "tool/state_changed"
                && event.payload["invocation_id"] == started_non_idempotent.to_string()
                && event.payload["from"] == "unknown"
                && event.payload["to"] == "started"
        }));

        // A second restart has nothing left to mutate. This also proves that
        // the recovery batch can be replayed into a stable core projection.
        let event_count = events.len();
        drop(first_restart);
        let second_restart =
            Daemon::open(DaemonConfig::in_directory(state_dir.path()).without_turn_execution())
                .unwrap();
        let rebuilt = second_restart.inner.ledger.project_core().unwrap();
        assert_eq!(
            second_restart
                .inner
                .ledger
                .replay(&thread_id.to_string(), 0)
                .unwrap()
                .len(),
            event_count
        );
        assert_eq!(rebuilt.invocations, projection.invocations);
        assert_eq!(rebuilt.turns[&turn_id], projection.turns[&turn_id]);
    }

    #[tokio::test]
    async fn cancelled_frame_read_preserves_partial_input() {
        let (mut client, server) = duplex(1_024);
        let mut reader = BufReader::new(server);
        let mut buffered = Vec::new();
        client.write_all(b"{\"json").await.unwrap();

        let timed_out = tokio::time::timeout(
            Duration::from_millis(10),
            read_frame(&mut reader, &mut buffered, 1_024),
        )
        .await;
        assert!(timed_out.is_err());
        assert_eq!(buffered, b"{\"json");

        client.write_all(b"rpc\":\"2.0\"}\n").await.unwrap();
        let frame = read_frame(&mut reader, &mut buffered, 1_024).await.unwrap();
        let Frame::Line(line) = frame else {
            panic!("expected a complete frame");
        };
        assert_eq!(line, b"{\"jsonrpc\":\"2.0\"}");
    }

    #[tokio::test]
    async fn oversized_frame_returns_json_rpc_error_before_disconnect() {
        let state = tempfile::tempdir().unwrap();
        let mut config = DaemonConfig::in_directory(state.path());
        config.max_line_bytes = 8;
        let daemon = Daemon::open(config).unwrap();
        let (client, server) = duplex(4_096);
        let (server_read, server_write) = split(server);
        let task = tokio::spawn(async move {
            daemon
                .serve_connection(BufReader::new(server_read), BufWriter::new(server_write))
                .await
        });
        let (client_read, mut client_write) = split(client);
        let mut client_read = BufReader::new(client_read);

        client_write.write_all(b"123456789\n").await.unwrap();
        let mut response = String::new();
        client_read.read_line(&mut response).await.unwrap();
        let response: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(response["error"]["code"], RpcError::INVALID_REQUEST);
        assert!(response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("too large"));
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn configured_provider_completes_a_turn_over_the_wire() {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let config =
            DaemonConfig::in_directory(state.path()).with_model_provider(ModelProviderConfig::new(
                Arc::new(FauxProvider),
                "faux-model",
                TokenBudget {
                    max_input_tokens: 4_096,
                    max_output_tokens: 256,
                },
            ));
        let daemon = Daemon::open(config).unwrap();
        let projection_daemon = daemon.clone();
        let (client, server) = duplex(64 * 1_024);
        let (server_read, server_write) = split(server);
        let task = tokio::spawn(async move {
            daemon
                .serve_connection(BufReader::new(server_read), BufWriter::new(server_write))
                .await
        });
        let (client_read, mut client_write) = split(client);
        let mut client_read = BufReader::new(client_read);

        let initialize = serde_json::to_value(InitializeParams {
            protocol_version: PROTOCOL_VERSION,
            client_info: ClientInfo {
                name: "wire-test".into(),
                version: "0".into(),
            },
            capabilities: ClientCapabilities {
                event_replay: true,
                ..ClientCapabilities::default()
            },
        })
        .unwrap();
        let response = send_json(
            &mut client_write,
            &mut client_read,
            wire_command(method::INITIALIZE, initialize),
        )
        .await;
        assert_eq!(
            response["result"]["protocolVersion"]["major"],
            PROTOCOL_VERSION.major
        );

        let response = send_json(
            &mut client_write,
            &mut client_read,
            wire_command(method::WORKSPACE_OPEN, json!({ "path": workspace.path() })),
        )
        .await;
        let workspace_id = response["result"]["workspace"]["id"].clone();
        let response = send_json(
            &mut client_write,
            &mut client_read,
            wire_command(method::THREAD_START, json!({ "workspaceId": workspace_id })),
        )
        .await;
        let thread_id = response["result"]["thread"]["id"].clone();
        let response = send_json(
            &mut client_write,
            &mut client_read,
            wire_command(
                method::THREAD_SUBSCRIBE,
                json!({ "threadId": thread_id, "afterSeq": 0 }),
            ),
        )
        .await;
        assert_eq!(response["result"]["replayedThroughSeq"], 1);
        assert_eq!(
            read_json(&mut client_read).await["params"]["kind"],
            "thread/started"
        );

        let response = send_json(
            &mut client_write,
            &mut client_read,
            wire_command(
                method::TURN_START,
                json!({
                    "threadId": thread_id,
                    "content": [ContentBlock::Text { text: "hello".into() }]
                }),
            ),
        )
        .await;
        let turn_id = response["result"]["turn"]["id"]
            .as_str()
            .unwrap()
            .parse::<TurnId>()
            .unwrap();

        let mut saw_text = false;
        loop {
            let notification = read_json(&mut client_read).await;
            let params = &notification["params"];
            saw_text |= params["kind"] == "model/event"
                && params["payload"]["model_event"]["type"] == "text_delta"
                && params["payload"]["model_event"]["text"] == "faux answer";
            if params["kind"] == "turn/state_changed" && params["payload"]["to"] == "completed" {
                break;
            }
        }
        assert!(saw_text);
        let projection = projection_daemon.projection().unwrap();
        assert_eq!(
            projection.turns[&turn_id].state,
            yeux_protocol::TurnState::Completed
        );
        assert!(projection.items.values().any(|item| {
            item.turn_id == turn_id
                && item.kind == yeux_protocol::ItemKind::AssistantMessage
                && item.content["content"][0]["text"] == "faux answer"
        }));

        client_write.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn structured_read_tool_loop_completes_over_the_wire() {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(
            workspace.path().join("facts.txt"),
            "ledger is authoritative",
        )
        .unwrap();
        let provider = Arc::new(ToolLoopProvider::default());
        let config =
            DaemonConfig::in_directory(state.path()).with_model_provider(ModelProviderConfig::new(
                provider.clone(),
                "tool-model",
                TokenBudget {
                    max_input_tokens: 4_096,
                    max_output_tokens: 256,
                },
            ));
        let daemon = Daemon::open(config).unwrap();
        let projection_daemon = daemon.clone();
        let (client, server) = duplex(128 * 1_024);
        let (server_read, server_write) = split(server);
        let task = tokio::spawn(async move {
            daemon
                .serve_connection(BufReader::new(server_read), BufWriter::new(server_write))
                .await
        });
        let (client_read, mut client_write) = split(client);
        let mut client_read = BufReader::new(client_read);

        let initialize = serde_json::to_value(InitializeParams {
            protocol_version: PROTOCOL_VERSION,
            client_info: ClientInfo {
                name: "tool-wire-test".into(),
                version: "0".into(),
            },
            capabilities: ClientCapabilities {
                event_replay: true,
                ..ClientCapabilities::default()
            },
        })
        .unwrap();
        send_json(
            &mut client_write,
            &mut client_read,
            wire_command(method::INITIALIZE, initialize),
        )
        .await;
        let opened = send_json(
            &mut client_write,
            &mut client_read,
            wire_command(method::WORKSPACE_OPEN, json!({ "path": workspace.path() })),
        )
        .await;
        let workspace_id = opened["result"]["workspace"]["id"].clone();
        let started = send_json(
            &mut client_write,
            &mut client_read,
            wire_command(method::THREAD_START, json!({ "workspaceId": workspace_id })),
        )
        .await;
        let thread_id = started["result"]["thread"]["id"].clone();
        let subscribed = send_json(
            &mut client_write,
            &mut client_read,
            wire_command(
                method::THREAD_SUBSCRIBE,
                json!({ "threadId": thread_id, "afterSeq": 0 }),
            ),
        )
        .await;
        assert_eq!(subscribed["result"]["replayedThroughSeq"], 1);
        assert_eq!(
            read_json(&mut client_read).await["params"]["kind"],
            "thread/started"
        );

        let started_turn = send_json(
            &mut client_write,
            &mut client_read,
            wire_command(
                method::TURN_START,
                json!({
                    "threadId": thread_id,
                    "content": [ContentBlock::Text { text: "read facts".into() }]
                }),
            ),
        )
        .await;
        let turn_id = started_turn["result"]["turn"]["id"]
            .as_str()
            .unwrap()
            .parse::<TurnId>()
            .unwrap();

        let mut saw_proposal = false;
        let mut saw_result = false;
        loop {
            let notification = read_json(&mut client_read).await;
            let params = &notification["params"];
            saw_proposal |= params["kind"] == "tool/proposed"
                && params["payload"]["tool_id"] == "workspace.read";
            saw_result |= params["kind"] == "item/added"
                && params["payload"]["item"]["kind"] == "tool_result"
                && params["payload"]["item"]["content"]["content"][0]["content"]["content"]
                    == "ledger is authoritative";
            if params["kind"] == "turn/state_changed" && params["payload"]["to"] == "completed" {
                break;
            }
        }
        assert!(saw_proposal);
        assert!(saw_result);

        {
            let requests = provider.requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert_eq!(requests[0].tools.len(), 3);
            assert!(requests[1]
                .messages
                .iter()
                .flat_map(|message| &message.content)
                .any(|block| matches!(
                    block,
                    ContentBlock::ToolResult { content, .. }
                        if content["content"] == "ledger is authoritative"
                )));
        }

        let projection = projection_daemon.projection().unwrap();
        assert_eq!(projection.turns[&turn_id].state, TurnState::Completed);
        assert!(projection
            .invocations
            .values()
            .any(|invocation| invocation.state == InvocationState::Completed));

        client_write.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn subscription_streams_long_replay_in_bounded_pages() {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let daemon = Daemon::open(DaemonConfig::in_directory(state.path())).unwrap();
        let mut control = ConnectionState {
            initialized: true,
            ..ConnectionState::default()
        };
        let (opened, _) = daemon.handle_line(
            &serde_json::to_string(&wire_command(
                method::WORKSPACE_OPEN,
                json!({ "path": workspace.path() }),
            ))
            .unwrap(),
            &mut control,
        );
        let workspace_id = opened["result"]["workspace"]["id"].clone();
        let (started, _) = daemon.handle_line(
            &serde_json::to_string(&wire_command(
                method::THREAD_START,
                json!({ "workspaceId": workspace_id }),
            ))
            .unwrap(),
            &mut control,
        );
        let thread_id = started["result"]["thread"]["id"]
            .as_str()
            .unwrap()
            .parse::<ThreadId>()
            .unwrap();
        let extra_events = REPLAY_PAGE_SIZE * 2 + 3;
        for index in 0..extra_events {
            daemon
                .inner
                .ledger
                .append(yeux_runtime::NewLedgerEvent::now(
                    thread_id.to_string(),
                    "runtime/diagnostic",
                    json!({
                        "code": "fixture",
                        "message": index.to_string(),
                        "recoverable": true
                    }),
                ))
                .unwrap();
        }

        let (client, server) = duplex(256 * 1_024);
        let (server_read, server_write) = split(server);
        let runtime = daemon.clone();
        let task = tokio::spawn(async move {
            runtime
                .serve_connection(BufReader::new(server_read), BufWriter::new(server_write))
                .await
        });
        let (client_read, mut client_write) = split(client);
        let mut client_read = BufReader::new(client_read);
        let initialize = serde_json::to_value(InitializeParams {
            protocol_version: PROTOCOL_VERSION,
            client_info: ClientInfo {
                name: "replay-test".into(),
                version: "0".into(),
            },
            capabilities: ClientCapabilities {
                event_replay: true,
                ..ClientCapabilities::default()
            },
        })
        .unwrap();
        send_json(
            &mut client_write,
            &mut client_read,
            wire_command(method::INITIALIZE, initialize),
        )
        .await;
        let response = send_json(
            &mut client_write,
            &mut client_read,
            wire_command(
                method::THREAD_SUBSCRIBE,
                json!({ "threadId": thread_id, "afterSeq": 0 }),
            ),
        )
        .await;
        let expected_count = extra_events + 1;
        assert_eq!(
            response["result"]["replayedThroughSeq"],
            expected_count as u64
        );
        for expected_seq in 1..=expected_count {
            let event = read_json(&mut client_read).await;
            assert_eq!(event["params"]["seq"], expected_seq as u64);
        }

        client_write.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn interrupt_discards_provider_deltas_emitted_after_cancellation() {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let provider = ControlledProvider {
            started: Arc::clone(&started),
            release: Arc::clone(&release),
        };
        let config =
            DaemonConfig::in_directory(state.path()).with_model_provider(ModelProviderConfig::new(
                Arc::new(provider),
                "controlled-model",
                TokenBudget {
                    max_input_tokens: 4_096,
                    max_output_tokens: 256,
                },
            ));
        let daemon = Daemon::open(config).unwrap();

        let (subscriber_client, subscriber_server) = duplex(64 * 1_024);
        let (subscriber_server_read, subscriber_server_write) = split(subscriber_server);
        let subscriber_daemon = daemon.clone();
        let subscriber_task = tokio::spawn(async move {
            subscriber_daemon
                .serve_connection(
                    BufReader::new(subscriber_server_read),
                    BufWriter::new(subscriber_server_write),
                )
                .await
        });
        let (subscriber_read, mut subscriber_write) = split(subscriber_client);
        let mut subscriber_read = BufReader::new(subscriber_read);

        let (control_client, control_server) = duplex(16 * 1_024);
        let (control_server_read, control_server_write) = split(control_server);
        let control_daemon = daemon.clone();
        let control_task = tokio::spawn(async move {
            control_daemon
                .serve_connection(
                    BufReader::new(control_server_read),
                    BufWriter::new(control_server_write),
                )
                .await
        });
        let (control_read, mut control_write) = split(control_client);
        let mut control_read = BufReader::new(control_read);

        let initialize = || {
            serde_json::to_value(InitializeParams {
                protocol_version: PROTOCOL_VERSION,
                client_info: ClientInfo {
                    name: "interrupt-test".into(),
                    version: "0".into(),
                },
                capabilities: ClientCapabilities {
                    event_replay: true,
                    ..ClientCapabilities::default()
                },
            })
            .unwrap()
        };
        send_json(
            &mut subscriber_write,
            &mut subscriber_read,
            wire_command(method::INITIALIZE, initialize()),
        )
        .await;
        send_json(
            &mut control_write,
            &mut control_read,
            wire_command(method::INITIALIZE, initialize()),
        )
        .await;
        let opened = send_json(
            &mut subscriber_write,
            &mut subscriber_read,
            wire_command(method::WORKSPACE_OPEN, json!({ "path": workspace.path() })),
        )
        .await;
        let created = send_json(
            &mut subscriber_write,
            &mut subscriber_read,
            wire_command(
                method::THREAD_START,
                json!({ "workspaceId": opened["result"]["workspace"]["id"] }),
            ),
        )
        .await;
        let thread_id = created["result"]["thread"]["id"].clone();
        send_json(
            &mut subscriber_write,
            &mut subscriber_read,
            wire_command(
                method::THREAD_SUBSCRIBE,
                json!({ "threadId": thread_id, "afterSeq": 1 }),
            ),
        )
        .await;

        let turn = send_json(
            &mut subscriber_write,
            &mut subscriber_read,
            wire_command(
                method::TURN_START,
                json!({
                    "threadId": thread_id,
                    "content": [{"type": "text", "text": "wait"}]
                }),
            ),
        )
        .await;
        let turn_id = turn["result"]["turn"]["id"].clone();
        tokio::time::timeout(Duration::from_secs(2), started.notified())
            .await
            .expect("provider did not begin streaming");
        let interrupted = send_json(
            &mut control_write,
            &mut control_read,
            wire_command(
                method::TURN_INTERRUPT,
                json!({
                    "threadId": thread_id,
                    "turnId": turn_id,
                    "reason": "test cancellation"
                }),
            ),
        )
        .await;
        assert_eq!(interrupted["result"]["accepted"], true);
        release.notify_one();

        let parsed_turn_id = turn_id.as_str().unwrap().parse::<TurnId>().unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let active = daemon
                    .inner
                    .active_turns
                    .lock()
                    .unwrap()
                    .contains_key(&parsed_turn_id);
                if !active {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancelled runner did not stop");

        let projection = daemon.projection().unwrap();
        assert_eq!(
            projection.turns[&parsed_turn_id].state,
            yeux_protocol::TurnState::Cancelled
        );
        let streamed_text: Vec<_> = projection
            .model_events
            .values()
            .flatten()
            .filter_map(|event| match event {
                ModelEvent::TextDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(streamed_text, ["before interrupt"]);

        subscriber_write.shutdown().await.unwrap();
        control_write.shutdown().await.unwrap();
        subscriber_task.await.unwrap().unwrap();
        control_task.await.unwrap().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn insecure_existing_socket_parent_is_rejected() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("shared");
        std::fs::create_dir(&parent).unwrap();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();

        let error = ensure_socket_parent(&parent).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[cfg(unix)]
    #[test]
    fn private_existing_socket_parent_is_accepted() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("private");
        std::fs::create_dir(&parent).unwrap();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).unwrap();

        ensure_socket_parent(&parent).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn newly_created_socket_parent_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("private").join("yeux");

        ensure_socket_parent(&parent).unwrap();

        let mode = std::fs::metadata(&parent).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }
}
