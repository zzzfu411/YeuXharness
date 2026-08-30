//! Persistent descriptors for runtime features whose executors land in later milestones.
//!
//! These records are intentionally inert: loading a plugin/MCP/skill descriptor
//! never executes project-controlled code.

use std::{fs, path::Path, sync::Mutex};

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DescriptorKind {
    Skill,
    McpServer,
    Plugin,
    Provider,
    Command,
}

impl DescriptorKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Skill => "skill",
            Self::McpServer => "mcp_server",
            Self::Plugin => "plugin",
            Self::Provider => "provider",
            Self::Command => "command",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegisteredDescriptor {
    pub id: String,
    pub kind: DescriptorKind,
    pub version: String,
    /// BLAKE3 digest of the trusted descriptor/manifest bytes.
    pub source_digest: String,
    pub enabled: bool,
    /// Provider-specific data. It is data only and must never contain secrets.
    pub descriptor: Value,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobRecord {
    pub id: String,
    pub schedule: String,
    pub prompt: String,
    pub model: String,
    pub workspace_id: String,
    pub permission_profile: String,
    pub tool_ids: Vec<String>,
    pub budget: Value,
    pub status: String,
    pub config_snapshot: Value,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentRecord {
    pub id: String,
    pub parent_id: Option<String>,
    pub thread_id: String,
    pub workspace_lease: Value,
    pub capability_grant: Value,
    pub budget: Value,
    pub status: String,
    pub handoff: Option<Value>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, thiserror::Error)]
pub enum DescriptorError {
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("descriptor store mutex is poisoned")]
    Poisoned,
    #[error("unknown descriptor kind: {0}")]
    UnknownKind(String),
}

pub type DescriptorResult<T> = std::result::Result<T, DescriptorError>;

pub struct DescriptorStore {
    connection: Mutex<Connection>,
}

impl std::fmt::Debug for DescriptorStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DescriptorStore")
            .finish_non_exhaustive()
    }
}

impl DescriptorStore {
    pub fn open(path: impl AsRef<Path>) -> DescriptorResult<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(path)?;
        configure(&connection)?;
        initialize(&connection)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        }
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    pub fn open_in_memory() -> DescriptorResult<Self> {
        let connection = Connection::open_in_memory()?;
        configure(&connection)?;
        initialize(&connection)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    pub fn put_descriptor(&self, descriptor: &RegisteredDescriptor) -> DescriptorResult<()> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| DescriptorError::Poisoned)?;
        connection.execute(
            "INSERT INTO descriptors (id, kind, version, source_digest, enabled, descriptor_json, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id, kind) DO UPDATE SET
                version = excluded.version,
                source_digest = excluded.source_digest,
                enabled = excluded.enabled,
                descriptor_json = excluded.descriptor_json,
                updated_at = excluded.updated_at",
            params![
                descriptor.id,
                descriptor.kind.as_str(),
                descriptor.version,
                descriptor.source_digest,
                descriptor.enabled,
                serde_json::to_string(&descriptor.descriptor)?,
                descriptor.updated_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn descriptor(
        &self,
        kind: DescriptorKind,
        id: &str,
    ) -> DescriptorResult<Option<RegisteredDescriptor>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| DescriptorError::Poisoned)?;
        Ok(connection
            .query_row(
                "SELECT id, kind, version, source_digest, enabled, descriptor_json, updated_at
                 FROM descriptors WHERE id = ?1 AND kind = ?2",
                params![id, kind.as_str()],
                descriptor_from_row,
            )
            .optional()?)
    }

    pub fn descriptors(&self, kind: DescriptorKind) -> DescriptorResult<Vec<RegisteredDescriptor>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| DescriptorError::Poisoned)?;
        let mut statement = connection.prepare(
            "SELECT id, kind, version, source_digest, enabled, descriptor_json, updated_at
             FROM descriptors WHERE kind = ?1 ORDER BY id ASC",
        )?;
        let rows = statement.query_map([kind.as_str()], descriptor_from_row)?;
        let descriptors = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(descriptors)
    }

    pub fn put_job(&self, job: &JobRecord) -> DescriptorResult<()> {
        self.put_record("jobs", &job.id, job)
    }

    pub fn job(&self, id: &str) -> DescriptorResult<Option<JobRecord>> {
        self.get_record("jobs", id)
    }

    pub fn jobs(&self) -> DescriptorResult<Vec<JobRecord>> {
        self.list_records("jobs")
    }

    pub fn put_agent(&self, agent: &AgentRecord) -> DescriptorResult<()> {
        self.put_record("agents", &agent.id, agent)
    }

    pub fn agent(&self, id: &str) -> DescriptorResult<Option<AgentRecord>> {
        self.get_record("agents", id)
    }

    pub fn agents(&self) -> DescriptorResult<Vec<AgentRecord>> {
        self.list_records("agents")
    }

    fn put_record<T: Serialize>(&self, table: &str, id: &str, value: &T) -> DescriptorResult<()> {
        debug_assert!(matches!(table, "jobs" | "agents"));
        let sql = format!(
            "INSERT INTO {table} (id, record_json) VALUES (?1, ?2)
             ON CONFLICT(id) DO UPDATE SET record_json = excluded.record_json"
        );
        let connection = self
            .connection
            .lock()
            .map_err(|_| DescriptorError::Poisoned)?;
        connection.execute(&sql, params![id, serde_json::to_string(value)?])?;
        Ok(())
    }

    fn get_record<T: for<'de> Deserialize<'de>>(
        &self,
        table: &str,
        id: &str,
    ) -> DescriptorResult<Option<T>> {
        debug_assert!(matches!(table, "jobs" | "agents"));
        let sql = format!("SELECT record_json FROM {table} WHERE id = ?1");
        let connection = self
            .connection
            .lock()
            .map_err(|_| DescriptorError::Poisoned)?;
        let json: Option<String> = connection
            .query_row(&sql, [id], |row| row.get(0))
            .optional()?;
        json.map(|value| serde_json::from_str(&value).map_err(Into::into))
            .transpose()
    }

    fn list_records<T: for<'de> Deserialize<'de>>(&self, table: &str) -> DescriptorResult<Vec<T>> {
        debug_assert!(matches!(table, "jobs" | "agents"));
        let sql = format!("SELECT record_json FROM {table} ORDER BY id ASC");
        let connection = self
            .connection
            .lock()
            .map_err(|_| DescriptorError::Poisoned)?;
        let mut statement = connection.prepare(&sql)?;
        let json = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        json.into_iter()
            .map(|value| serde_json::from_str(&value).map_err(Into::into))
            .collect()
    }
}

fn configure(connection: &Connection) -> DescriptorResult<()> {
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "synchronous", "FULL")?;
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(())
}

fn initialize(connection: &Connection) -> DescriptorResult<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS descriptors (
            id TEXT NOT NULL,
            kind TEXT NOT NULL,
            version TEXT NOT NULL,
            source_digest TEXT NOT NULL,
            enabled INTEGER NOT NULL,
            descriptor_json TEXT NOT NULL CHECK(json_valid(descriptor_json)),
            updated_at TEXT NOT NULL,
            PRIMARY KEY(id, kind)
         ) WITHOUT ROWID;
         CREATE TABLE IF NOT EXISTS jobs (
            id TEXT PRIMARY KEY,
            record_json TEXT NOT NULL CHECK(json_valid(record_json))
         ) WITHOUT ROWID;
         CREATE TABLE IF NOT EXISTS agents (
            id TEXT PRIMARY KEY,
            record_json TEXT NOT NULL CHECK(json_valid(record_json))
         ) WITHOUT ROWID;",
    )?;
    Ok(())
}

fn descriptor_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RegisteredDescriptor> {
    let kind: String = row.get(1)?;
    let kind = match kind.as_str() {
        "skill" => DescriptorKind::Skill,
        "mcp_server" => DescriptorKind::McpServer,
        "plugin" => DescriptorKind::Plugin,
        "provider" => DescriptorKind::Provider,
        "command" => DescriptorKind::Command,
        _ => {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                1,
                rusqlite::types::Type::Text,
                Box::new(DescriptorError::UnknownKind(kind)),
            ));
        }
    };
    let descriptor_json: String = row.get(5)?;
    let updated_at: String = row.get(6)?;
    Ok(RegisteredDescriptor {
        id: row.get(0)?,
        kind,
        version: row.get(2)?,
        source_digest: row.get(3)?,
        enabled: row.get(4)?,
        descriptor: serde_json::from_str(&descriptor_json).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                5,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
        updated_at: DateTime::parse_from_rfc3339(&updated_at)
            .map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    6,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?
            .with_timezone(&Utc),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn descriptors_round_trip_without_execution() {
        let store = DescriptorStore::open_in_memory().unwrap();
        let descriptor = RegisteredDescriptor {
            id: "example".into(),
            kind: DescriptorKind::Plugin,
            version: "1".into(),
            source_digest: "abc".into(),
            enabled: false,
            descriptor: json!({"command": "must-not-run"}),
            updated_at: Utc::now(),
        };
        store.put_descriptor(&descriptor).unwrap();
        assert_eq!(
            store.descriptor(DescriptorKind::Plugin, "example").unwrap(),
            Some(descriptor)
        );
    }

    #[test]
    fn jobs_and_agents_are_persistent_placeholders() {
        let store = DescriptorStore::open_in_memory().unwrap();
        let job = JobRecord {
            id: "daily".into(),
            schedule: "daily".into(),
            prompt: "inspect".into(),
            model: "local".into(),
            workspace_id: "w".into(),
            permission_profile: "observe".into(),
            tool_ids: vec!["workspace.read".into()],
            budget: json!({"turns": 1}),
            status: "paused".into(),
            config_snapshot: json!({}),
            updated_at: Utc::now(),
        };
        store.put_job(&job).unwrap();
        assert_eq!(store.job("daily").unwrap(), Some(job));
        assert_eq!(store.jobs().unwrap().len(), 1);
    }
}
