use crate::{CapabilityMode, CommandId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const JSONRPC_VERSION: &str = "2.0";

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ProtocolVersion {
    pub major: u16,
    pub minor: u16,
}

impl ProtocolVersion {
    pub const fn new(major: u16, minor: u16) -> Self {
        Self { major, minor }
    }

    /// Minor versions are backwards compatible; major versions are not.
    pub const fn accepts(self, peer: Self) -> bool {
        self.major == peer.major && self.minor >= peer.minor
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum RpcId {
    String(String),
    Number(i64),
}

impl From<&str> for RpcId {
    fn from(value: &str) -> Self {
        Self::String(value.to_owned())
    }
}

impl From<i64> for RpcId {
    fn from(value: i64) -> Self {
        Self::Number(value)
    }
}

fn jsonrpc_version() -> String {
    JSONRPC_VERSION.to_owned()
}

/// A client command. `command_id` is independent from the JSON-RPC `id` and
/// provides idempotency across reconnects and at-least-once delivery.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[schemars(bound = "P: JsonSchema + Default")]
pub struct CommandEnvelope<P = Value> {
    pub jsonrpc: String,
    pub id: RpcId,
    pub command_id: CommandId,
    pub method: String,
    #[serde(default)]
    pub params: P,
}

impl<P> CommandEnvelope<P> {
    pub fn new(
        id: impl Into<RpcId>,
        command_id: CommandId,
        method: impl Into<String>,
        params: P,
    ) -> Self {
        Self {
            jsonrpc: jsonrpc_version(),
            id: id.into(),
            command_id,
            method: method.into(),
            params,
        }
    }
}

/// A runtime-to-client request, such as an approval or user-input request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ServerRequestEnvelope<P = Value> {
    pub jsonrpc: String,
    pub id: RpcId,
    pub method: String,
    pub params: P,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct NotificationEnvelope<P = Value> {
    pub jsonrpc: String,
    pub method: String,
    pub params: P,
}

impl<P> NotificationEnvelope<P> {
    pub fn new(method: impl Into<String>, params: P) -> Self {
        Self {
            jsonrpc: jsonrpc_version(),
            method: method.into(),
            params,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ResponseEnvelope<R = Value> {
    pub jsonrpc: String,
    pub id: RpcId,
    #[serde(flatten)]
    pub payload: ResponsePayload<R>,
}

impl<R> ResponseEnvelope<R> {
    pub fn success(id: impl Into<RpcId>, result: R) -> Self {
        Self {
            jsonrpc: jsonrpc_version(),
            id: id.into(),
            payload: ResponsePayload::Success { result },
        }
    }

    pub fn failure(id: impl Into<RpcId>, error: RpcError) -> Self {
        Self {
            jsonrpc: jsonrpc_version(),
            id: id.into(),
            payload: ResponsePayload::Failure { error },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ResponsePayload<R> {
    Success { result: R },
    Failure { error: RpcError },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    pub const PARSE_ERROR: i32 = -32700;
    pub const INVALID_REQUEST: i32 = -32600;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
    pub const INTERNAL_ERROR: i32 = -32603;
    pub const INCOMPATIBLE_PROTOCOL: i32 = -32001;
    pub const COMMAND_CONFLICT: i32 = -32002;
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ClientInfo {
    pub name: String,
    pub version: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ClientCapabilities {
    #[serde(default)]
    pub event_replay: bool,
    #[serde(default)]
    pub server_requests: bool,
    #[serde(default)]
    pub rich_content: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    pub protocol_version: ProtocolVersion,
    pub client_info: ClientInfo,
    #[serde(default)]
    pub capabilities: ClientCapabilities,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    pub protocol_version: ProtocolVersion,
    pub server_info: ClientInfo,
    pub capabilities: ServerCapabilities,
    pub host_ceiling: CapabilityMode,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ServerCapabilities {
    #[serde(default)]
    pub unix_socket: bool,
    #[serde(default)]
    pub jobs: bool,
    #[serde(default)]
    pub subagents: bool,
    #[serde(default)]
    pub plugins: bool,
    /// True only when the daemon has registered the M2 write pipeline and the
    /// required host sandbox is available.
    #[serde(default)]
    pub write_tools: bool,
    #[serde(default)]
    pub process_tools: bool,
    #[serde(default)]
    pub sandbox: Option<String>,
}

/// Stable method names. Experimental methods must use an `experimental/` prefix.
pub mod method {
    pub const INITIALIZE: &str = "initialize";
    pub const WORKSPACE_OPEN: &str = "workspace/open";
    pub const WORKSPACE_TRUST: &str = "workspace/trust";
    pub const WORKSPACE_STATUS: &str = "workspace/status";
    pub const THREAD_START: &str = "thread/start";
    pub const THREAD_RESUME: &str = "thread/resume";
    pub const THREAD_FORK: &str = "thread/fork";
    pub const THREAD_READ: &str = "thread/read";
    pub const THREAD_LIST: &str = "thread/list";
    pub const THREAD_ARCHIVE: &str = "thread/archive";
    pub const THREAD_COMPACT: &str = "thread/compact";
    pub const THREAD_SUBSCRIBE: &str = "thread/subscribe";
    pub const TURN_START: &str = "turn/start";
    pub const TURN_STEER: &str = "turn/steer";
    pub const TURN_INTERRUPT: &str = "turn/interrupt";
    /// Resolve an invocation that crossed the execution boundary and was
    /// durably marked `unknown`. The command never executes the invocation
    /// again; it only records explicit external evidence.
    pub const INVOCATION_RECONCILE: &str = "invocation/reconcile";
    pub const MODEL_LIST: &str = "model/list";
    pub const SKILL_LIST: &str = "skill/list";
    pub const MCP_STATUS: &str = "mcp/status";
    pub const PLUGIN_LIST: &str = "plugin/list";
    pub const JOB_CREATE: &str = "job/create";
    pub const JOB_LIST: &str = "job/list";
    pub const JOB_PAUSE: &str = "job/pause";
    pub const JOB_RESUME: &str = "job/resume";
    pub const JOB_RUN: &str = "job/run";
    pub const APPROVAL_REQUEST: &str = "approval/request";
    pub const USER_INPUT: &str = "user/input";
    pub const EVENT: &str = "event";
}
