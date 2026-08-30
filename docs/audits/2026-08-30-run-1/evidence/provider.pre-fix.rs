//! OpenAI-compatible streaming provider adapter.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::{Client, StatusCode};
use serde_json::{json, Value};
use url::Url;
use yeux_core::{ModelEventSink, ModelProvider, PortError};
use yeux_protocol::{
    ContentBlock, ImageSource, MessageRole, ModelEvent, ModelMessage, ModelRequest,
    ProviderCapabilities, StopReason, Usage,
};

const MAX_PROVIDER_ERROR_CHARS: usize = 8 * 1024;
const MAX_SSE_BUFFER_BYTES: usize = 8 * 1024 * 1024;

pub use yeux_core::ModelProvider as RuntimeModelProvider;

#[derive(Debug, Clone)]
pub struct ProviderConfig {
    pub provider_id: String,
    pub base_url: Url,
    pub credential_handle: Option<String>,
    pub organization: Option<String>,
    pub timeout: Duration,
    pub capabilities: ProviderCapabilities,
}

#[async_trait]
pub trait CredentialSource: Send + Sync {
    /// Resolve a short-lived bearer token by opaque handle.
    async fn bearer_token(&self, handle: &str) -> Result<String, ProviderError>;
}

#[derive(Debug, Default)]
pub struct NoCredentials;

#[async_trait]
impl CredentialSource for NoCredentials {
    async fn bearer_token(&self, handle: &str) -> Result<String, ProviderError> {
        Err(ProviderError::CredentialUnavailable(handle.to_owned()))
    }
}

#[derive(Clone)]
pub struct OpenAiCompatibleProvider {
    config: ProviderConfig,
    client: Client,
    credentials: Arc<dyn CredentialSource>,
}

impl std::fmt::Debug for OpenAiCompatibleProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OpenAiCompatibleProvider")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("invalid provider configuration: {0}")]
    Configuration(String),
    #[error("credential is unavailable for handle {0}")]
    CredentialUnavailable(String),
    #[error("provider request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("provider returned HTTP {status}: {body}")]
    HttpStatus { status: StatusCode, body: String },
    #[error("provider stream is invalid: {0}")]
    InvalidStream(String),
    #[error("event sink rejected provider output: {0}")]
    Sink(PortError),
}

impl ProviderError {
    fn port_error(&self) -> PortError {
        let retryable = match self {
            Self::Http(error) => error.is_timeout() || error.is_connect(),
            Self::HttpStatus { status, .. } => {
                *status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
            }
            _ => false,
        };
        PortError {
            code: match self {
                Self::Configuration(_) => "provider_configuration",
                Self::CredentialUnavailable(_) => "credential_unavailable",
                Self::Http(_) => "provider_transport",
                Self::HttpStatus { .. } => "provider_http_status",
                Self::InvalidStream(_) => "provider_invalid_stream",
                Self::Sink(_) => "model_event_sink",
            }
            .into(),
            message: self.to_string(),
            retryable,
        }
    }
}

impl OpenAiCompatibleProvider {
    pub fn new(
        mut config: ProviderConfig,
        credentials: Arc<dyn CredentialSource>,
    ) -> Result<Self, ProviderError> {
        if !matches!(config.base_url.scheme(), "http" | "https") {
            return Err(ProviderError::Configuration(
                "base URL must use http or https".into(),
            ));
        }
        if !config.base_url.username().is_empty() || config.base_url.password().is_some() {
            return Err(ProviderError::Configuration(
                "credentials must not be embedded in the base URL".into(),
            ));
        }
        if config.base_url.query().is_some() || config.base_url.fragment().is_some() {
            return Err(ProviderError::Configuration(
                "base URL must not contain a query or fragment".into(),
            ));
        }
        if !config.base_url.path().ends_with('/') {
            let mut path = config.base_url.path().to_owned();
            path.push('/');
            config.base_url.set_path(&path);
        }
        let client = Client::builder().timeout(config.timeout).build()?;
        Ok(Self {
            config,
            client,
            credentials,
        })
    }

    pub fn without_credentials(config: ProviderConfig) -> Result<Self, ProviderError> {
        Self::new(config, Arc::new(NoCredentials))
    }

    pub async fn collect(&self, request: ModelRequest) -> Result<Vec<ModelEvent>, ProviderError> {
        #[derive(Default)]
        struct VecSink(Vec<ModelEvent>);
        #[async_trait]
        impl ModelEventSink for VecSink {
            async fn emit(&mut self, event: ModelEvent) -> Result<(), PortError> {
                self.0.push(event);
                Ok(())
            }
        }
        let mut sink = VecSink::default();
        self.stream_inner(request, &mut sink).await?;
        Ok(sink.0)
    }

    async fn stream_inner(
        &self,
        request: ModelRequest,
        sink: &mut (dyn ModelEventSink + Send),
    ) -> Result<(), ProviderError> {
        if request.provider != self.config.provider_id {
            return Err(ProviderError::Configuration(format!(
                "request provider {} does not match adapter {}",
                request.provider, self.config.provider_id
            )));
        }
        self.validate_request(&request)?;
        let endpoint = self
            .config
            .base_url
            .join("chat/completions")
            .map_err(|error| ProviderError::Configuration(error.to_string()))?;
        let mut builder = self.client.post(endpoint).json(&request_body(&request)?);
        if let Some(handle) = self.config.credential_handle.as_deref() {
            let token = self.credentials.bearer_token(handle).await?;
            builder = builder.bearer_auth(token);
        }
        if let Some(organization) = &self.config.organization {
            builder = builder.header("OpenAI-Organization", organization);
        }
        let response = builder.send().await?;
        let status = response.status();
        if !status.is_success() {
            let body = truncate_chars(
                response.text().await.unwrap_or_default(),
                MAX_PROVIDER_ERROR_CHARS,
            );
            return Err(ProviderError::HttpStatus { status, body });
        }

        let mut decoder = SseDecoder::default();
        let mut stream = response.bytes_stream();
        let mut tool_ids = BTreeMap::<u64, (String, String)>::new();
        let mut completed = false;
        while let Some(chunk) = stream.next().await {
            for data in decoder.push(&chunk?)? {
                if data == "[DONE]" {
                    if !completed {
                        sink.emit(ModelEvent::Completed {
                            stop_reason: StopReason::EndTurn,
                        })
                        .await
                        .map_err(ProviderError::Sink)?;
                        completed = true;
                    }
                    continue;
                }
                let value: Value = serde_json::from_str(&data).map_err(|error| {
                    ProviderError::InvalidStream(format!("invalid JSON event: {error}"))
                })?;
                for event in parse_chunk(&value, &mut tool_ids)? {
                    completed |= matches!(event, ModelEvent::Completed { .. });
                    sink.emit(event).await.map_err(ProviderError::Sink)?;
                }
            }
        }
        for data in decoder.finish()? {
            if data == "[DONE]" {
                if !completed {
                    sink.emit(ModelEvent::Completed {
                        stop_reason: StopReason::EndTurn,
                    })
                    .await
                    .map_err(ProviderError::Sink)?;
                    completed = true;
                }
                continue;
            }
            let value: Value = serde_json::from_str(&data).map_err(|error| {
                ProviderError::InvalidStream(format!("invalid final JSON event: {error}"))
            })?;
            for event in parse_chunk(&value, &mut tool_ids)? {
                completed |= matches!(event, ModelEvent::Completed { .. });
                sink.emit(event).await.map_err(ProviderError::Sink)?;
            }
        }
        if !completed {
            return Err(ProviderError::InvalidStream(
                "stream ended without a completion marker".into(),
            ));
        }
        Ok(())
    }

    fn validate_request(&self, request: &ModelRequest) -> Result<(), ProviderError> {
        if !request.tools.is_empty() && !self.config.capabilities.tool_calls {
            return Err(ProviderError::Configuration(
                "request contains tools but provider did not negotiate tool calls".into(),
            ));
        }
        let contains_image = request.messages.iter().any(|message| {
            message
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::Image { .. }))
        });
        if contains_image && !self.config.capabilities.vision {
            return Err(ProviderError::Configuration(
                "request contains images but provider did not negotiate vision".into(),
            ));
        }
        if self.config.capabilities.max_context_tokens > 0
            && request.budget.max_input_tokens > self.config.capabilities.max_context_tokens
        {
            return Err(ProviderError::Configuration(format!(
                "input budget {} exceeds provider context limit {}",
                request.budget.max_input_tokens, self.config.capabilities.max_context_tokens
            )));
        }
        Ok(())
    }
}

#[async_trait]
impl ModelProvider for OpenAiCompatibleProvider {
    fn provider_id(&self) -> &str {
        &self.config.provider_id
    }

    fn capabilities(&self) -> ProviderCapabilities {
        self.config.capabilities.clone()
    }

    async fn stream(
        &self,
        request: ModelRequest,
        sink: &mut (dyn ModelEventSink + Send),
    ) -> Result<(), PortError> {
        self.stream_inner(request, sink)
            .await
            .map_err(|error| error.port_error())
    }
}

fn request_body(request: &ModelRequest) -> Result<Value, ProviderError> {
    let messages = request
        .messages
        .iter()
        .map(message_value)
        .collect::<Result<Vec<_>, _>>()?;
    let tools: Vec<_> = request
        .tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "function": {
                    "name": tool.id,
                    "description": tool.description,
                    "parameters": tool.input_schema,
                }
            })
        })
        .collect();
    let mut body = json!({
        "model": request.model,
        "messages": messages,
        "stream": true,
        "stream_options": {"include_usage": true},
        "max_tokens": request.budget.max_output_tokens,
    });
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    Ok(body)
}

fn message_value(message: &ModelMessage) -> Result<Value, ProviderError> {
    let role = match message.role {
        MessageRole::System => "system",
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::Tool => "tool",
    };
    let mut text = String::new();
    let mut media = Vec::new();
    let mut tool_calls = Vec::new();
    let mut tool_call_id = None;
    for block in &message.content {
        match block {
            ContentBlock::Text { text: value } | ContentBlock::Reasoning { text: value } => {
                text.push_str(value)
            }
            ContentBlock::ToolCall {
                call_id,
                name,
                arguments,
            } => tool_calls.push(json!({
                "id": call_id,
                "type": "function",
                "function": {"name": name, "arguments": arguments.to_string()},
            })),
            ContentBlock::ToolResult {
                call_id, content, ..
            } => {
                tool_call_id = Some(call_id.clone());
                text.push_str(&content.to_string());
            }
            ContentBlock::Image { media_type, source } => match source {
                ImageSource::Url { url } => media.push(json!({
                    "type": "image_url",
                    "image_url": {"url": url},
                })),
                ImageSource::Base64 { data } => media.push(json!({
                    "type": "image_url",
                    "image_url": {"url": format!("data:{media_type};base64,{data}")},
                })),
                ImageSource::Artifact { uri } => {
                    return Err(ProviderError::Configuration(format!(
                        "artifact image must be resolved before provider request: {uri}"
                    )));
                }
            },
        }
    }
    let content = if media.is_empty() {
        Value::String(text)
    } else {
        if !text.is_empty() {
            media.insert(0, json!({"type": "text", "text": text}));
        }
        Value::Array(media)
    };
    let mut value = json!({"role": role, "content": content});
    if !tool_calls.is_empty() {
        value["tool_calls"] = Value::Array(tool_calls);
    }
    if let Some(tool_call_id) = tool_call_id {
        value["tool_call_id"] = Value::String(tool_call_id);
    }
    Ok(value)
}

fn parse_chunk(
    value: &Value,
    tool_ids: &mut BTreeMap<u64, (String, String)>,
) -> Result<Vec<ModelEvent>, ProviderError> {
    let mut events = Vec::new();
    if let Some(error) = value.get("error") {
        return Err(ProviderError::InvalidStream(format!(
            "provider error event: {error}"
        )));
    }
    if let Some(usage) = value.get("usage").filter(|usage| !usage.is_null()) {
        events.push(ModelEvent::Usage {
            usage: Usage {
                input_tokens: usage
                    .get("prompt_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                output_tokens: usage
                    .get("completion_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                cached_tokens: usage
                    .pointer("/prompt_tokens_details/cached_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                cost: None,
            },
        });
    }
    let Some(choice) = value.pointer("/choices/0") else {
        return Ok(events);
    };
    let delta = choice.get("delta").unwrap_or(&Value::Null);
    if let Some(text) = delta.get("content").and_then(Value::as_str) {
        events.push(ModelEvent::TextDelta { text: text.into() });
    }
    if let Some(text) = delta.get("reasoning_content").and_then(Value::as_str) {
        events.push(ModelEvent::ReasoningDelta { text: text.into() });
    }
    if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            let index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
            let existing = tool_ids.entry(index).or_default();
            if let Some(call_id) = call.get("id").and_then(Value::as_str) {
                existing.0 = call_id.into();
            }
            if let Some(name) = call.pointer("/function/name").and_then(Value::as_str) {
                existing.1 = name.into();
            }
            let arguments = call
                .pointer("/function/arguments")
                .and_then(Value::as_str)
                .unwrap_or("");
            if !arguments.is_empty() || !existing.0.is_empty() || !existing.1.is_empty() {
                events.push(ModelEvent::ToolCallDelta {
                    call_id: existing.0.clone(),
                    name: existing.1.clone(),
                    json_delta: arguments.into(),
                });
            }
        }
    }
    if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
        events.push(ModelEvent::Completed {
            stop_reason: match reason {
                "stop" => StopReason::EndTurn,
                "length" => StopReason::MaxTokens,
                "tool_calls" | "function_call" => StopReason::ToolUse,
                "content_filter" => StopReason::ContentFilter,
                other => StopReason::Other(other.into()),
            },
        });
    }
    Ok(events)
}

#[derive(Default)]
struct SseDecoder {
    buffer: Vec<u8>,
}

impl SseDecoder {
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>, ProviderError> {
        if self.buffer.len().saturating_add(bytes.len()) > MAX_SSE_BUFFER_BYTES {
            return Err(ProviderError::InvalidStream(format!(
                "SSE buffer exceeds {MAX_SSE_BUFFER_BYTES} bytes"
            )));
        }
        self.buffer.extend_from_slice(bytes);
        self.drain(false)
    }

    fn finish(&mut self) -> Result<Vec<String>, ProviderError> {
        self.drain(true)
    }

    fn drain(&mut self, finish: bool) -> Result<Vec<String>, ProviderError> {
        let mut events = Vec::new();
        loop {
            let boundary = self
                .buffer
                .windows(2)
                .position(|window| window == b"\n\n")
                .map(|position| (position, 2))
                .or_else(|| {
                    self.buffer
                        .windows(4)
                        .position(|window| window == b"\r\n\r\n")
                        .map(|position| (position, 4))
                });
            let Some((position, separator)) = boundary else {
                break;
            };
            let block: Vec<_> = self.buffer.drain(..position + separator).collect();
            if let Some(data) = decode_sse_block(&block[..position])? {
                events.push(data);
            }
        }
        if finish && !self.buffer.is_empty() {
            let remaining = std::mem::take(&mut self.buffer);
            if let Some(data) = decode_sse_block(&remaining)? {
                events.push(data);
            }
        }
        Ok(events)
    }
}

fn truncate_chars(value: String, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        value
    } else {
        value.chars().take(max_chars).collect()
    }
}

fn decode_sse_block(bytes: &[u8]) -> Result<Option<String>, ProviderError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|error| ProviderError::InvalidStream(format!("SSE is not UTF-8: {error}")))?;
    let data: Vec<_> = text
        .lines()
        .filter_map(|line| line.strip_prefix("data:").map(str::trim_start))
        .collect();
    Ok((!data.is_empty()).then(|| data.join("\n")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;
    use uuid::Uuid;
    use yeux_protocol::{ModelRequestId, ReasoningSupport, TokenBudget, TurnId};

    fn config(server: &MockServer) -> ProviderConfig {
        base_config(Url::parse(&format!("{}/v1/", server.base_url())).unwrap())
    }

    fn base_config(base_url: Url) -> ProviderConfig {
        ProviderConfig {
            provider_id: "test".into(),
            base_url,
            credential_handle: None,
            organization: None,
            timeout: Duration::from_secs(2),
            capabilities: ProviderCapabilities {
                tool_calls: true,
                parallel_tool_calls: false,
                reasoning: ReasoningSupport::Summary,
                vision: false,
                prompt_cache: false,
                max_context_tokens: 8_192,
            },
        }
    }

    fn request() -> ModelRequest {
        ModelRequest {
            request_id: ModelRequestId::from_uuid(Uuid::now_v7()),
            turn_id: TurnId::from_uuid(Uuid::now_v7()),
            provider: "test".into(),
            model: "model".into(),
            messages: vec![ModelMessage {
                role: MessageRole::User,
                content: vec![ContentBlock::Text { text: "hi".into() }],
            }],
            tools: vec![],
            budget: TokenBudget {
                max_input_tokens: 100,
                max_output_tokens: 20,
            },
            metadata: Value::Null,
        }
    }

    #[tokio::test]
    async fn parses_fragmented_semantics_and_emits_one_completion() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200)
                .header("content-type", "text/event-stream")
                .body(concat!(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\n",
                    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                    "data: [DONE]\n\n"
                ));
        });
        let provider = OpenAiCompatibleProvider::without_credentials(config(&server)).unwrap();
        let events = provider.collect(request()).await.unwrap();
        mock.assert();
        assert!(matches!(&events[0], ModelEvent::TextDelta { text } if text == "hello"));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, ModelEvent::Completed { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn sse_decoder_handles_arbitrary_transport_chunking() {
        let mut decoder = SseDecoder::default();
        assert!(decoder.push(b"data: {\"a").unwrap().is_empty());
        assert_eq!(
            decoder.push(b"\":1}\n\ndata: [DONE]\n\n").unwrap(),
            vec!["{\"a\":1}", "[DONE]"]
        );
    }

    #[test]
    fn sse_decoder_rejects_an_unbounded_partial_event() {
        let mut decoder = SseDecoder::default();
        let oversized = vec![b'x'; MAX_SSE_BUFFER_BYTES + 1];
        assert!(matches!(
            decoder.push(&oversized),
            Err(ProviderError::InvalidStream(_))
        ));
        assert!(decoder.buffer.is_empty());
    }

    #[test]
    fn provider_error_truncation_preserves_utf8_boundaries() {
        let body = "界".repeat(MAX_PROVIDER_ERROR_CHARS + 1);
        let truncated = truncate_chars(body, MAX_PROVIDER_ERROR_CHARS);
        assert_eq!(truncated.chars().count(), MAX_PROVIDER_ERROR_CHARS);
        assert!(truncated.is_char_boundary(truncated.len()));
    }

    #[test]
    fn normalizes_base_url_and_rejects_embedded_credentials() {
        let provider = OpenAiCompatibleProvider::without_credentials(base_config(
            Url::parse("https://example.test/v1").unwrap(),
        ))
        .unwrap();
        assert_eq!(provider.config.base_url.path(), "/v1/");

        let error = OpenAiCompatibleProvider::without_credentials(base_config(
            Url::parse("https://token@example.test/v1").unwrap(),
        ))
        .unwrap_err();
        assert!(matches!(error, ProviderError::Configuration(_)));
    }

    #[tokio::test]
    async fn capability_negotiation_rejects_unsupported_vision_before_network() {
        let provider = OpenAiCompatibleProvider::without_credentials(base_config(
            Url::parse("http://127.0.0.1:1/v1/").unwrap(),
        ))
        .unwrap();
        let mut request = request();
        request.messages[0].content.push(ContentBlock::Image {
            media_type: "image/png".into(),
            source: ImageSource::Url {
                url: "https://example.test/image.png".into(),
            },
        });
        assert!(matches!(
            provider.collect(request).await,
            Err(ProviderError::Configuration(_))
        ));
    }

    #[test]
    fn multimodal_message_uses_openai_data_url_shape() {
        let message = ModelMessage {
            role: MessageRole::User,
            content: vec![
                ContentBlock::Text {
                    text: "inspect".into(),
                },
                ContentBlock::Image {
                    media_type: "image/png".into(),
                    source: ImageSource::Base64 {
                        data: "YWJj".into(),
                    },
                },
            ],
        };
        let value = message_value(&message).unwrap();
        assert_eq!(
            value
                .pointer("/content/1/image_url/url")
                .and_then(Value::as_str),
            Some("data:image/png;base64,YWJj")
        );
    }

    #[tokio::test]
    async fn errors_if_stream_has_no_completion() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200)
                .header("content-type", "text/event-stream")
                .body("data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n");
        });
        let provider = OpenAiCompatibleProvider::without_credentials(config(&server)).unwrap();
        assert!(matches!(
            provider.collect(request()).await,
            Err(ProviderError::InvalidStream(_))
        ));
    }
}
