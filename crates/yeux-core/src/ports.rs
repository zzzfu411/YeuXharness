use async_trait::async_trait;
use thiserror::Error;
use yeux_protocol::{
    EventEnvelope, ModelEvent, ModelRequest, PreparedInvocation, ProviderCapabilities, ThreadId,
    ToolResult, ToolSpec,
};

#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[error("{code}: {message}")]
pub struct PortError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
}

#[async_trait]
pub trait ModelEventSink: Send {
    async fn emit(&mut self, event: ModelEvent) -> Result<(), PortError>;
}

/// Provider adapter port. Implementations live in `yeux-runtime`.
#[async_trait]
pub trait ModelProvider: Send + Sync {
    fn provider_id(&self) -> &str;
    fn capabilities(&self) -> ProviderCapabilities;
    async fn stream(
        &self,
        request: ModelRequest,
        sink: &mut (dyn ModelEventSink + Send),
    ) -> Result<(), PortError>;
}

/// Executor port accepts only a fully prepared token. Runtime must call the
/// core policy and approval validators before this method.
#[async_trait]
pub trait ToolExecutor: Send + Sync {
    fn specs(&self) -> Vec<ToolSpec>;
    async fn execute_prepared(
        &self,
        invocation: &PreparedInvocation,
    ) -> Result<ToolResult, PortError>;
}

/// Append-only event storage port. Implementations must atomically reject a
/// duplicate event ID or sequence number.
#[async_trait]
pub trait EventStore: Send + Sync {
    async fn append(&self, event: &EventEnvelope) -> Result<(), PortError>;
    async fn load_thread(
        &self,
        thread_id: ThreadId,
        after_seq: u64,
    ) -> Result<Vec<EventEnvelope>, PortError>;
}
