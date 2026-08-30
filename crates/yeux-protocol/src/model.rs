use crate::{ModelRequestId, ToolSpec, TurnId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningSupport {
    None,
    Summary,
    Full,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ProviderCapabilities {
    pub tool_calls: bool,
    pub parallel_tool_calls: bool,
    pub reasoning: ReasoningSupport,
    pub vision: bool,
    pub prompt_cache: bool,
    pub max_context_tokens: u64,
}

impl Default for ProviderCapabilities {
    fn default() -> Self {
        Self {
            tool_calls: false,
            parallel_tool_calls: false,
            reasoning: ReasoningSupport::None,
            vision: false,
            prompt_cache: false,
            max_context_tokens: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Reasoning {
        text: String,
    },
    Image {
        media_type: String,
        #[serde(flatten)]
        source: ImageSource,
    },
    ToolCall {
        call_id: String,
        name: String,
        arguments: Value,
    },
    ToolResult {
        call_id: String,
        content: Value,
        #[serde(default)]
        is_error: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "source_type", rename_all = "snake_case")]
pub enum ImageSource {
    Url { url: String },
    Base64 { data: String },
    Artifact { uri: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelMessage {
    pub role: MessageRole,
    pub content: Vec<ContentBlock>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TokenBudget {
    pub max_input_tokens: u64,
    pub max_output_tokens: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelRequest {
    pub request_id: ModelRequestId,
    pub turn_id: TurnId,
    pub provider: String,
    pub model: String,
    pub messages: Vec<ModelMessage>,
    #[serde(default)]
    pub tools: Vec<ToolSpec>,
    pub budget: TokenBudget,
    #[serde(default)]
    pub metadata: Value,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Money {
    pub currency: String,
    /// Millionths of the currency unit; avoids floating-point accounting.
    pub micros: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<Money>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    ToolUse,
    Cancelled,
    ContentFilter,
    Other(String),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelEvent {
    TextDelta {
        text: String,
    },
    ReasoningDelta {
        text: String,
    },
    ToolCallDelta {
        call_id: String,
        name: String,
        json_delta: String,
    },
    Usage {
        usage: Usage,
    },
    Completed {
        stop_reason: StopReason,
    },
    Failed {
        code: String,
        message: String,
        #[serde(default)]
        retryable: bool,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelDescriptor {
    pub provider: String,
    pub model: String,
    pub display_name: String,
    pub capabilities: ProviderCapabilities,
}
