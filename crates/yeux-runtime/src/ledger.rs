//! Append-only SQLite event ledger and deterministic projections.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
    sync::{Mutex, MutexGuard},
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;
use yeux_core::{EventStore, PortError};
use yeux_protocol::{
    AgentId, CausationId, Event, EventEnvelope, EventId, ProtocolVersion, ThreadId, TurnId,
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

        let next: u64 = transaction.query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM events WHERE thread_id = ?1",
            [&input.thread_id],
            |row| row.get(0),
        )?;
        let append_order: u64 = transaction.query_row(
            "SELECT COALESCE(MAX(append_order), 0) + 1 FROM events",
            [],
            |row| row.get(0),
        )?;
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
        let seq: u64 = transaction.query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM events WHERE thread_id = ?1",
            [&input.thread_id],
            |row| row.get(0),
        )?;
        let append_order: u64 = transaction.query_row(
            "SELECT COALESCE(MAX(append_order), 0) + 1 FROM events",
            [],
            |row| row.get(0),
        )?;
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
        let expected: u64 = transaction.query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM events WHERE thread_id = ?1",
            [&event.thread_id],
            |row| row.get(0),
        )?;
        if event.seq != expected {
            return Err(LedgerError::SequenceGap {
                thread_id: event.thread_id.clone(),
                expected,
                found: event.seq,
            });
        }
        let append_order: u64 = transaction.query_row(
            "SELECT COALESCE(MAX(append_order), 0) + 1 FROM events",
            [],
            |row| row.get(0),
        )?;
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
        validate_sequence(thread_id, after_seq + 1, &events)?;
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
        expected += 1;
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
        Event, EventId, Thread, ThreadId, ThreadStatus, Workspace, WorkspaceId, WorkspaceIdentity,
        WorkspaceTrust,
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
