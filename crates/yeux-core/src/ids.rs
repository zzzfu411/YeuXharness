use std::{collections::VecDeque, sync::Mutex};
use thiserror::Error;
use uuid::{Uuid, Version};
use yeux_protocol::{
    AgentRunId, ApprovalId, ArtifactId, CommandId, EventId, InvocationId, ItemId, JobId,
    ModelRequestId, ThreadId, TurnId, WorkspaceId,
};

#[derive(Debug, Error, Clone, Eq, PartialEq)]
pub enum IdError {
    #[error("the injected ID sequence is exhausted")]
    Exhausted,
    #[error("expected a UUIDv7, got {0}")]
    NotVersionSeven(Uuid),
}

/// UUID source. Tests inject a deterministic sequence; production uses UUIDv7.
pub trait IdGenerator: Send + Sync {
    fn next_uuid(&self) -> Result<Uuid, IdError>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct UuidV7Generator;

impl IdGenerator for UuidV7Generator {
    fn next_uuid(&self) -> Result<Uuid, IdError> {
        Ok(Uuid::now_v7())
    }
}

#[derive(Debug)]
pub struct SequenceIdGenerator {
    values: Mutex<VecDeque<Uuid>>,
}

impl SequenceIdGenerator {
    pub fn new(values: impl IntoIterator<Item = Uuid>) -> Result<Self, IdError> {
        let values: VecDeque<_> = values.into_iter().collect();
        if let Some(value) = values
            .iter()
            .find(|value| value.get_version() != Some(Version::SortRand))
        {
            return Err(IdError::NotVersionSeven(*value));
        }
        Ok(Self {
            values: Mutex::new(values),
        })
    }
}

impl IdGenerator for SequenceIdGenerator {
    fn next_uuid(&self) -> Result<Uuid, IdError> {
        self.values
            .lock()
            .expect("sequence ID mutex poisoned")
            .pop_front()
            .ok_or(IdError::Exhausted)
    }
}

/// Typed ID facade; prevents accidentally using a turn ID as an invocation ID.
pub struct IdFactory<G> {
    generator: G,
}

impl<G: IdGenerator> IdFactory<G> {
    pub fn new(generator: G) -> Self {
        Self { generator }
    }

    fn next(&self) -> Result<Uuid, IdError> {
        let value = self.generator.next_uuid()?;
        if value.get_version() != Some(Version::SortRand) {
            return Err(IdError::NotVersionSeven(value));
        }
        Ok(value)
    }

    pub fn command(&self) -> Result<CommandId, IdError> {
        self.next().map(Into::into)
    }
    pub fn event(&self) -> Result<EventId, IdError> {
        self.next().map(Into::into)
    }
    pub fn workspace(&self) -> Result<WorkspaceId, IdError> {
        self.next().map(Into::into)
    }
    pub fn thread(&self) -> Result<ThreadId, IdError> {
        self.next().map(Into::into)
    }
    pub fn turn(&self) -> Result<TurnId, IdError> {
        self.next().map(Into::into)
    }
    pub fn item(&self) -> Result<ItemId, IdError> {
        self.next().map(Into::into)
    }
    pub fn invocation(&self) -> Result<InvocationId, IdError> {
        self.next().map(Into::into)
    }
    pub fn job(&self) -> Result<JobId, IdError> {
        self.next().map(Into::into)
    }
    pub fn agent_run(&self) -> Result<AgentRunId, IdError> {
        self.next().map(Into::into)
    }
    pub fn model_request(&self) -> Result<ModelRequestId, IdError> {
        self.next().map(Into::into)
    }
    pub fn approval(&self) -> Result<ApprovalId, IdError> {
        self.next().map(Into::into)
    }
    pub fn artifact(&self) -> Result<ArtifactId, IdError> {
        self.next().map(Into::into)
    }
}
