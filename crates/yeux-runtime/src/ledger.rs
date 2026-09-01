//! Append-only SQLite event ledger and deterministic projections.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
    sync::{Mutex, MutexGuard},
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;
use yeux_core::{
    can_reconcile_invocation, can_transition_invocation,
    can_transition_invocation_with_idempotency, digest_serializable, EventStore, PortError,
};
use yeux_protocol::{
    AgentId, CausationId, ContentBlock, EffectSet, Event, EventEnvelope, EventId, Idempotency,
    InvocationId, InvocationState, ItemKind, ProtocolVersion, ThreadId, TurnId,
};

/// A persisted event. `seq` is monotonic within a thread, not globally.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LedgerEvent {
    pub schema_version: ProtocolVersion,
    pub event_id: String,
    pub thread_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub agent_id: String,
    pub seq: u64,
    pub time: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub causation_id: Option<String>,
    pub kind: String,
    pub payload: Value,
}

/// Input to [`EventLedger::append`]. The ledger owns sequence allocation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewLedgerEvent {
    pub schema_version: ProtocolVersion,
    pub event_id: String,
    pub thread_id: String,
    pub turn_id: Option<String>,
    pub agent_id: String,
    pub time: DateTime<Utc>,
    pub causation_id: Option<String>,
    pub kind: String,
    pub payload: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewCommandReceipt {
    pub command_id: String,
    pub method: String,
    pub params_digest: String,
    pub response: Value,
    pub created_at: DateTime<Utc>,
}

/// The two durable events that expose one finished tool invocation.
///
/// The ledger stores the model-visible result first and the terminal state
/// second in one SQLite transaction. Consequently, every replay prefix either
/// sees neither event, sees a result while the invocation is still running, or
/// sees both; it never observes `Completed`/`Failed` without its `ToolResult`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewInvocationOutcome {
    pub tool_result: NewLedgerEvent,
    pub terminal_state: NewLedgerEvent,
}

/// A durable `Started -> Unknown` transition emitted when the runtime cannot
/// prove whether an invocation's external work completed.  This is deliberately
/// a separate input type/API from [`NewInvocationOutcome`]: an unknown marker
/// has no model-visible result and must never be mistaken for a successful or
/// failed terminal outcome.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewInvocationUnknown {
    pub state: NewLedgerEvent,
}

/// The two durable events emitted when an invocation's outcome is unknown but
/// the daemon has a bounded diagnostic it can show to the model.  The marker
/// remains non-terminal; pairing it with the diagnostic in one transaction
/// prevents a crash from exposing only one half of the explanation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewInvocationUnknownOutcome {
    pub unknown_state: NewLedgerEvent,
    pub tool_result: NewLedgerEvent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandReceipt {
    pub command_id: String,
    pub method: String,
    pub params_digest: String,
    pub response: Value,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandAppendResult {
    pub event: Option<LedgerEvent>,
    pub response: Value,
    /// True means the command had already committed and no event was appended.
    pub replayed: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandBatchAppendResult {
    pub events: Vec<LedgerEvent>,
    pub response: Value,
    /// True means the command had already committed and no event was appended.
    pub replayed: bool,
}

impl NewLedgerEvent {
    pub fn now(thread_id: impl Into<String>, kind: impl Into<String>, payload: Value) -> Self {
        Self {
            schema_version: yeux_protocol::PROTOCOL_VERSION,
            event_id: Uuid::now_v7().to_string(),
            thread_id: thread_id.into(),
            turn_id: None,
            agent_id: "agent_root".into(),
            time: Utc::now(),
            causation_id: None,
            kind: kind.into(),
            payload,
        }
    }
}

impl TryFrom<EventEnvelope> for LedgerEvent {
    type Error = LedgerError;

    fn try_from(envelope: EventEnvelope) -> LedgerResult<Self> {
        let serialized = serde_json::to_value(&envelope.event)?;
        let object = serialized.as_object().ok_or_else(|| {
            LedgerError::InvalidEnvelope("serialized Event must be an object".into())
        })?;
        let kind = object
            .get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| LedgerError::InvalidEnvelope("Event is missing kind".into()))?
            .to_owned();
        let payload = object.get("payload").cloned().unwrap_or(Value::Null);
        Ok(Self {
            schema_version: envelope.schema_version,
            event_id: envelope.event_id.to_string(),
            thread_id: envelope.thread_id.to_string(),
            turn_id: envelope.turn_id.map(|id| id.to_string()),
            agent_id: envelope.agent_id.to_string(),
            seq: envelope.seq,
            time: envelope.time,
            causation_id: envelope.causation_id.map(|id| id.to_string()),
            kind,
            payload,
        })
    }
}

impl TryFrom<LedgerEvent> for EventEnvelope {
    type Error = LedgerError;

    fn try_from(event: LedgerEvent) -> LedgerResult<Self> {
        if !yeux_protocol::PROTOCOL_VERSION.accepts(event.schema_version) {
            return Err(LedgerError::InvalidEnvelope(format!(
                "unsupported event schema {}.{}; runtime supports {}.{}",
                event.schema_version.major,
                event.schema_version.minor,
                yeux_protocol::PROTOCOL_VERSION.major,
                yeux_protocol::PROTOCOL_VERSION.minor,
            )));
        }
        let protocol_event: Event = serde_json::from_value(serde_json::json!({
            "kind": event.kind,
            "payload": event.payload,
        }))?;
        Ok(EventEnvelope {
            schema_version: event.schema_version,
            event_id: EventId::from_uuid(parse_uuid("event_id", &event.event_id)?),
            thread_id: ThreadId::from_uuid(parse_uuid("thread_id", &event.thread_id)?),
            turn_id: event
                .turn_id
                .as_deref()
                .map(|value| parse_uuid("turn_id", value).map(TurnId::from_uuid))
                .transpose()?,
            agent_id: AgentId::new(event.agent_id),
            seq: event.seq,
            time: event.time,
            causation_id: event.causation_id.map(CausationId::new),
            event: protocol_event,
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("serialized payload is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("event id {event_id} was reused with different contents")]
    EventIdConflict { event_id: String },
    #[error("command id {command_id} was reused for different method or parameters")]
    CommandIdConflict { command_id: String },
    #[error("an event-producing command must append at least one event")]
    EmptyEventBatch,
    #[error("invalid invocation outcome event batch: {0}")]
    InvalidInvocationOutcome(String),
    #[error("invocation {invocation_id} expected state {expected:?}, found {found:?}")]
    InvocationStateConflict {
        invocation_id: String,
        expected: InvocationState,
        found: Option<InvocationState>,
    },
    #[error("event sequence exhausted for {scope}")]
    SequenceOverflow { scope: String },
    #[error(
        "event sequence for thread {thread_id} is corrupt: expected {expected}, found {found}"
    )]
    SequenceGap {
        thread_id: String,
        expected: u64,
        found: u64,
    },
    #[error("ledger mutex is poisoned")]
    Poisoned,
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid protocol envelope: {0}")]
    InvalidEnvelope(String),
}

#[derive(Debug, thiserror::Error)]
pub enum CoreProjectionError {
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error(transparent)]
    Replay(#[from] yeux_core::ReplayError),
}

pub type LedgerResult<T> = std::result::Result<T, LedgerError>;

/// Durable event store. Updates and deletes are rejected by database triggers.
pub struct EventLedger {
    connection: Mutex<Connection>,
}

impl std::fmt::Debug for EventLedger {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EventLedger")
            .finish_non_exhaustive()
    }
}

impl EventLedger {
    pub fn open(path: impl AsRef<Path>) -> LedgerResult<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(path)?;
        configure(&connection)?;
        initialize_schema(&connection)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        }
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    pub fn open_in_memory() -> LedgerResult<Self> {
        let connection = Connection::open_in_memory()?;
        configure(&connection)?;
        initialize_schema(&connection)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    /// Append exactly once by `event_id` and allocate the next per-thread seq.
    ///
    /// Repeating the same event is idempotent. Reusing its id with different
    /// contents is rejected rather than silently accepting divergent history.
    pub fn append(&self, input: NewLedgerEvent) -> LedgerResult<LedgerEvent> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        if let Some(existing) = get_event_by_id(&transaction, &input.event_id)? {
            if new_event_matches(&input, &existing) {
                transaction.commit()?;
                return Ok(existing);
            }
            return Err(LedgerError::EventIdConflict {
                event_id: input.event_id,
            });
        }

        let current: u64 = transaction.query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM events WHERE thread_id = ?1",
            [&input.thread_id],
            |row| row.get(0),
        )?;
        let next = next_sqlite_sequence(current, format!("thread {}", input.thread_id))?;
        let current_append_order: u64 = transaction.query_row(
            "SELECT COALESCE(MAX(append_order), 0) FROM events",
            [],
            |row| row.get(0),
        )?;
        let append_order = next_sqlite_sequence(current_append_order, "global append order")?;
        let payload = serde_json::to_string(&input.payload)?;
        transaction.execute(
            "INSERT INTO events (
                append_order, schema_version, event_id, thread_id, turn_id, agent_id, seq,
                event_time, causation_id, kind, payload_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                append_order,
                serde_json::to_string(&input.schema_version)?,
                input.event_id,
                input.thread_id,
                input.turn_id,
                input.agent_id,
                next,
                input.time.to_rfc3339(),
                input.causation_id,
                input.kind,
                payload,
            ],
        )?;
        let event = get_event_by_id(&transaction, &input.event_id)?.expect("inserted event exists");
        transaction.commit()?;
        Ok(event)
    }

    /// Atomically append a runtime-produced event batch without a command
    /// receipt.
    ///
    /// This is the persistence primitive used when one logical runtime fact
    /// spans multiple events, for example an invocation terminal transition
    /// and its model-visible `ToolResult`. A crash can expose either the whole
    /// batch or none of it. Repeating an identical, fully committed batch is
    /// idempotent by event ID; partial or divergent reuse is rejected.
    pub fn append_batch(&self, inputs: Vec<NewLedgerEvent>) -> LedgerResult<Vec<LedgerEvent>> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let events = append_batch_transaction(&transaction, inputs)?;
        transaction.commit()?;
        Ok(events)
    }

    /// Atomically append a model-visible `ToolResult` and the matching terminal
    /// invocation state.
    ///
    /// This entry point validates the pair before acquiring a sequence number:
    /// both events must have the same protocol scope and causation, the result
    /// item must name the same invocation, and success/error polarity must agree
    /// with the terminal state. Unknown outcomes must use the explicit
    /// `tool/reconciled` event rather than an ordinary state transition.
    pub fn append_invocation_outcome(
        &self,
        input: NewInvocationOutcome,
    ) -> LedgerResult<Vec<LedgerEvent>> {
        validate_invocation_outcome(&input)?;
        let (invocation_id, expected_state, expected_final_state) =
            invocation_outcome_precondition(&input)?;
        self.append_invocation_events_checked(
            vec![input.tool_result, input.terminal_state],
            invocation_id,
            expected_state,
            expected_final_state,
        )
    }

    /// Persist the conservative marker for work that crossed the execution
    /// boundary but whose external outcome is no longer observable.
    ///
    /// This helper intentionally accepts only `Started -> Unknown`; callers
    /// cannot use it to manufacture a terminal state or to silently retry an
    /// invocation.  Reconciliation must subsequently use
    /// [`Self::append_invocation_reconciliation`] with explicit evidence.
    pub fn append_invocation_unknown(
        &self,
        input: NewInvocationUnknown,
    ) -> LedgerResult<LedgerEvent> {
        let invocation_id = validate_invocation_unknown(&input.state)?;
        let mut events = self.append_invocation_events_checked(
            vec![input.state],
            invocation_id,
            InvocationState::Started,
            InvocationState::Unknown,
        )?;
        Ok(events
            .pop()
            .expect("a one-event invocation marker always returns one event"))
    }

    /// Atomically append a `Started -> Unknown` marker and its bounded,
    /// model-visible diagnostic.  This still does not create a terminal
    /// outcome or authorize a retry; reconciliation must resolve `Unknown`
    /// with explicit evidence later.
    pub fn append_invocation_unknown_outcome(
        &self,
        input: NewInvocationUnknownOutcome,
    ) -> LedgerResult<Vec<LedgerEvent>> {
        let invocation_id = validate_invocation_unknown_outcome(&input)?;
        self.append_invocation_events_checked(
            vec![input.unknown_state, input.tool_result],
            invocation_id,
            InvocationState::Started,
            InvocationState::Unknown,
        )
    }

    /// Atomically persist a model-visible result together with an explicit
    /// reconciliation conclusion for a previously unknown invocation.
    ///
    /// The terminal event must be `tool/reconciled`; ordinary
    /// `tool/state_changed` events are rejected so a caller cannot turn an
    /// indeterminate side effect into a normal completion without evidence.
    pub fn append_invocation_reconciliation(
        &self,
        input: NewInvocationOutcome,
    ) -> LedgerResult<Vec<LedgerEvent>> {
        validate_invocation_reconciliation(&input)?;
        let (invocation_id, expected_state, expected_final_state) =
            invocation_outcome_precondition(&input)?;
        debug_assert_eq!(expected_state, InvocationState::Unknown);
        self.append_invocation_events_checked(
            vec![input.tool_result, input.terminal_state],
            invocation_id,
            InvocationState::Unknown,
            expected_final_state,
        )
    }

    /// Append invocation events while holding the ledger mutex and transaction
    /// together with a state precondition.  This closes the check/append race
    /// that would otherwise let two recovery workers both emit a transition
    /// from the same state.
    fn append_invocation_events_checked(
        &self,
        inputs: Vec<NewLedgerEvent>,
        invocation_id: InvocationId,
        expected_state: InvocationState,
        expected_final_state: InvocationState,
    ) -> LedgerResult<Vec<LedgerEvent>> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        // A fully committed retry is allowed even though the projected state
        // has already advanced to its terminal value.  The batch helper still
        // compares every field and rejects divergent reuse.  We nevertheless
        // rebuild the invocation history below for retries as well: otherwise
        // an outcome batch imported without a proposal (or against a malformed
        // proposal) could be replayed successfully and hide an unreplayable
        // ledger.
        let fully_existing = inputs.iter().try_fold(true, |all, input| {
            Ok::<_, LedgerError>(all && get_event_by_id(&transaction, &input.event_id)?.is_some())
        })?;
        let invocation_id = invocation_id.to_string();
        let found = invocation_state_in(&transaction, &invocation_id)?;
        if !fully_existing {
            if found != Some(expected_state) {
                return Err(LedgerError::InvocationStateConflict {
                    invocation_id: invocation_id.clone(),
                    expected: expected_state,
                    found,
                });
            }
        } else if found != Some(expected_final_state) {
            // An idempotent retry must describe the same durable outcome that
            // is already projected.  Without this check, a generic append
            // could pre-seed a pair and a later typed retry would report
            // success even though the invocation had subsequently been
            // reconciled to a different terminal state.
            return Err(LedgerError::InvocationStateConflict {
                invocation_id: invocation_id.clone(),
                expected: expected_final_state,
                found,
            });
        }
        if found.is_none() {
            return Err(LedgerError::InvalidInvocationOutcome(
                "invocation outcome requires a persisted proposal".into(),
            ));
        }

        // Bind every derived event to the original proposal's protocol scope.
        // A valid state transition with a forged thread/turn/agent envelope
        // would otherwise be appendable and only fail much later during
        // projection replay.  Reject it while the same transaction still
        // holds the invocation state precondition.
        let Some((proposal_thread, proposal_turn, proposal_agent, proposal_call_id)) =
            invocation_scope_in(&transaction, &invocation_id)?
        else {
            return Err(LedgerError::InvalidInvocationOutcome(
                "invocation outcome requires a persisted proposal".into(),
            ));
        };
        if inputs.iter().any(|input| {
            input.thread_id != proposal_thread
                || input.turn_id != proposal_turn
                || input.agent_id != proposal_agent
        }) {
            return Err(LedgerError::InvalidInvocationOutcome(
                "invocation outcome scope does not match its proposal".into(),
            ));
        }
        if inputs.iter().any(|input| {
            tool_result_call_id(input).is_some_and(|call_id| call_id != proposal_call_id)
        }) {
            return Err(LedgerError::InvalidInvocationOutcome(
                "ToolResult call_id does not match its invocation proposal".into(),
            ));
        }

        let events = append_batch_transaction(&transaction, inputs)?;
        transaction.commit()?;
        Ok(events)
    }

    /// Atomically append an event and durable command receipt.
    ///
    /// A repeated `command_id` with the same method and parameter digest returns
    /// the original response and does not append again. Divergent reuse fails.
    pub fn append_with_receipt(
        &self,
        input: NewLedgerEvent,
        receipt: NewCommandReceipt,
    ) -> LedgerResult<CommandAppendResult> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = get_command_receipt(&transaction, &receipt.command_id)? {
            if existing.method == receipt.method && existing.params_digest == receipt.params_digest
            {
                transaction.commit()?;
                return Ok(CommandAppendResult {
                    event: None,
                    response: existing.response,
                    replayed: true,
                });
            }
            return Err(LedgerError::CommandIdConflict {
                command_id: receipt.command_id,
            });
        }
        if get_event_by_id(&transaction, &input.event_id)?.is_some() {
            return Err(LedgerError::EventIdConflict {
                event_id: input.event_id,
            });
        }
        let current_seq: u64 = transaction.query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM events WHERE thread_id = ?1",
            [&input.thread_id],
            |row| row.get(0),
        )?;
        let seq = next_sqlite_sequence(current_seq, format!("thread {}", input.thread_id))?;
        let current_append_order: u64 = transaction.query_row(
            "SELECT COALESCE(MAX(append_order), 0) FROM events",
            [],
            |row| row.get(0),
        )?;
        let append_order = next_sqlite_sequence(current_append_order, "global append order")?;
        transaction.execute(
            "INSERT INTO events (
                append_order, schema_version, event_id, thread_id, turn_id, agent_id, seq,
                event_time, causation_id, kind, payload_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                append_order,
                serde_json::to_string(&input.schema_version)?,
                input.event_id,
                input.thread_id,
                input.turn_id,
                input.agent_id,
                seq,
                input.time.to_rfc3339(),
                input.causation_id,
                input.kind,
                serde_json::to_string(&input.payload)?,
            ],
        )?;
        transaction.execute(
            "INSERT INTO command_receipts
                (command_id, method, params_digest, response_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                receipt.command_id,
                receipt.method,
                receipt.params_digest,
                serde_json::to_string(&receipt.response)?,
                receipt.created_at.to_rfc3339(),
            ],
        )?;
        let event = get_event_by_id(&transaction, &input.event_id)?.expect("inserted event exists");
        let response = receipt.response;
        transaction.commit()?;
        Ok(CommandAppendResult {
            event: Some(event),
            response,
            replayed: false,
        })
    }

    /// Atomically append every event produced by one command and its durable
    /// receipt. Sequence numbers and global append order are allocated inside
    /// the same transaction, so a crash cannot expose a partial command.
    pub fn append_batch_with_receipt(
        &self,
        inputs: Vec<NewLedgerEvent>,
        receipt: NewCommandReceipt,
    ) -> LedgerResult<CommandBatchAppendResult> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = get_command_receipt(&transaction, &receipt.command_id)? {
            if existing.method == receipt.method && existing.params_digest == receipt.params_digest
            {
                transaction.commit()?;
                return Ok(CommandBatchAppendResult {
                    events: Vec::new(),
                    response: existing.response,
                    replayed: true,
                });
            }
            return Err(LedgerError::CommandIdConflict {
                command_id: receipt.command_id,
            });
        }
        if inputs.is_empty() {
            return Err(LedgerError::EmptyEventBatch);
        }

        // Preflight the entire batch before the first insert. The transaction
        // would roll partial writes back, but preflight also avoids allocating
        // transient sequence numbers for a batch known to conflict.
        let mut event_ids = BTreeSet::new();
        for input in &inputs {
            if !event_ids.insert(input.event_id.as_str())
                || get_event_by_id(&transaction, &input.event_id)?.is_some()
            {
                return Err(LedgerError::EventIdConflict {
                    event_id: input.event_id.clone(),
                });
            }
        }

        let mut last_seq_by_thread = BTreeMap::<String, u64>::new();
        for input in &inputs {
            if !last_seq_by_thread.contains_key(&input.thread_id) {
                let last_seq = transaction.query_row(
                    "SELECT COALESCE(MAX(seq), 0) FROM events WHERE thread_id = ?1",
                    [&input.thread_id],
                    |row| row.get(0),
                )?;
                last_seq_by_thread.insert(input.thread_id.clone(), last_seq);
            }
        }
        let mut append_order: u64 = transaction.query_row(
            "SELECT COALESCE(MAX(append_order), 0) FROM events",
            [],
            |row| row.get(0),
        )?;
        let mut events = Vec::with_capacity(inputs.len());
        for input in inputs {
            let last_seq = last_seq_by_thread
                .get_mut(&input.thread_id)
                .expect("every input thread was preloaded");
            *last_seq = next_sqlite_sequence(*last_seq, format!("thread {}", input.thread_id))?;
            append_order = next_sqlite_sequence(append_order, "global append order")?;
            let event = LedgerEvent {
                schema_version: input.schema_version,
                event_id: input.event_id,
                thread_id: input.thread_id,
                turn_id: input.turn_id,
                agent_id: input.agent_id,
                seq: *last_seq,
                time: input.time,
                causation_id: input.causation_id,
                kind: input.kind,
                payload: input.payload,
            };
            insert_event(&transaction, &event, append_order)?;
            events.push(event);
        }
        let stored_receipt = insert_command_receipt(&transaction, &receipt)?;
        transaction.commit()?;
        Ok(CommandBatchAppendResult {
            events,
            response: stored_receipt.response,
            replayed: false,
        })
    }

    pub fn command_receipt(&self, command_id: &str) -> LedgerResult<Option<CommandReceipt>> {
        let connection = self.lock()?;
        get_command_receipt(&connection, command_id)
    }

    /// Persist the response for a command that does not atomically append an
    /// event. Repeating an identical receipt is idempotent; divergent reuse is
    /// rejected. Event-producing code should prefer
    /// [`Self::append_with_receipt`] or [`Self::append_batch_with_receipt`].
    pub fn record_command_receipt(
        &self,
        receipt: NewCommandReceipt,
    ) -> LedgerResult<CommandReceipt> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = get_command_receipt(&transaction, &receipt.command_id)? {
            if existing.method == receipt.method && existing.params_digest == receipt.params_digest
            {
                transaction.commit()?;
                return Ok(existing);
            }
            return Err(LedgerError::CommandIdConflict {
                command_id: receipt.command_id,
            });
        }
        let stored = insert_command_receipt(&transaction, &receipt)?;
        transaction.commit()?;
        Ok(stored)
    }

    /// Import an already sequenced event. Intended for verified JSONL import.
    /// The next sequence must be exact; gaps and history rewrites are rejected.
    pub fn import(&self, event: &LedgerEvent) -> LedgerResult<()> {
        self.import_with_duplicate_policy(event, true)
    }

    fn import_with_duplicate_policy(
        &self,
        event: &LedgerEvent,
        duplicate_ok: bool,
    ) -> LedgerResult<()> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = get_event_by_id(&transaction, &event.event_id)? {
            if duplicate_ok && &existing == event {
                transaction.commit()?;
                return Ok(());
            }
            return Err(LedgerError::EventIdConflict {
                event_id: event.event_id.clone(),
            });
        }
        let current_seq: u64 = transaction.query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM events WHERE thread_id = ?1",
            [&event.thread_id],
            |row| row.get(0),
        )?;
        let expected = next_sqlite_sequence(current_seq, format!("thread {}", event.thread_id))?;
        if event.seq != expected {
            return Err(LedgerError::SequenceGap {
                thread_id: event.thread_id.clone(),
                expected,
                found: event.seq,
            });
        }
        let current_append_order: u64 = transaction.query_row(
            "SELECT COALESCE(MAX(append_order), 0) FROM events",
            [],
            |row| row.get(0),
        )?;
        let append_order = next_sqlite_sequence(current_append_order, "global append order")?;
        transaction.execute(
            "INSERT INTO events (
                append_order, schema_version, event_id, thread_id, turn_id, agent_id, seq,
                event_time, causation_id, kind, payload_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                append_order,
                serde_json::to_string(&event.schema_version)?,
                event.event_id,
                event.thread_id,
                event.turn_id,
                event.agent_id,
                event.seq,
                event.time.to_rfc3339(),
                event.causation_id,
                event.kind,
                serde_json::to_string(&event.payload)?,
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Read ordered events after `after_seq` without executing any adapter.
    pub fn replay(&self, thread_id: &str, after_seq: u64) -> LedgerResult<Vec<LedgerEvent>> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT schema_version, event_id, thread_id, turn_id, agent_id, seq,
                    event_time, causation_id, kind, payload_json
             FROM events WHERE thread_id = ?1 AND seq > ?2 ORDER BY seq ASC",
        )?;
        let events = statement
            .query_map(params![thread_id, after_seq], row_to_event)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        validate_sequence(thread_id, after_seq.saturating_add(1), &events)?;
        Ok(events)
    }

    /// Read one bounded replay page. This is used by transports so reconnecting
    /// to a long thread never requires materializing the entire history.
    pub fn replay_page(
        &self,
        thread_id: &str,
        after_seq: u64,
        limit: usize,
    ) -> LedgerResult<Vec<LedgerEvent>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT schema_version, event_id, thread_id, turn_id, agent_id, seq,
                    event_time, causation_id, kind, payload_json
             FROM events WHERE thread_id = ?1 AND seq > ?2 ORDER BY seq ASC LIMIT ?3",
        )?;
        let events = statement
            .query_map(params![thread_id, after_seq, limit as u64], row_to_event)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        validate_sequence(thread_id, after_seq.saturating_add(1), &events)?;
        Ok(events)
    }

    pub fn all_events(&self) -> LedgerResult<Vec<LedgerEvent>> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT schema_version, event_id, thread_id, turn_id, agent_id, seq,
                    event_time, causation_id, kind, payload_json
             FROM events ORDER BY append_order ASC",
        )?;
        let rows = statement.query_map([], row_to_event)?;
        let events = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(events)
    }

    pub fn search(&self, query: &str, limit: usize) -> LedgerResult<Vec<LedgerEvent>> {
        let connection = self.lock()?;
        let pattern = format!("%{}%", query.replace('%', "\\%").replace('_', "\\_"));
        let mut statement = connection.prepare(
            "SELECT schema_version, event_id, thread_id, turn_id, agent_id, seq,
                    event_time, causation_id, kind, payload_json
             FROM events
             WHERE (kind LIKE ?1 ESCAPE '\\' OR payload_json LIKE ?1 ESCAPE '\\')
             ORDER BY event_time DESC LIMIT ?2",
        )?;
        let rows = statement.query_map(params![pattern, limit as u64], row_to_event)?;
        let events = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(events)
    }

    pub fn project_thread(&self, thread_id: &str) -> LedgerResult<ProjectionThread> {
        let events = self.replay(thread_id, 0)?;
        let mut projection = ProjectionThread::new(thread_id);
        for event in events {
            projection.apply(event);
        }
        Ok(projection)
    }

    pub fn project_all(&self) -> LedgerResult<Projection> {
        let events = self.all_events()?;
        let mut projection = Projection::default();
        for event in events {
            projection.apply(event);
        }
        Ok(projection)
    }

    /// Rebuild the canonical core projection in durable global append order.
    pub fn project_core(&self) -> Result<yeux_core::Projection, CoreProjectionError> {
        let events = self
            .all_events()?
            .into_iter()
            .map(EventEnvelope::try_from)
            .collect::<LedgerResult<Vec<_>>>()?;
        Ok(yeux_core::replay(&events)?)
    }

    fn lock(&self) -> LedgerResult<MutexGuard<'_, Connection>> {
        self.connection.lock().map_err(|_| LedgerError::Poisoned)
    }
}

/// Append a batch using an already-open transaction. Keeping allocation and
/// insertion in this helper lets typed invocation APIs add a state precondition
/// while retaining exactly the same all-or-nothing/idempotent behavior as the
/// public [`EventLedger::append_batch`] method.
fn append_batch_transaction(
    transaction: &Transaction<'_>,
    inputs: Vec<NewLedgerEvent>,
) -> LedgerResult<Vec<LedgerEvent>> {
    if inputs.is_empty() {
        return Err(LedgerError::EmptyEventBatch);
    }

    let mut event_ids = BTreeSet::new();
    let mut existing = Vec::with_capacity(inputs.len());
    for input in &inputs {
        if !event_ids.insert(input.event_id.as_str()) {
            return Err(LedgerError::EventIdConflict {
                event_id: input.event_id.clone(),
            });
        }
        existing.push(get_event_by_id(transaction, &input.event_id)?);
    }

    let existing_count = existing.iter().filter(|event| event.is_some()).count();
    if existing_count == inputs.len() {
        let mut replayed = Vec::with_capacity(inputs.len());
        for (input, event) in inputs.iter().zip(existing) {
            let event = event.expect("every event was counted as existing");
            if !new_event_matches(input, &event) {
                return Err(LedgerError::EventIdConflict {
                    event_id: input.event_id.clone(),
                });
            }
            replayed.push(event);
        }
        return Ok(replayed);
    }
    if existing_count != 0 {
        let event_id = inputs
            .iter()
            .zip(existing)
            .find_map(|(input, event)| event.map(|_| input.event_id.clone()))
            .expect("a partial batch contains an existing event");
        return Err(LedgerError::EventIdConflict { event_id });
    }

    let mut last_seq_by_thread = BTreeMap::<String, u64>::new();
    for input in &inputs {
        if !last_seq_by_thread.contains_key(&input.thread_id) {
            let last_seq = transaction.query_row(
                "SELECT COALESCE(MAX(seq), 0) FROM events WHERE thread_id = ?1",
                [&input.thread_id],
                |row| row.get(0),
            )?;
            last_seq_by_thread.insert(input.thread_id.clone(), last_seq);
        }
    }
    let mut append_order: u64 = transaction.query_row(
        "SELECT COALESCE(MAX(append_order), 0) FROM events",
        [],
        |row| row.get(0),
    )?;
    let mut events = Vec::with_capacity(inputs.len());
    for input in inputs {
        let last_seq = last_seq_by_thread
            .get_mut(&input.thread_id)
            .expect("every input thread was preloaded");
        *last_seq = next_sqlite_sequence(*last_seq, format!("thread {}", input.thread_id))?;
        append_order = next_sqlite_sequence(append_order, "global append order")?;
        let event = LedgerEvent {
            schema_version: input.schema_version,
            event_id: input.event_id,
            thread_id: input.thread_id,
            turn_id: input.turn_id,
            agent_id: input.agent_id,
            seq: *last_seq,
            time: input.time,
            causation_id: input.causation_id,
            kind: input.kind,
            payload: input.payload,
        };
        insert_event(transaction, &event, append_order)?;
        events.push(event);
    }
    Ok(events)
}

#[async_trait]
impl EventStore for EventLedger {
    async fn append(&self, event: &EventEnvelope) -> Result<(), PortError> {
        let event = LedgerEvent::try_from(event.clone()).map_err(ledger_port_error)?;
        self.import_with_duplicate_policy(&event, false)
            .map_err(ledger_port_error)
    }

    async fn load_thread(
        &self,
        thread_id: ThreadId,
        after_seq: u64,
    ) -> Result<Vec<EventEnvelope>, PortError> {
        self.replay(&thread_id.to_string(), after_seq)
            .map_err(ledger_port_error)?
            .into_iter()
            .map(|event| EventEnvelope::try_from(event).map_err(ledger_port_error))
            .collect()
    }
}

fn ledger_port_error(error: LedgerError) -> PortError {
    PortError {
        code: "event_store".into(),
        message: error.to_string(),
        retryable: matches!(
            &error,
            LedgerError::Database(rusqlite::Error::SqliteFailure(inner, _))
                if matches!(
                    inner.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                )
        ),
    }
}

fn validate_invocation_outcome(input: &NewInvocationOutcome) -> LedgerResult<()> {
    let result_scope = &input.tool_result;
    let terminal_scope = &input.terminal_state;
    if result_scope.schema_version != terminal_scope.schema_version
        || result_scope.thread_id != terminal_scope.thread_id
        || result_scope.turn_id != terminal_scope.turn_id
        || result_scope.agent_id != terminal_scope.agent_id
        || result_scope.causation_id != terminal_scope.causation_id
    {
        return Err(LedgerError::InvalidInvocationOutcome(
            "ToolResult and terminal state must share schema, thread, turn, agent, and causation"
                .into(),
        ));
    }
    if !yeux_protocol::PROTOCOL_VERSION.accepts(result_scope.schema_version) {
        return Err(LedgerError::InvalidInvocationOutcome(format!(
            "unsupported event schema {}.{}",
            result_scope.schema_version.major, result_scope.schema_version.minor,
        )));
    }
    if result_scope.turn_id.is_none() {
        return Err(LedgerError::InvalidInvocationOutcome(
            "invocation outcome events require a turn".into(),
        ));
    }

    let result_event = decode_new_event(result_scope)?;
    let terminal_event = decode_new_event(terminal_scope)?;
    let (invocation_id, terminal_state) = match terminal_event {
        Event::InvocationStateChanged {
            invocation_id,
            from,
            to,
            ..
        } if to.is_terminal() && can_transition_invocation(from, to) => (invocation_id, to),
        Event::InvocationStateChanged {
            from: InvocationState::Unknown,
            ..
        } => {
            return Err(LedgerError::InvalidInvocationOutcome(
                "Unknown must be resolved by tool/reconciled evidence".into(),
            ));
        }
        Event::InvocationReconciled {
            invocation_id,
            outcome,
            evidence,
        } => {
            if evidence.source.trim().is_empty() || evidence.summary.trim().is_empty() {
                return Err(LedgerError::InvalidInvocationOutcome(
                    "reconciliation evidence source and summary are required".into(),
                ));
            }
            (invocation_id, outcome.state())
        }
        _ => {
            return Err(LedgerError::InvalidInvocationOutcome(
                "terminal_state must be a terminal tool/state_changed or tool/reconciled event"
                    .into(),
            ));
        }
    };

    let item = match result_event {
        Event::ItemAdded { item } if item.kind == ItemKind::ToolResult => item,
        _ => {
            return Err(LedgerError::InvalidInvocationOutcome(
                "tool_result must be an item/added ToolResult".into(),
            ));
        }
    };
    if item.thread_id.to_string() != result_scope.thread_id
        || Some(item.turn_id.to_string()) != result_scope.turn_id
        || item.agent_id.to_string() != result_scope.agent_id
    {
        return Err(LedgerError::InvalidInvocationOutcome(
            "ToolResult item parent does not match its event envelope".into(),
        ));
    }
    let item_invocation_id: InvocationId =
        serde_json::from_value(item.content.get("invocation_id").cloned().ok_or_else(|| {
            LedgerError::InvalidInvocationOutcome("ToolResult item is missing invocation_id".into())
        })?)
        .map_err(|error| {
            LedgerError::InvalidInvocationOutcome(format!(
                "ToolResult item has an invalid invocation_id: {error}"
            ))
        })?;
    if item_invocation_id != invocation_id {
        return Err(LedgerError::InvalidInvocationOutcome(
            "ToolResult and terminal state name different invocations".into(),
        ));
    }
    let blocks: Vec<ContentBlock> =
        serde_json::from_value(item.content.get("content").cloned().ok_or_else(|| {
            LedgerError::InvalidInvocationOutcome(
                "ToolResult item is missing content blocks".into(),
            )
        })?)
        .map_err(|error| {
            LedgerError::InvalidInvocationOutcome(format!(
                "ToolResult item content is invalid: {error}"
            ))
        })?;
    let is_error = match blocks.as_slice() {
        [ContentBlock::ToolResult {
            call_id, is_error, ..
        }] if !call_id.trim().is_empty() => *is_error,
        _ => {
            return Err(LedgerError::InvalidInvocationOutcome(
                "ToolResult item must contain exactly one named tool_result block".into(),
            ));
        }
    };
    let expected_error = terminal_state != InvocationState::Completed;
    if is_error != expected_error {
        return Err(LedgerError::InvalidInvocationOutcome(format!(
            "ToolResult is_error={is_error} disagrees with terminal state {terminal_state:?}"
        )));
    }
    Ok(())
}

fn validate_invocation_unknown(input: &NewLedgerEvent) -> LedgerResult<InvocationId> {
    if !yeux_protocol::PROTOCOL_VERSION.accepts(input.schema_version) {
        return Err(LedgerError::InvalidInvocationOutcome(format!(
            "unsupported event schema {}.{}",
            input.schema_version.major, input.schema_version.minor,
        )));
    }
    if input.turn_id.is_none() {
        return Err(LedgerError::InvalidInvocationOutcome(
            "Started -> Unknown events require a turn".into(),
        ));
    }
    match decode_new_event(input)? {
        Event::InvocationStateChanged {
            invocation_id,
            from: InvocationState::Started,
            to: InvocationState::Unknown,
            reason,
            ..
        } => {
            if reason
                .as_deref()
                .is_none_or(|value| value.trim().is_empty())
            {
                return Err(LedgerError::InvalidInvocationOutcome(
                    "Started -> Unknown requires a non-empty reason".into(),
                ));
            }
            Ok(invocation_id)
        }
        _ => Err(LedgerError::InvalidInvocationOutcome(
            "unknown marker must be a Started -> Unknown tool/state_changed event".into(),
        )),
    }
}

fn validate_invocation_unknown_outcome(
    input: &NewInvocationUnknownOutcome,
) -> LedgerResult<InvocationId> {
    let state_scope = &input.unknown_state;
    let result_scope = &input.tool_result;
    if state_scope.event_id == result_scope.event_id {
        return Err(LedgerError::InvalidInvocationOutcome(
            "Unknown marker and ToolResult must use distinct event IDs".into(),
        ));
    }
    if state_scope.schema_version != result_scope.schema_version
        || state_scope.thread_id != result_scope.thread_id
        || state_scope.turn_id != result_scope.turn_id
        || state_scope.agent_id != result_scope.agent_id
        || state_scope.causation_id != result_scope.causation_id
    {
        return Err(LedgerError::InvalidInvocationOutcome(
            "Unknown marker and ToolResult must share schema, thread, turn, agent, and causation"
                .into(),
        ));
    }
    if !yeux_protocol::PROTOCOL_VERSION.accepts(state_scope.schema_version) {
        return Err(LedgerError::InvalidInvocationOutcome(format!(
            "unsupported event schema {}.{}",
            state_scope.schema_version.major, state_scope.schema_version.minor,
        )));
    }
    if state_scope.turn_id.is_none() {
        return Err(LedgerError::InvalidInvocationOutcome(
            "Unknown outcomes require a turn".into(),
        ));
    }

    let invocation_id = match decode_new_event(state_scope)? {
        Event::InvocationStateChanged {
            invocation_id,
            from: InvocationState::Started,
            to: InvocationState::Unknown,
            reason,
            ..
        } => {
            if reason
                .as_deref()
                .is_none_or(|value| value.trim().is_empty())
            {
                return Err(LedgerError::InvalidInvocationOutcome(
                    "Started -> Unknown requires a non-empty reason".into(),
                ));
            }
            invocation_id
        }
        _ => {
            return Err(LedgerError::InvalidInvocationOutcome(
                "unknown outcome marker must be a Started -> Unknown event".into(),
            ));
        }
    };

    let result_event = decode_new_event(result_scope)?;
    let item = match result_event {
        Event::ItemAdded { item } if item.kind == ItemKind::ToolResult => item,
        _ => {
            return Err(LedgerError::InvalidInvocationOutcome(
                "unknown outcome must include an item/added ToolResult".into(),
            ));
        }
    };
    if item.thread_id.to_string() != result_scope.thread_id
        || Some(item.turn_id.to_string()) != result_scope.turn_id
        || item.agent_id.to_string() != result_scope.agent_id
    {
        return Err(LedgerError::InvalidInvocationOutcome(
            "Unknown ToolResult item parent does not match its event envelope".into(),
        ));
    }
    let item_invocation_id: InvocationId =
        serde_json::from_value(item.content.get("invocation_id").cloned().ok_or_else(|| {
            LedgerError::InvalidInvocationOutcome(
                "Unknown ToolResult is missing invocation_id".into(),
            )
        })?)
        .map_err(|error| {
            LedgerError::InvalidInvocationOutcome(format!(
                "Unknown ToolResult has an invalid invocation_id: {error}"
            ))
        })?;
    if item_invocation_id != invocation_id {
        return Err(LedgerError::InvalidInvocationOutcome(
            "Unknown marker and ToolResult name different invocations".into(),
        ));
    }
    let blocks: Vec<ContentBlock> =
        serde_json::from_value(item.content.get("content").cloned().ok_or_else(|| {
            LedgerError::InvalidInvocationOutcome(
                "Unknown ToolResult is missing content blocks".into(),
            )
        })?)
        .map_err(|error| {
            LedgerError::InvalidInvocationOutcome(format!(
                "Unknown ToolResult content is invalid: {error}"
            ))
        })?;
    match blocks.as_slice() {
        [ContentBlock::ToolResult {
            call_id,
            is_error: true,
            ..
        }] if !call_id.trim().is_empty() => Ok(invocation_id),
        _ => Err(LedgerError::InvalidInvocationOutcome(
            "Unknown ToolResult must contain exactly one error tool_result block".into(),
        )),
    }
}

fn validate_invocation_reconciliation(input: &NewInvocationOutcome) -> LedgerResult<()> {
    validate_invocation_outcome(input)?;
    match decode_new_event(&input.terminal_state)? {
        Event::InvocationReconciled { .. } => Ok(()),
        _ => Err(LedgerError::InvalidInvocationOutcome(
            "reconciliation outcome must use the explicit tool/reconciled event".into(),
        )),
    }
}

/// Returns the state that must currently be projected before an outcome can be
/// committed together with the state the input batch will leave projected.
/// Normal terminal events carry their `from`/`to` states; reconciliation is
/// intentionally only legal after `Unknown`.
fn invocation_outcome_precondition(
    input: &NewInvocationOutcome,
) -> LedgerResult<(InvocationId, InvocationState, InvocationState)> {
    match decode_new_event(&input.terminal_state)? {
        Event::InvocationStateChanged {
            invocation_id,
            from,
            to,
            ..
        } if to.is_terminal() => Ok((invocation_id, from, to)),
        Event::InvocationReconciled {
            invocation_id,
            outcome,
            ..
        } => Ok((invocation_id, InvocationState::Unknown, outcome.state())),
        _ => Err(LedgerError::InvalidInvocationOutcome(
            "cannot determine invocation outcome precondition".into(),
        )),
    }
}

/// Reconstruct one invocation's state from durable events while the caller's
/// append transaction is still open.  This is intentionally a small, local
/// projection rather than a call to a runtime executor: recovery and outcome
/// commits must never perform external work.
fn invocation_state_in(
    connection: &Connection,
    invocation_id: &str,
) -> LedgerResult<Option<InvocationState>> {
    let mut statement = connection.prepare(
        "SELECT kind, payload_json FROM events
         WHERE kind IN ('tool/proposed', 'tool/state_changed', 'tool/reconciled')
         ORDER BY append_order ASC",
    )?;
    let mut rows = statement.query([])?;
    let mut state = None;
    let mut idempotency = None;
    while let Some(row) = rows.next()? {
        let kind: String = row.get(0)?;
        let payload_json: String = row.get(1)?;
        let payload: Value = serde_json::from_str(&payload_json).map_err(|error| {
            LedgerError::InvalidInvocationOutcome(format!(
                "invocation history contains invalid JSON payload: {error}"
            ))
        })?;
        let event: Event = serde_json::from_value(serde_json::json!({
            "kind": kind,
            "payload": payload,
        }))
        .map_err(|error| {
            LedgerError::InvalidInvocationOutcome(format!(
                "invocation history contains an incomplete protocol event: {error}"
            ))
        })?;
        match event {
            Event::InvocationProposed {
                invocation_id: candidate,
                call_id,
                tool_id,
                tool_version,
                normalized_arguments_digest,
                effects,
                effect_digest,
                idempotency: declared_idempotency,
                ..
            } if candidate.to_string() == invocation_id => {
                if state.is_some() {
                    return Err(LedgerError::InvalidInvocationOutcome(
                        "invocation has more than one proposal".into(),
                    ));
                }
                validate_invocation_proposal_evidence(InvocationProposalEvidence {
                    invocation_id: &candidate,
                    call_id: &call_id,
                    tool_id: &tool_id,
                    tool_version: &tool_version,
                    normalized_arguments_digest: &normalized_arguments_digest,
                    effects: &effects,
                    effect_digest: &effect_digest,
                    declared_idempotency,
                })?;
                idempotency = Some(declared_idempotency);
                state = Some(InvocationState::Proposed);
            }
            Event::InvocationStateChanged {
                invocation_id: candidate,
                from,
                to,
                ..
            } if candidate.to_string() == invocation_id => {
                let current = state.ok_or_else(|| {
                    LedgerError::InvalidInvocationOutcome(
                        "invocation state change has no proposal".into(),
                    )
                })?;
                if current != from
                    || !can_transition_invocation_with_idempotency(
                        from,
                        to,
                        idempotency.unwrap_or(Idempotency::Unknown),
                    )
                {
                    return Err(LedgerError::InvalidInvocationOutcome(
                        "invocation state history is not a valid transition".into(),
                    ));
                }
                state = Some(to);
            }
            Event::InvocationReconciled {
                invocation_id: candidate,
                outcome,
                evidence,
            } if candidate.to_string() == invocation_id => {
                let current = state.ok_or_else(|| {
                    LedgerError::InvalidInvocationOutcome(
                        "invocation reconciliation has no proposal".into(),
                    )
                })?;
                if evidence.source.trim().is_empty()
                    || evidence.summary.trim().is_empty()
                    || !can_reconcile_invocation(current, outcome)
                {
                    return Err(LedgerError::InvalidInvocationOutcome(
                        "invocation reconciliation is not valid from the projected state".into(),
                    ));
                }
                state = Some(outcome.state());
            }
            _ => {}
        }
    }
    Ok(state)
}

/// Validate the evidence carried by a persisted invocation proposal before a
/// typed outcome is allowed to extend its history.  Generic append/import
/// intentionally remain append-only/schema-level APIs, so this check is kept
/// at the transaction-local lookup used by outcome/recovery helpers.  Without
/// it a forged proposal could establish a seemingly valid state and receive a
/// result before core replay later rejects its digest or metadata.
struct InvocationProposalEvidence<'a> {
    invocation_id: &'a InvocationId,
    call_id: &'a str,
    tool_id: &'a str,
    tool_version: &'a str,
    normalized_arguments_digest: &'a str,
    effects: &'a EffectSet,
    effect_digest: &'a str,
    declared_idempotency: Idempotency,
}

fn validate_invocation_proposal_evidence(
    proposal: InvocationProposalEvidence<'_>,
) -> LedgerResult<()> {
    let InvocationProposalEvidence {
        invocation_id,
        call_id,
        tool_id,
        tool_version,
        normalized_arguments_digest,
        effects,
        effect_digest,
        declared_idempotency,
    } = proposal;
    for (field, value) in [
        ("call_id", call_id),
        ("tool_id", tool_id),
        ("tool_version", tool_version),
        ("normalized_arguments_digest", normalized_arguments_digest),
        ("effect_digest", effect_digest),
    ] {
        if value.trim().is_empty() {
            return Err(LedgerError::InvalidInvocationOutcome(format!(
                "invocation proposal {invocation_id} has an empty {field}"
            )));
        }
    }

    if effects.idempotency != declared_idempotency {
        return Err(LedgerError::InvalidInvocationOutcome(
            "invocation proposal idempotency disagrees with effects".into(),
        ));
    }

    let projected_effect_digest = digest_serializable(effects).map_err(|error| {
        LedgerError::InvalidInvocationOutcome(format!(
            "invocation proposal effects cannot be digested: {error}"
        ))
    })?;
    if projected_effect_digest != effect_digest {
        return Err(LedgerError::InvalidInvocationOutcome(format!(
            "invocation proposal effect_digest does not match effects for {invocation_id}"
        )));
    }

    Ok(())
}

fn validate_invocation_proposal_scope(
    thread_id: &str,
    turn_id: Option<&str>,
    agent_id: &str,
) -> LedgerResult<()> {
    if thread_id.trim().is_empty() {
        return Err(LedgerError::InvalidInvocationOutcome(
            "invocation proposal has an empty thread_id".into(),
        ));
    }
    if turn_id.is_none_or(|value| value.trim().is_empty()) {
        return Err(LedgerError::InvalidInvocationOutcome(
            "invocation proposal requires a non-empty turn_id".into(),
        ));
    }
    if agent_id.trim().is_empty() {
        return Err(LedgerError::InvalidInvocationOutcome(
            "invocation proposal has an empty agent_id".into(),
        ));
    }
    Ok(())
}

/// Return the protocol scope captured by an invocation proposal.  This query
/// is deliberately transaction-local so a caller cannot validate against one
/// proposal and append against another concurrently.
type InvocationScope = (String, Option<String>, String, String);

fn invocation_scope_in(
    connection: &Connection,
    invocation_id: &str,
) -> LedgerResult<Option<InvocationScope>> {
    let mut statement = connection.prepare(
        "SELECT thread_id, turn_id, agent_id, payload_json FROM events
         WHERE kind = 'tool/proposed' ORDER BY append_order ASC",
    )?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let thread_id: String = row.get(0)?;
        let turn_id: Option<String> = row.get(1)?;
        let agent_id: String = row.get(2)?;
        let payload_json: String = row.get(3)?;
        let payload: Value = serde_json::from_str(&payload_json).map_err(|error| {
            LedgerError::InvalidInvocationOutcome(format!(
                "invocation proposal contains invalid JSON payload: {error}"
            ))
        })?;
        let event: Event = serde_json::from_value(serde_json::json!({
            "kind": "tool/proposed",
            "payload": payload,
        }))
        .map_err(|error| {
            LedgerError::InvalidInvocationOutcome(format!(
                "invocation proposal is incomplete: {error}"
            ))
        })?;
        if let Event::InvocationProposed {
            invocation_id: candidate,
            call_id,
            ..
        } = event
        {
            if candidate.to_string() == invocation_id {
                validate_invocation_proposal_scope(&thread_id, turn_id.as_deref(), &agent_id)?;
                return Ok(Some((thread_id, turn_id, agent_id, call_id)));
            }
        }
    }
    Ok(None)
}

/// Extract the call ID from a model-visible ToolResult event. Validators
/// already require the exact one-block shape; this helper is deliberately
/// conservative and returns `None` for any other event kind.
fn tool_result_call_id(input: &NewLedgerEvent) -> Option<String> {
    let Event::ItemAdded { item } = decode_new_event(input).ok()? else {
        return None;
    };
    if item.kind != ItemKind::ToolResult {
        return None;
    }
    let blocks: Vec<ContentBlock> =
        serde_json::from_value(item.content.get("content")?.clone()).ok()?;
    match blocks.as_slice() {
        [ContentBlock::ToolResult { call_id, .. }] => Some(call_id.clone()),
        _ => None,
    }
}

fn decode_new_event(input: &NewLedgerEvent) -> LedgerResult<Event> {
    serde_json::from_value(serde_json::json!({
        "kind": input.kind,
        "payload": input.payload,
    }))
    .map_err(|error| {
        LedgerError::InvalidInvocationOutcome(format!(
            "event {} is not valid protocol data: {error}",
            input.event_id
        ))
    })
}

fn configure(connection: &Connection) -> LedgerResult<()> {
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "synchronous", "FULL")?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(())
}

fn initialize_schema(connection: &Connection) -> LedgerResult<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS events (
            append_order INTEGER NOT NULL UNIQUE CHECK(append_order > 0),
            schema_version TEXT NOT NULL,
            event_id TEXT NOT NULL UNIQUE,
            thread_id TEXT NOT NULL,
            turn_id TEXT,
            agent_id TEXT NOT NULL,
            seq INTEGER NOT NULL CHECK(seq > 0),
            event_time TEXT NOT NULL,
            causation_id TEXT,
            kind TEXT NOT NULL,
            payload_json TEXT NOT NULL CHECK(json_valid(payload_json)),
            PRIMARY KEY(thread_id, seq)
         ) WITHOUT ROWID;
         CREATE INDEX IF NOT EXISTS events_turn_idx ON events(turn_id, seq);
         CREATE INDEX IF NOT EXISTS events_time_idx ON events(event_time);
         CREATE TABLE IF NOT EXISTS command_receipts (
            command_id TEXT PRIMARY KEY,
            method TEXT NOT NULL,
            params_digest TEXT NOT NULL,
            response_json TEXT NOT NULL CHECK(json_valid(response_json)),
            created_at TEXT NOT NULL
         ) WITHOUT ROWID;
         CREATE TRIGGER IF NOT EXISTS events_no_update
           BEFORE UPDATE ON events BEGIN SELECT RAISE(ABORT, 'events are append-only'); END;
         CREATE TRIGGER IF NOT EXISTS events_no_delete
           BEFORE DELETE ON events BEGIN SELECT RAISE(ABORT, 'events are append-only'); END;
         CREATE TRIGGER IF NOT EXISTS receipts_no_update
           BEFORE UPDATE ON command_receipts BEGIN SELECT RAISE(ABORT, 'receipts are append-only'); END;
         CREATE TRIGGER IF NOT EXISTS receipts_no_delete
           BEFORE DELETE ON command_receipts BEGIN SELECT RAISE(ABORT, 'receipts are append-only'); END;",
    )?;
    Ok(())
}

fn row_to_event(row: &rusqlite::Row<'_>) -> rusqlite::Result<LedgerEvent> {
    let time: String = row.get(6)?;
    let payload: String = row.get(9)?;
    Ok(LedgerEvent {
        schema_version: {
            let version: String = row.get(0)?;
            serde_json::from_str(&version).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?
        },
        event_id: row.get(1)?,
        thread_id: row.get(2)?,
        turn_id: row.get(3)?,
        agent_id: row.get(4)?,
        seq: row.get(5)?,
        time: DateTime::parse_from_rfc3339(&time)
            .map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    6,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?
            .with_timezone(&Utc),
        causation_id: row.get(7)?,
        kind: row.get(8)?,
        payload: serde_json::from_str(&payload).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                9,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
    })
}

fn get_event_by_id(connection: &Connection, event_id: &str) -> LedgerResult<Option<LedgerEvent>> {
    Ok(connection
        .query_row(
            "SELECT schema_version, event_id, thread_id, turn_id, agent_id, seq,
                    event_time, causation_id, kind, payload_json
             FROM events WHERE event_id = ?1",
            [event_id],
            row_to_event,
        )
        .optional()?)
}

fn insert_event(
    connection: &Connection,
    event: &LedgerEvent,
    append_order: u64,
) -> LedgerResult<()> {
    connection.execute(
        "INSERT INTO events (
            append_order, schema_version, event_id, thread_id, turn_id, agent_id, seq,
            event_time, causation_id, kind, payload_json
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            append_order,
            serde_json::to_string(&event.schema_version)?,
            &event.event_id,
            &event.thread_id,
            &event.turn_id,
            &event.agent_id,
            event.seq,
            event.time.to_rfc3339(),
            &event.causation_id,
            &event.kind,
            serde_json::to_string(&event.payload)?,
        ],
    )?;
    Ok(())
}

fn insert_command_receipt(
    connection: &Connection,
    receipt: &NewCommandReceipt,
) -> LedgerResult<CommandReceipt> {
    connection.execute(
        "INSERT INTO command_receipts
            (command_id, method, params_digest, response_json, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            &receipt.command_id,
            &receipt.method,
            &receipt.params_digest,
            serde_json::to_string(&receipt.response)?,
            receipt.created_at.to_rfc3339(),
        ],
    )?;
    Ok(CommandReceipt {
        command_id: receipt.command_id.clone(),
        method: receipt.method.clone(),
        params_digest: receipt.params_digest.clone(),
        response: receipt.response.clone(),
        created_at: receipt.created_at,
    })
}

fn get_command_receipt(
    connection: &Connection,
    command_id: &str,
) -> LedgerResult<Option<CommandReceipt>> {
    let row: Option<(String, String, String, String)> = connection
        .query_row(
            "SELECT method, params_digest, response_json, created_at
             FROM command_receipts WHERE command_id = ?1",
            [command_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    row.map(|(method, params_digest, response, created_at)| {
        Ok(CommandReceipt {
            command_id: command_id.to_owned(),
            method,
            params_digest,
            response: serde_json::from_str(&response)?,
            created_at: DateTime::parse_from_rfc3339(&created_at)
                .map_err(|error| LedgerError::InvalidEnvelope(error.to_string()))?
                .with_timezone(&Utc),
        })
    })
    .transpose()
}

fn new_event_matches(input: &NewLedgerEvent, existing: &LedgerEvent) -> bool {
    input.schema_version == existing.schema_version
        && input.event_id == existing.event_id
        && input.thread_id == existing.thread_id
        && input.turn_id == existing.turn_id
        && input.agent_id == existing.agent_id
        && input.time == existing.time
        && input.causation_id == existing.causation_id
        && input.kind == existing.kind
        && input.payload == existing.payload
}

fn validate_sequence(
    thread_id: &str,
    mut expected: u64,
    events: &[LedgerEvent],
) -> LedgerResult<()> {
    for event in events {
        if event.seq != expected {
            return Err(LedgerError::SequenceGap {
                thread_id: thread_id.to_owned(),
                expected,
                found: event.seq,
            });
        }
        expected = expected.saturating_add(1);
    }
    Ok(())
}

fn next_sqlite_sequence(current: u64, scope: impl Into<String>) -> LedgerResult<u64> {
    if current >= i64::MAX as u64 {
        return Err(LedgerError::SequenceOverflow {
            scope: scope.into(),
        });
    }
    Ok(current + 1)
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Projection {
    pub threads: BTreeMap<String, ProjectionThread>,
}

impl Projection {
    pub fn apply(&mut self, event: LedgerEvent) {
        self.threads
            .entry(event.thread_id.clone())
            .or_insert_with(|| ProjectionThread::new(&event.thread_id))
            .apply(event);
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectionThread {
    pub thread_id: String,
    pub last_seq: u64,
    pub archived: bool,
    pub active_turn_id: Option<String>,
    pub turns: BTreeMap<String, ProjectionTurn>,
    pub items: Vec<ProjectionItem>,
}

impl ProjectionThread {
    fn new(thread_id: impl Into<String>) -> Self {
        Self {
            thread_id: thread_id.into(),
            last_seq: 0,
            archived: false,
            active_turn_id: None,
            turns: BTreeMap::new(),
            items: Vec::new(),
        }
    }

    pub fn apply(&mut self, event: LedgerEvent) {
        self.last_seq = event.seq;
        match event.kind.as_str() {
            "thread/archived" => self.archived = true,
            "thread/resumed" => self.archived = false,
            "turn/started" => {
                if let Some(turn_id) = event.turn_id.clone().or_else(|| payload_id(&event.payload))
                {
                    self.active_turn_id = Some(turn_id.clone());
                    self.turns.insert(
                        turn_id.clone(),
                        ProjectionTurn {
                            turn_id,
                            status: "active".into(),
                            started_seq: event.seq,
                            finished_seq: None,
                        },
                    );
                }
            }
            "turn/state_changed" => {
                if let Some(turn_id) = event.turn_id.clone().or_else(|| payload_id(&event.payload))
                {
                    if let Some(turn) = self.turns.get_mut(&turn_id) {
                        let status = event
                            .payload
                            .get("to")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown")
                            .to_owned();
                        turn.status = status.clone();
                        if matches!(status.as_str(), "completed" | "failed" | "cancelled") {
                            turn.finished_seq = Some(event.seq);
                        }
                    }
                    let terminal =
                        event
                            .payload
                            .get("to")
                            .and_then(Value::as_str)
                            .is_some_and(|status| {
                                matches!(status, "completed" | "failed" | "cancelled")
                            });
                    if terminal && self.active_turn_id.as_deref() == Some(&turn_id) {
                        self.active_turn_id = None;
                    }
                }
            }
            _ => self.items.push(ProjectionItem {
                seq: event.seq,
                turn_id: event.turn_id,
                kind: event.kind,
                payload: event.payload,
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectionTurn {
    pub turn_id: String,
    pub status: String,
    pub started_seq: u64,
    pub finished_seq: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectionItem {
    pub seq: u64,
    pub turn_id: Option<String>,
    pub kind: String,
    pub payload: Value,
}

fn payload_id(payload: &Value) -> Option<String> {
    payload
        .get("turn_id")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn parse_uuid(field: &str, value: &str) -> LedgerResult<Uuid> {
    Uuid::parse_str(value)
        .map_err(|error| LedgerError::InvalidEnvelope(format!("invalid {field}: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use uuid::Uuid;
    use yeux_protocol::{
        AgentId, ContentBlock, EffectSet, Event, EventId, InvocationId,
        InvocationReconciliationEvidence, InvocationReconciliationOutcome, InvocationState, Item,
        ItemId, ItemKind, Thread, ThreadId, ThreadStatus, TurnId, Workspace, WorkspaceId,
        WorkspaceIdentity, WorkspaceTrust,
    };

    fn event(thread: &str, event_id: &str, kind: &str) -> NewLedgerEvent {
        NewLedgerEvent {
            schema_version: yeux_protocol::PROTOCOL_VERSION,
            event_id: event_id.into(),
            thread_id: thread.into(),
            turn_id: Some("turn-1".into()),
            agent_id: "root".into(),
            time: DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            causation_id: None,
            kind: kind.into(),
            payload: json!({"turn_id": "turn-1"}),
        }
    }

    fn protocol_event(
        event_id: &str,
        thread_id: ThreadId,
        turn_id: TurnId,
        causation_id: &str,
        event: Event,
    ) -> NewLedgerEvent {
        let serialized = serde_json::to_value(event).unwrap();
        NewLedgerEvent {
            schema_version: yeux_protocol::PROTOCOL_VERSION,
            event_id: event_id.into(),
            thread_id: thread_id.to_string(),
            turn_id: Some(turn_id.to_string()),
            agent_id: "root".into(),
            time: DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            causation_id: Some(causation_id.into()),
            kind: serialized["kind"].as_str().unwrap().into(),
            payload: serialized["payload"].clone(),
        }
    }

    fn invocation_outcome(
        invocation_id: InvocationId,
        terminal_state: InvocationState,
    ) -> NewInvocationOutcome {
        let thread_id = ThreadId::from_uuid(Uuid::now_v7());
        let turn_id = TurnId::from_uuid(Uuid::now_v7());
        let call_id = "provider-call-1";
        let causation_id = invocation_id.to_string();
        let item = Item {
            id: ItemId::from_uuid(Uuid::now_v7()),
            thread_id,
            turn_id,
            agent_id: AgentId::from("root"),
            kind: ItemKind::ToolResult,
            content: json!({
                "invocation_id": invocation_id,
                "content": [ContentBlock::ToolResult {
                    call_id: call_id.into(),
                    content: json!({"ok": terminal_state == InvocationState::Completed}),
                    is_error: terminal_state != InvocationState::Completed,
                }],
            }),
            created_at: DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
        };
        NewInvocationOutcome {
            tool_result: protocol_event(
                "outcome-result",
                thread_id,
                turn_id,
                &causation_id,
                Event::ItemAdded { item },
            ),
            terminal_state: protocol_event(
                "outcome-terminal",
                thread_id,
                turn_id,
                &causation_id,
                Event::InvocationStateChanged {
                    invocation_id,
                    from: InvocationState::Started,
                    to: terminal_state,
                    reason: None,
                },
            ),
        }
    }

    fn invocation_unknown(invocation_id: InvocationId) -> NewInvocationUnknown {
        let thread_id = ThreadId::from_uuid(Uuid::now_v7());
        let turn_id = TurnId::from_uuid(Uuid::now_v7());
        NewInvocationUnknown {
            state: protocol_event(
                "unknown-marker",
                thread_id,
                turn_id,
                &invocation_id.to_string(),
                Event::InvocationStateChanged {
                    invocation_id,
                    from: InvocationState::Started,
                    to: InvocationState::Unknown,
                    reason: Some("daemon restart left the external outcome indeterminate".into()),
                },
            ),
        }
    }

    fn invocation_unknown_outcome(invocation_id: InvocationId) -> NewInvocationUnknownOutcome {
        let outcome = invocation_outcome(invocation_id, InvocationState::Failed);
        let marker = unknown_for_outcome(&outcome);
        NewInvocationUnknownOutcome {
            unknown_state: marker.state,
            tool_result: outcome.tool_result,
        }
    }

    /// Seed the minimum valid invocation history needed by the guarded outcome
    /// helpers.  Production callers already have these proposal/prepare/start
    /// events; tests keep the fixture local so outcome tests exercise the same
    /// state precondition rather than an orphan event pair.
    fn seed_started_invocation(ledger: &EventLedger, input: &NewInvocationOutcome) {
        seed_started_invocation_with_proposal(ledger, input, |_| {});
    }

    fn seed_started_invocation_with_proposal(
        ledger: &EventLedger,
        input: &NewInvocationOutcome,
        mutate: impl FnOnce(&mut Event),
    ) {
        let terminal = decode_new_event(&input.terminal_state).unwrap();
        let invocation_id = match terminal {
            Event::InvocationStateChanged { invocation_id, .. }
            | Event::InvocationReconciled { invocation_id, .. } => invocation_id,
            _ => panic!("fixture terminal event must identify an invocation"),
        };
        let thread_id = input.tool_result.thread_id.parse::<ThreadId>().unwrap();
        let turn_id = input
            .tool_result
            .turn_id
            .as_deref()
            .unwrap()
            .parse::<TurnId>()
            .unwrap();
        let effects = EffectSet::default();
        let mut proposal_event = Event::InvocationProposed {
            invocation_id,
            call_id: "provider-call-1".into(),
            tool_id: "fixture.tool".into(),
            tool_version: "1".into(),
            normalized_arguments_digest: "fixture-args".into(),
            effect_digest: digest_serializable(&effects).unwrap(),
            idempotency: effects.idempotency,
            effects,
        };
        mutate(&mut proposal_event);
        let proposal = protocol_event(
            &format!("proposal-{invocation_id}"),
            thread_id,
            turn_id,
            &invocation_id.to_string(),
            proposal_event,
        );
        ledger.append(proposal).unwrap();
        for (index, (from, to)) in [
            (InvocationState::Proposed, InvocationState::Approved),
            (InvocationState::Approved, InvocationState::Prepared),
            (InvocationState::Prepared, InvocationState::Started),
        ]
        .into_iter()
        .enumerate()
        {
            ledger
                .append(protocol_event(
                    &format!("start-{invocation_id}-{index}"),
                    thread_id,
                    turn_id,
                    &invocation_id.to_string(),
                    Event::InvocationStateChanged {
                        invocation_id,
                        from,
                        to,
                        reason: None,
                    },
                ))
                .unwrap();
        }
    }

    fn seed_unknown_invocation(ledger: &EventLedger, marker: &NewInvocationUnknown) {
        let event = decode_new_event(&marker.state).unwrap();
        let invocation_id = match event {
            Event::InvocationStateChanged { invocation_id, .. } => invocation_id,
            _ => panic!("fixture marker must be a state change"),
        };
        let thread_id = marker.state.thread_id.parse::<ThreadId>().unwrap();
        let turn_id = marker
            .state
            .turn_id
            .as_deref()
            .unwrap()
            .parse::<TurnId>()
            .unwrap();
        let outcome = invocation_outcome(invocation_id, InvocationState::Failed);
        let mut outcome = outcome;
        outcome.tool_result.thread_id = thread_id.to_string();
        outcome.tool_result.turn_id = Some(turn_id.to_string());
        outcome.terminal_state.thread_id = thread_id.to_string();
        outcome.terminal_state.turn_id = Some(turn_id.to_string());
        seed_started_invocation(ledger, &outcome);
    }

    fn unknown_for_outcome(input: &NewInvocationOutcome) -> NewInvocationUnknown {
        let terminal = decode_new_event(&input.terminal_state).unwrap();
        let invocation_id = match terminal {
            Event::InvocationStateChanged { invocation_id, .. }
            | Event::InvocationReconciled { invocation_id, .. } => invocation_id,
            _ => panic!("fixture terminal event must identify an invocation"),
        };
        let thread_id = input.tool_result.thread_id.parse::<ThreadId>().unwrap();
        let turn_id = input
            .tool_result
            .turn_id
            .as_deref()
            .unwrap()
            .parse::<TurnId>()
            .unwrap();
        NewInvocationUnknown {
            state: protocol_event(
                &format!("unknown-{invocation_id}"),
                thread_id,
                turn_id,
                &invocation_id.to_string(),
                Event::InvocationStateChanged {
                    invocation_id,
                    from: InvocationState::Started,
                    to: InvocationState::Unknown,
                    reason: Some("external outcome requires reconciliation".into()),
                },
            ),
        }
    }

    #[test]
    fn allocates_independent_monotonic_thread_sequences() {
        let ledger = EventLedger::open_in_memory().unwrap();
        assert_eq!(
            ledger
                .append(event("a", "1", "thread/started"))
                .unwrap()
                .seq,
            1
        );
        assert_eq!(
            ledger
                .append(event("b", "2", "thread/started"))
                .unwrap()
                .seq,
            1
        );
        assert_eq!(
            ledger.append(event("a", "3", "turn/started")).unwrap().seq,
            2
        );
    }

    #[test]
    fn replay_page_is_bounded_and_contiguous() {
        let ledger = EventLedger::open_in_memory().unwrap();
        for index in 1..=5 {
            ledger
                .append(event(
                    "thread",
                    &format!("event-{index}"),
                    "runtime/diagnostic",
                ))
                .unwrap();
        }

        let first = ledger.replay_page("thread", 0, 2).unwrap();
        assert_eq!(
            first.iter().map(|event| event.seq).collect::<Vec<_>>(),
            [1, 2]
        );
        let second = ledger.replay_page("thread", 2, 2).unwrap();
        assert_eq!(
            second.iter().map(|event| event.seq).collect::<Vec<_>>(),
            [3, 4]
        );
        let last = ledger.replay_page("thread", 4, 2).unwrap();
        assert_eq!(last.iter().map(|event| event.seq).collect::<Vec<_>>(), [5]);
        assert!(ledger.replay_page("thread", 0, 0).unwrap().is_empty());
    }

    #[test]
    fn duplicate_is_idempotent_but_divergence_is_rejected() {
        let ledger = EventLedger::open_in_memory().unwrap();
        let original = event("a", "same", "thread/started");
        let first = ledger.append(original.clone()).unwrap();
        assert_eq!(ledger.append(original).unwrap(), first);

        let conflict = event("a", "same", "turn/started");
        assert!(matches!(
            ledger.append(conflict),
            Err(LedgerError::EventIdConflict { .. })
        ));
    }

    #[test]
    fn import_rejects_gaps() {
        let ledger = EventLedger::open_in_memory().unwrap();
        let mut imported = LedgerEvent {
            schema_version: yeux_protocol::PROTOCOL_VERSION,
            event_id: "one".into(),
            thread_id: "t".into(),
            turn_id: None,
            agent_id: "root".into(),
            seq: 2,
            time: Utc::now(),
            causation_id: None,
            kind: "thread/started".into(),
            payload: json!({}),
        };
        assert!(matches!(
            ledger.import(&imported),
            Err(LedgerError::SequenceGap { .. })
        ));
        imported.seq = 1;
        ledger.import(&imported).unwrap();
    }

    #[test]
    fn projection_is_rebuilt_only_from_events() {
        let ledger = EventLedger::open_in_memory().unwrap();
        ledger.append(event("t", "1", "turn/started")).unwrap();
        ledger.append(event("t", "2", "model/event")).unwrap();
        let mut completed = event("t", "3", "turn/state_changed");
        completed.payload = json!({"turn_id": "turn-1", "to": "completed"});
        ledger.append(completed).unwrap();

        let projection = ledger.project_thread("t").unwrap();
        assert_eq!(projection.last_seq, 3);
        assert!(projection.active_turn_id.is_none());
        assert_eq!(projection.turns["turn-1"].status, "completed");
        assert_eq!(projection.items.len(), 1);
    }

    #[test]
    fn append_only_triggers_reject_rewrites() {
        let ledger = EventLedger::open_in_memory().unwrap();
        ledger.append(event("t", "1", "thread/started")).unwrap();
        let connection = ledger.lock().unwrap();
        let update = connection.execute("UPDATE events SET kind = 'changed'", []);
        let delete = connection.execute("DELETE FROM events", []);
        assert!(update.is_err());
        assert!(delete.is_err());
    }

    #[test]
    fn search_escapes_sql_wildcards() {
        let ledger = EventLedger::open_in_memory().unwrap();
        let mut value = event("t", "1", "message");
        value.payload = json!({"text": "literal 100%"});
        ledger.append(value).unwrap();
        assert_eq!(ledger.search("100%", 10).unwrap().len(), 1);
        assert!(ledger.search("100_", 10).unwrap().is_empty());
    }

    #[test]
    fn protocol_envelope_round_trip_is_lossless() {
        let envelope = EventEnvelope {
            schema_version: yeux_protocol::PROTOCOL_VERSION,
            event_id: EventId::from_uuid(Uuid::now_v7()),
            thread_id: ThreadId::from_uuid(Uuid::now_v7()),
            turn_id: None,
            agent_id: AgentId::new("root"),
            seq: 1,
            time: Utc::now(),
            causation_id: Some(CausationId::new("command:test")),
            event: Event::RuntimeDiagnostic {
                code: "test".into(),
                message: "round trip".into(),
                recoverable: true,
            },
        };
        let persisted = LedgerEvent::try_from(envelope.clone()).unwrap();
        assert_eq!(persisted.kind, "runtime/diagnostic");
        assert_eq!(EventEnvelope::try_from(persisted).unwrap(), envelope);
    }

    #[test]
    fn incompatible_persisted_schema_is_rejected_before_payload_decode() {
        let persisted = LedgerEvent {
            schema_version: ProtocolVersion::new(1, 0),
            event_id: Uuid::now_v7().to_string(),
            thread_id: Uuid::now_v7().to_string(),
            turn_id: None,
            agent_id: "root".into(),
            seq: 1,
            time: Utc::now(),
            causation_id: None,
            kind: "tool/proposed".into(),
            payload: json!({"legacy": true}),
        };

        assert!(matches!(
            EventEnvelope::try_from(persisted),
            Err(LedgerError::InvalidEnvelope(message))
                if message.contains("unsupported event schema 1.0")
        ));
    }

    #[test]
    fn command_receipt_replays_response_without_appending_again() {
        let ledger = EventLedger::open_in_memory().unwrap();
        let input = event("t", "event-1", "runtime/diagnostic");
        let receipt = NewCommandReceipt {
            command_id: "command-1".into(),
            method: "thread/start".into(),
            params_digest: "params".into(),
            response: json!({"thread_id": "t"}),
            created_at: Utc::now(),
        };
        let first = ledger
            .append_with_receipt(input.clone(), receipt.clone())
            .unwrap();
        assert!(!first.replayed);
        let second = ledger.append_with_receipt(input, receipt.clone()).unwrap();
        assert!(second.replayed);
        assert_eq!(second.response, receipt.response);
        assert_eq!(ledger.replay("t", 0).unwrap().len(), 1);

        let mut conflict = receipt;
        conflict.params_digest = "different".into();
        assert!(matches!(
            ledger.append_with_receipt(event("t", "event-2", "runtime/diagnostic"), conflict),
            Err(LedgerError::CommandIdConflict { .. })
        ));
    }

    #[test]
    fn records_idempotent_receipts_for_commands_without_events() {
        let ledger = EventLedger::open_in_memory().unwrap();
        let receipt = NewCommandReceipt {
            command_id: "command-read".into(),
            method: "thread/list".into(),
            params_digest: "digest".into(),
            response: json!({"threads": []}),
            created_at: DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
        };
        let first = ledger.record_command_receipt(receipt.clone()).unwrap();
        let mut retry = receipt.clone();
        retry.response = json!({"threads": ["must-not-replace-original"]});
        retry.created_at = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        let second = ledger.record_command_receipt(retry).unwrap();
        assert_eq!(first, second);
        assert_eq!(second.response, json!({"threads": []}));
        assert_eq!(second.created_at, receipt.created_at);

        let mut conflict = receipt;
        conflict.params_digest = "different".into();
        assert!(matches!(
            ledger.record_command_receipt(conflict),
            Err(LedgerError::CommandIdConflict { .. })
        ));
    }

    #[test]
    fn runtime_batch_is_atomic_and_idempotent_by_event_id() {
        let ledger = EventLedger::open_in_memory().unwrap();
        let inputs = vec![
            event("thread", "runtime-batch-1", "tool/state_changed"),
            event("thread", "runtime-batch-2", "item/added"),
        ];

        let first = ledger.append_batch(inputs.clone()).unwrap();
        assert_eq!(
            first.iter().map(|event| event.seq).collect::<Vec<_>>(),
            vec![1, 2]
        );

        let retry = ledger.append_batch(inputs).unwrap();
        assert_eq!(retry, first);
        assert_eq!(ledger.replay("thread", 0).unwrap(), first);
    }

    #[test]
    fn runtime_batch_rejects_empty_partial_and_divergent_reuse() {
        let ledger = EventLedger::open_in_memory().unwrap();
        assert!(matches!(
            ledger.append_batch(Vec::new()),
            Err(LedgerError::EmptyEventBatch)
        ));

        let existing = event("thread", "runtime-existing", "runtime/diagnostic");
        ledger.append(existing.clone()).unwrap();
        assert!(matches!(
            ledger.append_batch(vec![
                existing.clone(),
                event("thread", "runtime-new", "item/added"),
            ]),
            Err(LedgerError::EventIdConflict { .. })
        ));
        assert_eq!(ledger.replay("thread", 0).unwrap().len(), 1);

        let mut divergent = existing;
        divergent.kind = "different".into();
        assert!(matches!(
            ledger.append_batch(vec![divergent]),
            Err(LedgerError::EventIdConflict { .. })
        ));
        assert_eq!(ledger.replay("thread", 0).unwrap().len(), 1);
    }

    #[test]
    fn invocation_outcome_is_atomic_idempotent_and_prefix_safe() {
        let ledger = EventLedger::open_in_memory().unwrap();
        let input = invocation_outcome(
            InvocationId::from_uuid(Uuid::now_v7()),
            InvocationState::Completed,
        );
        let thread_id = input.tool_result.thread_id.clone();
        seed_started_invocation(&ledger, &input);

        let first = ledger.append_invocation_outcome(input.clone()).unwrap();
        assert_eq!(first.len(), 2);
        assert_eq!(first[0].kind, "item/added");
        assert_eq!(first[1].kind, "tool/state_changed");
        assert_eq!([first[0].seq, first[1].seq], [5, 6]);

        // Even a one-event replay page cannot expose Completed without its
        // model-visible result because the result has the lower sequence.
        let prefix = ledger.replay_page(&thread_id, 4, 1).unwrap();
        assert_eq!(prefix.len(), 1);
        assert_eq!(prefix[0].kind, "item/added");

        let retry = ledger.append_invocation_outcome(input).unwrap();
        assert_eq!(retry, first);
        let all_events = ledger.replay(&thread_id, 0).unwrap();
        assert_eq!(&all_events[4..], first.as_slice());
    }

    #[test]
    fn invocation_outcome_retry_rejects_a_fully_existing_batch_without_proposal() {
        let ledger = EventLedger::open_in_memory().unwrap();
        let input = invocation_outcome(
            InvocationId::from_uuid(Uuid::now_v7()),
            InvocationState::Completed,
        );
        let thread_id = input.tool_result.thread_id.clone();

        // Simulate an outcome pair imported through the generic append API.
        // Both event IDs already exist, so the typed helper's idempotent path
        // must still verify that a durable InvocationProposed anchor exists.
        ledger
            .append_batch(vec![
                input.tool_result.clone(),
                input.terminal_state.clone(),
            ])
            .unwrap();

        assert!(matches!(
            ledger.append_invocation_outcome(input),
            Err(LedgerError::InvalidInvocationOutcome(message))
                if message.contains("no proposal")
        ));
        assert_eq!(ledger.replay(&thread_id, 0).unwrap().len(), 2);
    }

    #[test]
    fn invocation_outcome_retry_rejects_generic_terminal_pair_with_wrong_final_state() {
        let ledger = EventLedger::open_in_memory().unwrap();
        let invocation_id = InvocationId::from_uuid(Uuid::now_v7());
        let input = invocation_outcome(invocation_id, InvocationState::Failed);
        let thread_id = input.tool_result.thread_id.clone();
        seed_started_invocation(&ledger, &input);

        // A generic append can pre-seed a complete proposal/Started/terminal
        // history.  A later typed retry must not treat the existing event IDs
        // as sufficient evidence when its projected final state disagrees.
        ledger
            .append_batch(vec![
                input.tool_result.clone(),
                input.terminal_state.clone(),
            ])
            .unwrap();

        let mut divergent_retry = input;
        divergent_retry.terminal_state.payload["to"] = json!("completed");
        divergent_retry.tool_result.payload["item"]["content"]["content"][0]["is_error"] =
            json!(false);

        assert!(matches!(
            ledger.append_invocation_outcome(divergent_retry),
            Err(LedgerError::InvocationStateConflict {
                expected: InvocationState::Completed,
                found: Some(InvocationState::Failed),
                ..
            })
        ));
        // The fail-closed retry must not append a duplicate or partial event.
        assert_eq!(ledger.replay(&thread_id, 0).unwrap().len(), 6);
    }

    #[test]
    fn invocation_outcome_validation_fails_before_any_append() {
        let ledger = EventLedger::open_in_memory().unwrap();
        let mut input = invocation_outcome(
            InvocationId::from_uuid(Uuid::now_v7()),
            InvocationState::Completed,
        );
        let thread_id = input.tool_result.thread_id.clone();
        input.terminal_state.agent_id = "different-agent".into();

        assert!(matches!(
            ledger.append_invocation_outcome(input),
            Err(LedgerError::InvalidInvocationOutcome(_))
        ));
        assert!(ledger.replay(&thread_id, 0).unwrap().is_empty());
    }

    #[test]
    fn invocation_outcome_rejects_forged_persisted_proposal_before_append() {
        let ledger = EventLedger::open_in_memory().unwrap();
        let input = invocation_outcome(
            InvocationId::from_uuid(Uuid::now_v7()),
            InvocationState::Completed,
        );
        let thread_id = input.tool_result.thread_id.clone();
        seed_started_invocation_with_proposal(&ledger, &input, |proposal| {
            if let Event::InvocationProposed { effect_digest, .. } = proposal {
                *effect_digest = "forged-effect-digest".into();
            }
        });

        assert!(matches!(
            ledger.append_invocation_outcome(input),
            Err(LedgerError::InvalidInvocationOutcome(message))
                if message.contains("effect_digest")
        ));
        // The malformed generic proposal remains append-only evidence, but no
        // derived ToolResult/terminal events may be added after it.
        assert_eq!(ledger.replay(&thread_id, 0).unwrap().len(), 4);
    }

    #[test]
    fn invocation_outcome_rejects_incomplete_persisted_proposal_before_append() {
        let ledger = EventLedger::open_in_memory().unwrap();
        let input = invocation_outcome(
            InvocationId::from_uuid(Uuid::now_v7()),
            InvocationState::Failed,
        );
        let thread_id = input.tool_result.thread_id.clone();
        seed_started_invocation_with_proposal(&ledger, &input, |proposal| {
            if let Event::InvocationProposed { call_id, .. } = proposal {
                call_id.clear();
            }
        });

        assert!(matches!(
            ledger.append_invocation_outcome(input),
            Err(LedgerError::InvalidInvocationOutcome(message))
                if message.contains("call_id")
        ));
        assert_eq!(ledger.replay(&thread_id, 0).unwrap().len(), 4);
    }

    #[test]
    fn invocation_outcome_rejects_idempotency_mismatch_in_persisted_proposal() {
        let ledger = EventLedger::open_in_memory().unwrap();
        let input = invocation_outcome(
            InvocationId::from_uuid(Uuid::now_v7()),
            InvocationState::Completed,
        );
        let thread_id = input.tool_result.thread_id.clone();
        seed_started_invocation_with_proposal(&ledger, &input, |proposal| {
            if let Event::InvocationProposed { idempotency, .. } = proposal {
                *idempotency = Idempotency::Unknown;
            }
        });

        assert!(matches!(
            ledger.append_invocation_outcome(input),
            Err(LedgerError::InvalidInvocationOutcome(message))
                if message.contains("idempotency")
        ));
        assert_eq!(ledger.replay(&thread_id, 0).unwrap().len(), 4);
    }

    #[test]
    fn invocation_outcome_conflict_rolls_back_the_other_event() {
        let ledger = EventLedger::open_in_memory().unwrap();
        let input = invocation_outcome(
            InvocationId::from_uuid(Uuid::now_v7()),
            InvocationState::Failed,
        );
        let thread_id = input.tool_result.thread_id.clone();
        seed_started_invocation(&ledger, &input);
        let conflict = event(
            "unrelated",
            &input.terminal_state.event_id,
            "runtime/diagnostic",
        );
        ledger.append(conflict).unwrap();

        assert!(matches!(
            ledger.append_invocation_outcome(input),
            Err(LedgerError::EventIdConflict { .. })
        ));
        assert_eq!(ledger.replay(&thread_id, 0).unwrap().len(), 4);
    }

    #[test]
    fn invocation_outcome_requires_explicit_reconciliation_evidence() {
        let ledger = EventLedger::open_in_memory().unwrap();
        let invocation_id = InvocationId::from_uuid(Uuid::now_v7());
        let mut input = invocation_outcome(invocation_id, InvocationState::Failed);
        input.terminal_state.payload["from"] = json!("unknown");
        assert!(matches!(
            ledger.append_invocation_outcome(input.clone()),
            Err(LedgerError::InvalidInvocationOutcome(message))
                if message.contains("tool/reconciled")
        ));

        input.terminal_state = protocol_event(
            "outcome-terminal-reconciled",
            input.tool_result.thread_id.parse::<ThreadId>().unwrap(),
            input
                .tool_result
                .turn_id
                .as_deref()
                .unwrap()
                .parse::<TurnId>()
                .unwrap(),
            input.tool_result.causation_id.as_deref().unwrap(),
            Event::InvocationReconciled {
                invocation_id,
                outcome: InvocationReconciliationOutcome::Failed,
                evidence: InvocationReconciliationEvidence {
                    source: "executor_receipt".into(),
                    summary: "external receipt proves the operation failed".into(),
                    artifact_uri: None,
                },
            },
        );
        seed_started_invocation(&ledger, &input);
        ledger
            .append_invocation_unknown(unknown_for_outcome(&input))
            .unwrap();
        let committed = ledger.append_invocation_outcome(input).unwrap();
        assert_eq!(committed[1].kind, "tool/reconciled");
    }

    #[test]
    fn unknown_marker_is_explicit_and_idempotent() {
        let ledger = EventLedger::open_in_memory().unwrap();
        let invocation_id = InvocationId::from_uuid(Uuid::now_v7());
        let marker = invocation_unknown(invocation_id);
        seed_unknown_invocation(&ledger, &marker);
        let first = ledger.append_invocation_unknown(marker.clone()).unwrap();
        assert_eq!(first.kind, "tool/state_changed");
        assert_eq!(first.payload["from"], "started");
        assert_eq!(first.payload["to"], "unknown");
        assert_eq!(ledger.append_invocation_unknown(marker).unwrap(), first);

        let mut malformed = invocation_unknown(invocation_id);
        malformed.state.event_id = "different-marker".into();
        malformed.state.payload["to"] = json!("started");
        assert!(matches!(
            ledger.append_invocation_unknown(malformed),
            Err(LedgerError::InvalidInvocationOutcome(message))
                if message.contains("Started -> Unknown")
        ));
        assert_eq!(ledger.replay(&first.thread_id, 0).unwrap().len(), 5);
    }

    #[test]
    fn unknown_marker_scope_must_match_its_proposal() {
        let ledger = EventLedger::open_in_memory().unwrap();
        let invocation_id = InvocationId::from_uuid(Uuid::now_v7());
        let marker = invocation_unknown(invocation_id);
        let thread_id = marker.state.thread_id.clone();
        seed_unknown_invocation(&ledger, &marker);

        let mut forged = marker;
        forged.state.event_id = "forged-scope-marker".into();
        forged.state.agent_id = "different-agent".into();
        assert!(matches!(
            ledger.append_invocation_unknown(forged),
            Err(LedgerError::InvalidInvocationOutcome(message))
                if message.contains("scope")
        ));
        assert_eq!(ledger.replay(&thread_id, 0).unwrap().len(), 4);
    }

    #[test]
    fn unknown_outcome_is_atomic_and_idempotent() {
        let ledger = EventLedger::open_in_memory().unwrap();
        let invocation_id = InvocationId::from_uuid(Uuid::now_v7());
        let input = invocation_unknown_outcome(invocation_id);
        let thread_id = input.unknown_state.thread_id.clone();
        let seed = NewInvocationOutcome {
            tool_result: input.tool_result.clone(),
            terminal_state: protocol_event(
                "unknown-outcome-seed-terminal",
                input.unknown_state.thread_id.parse::<ThreadId>().unwrap(),
                input
                    .unknown_state
                    .turn_id
                    .as_deref()
                    .unwrap()
                    .parse::<TurnId>()
                    .unwrap(),
                input.unknown_state.causation_id.as_deref().unwrap(),
                Event::InvocationStateChanged {
                    invocation_id,
                    from: InvocationState::Started,
                    to: InvocationState::Failed,
                    reason: Some("seed only".into()),
                },
            ),
        };
        seed_started_invocation(&ledger, &seed);

        let first = ledger
            .append_invocation_unknown_outcome(input.clone())
            .unwrap();
        assert_eq!(first.len(), 2);
        assert_eq!(first[0].kind, "tool/state_changed");
        assert_eq!(first[1].kind, "item/added");
        assert_eq!(first[0].payload["from"], "started");
        assert_eq!(first[0].payload["to"], "unknown");
        assert_eq!(first[1].payload["item"]["kind"], "tool_result");
        assert_eq!(
            ledger.append_invocation_unknown_outcome(input).unwrap(),
            first
        );
        assert_eq!(ledger.replay(&thread_id, 0).unwrap().len(), 6);
    }

    #[test]
    fn unknown_outcome_call_id_must_match_its_proposal() {
        let ledger = EventLedger::open_in_memory().unwrap();
        let invocation_id = InvocationId::from_uuid(Uuid::now_v7());
        let mut input = invocation_unknown_outcome(invocation_id);
        let seed = NewInvocationOutcome {
            tool_result: input.tool_result.clone(),
            terminal_state: protocol_event(
                "unknown-call-id-seed-terminal",
                input.unknown_state.thread_id.parse::<ThreadId>().unwrap(),
                input
                    .unknown_state
                    .turn_id
                    .as_deref()
                    .unwrap()
                    .parse::<TurnId>()
                    .unwrap(),
                input.unknown_state.causation_id.as_deref().unwrap(),
                Event::InvocationStateChanged {
                    invocation_id,
                    from: InvocationState::Started,
                    to: InvocationState::Failed,
                    reason: Some("seed only".into()),
                },
            ),
        };
        let thread_id = input.unknown_state.thread_id.clone();
        seed_started_invocation(&ledger, &seed);
        input.tool_result.payload["item"]["content"]["content"][0]["call_id"] =
            json!("forged-call-id");

        assert!(matches!(
            ledger.append_invocation_unknown_outcome(input),
            Err(LedgerError::InvalidInvocationOutcome(message))
                if message.contains("call_id")
        ));
        assert_eq!(ledger.replay(&thread_id, 0).unwrap().len(), 4);
    }

    #[test]
    fn reconciliation_helper_rejects_ordinary_terminal_events() {
        let ledger = EventLedger::open_in_memory().unwrap();
        let input = invocation_outcome(
            InvocationId::from_uuid(Uuid::now_v7()),
            InvocationState::Completed,
        );
        assert!(matches!(
            ledger.append_invocation_reconciliation(input),
            Err(LedgerError::InvalidInvocationOutcome(message))
                if message.contains("tool/reconciled")
        ));
    }

    #[test]
    fn outcome_batch_rolls_back_when_terminal_insert_fails() {
        let ledger = EventLedger::open_in_memory().unwrap();
        let input = invocation_outcome(
            InvocationId::from_uuid(Uuid::now_v7()),
            InvocationState::Completed,
        );
        seed_started_invocation(&ledger, &input);
        {
            let connection = ledger.lock().unwrap();
            connection
                .execute_batch(
                    "CREATE TRIGGER reject_invocation_terminal
                     BEFORE INSERT ON events
                     WHEN NEW.kind = 'tool/state_changed'
                     BEGIN SELECT RAISE(ABORT, 'injected terminal failure'); END;",
                )
                .unwrap();
        }
        let thread_id = input.tool_result.thread_id.clone();
        assert!(matches!(
            ledger.append_invocation_outcome(input.clone()),
            Err(LedgerError::Database(_))
        ));
        assert_eq!(ledger.replay(&thread_id, 0).unwrap().len(), 4);

        // Removing the injected fault and retrying the exact same batch is
        // safe: no prefix from the failed transaction survived.
        {
            let connection = ledger.lock().unwrap();
            connection
                .execute("DROP TRIGGER reject_invocation_terminal", [])
                .unwrap();
        }
        let committed = ledger.append_invocation_outcome(input).unwrap();
        assert_eq!(committed.len(), 2);
        assert_eq!(ledger.replay(&thread_id, 0).unwrap().len(), 6);
    }

    #[test]
    fn outcome_batch_survives_reopen_and_retries_by_event_id() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("outcome.sqlite3");
        let input = invocation_outcome(
            InvocationId::from_uuid(Uuid::now_v7()),
            InvocationState::Failed,
        );
        let thread_id = input.tool_result.thread_id.clone();
        let committed = {
            let ledger = EventLedger::open(&path).unwrap();
            seed_started_invocation(&ledger, &input);
            ledger.append_invocation_outcome(input.clone()).unwrap()
        };
        let reopened = EventLedger::open(&path).unwrap();
        let all_events = reopened.replay(&thread_id, 0).unwrap();
        assert_eq!(all_events.len(), 6);
        assert_eq!(&all_events[4..], committed.as_slice());
        assert_eq!(
            reopened.append_invocation_outcome(input).unwrap(),
            committed
        );
    }

    #[test]
    fn reconciliation_batch_survives_reopen_without_reexecuting_work() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("reconciliation.sqlite3");
        let invocation_id = InvocationId::from_uuid(Uuid::now_v7());
        let mut input = invocation_outcome(invocation_id, InvocationState::Failed);
        let terminal = protocol_event(
            "reconciled-terminal",
            input.tool_result.thread_id.parse::<ThreadId>().unwrap(),
            input
                .tool_result
                .turn_id
                .as_deref()
                .unwrap()
                .parse::<TurnId>()
                .unwrap(),
            input.tool_result.causation_id.as_deref().unwrap(),
            Event::InvocationReconciled {
                invocation_id,
                outcome: InvocationReconciliationOutcome::Failed,
                evidence: InvocationReconciliationEvidence {
                    source: "durable-receipt".into(),
                    summary: "executor receipt proves no write was committed".into(),
                    artifact_uri: None,
                },
            },
        );
        input.terminal_state = terminal;
        let thread_id = input.tool_result.thread_id.clone();
        let committed = {
            let ledger = EventLedger::open(&path).unwrap();
            seed_started_invocation(&ledger, &input);
            ledger
                .append_invocation_unknown(unknown_for_outcome(&input))
                .unwrap();
            ledger
                .append_invocation_reconciliation(input.clone())
                .unwrap()
        };

        let reopened = EventLedger::open(&path).unwrap();
        let events = reopened.replay(&thread_id, 0).unwrap();
        assert_eq!(events.len(), 7);
        assert_eq!(&events[5..], committed.as_slice());
        // A retry after restart is an event-ID replay, not a second external
        // execution or a second reconciliation decision.
        assert_eq!(
            reopened.append_invocation_reconciliation(input).unwrap(),
            committed
        );
        assert_eq!(reopened.replay(&thread_id, 0).unwrap().len(), 7);
    }

    #[test]
    fn batch_and_receipt_commit_as_one_idempotent_command() {
        let ledger = EventLedger::open_in_memory().unwrap();
        let receipt = NewCommandReceipt {
            command_id: "command-batch".into(),
            method: "turn/start".into(),
            params_digest: "digest".into(),
            response: json!({"turn": "turn-1"}),
            created_at: DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
        };
        let events = vec![
            event("thread", "batch-1", "turn/started"),
            event("thread", "batch-2", "item/added"),
        ];
        let first = ledger
            .append_batch_with_receipt(events.clone(), receipt.clone())
            .unwrap();
        assert_eq!(first.events.len(), 2);
        assert_eq!(first.events[0].seq, 1);
        assert_eq!(first.events[1].seq, 2);
        assert!(!first.replayed);

        let second = ledger
            .append_batch_with_receipt(events, receipt.clone())
            .unwrap();
        assert!(second.replayed);
        assert!(second.events.is_empty());
        assert_eq!(second.response, receipt.response);
        assert_eq!(ledger.replay("thread", 0).unwrap().len(), 2);
        assert_eq!(
            ledger
                .command_receipt("command-batch")
                .unwrap()
                .unwrap()
                .response,
            receipt.response
        );
        let receipt_only_retry = ledger
            .append_batch_with_receipt(Vec::new(), receipt)
            .unwrap();
        assert!(receipt_only_retry.replayed);
        assert!(receipt_only_retry.events.is_empty());
    }

    #[test]
    fn batch_allocates_sequences_per_thread_and_preserves_global_input_order() {
        let ledger = EventLedger::open_in_memory().unwrap();
        ledger
            .append(event("thread-a", "seed", "runtime/diagnostic"))
            .unwrap();
        let inputs = vec![
            event("thread-a", "a-1", "runtime/diagnostic"),
            event("thread-b", "b-1", "runtime/diagnostic"),
            event("thread-a", "a-2", "runtime/diagnostic"),
        ];
        let receipt = NewCommandReceipt {
            command_id: "command-cross-thread".into(),
            method: "test/batch".into(),
            params_digest: "digest".into(),
            response: json!({}),
            created_at: Utc::now(),
        };
        let result = ledger.append_batch_with_receipt(inputs, receipt).unwrap();
        assert_eq!(
            result
                .events
                .iter()
                .map(|event| event.seq)
                .collect::<Vec<_>>(),
            vec![2, 1, 3]
        );
        assert_eq!(
            ledger
                .all_events()
                .unwrap()
                .iter()
                .map(|event| event.event_id.as_str())
                .collect::<Vec<_>>(),
            vec!["seed", "a-1", "b-1", "a-2"]
        );
    }

    #[test]
    fn batch_conflict_rolls_back_every_prior_event_and_sequence() {
        let ledger = EventLedger::open_in_memory().unwrap();
        ledger
            .append(event("existing", "conflict", "runtime/diagnostic"))
            .unwrap();
        let receipt = NewCommandReceipt {
            command_id: "command-rollback".into(),
            method: "turn/start".into(),
            params_digest: "digest".into(),
            response: json!({}),
            created_at: Utc::now(),
        };
        let error = ledger
            .append_batch_with_receipt(
                vec![
                    event("new-thread", "would-be-first", "turn/started"),
                    event("existing", "conflict", "item/added"),
                ],
                receipt,
            )
            .unwrap_err();
        assert!(matches!(error, LedgerError::EventIdConflict { .. }));
        assert!(ledger.replay("new-thread", 0).unwrap().is_empty());
        assert!(ledger
            .command_receipt("command-rollback")
            .unwrap()
            .is_none());

        let next = ledger
            .append(event("new-thread", "after-rollback", "runtime/diagnostic"))
            .unwrap();
        assert_eq!(next.seq, 1);
        assert_eq!(ledger.all_events().unwrap().len(), 2);
    }

    #[test]
    fn receipt_insert_failure_rolls_back_the_entire_batch() {
        let ledger = EventLedger::open_in_memory().unwrap();
        {
            let connection = ledger.lock().unwrap();
            connection
                .execute_batch(
                    "CREATE TRIGGER reject_test_receipt
                     BEFORE INSERT ON command_receipts
                     WHEN NEW.command_id = 'reject-receipt'
                     BEGIN SELECT RAISE(ABORT, 'injected receipt failure'); END;",
                )
                .unwrap();
        }
        let receipt = NewCommandReceipt {
            command_id: "reject-receipt".into(),
            method: "turn/start".into(),
            params_digest: "digest".into(),
            response: json!({}),
            created_at: Utc::now(),
        };
        let result = ledger.append_batch_with_receipt(
            vec![
                event("thread", "rollback-1", "turn/started"),
                event("thread", "rollback-2", "item/added"),
            ],
            receipt,
        );
        assert!(matches!(result, Err(LedgerError::Database(_))));
        assert!(ledger.replay("thread", 0).unwrap().is_empty());
        assert!(ledger.command_receipt("reject-receipt").unwrap().is_none());

        let next = ledger
            .append(event("thread", "after-trigger", "runtime/diagnostic"))
            .unwrap();
        assert_eq!(next.seq, 1);
    }

    #[test]
    fn empty_event_batch_is_rejected_without_recording_a_receipt() {
        let ledger = EventLedger::open_in_memory().unwrap();
        let receipt = NewCommandReceipt {
            command_id: "empty-batch".into(),
            method: "turn/start".into(),
            params_digest: "digest".into(),
            response: json!({}),
            created_at: Utc::now(),
        };
        assert!(matches!(
            ledger.append_batch_with_receipt(Vec::new(), receipt),
            Err(LedgerError::EmptyEventBatch)
        ));
        assert!(ledger.command_receipt("empty-batch").unwrap().is_none());
    }

    #[test]
    fn sqlite_sequence_limit_fails_before_integer_overflow() {
        assert_eq!(
            next_sqlite_sequence(i64::MAX as u64 - 1, "test").unwrap(),
            i64::MAX as u64
        );
        assert!(matches!(
            next_sqlite_sequence(i64::MAX as u64, "test"),
            Err(LedgerError::SequenceOverflow { .. })
        ));
    }

    #[test]
    fn concurrent_batches_on_one_thread_are_serialized_without_sequence_overlap() {
        use std::sync::{Arc, Barrier};

        let ledger = Arc::new(EventLedger::open_in_memory().unwrap());
        let barrier = Arc::new(Barrier::new(3));
        let mut workers = Vec::new();
        for batch in 0..2 {
            let ledger = Arc::clone(&ledger);
            let barrier = Arc::clone(&barrier);
            workers.push(std::thread::spawn(move || {
                let receipt = NewCommandReceipt {
                    command_id: format!("concurrent-command-{batch}"),
                    method: "turn/start".into(),
                    params_digest: format!("digest-{batch}"),
                    response: json!({"batch": batch}),
                    created_at: Utc::now(),
                };
                let events = vec![
                    event(
                        "shared-thread",
                        &format!("concurrent-{batch}-1"),
                        "turn/started",
                    ),
                    event(
                        "shared-thread",
                        &format!("concurrent-{batch}-2"),
                        "item/added",
                    ),
                ];
                barrier.wait();
                ledger.append_batch_with_receipt(events, receipt).unwrap()
            }));
        }
        barrier.wait();
        let results = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>();

        for result in &results {
            assert_eq!(result.events[1].seq, result.events[0].seq + 1);
        }
        assert_eq!(
            ledger
                .replay("shared-thread", 0)
                .unwrap()
                .iter()
                .map(|event| event.seq)
                .collect::<Vec<_>>(),
            vec![1, 2, 3, 4]
        );
        assert!(ledger
            .command_receipt("concurrent-command-0")
            .unwrap()
            .is_some());
        assert!(ledger
            .command_receipt("concurrent-command-1")
            .unwrap()
            .is_some());
    }

    #[test]
    fn durable_receipt_deduplicates_batch_after_database_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ledger.sqlite3");
        let receipt = NewCommandReceipt {
            command_id: "restart-command".into(),
            method: "turn/start".into(),
            params_digest: "restart-digest".into(),
            response: json!({"turn_id": "turn-after-restart"}),
            created_at: Utc::now(),
        };
        {
            let ledger = EventLedger::open(&path).unwrap();
            let result = ledger
                .append_batch_with_receipt(
                    vec![
                        event("restart-thread", "restart-1", "turn/started"),
                        event("restart-thread", "restart-2", "item/added"),
                    ],
                    receipt.clone(),
                )
                .unwrap();
            assert!(!result.replayed);
        }

        let reopened = EventLedger::open(&path).unwrap();
        let replayed = reopened
            .append_batch_with_receipt(Vec::new(), receipt.clone())
            .unwrap();
        assert!(replayed.replayed);
        assert_eq!(replayed.response, receipt.response);
        assert_eq!(reopened.replay("restart-thread", 0).unwrap().len(), 2);
    }

    #[test]
    fn core_projection_uses_global_append_order_for_cross_thread_dependencies() {
        let ledger = EventLedger::open_in_memory().unwrap();
        let workspace_id = WorkspaceId::from_uuid(Uuid::now_v7());
        let meta_thread_id = ThreadId::from_uuid(Uuid::now_v7());
        let thread_id = ThreadId::from_uuid(Uuid::now_v7());
        let now = Utc::now();
        let workspace = Workspace {
            id: workspace_id,
            root: "/workspace".into(),
            identity: WorkspaceIdentity {
                canonical_root: "/workspace".into(),
                digest: "identity".into(),
                device: None,
                inode: None,
                git_common_dir: None,
            },
            trust: WorkspaceTrust::Trusted,
            opened_at: now,
        };
        let opened = EventEnvelope {
            schema_version: yeux_protocol::PROTOCOL_VERSION,
            event_id: EventId::from_uuid(Uuid::now_v7()),
            thread_id: meta_thread_id,
            turn_id: None,
            agent_id: AgentId::new("root"),
            seq: 1,
            time: now,
            causation_id: None,
            event: Event::WorkspaceOpened {
                workspace: workspace.clone(),
            },
        };
        let thread = Thread {
            id: thread_id,
            workspace_id,
            parent_thread_id: None,
            parent_seq: None,
            title: None,
            status: ThreadStatus::Idle,
            created_at: now,
            updated_at: now,
            last_seq: 0,
        };
        let started = EventEnvelope {
            schema_version: yeux_protocol::PROTOCOL_VERSION,
            event_id: EventId::from_uuid(Uuid::now_v7()),
            thread_id,
            turn_id: None,
            agent_id: AgentId::new("root"),
            seq: 1,
            time: now,
            causation_id: None,
            event: Event::ThreadStarted {
                thread: thread.clone(),
            },
        };
        ledger
            .import(&LedgerEvent::try_from(opened).unwrap())
            .unwrap();
        ledger
            .import(&LedgerEvent::try_from(started).unwrap())
            .unwrap();

        let projection = ledger.project_core().unwrap();
        assert_eq!(projection.workspaces.get(&workspace_id), Some(&workspace));
        let projected_thread = projection.threads.get(&thread_id).unwrap();
        assert_eq!(projected_thread.id, thread.id);
        assert_eq!(projected_thread.workspace_id, thread.workspace_id);
        assert_eq!(projected_thread.last_seq, 1);
    }
}
