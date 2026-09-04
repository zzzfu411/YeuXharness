//! OpenAI-compatible streaming provider adapter.

use std::{collections::BTreeMap, fmt, sync::Arc, time::Duration};

use async_trait::async_trait;
use futures_util::{Stream, StreamExt};
use reqwest::{Client, StatusCode};
use serde_json::{json, Value};
use url::Url;
use yeux_core::{ModelEventSink, ModelProvider, PortError};
use yeux_protocol::{
    ContentBlock, ImageSource, MessageRole, ModelEvent, ModelMessage, ModelRequest,
    ProviderCapabilities, StopReason, Usage,
};

pub use crate::credentials::CredentialBroker;
use crate::credentials::CredentialLease;

const MAX_PROVIDER_ERROR_BYTES: usize = 8 * 1024;
/// Credential handles are identifiers, not secret values. Keep them bounded
/// so a malformed handle cannot become an unbounded diagnostic or header.
pub const MAX_CREDENTIAL_HANDLE_BYTES: usize = 256;
/// Upper bound for one in-memory bearer value. Provider tokens are headers,
/// not arbitrary payloads; bounding them also limits broker-induced memory
/// pressure.
pub const MAX_CREDENTIAL_VALUE_BYTES: usize = 16 * 1024;
const MAX_SSE_BUFFER_BYTES: usize = 8 * 1024 * 1024;
const MAX_SSE_STREAM_BYTES: usize = 64 * 1024 * 1024;
const MAX_SSE_EVENTS: usize = 100_000;
const MAX_MODEL_EVENTS: usize = 100_000;
const MAX_ACCUMULATED_OUTPUT_BYTES: usize = 32 * 1024 * 1024;
const MAX_TRACKED_TOOL_CALLS: usize = 4_096;

pub use yeux_core::ModelProvider as RuntimeModelProvider;

#[derive(Clone)]
pub struct ProviderConfig {
    pub provider_id: String,
    pub base_url: Url,
    pub credential_handle: Option<String>,
    pub organization: Option<String>,
    pub timeout: Duration,
    pub capabilities: ProviderCapabilities,
}

impl fmt::Debug for ProviderConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderConfig")
            .field("provider_id", &self.provider_id)
            .field("base_url", &redacted_url(&self.base_url))
            // Handles are opaque identifiers. Redact them as well so an
            // accidentally supplied token cannot appear in debug output.
            .field(
                "credential_handle",
                &self.credential_handle.as_ref().map(|_| "REDACTED"),
            )
            .field("organization", &self.organization)
            .field("timeout", &self.timeout)
            .field("capabilities", &self.capabilities)
            .finish()
    }
}

fn redacted_url(url: &Url) -> String {
    let mut value = url.clone();
    // `Url`'s normal Debug/Display includes userinfo. Scrub it even for an
    // invalid configuration so diagnostics cannot reveal a token before the
    // constructor rejects embedded credentials.
    let _ = value.set_username("");
    let _ = value.set_password(None);
    value.to_string()
}

#[async_trait]
pub trait CredentialSource: Send + Sync {
    /// Resolve a short-lived bearer token by opaque handle.
    async fn bearer_token(&self, handle: &str) -> Result<String, ProviderError>;
}

/// Adapter that keeps the broker boundary explicit for provider clients. The
/// raw bearer token exists only inside the provider HTTP request; it is never
/// included in a tool argument, effect, event, or debug value.
pub struct BrokerCredentialSource {
    broker: Arc<dyn CredentialBroker>,
}

impl std::fmt::Debug for BrokerCredentialSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("BrokerCredentialSource(REDACTED)")
    }
}

impl BrokerCredentialSource {
    pub fn new(broker: Arc<dyn CredentialBroker>) -> Self {
        Self { broker }
    }
}

#[async_trait]
impl CredentialSource for BrokerCredentialSource {
    async fn bearer_token(&self, handle: &str) -> Result<String, ProviderError> {
        // Never propagate broker-provided error text. A faulty adapter could
        // accidentally put the raw credential in `CredentialError::Unavailable`;
        // only the already-validated opaque handle is safe to expose.
        let lease: CredentialLease = self
            .broker
            .resolve(handle)
            .await
            .map_err(|_| ProviderError::CredentialUnavailable(handle.to_owned()))?;
        Ok(lease.with_value(str::to_owned))
    }
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

#[derive(thiserror::Error)]
pub enum ProviderError {
    #[error("invalid provider configuration: {0}")]
    Configuration(String),
    // The payload remains available for typed handling, but is deliberately
    // omitted from Display because callers may accidentally use a raw token as
    // the opaque handle. Diagnostics must never render either value.
    #[error("credential is unavailable")]
    CredentialUnavailable(String),
    #[error("credential value is invalid")]
    CredentialInvalid,
    #[error("provider request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("provider returned HTTP {status}: {body}")]
    HttpStatus { status: StatusCode, body: String },
    #[error("provider stream is invalid: {0}")]
    InvalidStream(String),
    #[error("provider SSE stream exceeds the {limit}-byte response limit")]
    StreamBytesLimit { limit: usize },
    #[error("provider SSE stream exceeds the {limit}-event limit")]
    StreamEventLimit { limit: usize },
    #[error("provider output exceeds the {limit}-event limit")]
    ModelEventLimit { limit: usize },
    #[error("provider accumulated output exceeds the {limit}-byte limit")]
    OutputBytesLimit { limit: usize },
    #[error("provider stream exceeds the {limit}-tool-call state limit")]
    ToolCallLimit { limit: usize },
    #[error("event sink rejected provider output: {0}")]
    Sink(PortError),
}

impl fmt::Debug for ProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Keep diagnostics useful without allowing an opaque credential handle
        // (or a future accidentally populated sensitive payload) to escape.
        match self {
            Self::Configuration(message) => formatter
                .debug_tuple("ProviderError::Configuration")
                .field(message)
                .finish(),
            Self::CredentialUnavailable(_) => {
                formatter.write_str("ProviderError::CredentialUnavailable(REDACTED)")
            }
            Self::CredentialInvalid => formatter.write_str("ProviderError::CredentialInvalid"),
            Self::Http(error) => formatter
                .debug_tuple("ProviderError::Http")
                .field(error)
                .finish(),
            Self::HttpStatus { status, .. } => formatter
                .debug_struct("ProviderError::HttpStatus")
                .field("status", status)
                .field("body", &"REDACTED")
                .finish(),
            Self::InvalidStream(message) => formatter
                .debug_tuple("ProviderError::InvalidStream")
                .field(message)
                .finish(),
            Self::StreamBytesLimit { limit } => formatter
                .debug_struct("ProviderError::StreamBytesLimit")
                .field("limit", limit)
                .finish(),
            Self::StreamEventLimit { limit } => formatter
                .debug_struct("ProviderError::StreamEventLimit")
                .field("limit", limit)
                .finish(),
            Self::ModelEventLimit { limit } => formatter
                .debug_struct("ProviderError::ModelEventLimit")
                .field("limit", limit)
                .finish(),
            Self::OutputBytesLimit { limit } => formatter
                .debug_struct("ProviderError::OutputBytesLimit")
                .field("limit", limit)
                .finish(),
            Self::ToolCallLimit { limit } => formatter
                .debug_struct("ProviderError::ToolCallLimit")
                .field("limit", limit)
                .finish(),
            Self::Sink(error) => formatter
                .debug_tuple("ProviderError::Sink")
                .field(error)
                .finish(),
        }
    }
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
                Self::CredentialInvalid => "credential_invalid",
                Self::Http(_) => "provider_transport",
                Self::HttpStatus { .. } => "provider_http_status",
                Self::InvalidStream(_) => "provider_invalid_stream",
                Self::StreamBytesLimit { .. } => "provider_stream_bytes_limit",
                Self::StreamEventLimit { .. } => "provider_stream_event_limit",
                Self::ModelEventLimit { .. } => "provider_model_event_limit",
                Self::OutputBytesLimit { .. } => "provider_output_bytes_limit",
                Self::ToolCallLimit { .. } => "provider_tool_call_limit",
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
        if let Some(handle) = config.credential_handle.as_deref() {
            validate_credential_handle(handle)?;
        }
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

    pub fn with_credential_broker(
        config: ProviderConfig,
        broker: Arc<dyn CredentialBroker>,
    ) -> Result<Self, ProviderError> {
        Self::new(config, Arc::new(BrokerCredentialSource::new(broker)))
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
        let mut bearer_token = None;
        if let Some(handle) = self.config.credential_handle.as_deref() {
            let token = self.credentials.bearer_token(handle).await?;
            validate_credential_value(&token)?;
            builder = builder.bearer_auth(&token);
            bearer_token = Some(token);
        }
        if let Some(organization) = &self.config.organization {
            builder = builder.header("OpenAI-Organization", organization);
        }
        let response = builder.send().await?;
        let status = response.status();
        if !status.is_success() {
            let body = read_provider_error_body(response, bearer_token.as_deref()).await;
            return Err(ProviderError::HttpStatus { status, body });
        }

        let mut decoder = SseDecoder::default();
        // The provider may echo an Authorization value across several SSE
        // events. Keep the matcher request-scoped so a split credential is
        // never emitted while also ensuring no credential survives into a
        // provider cache or a later request.
        let mut credential_redactor = CredentialStreamRedactor::new(bearer_token.as_deref());
        let mut stream = response.bytes_stream();
        let mut tool_ids = BTreeMap::<u64, (String, String)>::new();
        let mut accounting = StreamAccounting::default();
        let mut completed = false;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            accounting
                .record_stream_bytes(chunk.len())
                .map_err(|error| redact_provider_error(error, bearer_token.as_deref()))?;
            for data in decoder
                .push(&chunk)
                .map_err(|error| redact_provider_error(error, bearer_token.as_deref()))?
            {
                process_sse_data(
                    data,
                    sink,
                    &mut tool_ids,
                    &mut accounting,
                    &mut completed,
                    &mut credential_redactor,
                )
                .await
                .map_err(|error| redact_provider_error(error, bearer_token.as_deref()))?;
            }
        }
        for data in decoder
            .finish()
            .map_err(|error| redact_provider_error(error, bearer_token.as_deref()))?
        {
            process_sse_data(
                data,
                sink,
                &mut tool_ids,
                &mut accounting,
                &mut completed,
                &mut credential_redactor,
            )
            .await
            .map_err(|error| redact_provider_error(error, bearer_token.as_deref()))?;
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

fn validate_credential_handle(handle: &str) -> Result<(), ProviderError> {
    if handle.is_empty()
        || handle.len() > MAX_CREDENTIAL_HANDLE_BYTES
        || handle
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        return Err(ProviderError::Configuration(format!(
            "credential handle must be a non-empty opaque identifier of at most {MAX_CREDENTIAL_HANDLE_BYTES} bytes without whitespace or control characters"
        )));
    }
    Ok(())
}

fn validate_credential_value(value: &str) -> Result<(), ProviderError> {
    // HTTP Authorization values must not contain control bytes, whitespace,
    // or non-ASCII bytes. Rejecting them before reqwest builds the request
    // prevents header injection and keeps malformed values out of diagnostics.
    if value.is_empty()
        || value.len() > MAX_CREDENTIAL_VALUE_BYTES
        || value
            .bytes()
            .any(|byte| byte <= b' ' || byte == 0x7f || byte >= 0x80)
    {
        return Err(ProviderError::CredentialInvalid);
    }
    Ok(())
}

/// A request-scoped search pattern. Only the failure table is owned; the raw
/// credential stays borrowed from the request-local bearer value.
struct SecretPattern<'a> {
    value: &'a str,
    failure: Vec<usize>,
    replacement: &'static str,
}

impl<'a> SecretPattern<'a> {
    fn new(value: &'a str) -> Option<Self> {
        if value.is_empty() {
            return None;
        }

        let bytes = value.as_bytes();
        let mut failure = vec![0; bytes.len()];
        let mut prefix = 0;
        for index in 1..bytes.len() {
            while prefix > 0 && bytes[index] != bytes[prefix] {
                prefix = failure[prefix - 1];
            }
            if bytes[index] == bytes[prefix] {
                prefix += 1;
            }
            failure[index] = prefix;
        }

        // Credential validation rejects whitespace, so the surrounding spaces
        // make it impossible for otherwise-safe output on either side to join
        // into the secret. Pick a label that also does not contain the secret;
        // the single-space fallback is safe for every accepted credential.
        let replacement = [" [REDACTED] ", " [CREDENTIAL] ", " [HIDDEN] ", " "]
            .into_iter()
            .find(|candidate| !candidate.contains(value))
            .unwrap_or("");

        Some(Self {
            value,
            failure,
            replacement,
        })
    }

    fn bytes(&self) -> &[u8] {
        self.value.as_bytes()
    }
}

/// Streaming KMP state. The withheld bytes are always exactly
/// `pattern[..matched]`, so retaining the length is sufficient and bounds each
/// logical stream to at most `credential.len() - 1` pending bytes.
#[derive(Default)]
struct SecretStreamState {
    matched: usize,
}

impl SecretStreamState {
    fn redact_chunk(
        &mut self,
        pattern: &SecretPattern<'_>,
        chunk: &str,
        limit: usize,
    ) -> Result<String, ProviderError> {
        let pattern_bytes = pattern.bytes();
        let mut output = Vec::with_capacity(chunk.len().min(limit));

        for &byte in chunk.as_bytes() {
            while self.matched > 0 && byte != pattern_bytes[self.matched] {
                let previous = self.matched;
                self.matched = pattern.failure[previous - 1];
                append_redacted_bytes(
                    &mut output,
                    &pattern_bytes[..previous - self.matched],
                    limit,
                )?;
            }

            if byte == pattern_bytes[self.matched] {
                self.matched += 1;
                if self.matched == pattern_bytes.len() {
                    append_redacted_bytes(&mut output, pattern.replacement.as_bytes(), limit)?;
                    self.matched = 0;
                }
            } else {
                append_redacted_bytes(&mut output, &[byte], limit)?;
            }
        }

        String::from_utf8(output).map_err(|_| {
            ProviderError::InvalidStream("credential redactor produced invalid UTF-8".into())
        })
    }

    /// Conceal an incomplete trailing prefix before a streaming field closes.
    fn finish_redacted(&mut self, pattern: &SecretPattern<'_>) -> Option<String> {
        (self.matched > 0).then(|| {
            self.matched = 0;
            pattern.replacement.to_owned()
        })
    }
}

fn append_redacted_bytes(
    output: &mut Vec<u8>,
    bytes: &[u8],
    limit: usize,
) -> Result<(), ProviderError> {
    output
        .len()
        .checked_add(bytes.len())
        .filter(|length| *length <= limit)
        .ok_or(ProviderError::OutputBytesLimit { limit })?;
    output.extend_from_slice(bytes);
    Ok(())
}

fn redact_closed_secret(value: &str, pattern: &SecretPattern<'_>) -> Result<String, ProviderError> {
    let mut state = SecretStreamState::default();
    let mut output = state.redact_chunk(pattern, value, MAX_ACCUMULATED_OUTPUT_BYTES)?;
    if let Some(trailing) = state.finish_redacted(pattern) {
        output
            .len()
            .checked_add(trailing.len())
            .filter(|length| *length <= MAX_ACCUMULATED_OUTPUT_BYTES)
            .ok_or(ProviderError::OutputBytesLimit {
                limit: MAX_ACCUMULATED_OUTPUT_BYTES,
            })?;
        output.push_str(&trailing);
    }
    Ok(output)
}

/// Replace a known credential in runtime-owned diagnostics. A trailing secret
/// prefix is also concealed because bounded error bodies may end in the middle
/// of a credential. On pathological expansion, return only the safe marker.
fn redact_secret(value: &str, secret: Option<&str>) -> String {
    let Some(pattern) = secret.and_then(SecretPattern::new) else {
        return value.to_owned();
    };
    let mut state = SecretStreamState::default();
    let result = (|| {
        let mut output = state.redact_chunk(&pattern, value, MAX_ACCUMULATED_OUTPUT_BYTES)?;
        if let Some(trailing) = state.finish_redacted(&pattern) {
            output
                .len()
                .checked_add(trailing.len())
                .filter(|length| *length <= MAX_ACCUMULATED_OUTPUT_BYTES)
                .ok_or(ProviderError::OutputBytesLimit {
                    limit: MAX_ACCUMULATED_OUTPUT_BYTES,
                })?;
            output.push_str(&trailing);
        }
        Ok::<_, ProviderError>(output)
    })();
    result.unwrap_or_else(|_| pattern.replacement.to_owned())
}

#[derive(Default)]
struct ToolSecretStreamState {
    json_delta: SecretStreamState,
    call_id: String,
    name: String,
}

/// The state lives only for one HTTP response. Text, reasoning, and each tool
/// argument stream are independent because their consumers concatenate those
/// fields independently.
struct CredentialStreamRedactor<'a> {
    pattern: Option<SecretPattern<'a>>,
    text: SecretStreamState,
    reasoning: SecretStreamState,
    tools: BTreeMap<u64, ToolSecretStreamState>,
}

impl<'a> CredentialStreamRedactor<'a> {
    fn new(secret: Option<&'a str>) -> Self {
        Self {
            pattern: secret.and_then(SecretPattern::new),
            text: SecretStreamState::default(),
            reasoning: SecretStreamState::default(),
            tools: BTreeMap::new(),
        }
    }

    fn redact_event(&mut self, parsed: ParsedModelEvent) -> Result<Vec<ModelEvent>, ProviderError> {
        let Some(pattern) = self.pattern.as_ref() else {
            return Ok(vec![parsed.event]);
        };
        let mut event = parsed.event;
        match &mut event {
            ModelEvent::TextDelta { text } => {
                *text = self
                    .text
                    .redact_chunk(pattern, text, MAX_ACCUMULATED_OUTPUT_BYTES)?;
            }
            ModelEvent::ReasoningDelta { text } => {
                *text = self
                    .reasoning
                    .redact_chunk(pattern, text, MAX_ACCUMULATED_OUTPUT_BYTES)?;
            }
            ModelEvent::ToolCallDelta {
                call_id,
                name,
                json_delta,
            } => {
                let index = parsed.tool_index.ok_or_else(|| {
                    ProviderError::InvalidStream(
                        "tool-call event is missing its provider index".into(),
                    )
                })?;
                if !self.tools.contains_key(&index) && self.tools.len() >= MAX_TRACKED_TOOL_CALLS {
                    return Err(ProviderError::ToolCallLimit {
                        limit: MAX_TRACKED_TOOL_CALLS,
                    });
                }
                let call_id_redacted = redact_closed_secret(call_id, pattern)?;
                let name_redacted = redact_closed_secret(name, pattern)?;
                let tool = self.tools.entry(index).or_default();
                tool.call_id.clone_from(&call_id_redacted);
                tool.name.clone_from(&name_redacted);
                *call_id = call_id_redacted;
                *name = name_redacted;
                *json_delta = tool.json_delta.redact_chunk(
                    pattern,
                    json_delta,
                    MAX_ACCUMULATED_OUTPUT_BYTES,
                )?;
            }
            ModelEvent::Completed {
                stop_reason: StopReason::Other(reason),
            } => {
                *reason = redact_closed_secret(reason, pattern)?;
            }
            ModelEvent::Failed { code, message, .. } => {
                *code = redact_closed_secret(code, pattern)?;
                *message = redact_closed_secret(message, pattern)?;
            }
            ModelEvent::Usage { .. } | ModelEvent::Completed { .. } => {}
        }

        if matches!(
            event,
            ModelEvent::Completed { .. } | ModelEvent::Failed { .. }
        ) {
            let mut events = self.finish_pending();
            events.push(event);
            Ok(events)
        } else {
            Ok(vec![event])
        }
    }

    fn finish_pending(&mut self) -> Vec<ModelEvent> {
        let Some(pattern) = self.pattern.as_ref() else {
            return Vec::new();
        };
        let mut events = Vec::new();
        if let Some(text) = self.text.finish_redacted(pattern) {
            events.push(ModelEvent::TextDelta { text });
        }
        if let Some(text) = self.reasoning.finish_redacted(pattern) {
            events.push(ModelEvent::ReasoningDelta { text });
        }
        for tool in self.tools.values_mut() {
            if let Some(json_delta) = tool.json_delta.finish_redacted(pattern) {
                events.push(ModelEvent::ToolCallDelta {
                    call_id: tool.call_id.clone(),
                    name: tool.name.clone(),
                    json_delta,
                });
            }
        }
        events
    }
}

fn redact_provider_error(error: ProviderError, secret: Option<&str>) -> ProviderError {
    let Some(secret) = secret else {
        return error;
    };
    match error {
        ProviderError::Configuration(message) => {
            ProviderError::Configuration(redact_secret(&message, Some(secret)))
        }
        ProviderError::CredentialUnavailable(handle) => {
            ProviderError::CredentialUnavailable(redact_secret(&handle, Some(secret)))
        }
        ProviderError::HttpStatus { status, body } => ProviderError::HttpStatus {
            status,
            body: redact_secret(&body, Some(secret)),
        },
        ProviderError::InvalidStream(message) => {
            ProviderError::InvalidStream(redact_secret(&message, Some(secret)))
        }
        ProviderError::Sink(mut port_error) => {
            port_error.message = redact_secret(&port_error.message, Some(secret));
            ProviderError::Sink(port_error)
        }
        other => other,
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

#[derive(Default)]
struct StreamAccounting {
    stream_bytes: usize,
    sse_events: usize,
    model_events: usize,
    output_bytes: usize,
}

impl StreamAccounting {
    fn record_stream_bytes(&mut self, bytes: usize) -> Result<(), ProviderError> {
        self.stream_bytes = self
            .stream_bytes
            .checked_add(bytes)
            .filter(|total| *total <= MAX_SSE_STREAM_BYTES)
            .ok_or(ProviderError::StreamBytesLimit {
                limit: MAX_SSE_STREAM_BYTES,
            })?;
        Ok(())
    }

    fn record_sse_event(&mut self) -> Result<(), ProviderError> {
        self.sse_events = self
            .sse_events
            .checked_add(1)
            .filter(|total| *total <= MAX_SSE_EVENTS)
            .ok_or(ProviderError::StreamEventLimit {
                limit: MAX_SSE_EVENTS,
            })?;
        Ok(())
    }

    fn record_model_event(&mut self, event: &ModelEvent) -> Result<(), ProviderError> {
        let model_events = self
            .model_events
            .checked_add(1)
            .filter(|total| *total <= MAX_MODEL_EVENTS)
            .ok_or(ProviderError::ModelEventLimit {
                limit: MAX_MODEL_EVENTS,
            })?;
        let output_bytes = self
            .output_bytes
            .checked_add(model_event_output_bytes(event))
            .filter(|total| *total <= MAX_ACCUMULATED_OUTPUT_BYTES)
            .ok_or(ProviderError::OutputBytesLimit {
                limit: MAX_ACCUMULATED_OUTPUT_BYTES,
            })?;
        self.model_events = model_events;
        self.output_bytes = output_bytes;
        Ok(())
    }
}

fn model_event_output_bytes(event: &ModelEvent) -> usize {
    match event {
        ModelEvent::TextDelta { text } | ModelEvent::ReasoningDelta { text } => text.len(),
        ModelEvent::ToolCallDelta {
            call_id,
            name,
            json_delta,
        } => call_id
            .len()
            .saturating_add(name.len())
            .saturating_add(json_delta.len()),
        ModelEvent::Failed { message, .. } => message.len(),
        ModelEvent::Usage { .. } | ModelEvent::Completed { .. } => 0,
    }
}

async fn process_sse_data(
    data: String,
    sink: &mut (dyn ModelEventSink + Send),
    tool_ids: &mut BTreeMap<u64, (String, String)>,
    accounting: &mut StreamAccounting,
    completed: &mut bool,
    credential_redactor: &mut CredentialStreamRedactor<'_>,
) -> Result<(), ProviderError> {
    accounting.record_sse_event()?;
    if data == "[DONE]" {
        if !*completed {
            let events = credential_redactor.redact_event(ParsedModelEvent::plain(
                ModelEvent::Completed {
                    stop_reason: StopReason::EndTurn,
                },
            ))?;
            emit_model_events(events, sink, accounting, completed).await?;
        }
        return Ok(());
    }

    let value: Value = serde_json::from_str(&data)
        .map_err(|error| ProviderError::InvalidStream(format!("invalid JSON event: {error}")))?;
    for parsed in parse_chunk(&value, tool_ids)? {
        let events = credential_redactor.redact_event(parsed)?;
        emit_model_events(events, sink, accounting, completed).await?;
    }
    Ok(())
}

async fn emit_model_events(
    events: Vec<ModelEvent>,
    sink: &mut (dyn ModelEventSink + Send),
    accounting: &mut StreamAccounting,
    completed: &mut bool,
) -> Result<(), ProviderError> {
    for event in events {
        accounting.record_model_event(&event)?;
        *completed |= matches!(event, ModelEvent::Completed { .. });
        sink.emit(event).await.map_err(ProviderError::Sink)?;
    }
    Ok(())
}

struct BoundedBody {
    bytes: Vec<u8>,
    truncated: bool,
    read_failed: bool,
}

async fn read_bounded_body<S, B, E>(stream: S, limit: usize) -> BoundedBody
where
    S: Stream<Item = Result<B, E>>,
    B: AsRef<[u8]>,
{
    futures_util::pin_mut!(stream);
    let mut bytes = Vec::with_capacity(limit.min(64 * 1024));
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(_) => {
                return BoundedBody {
                    bytes,
                    truncated: false,
                    read_failed: true,
                };
            }
        };
        let chunk = chunk.as_ref();
        if chunk.is_empty() {
            continue;
        }
        let remaining = limit.saturating_sub(bytes.len());
        if remaining == 0 {
            return BoundedBody {
                bytes,
                truncated: true,
                read_failed: false,
            };
        }
        let take = remaining.min(chunk.len());
        bytes.extend_from_slice(&chunk[..take]);
        if take < chunk.len() {
            return BoundedBody {
                bytes,
                truncated: true,
                read_failed: false,
            };
        }
    }
    BoundedBody {
        bytes,
        truncated: false,
        read_failed: false,
    }
}

async fn read_provider_error_body(response: reqwest::Response, secret: Option<&str>) -> String {
    let body = read_bounded_body(response.bytes_stream(), MAX_PROVIDER_ERROR_BYTES).await;
    summarize_error_body(body, secret)
}

fn summarize_error_body(body: BoundedBody, secret: Option<&str>) -> String {
    let bytes = if body.truncated {
        match std::str::from_utf8(&body.bytes) {
            Ok(_) => body.bytes.as_slice(),
            Err(error) if error.error_len().is_none() => &body.bytes[..error.valid_up_to()],
            Err(_) => body.bytes.as_slice(),
        }
    } else {
        body.bytes.as_slice()
    };
    // Scrub before adding truncation/read-failure annotations, so a credential
    // prefix at the true body boundary cannot be exposed by the annotation's
    // first byte forcing a streaming mismatch.
    let mut summary = redact_secret(&String::from_utf8_lossy(bytes), secret);
    if summary.is_empty() {
        summary.push_str("<empty response body>");
    }
    if body.truncated {
        summary.push_str(&format!(
            "\n[provider error body truncated after {MAX_PROVIDER_ERROR_BYTES} bytes]"
        ));
    }
    if body.read_failed {
        summary.push_str("\n[provider error body read failed]");
    }
    // Static diagnostics must also remain safe for unusually short tokens.
    redact_secret(&summary, secret)
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

struct ParsedModelEvent {
    event: ModelEvent,
    // Tool IDs may arrive late or change. Retain the stable provider index for
    // redaction state without adding it to the public ModelEvent protocol.
    tool_index: Option<u64>,
}

impl fmt::Debug for ParsedModelEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ParsedModelEvent")
            .field("event", &"REDACTED")
            .field("tool_index", &self.tool_index)
            .finish()
    }
}

impl ParsedModelEvent {
    fn plain(event: ModelEvent) -> Self {
        Self {
            event,
            tool_index: None,
        }
    }
}

fn parse_chunk(
    value: &Value,
    tool_ids: &mut BTreeMap<u64, (String, String)>,
) -> Result<Vec<ParsedModelEvent>, ProviderError> {
    let mut events = Vec::new();
    if let Some(error) = value.get("error") {
        return Err(ProviderError::InvalidStream(format!(
            "provider error event: {error}"
        )));
    }
    if let Some(usage) = value.get("usage").filter(|usage| !usage.is_null()) {
        events.push(ParsedModelEvent::plain(ModelEvent::Usage {
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
        }));
    }
    let Some(choice) = value.pointer("/choices/0") else {
        return Ok(events);
    };
    let delta = choice.get("delta").unwrap_or(&Value::Null);
    if let Some(text) = delta.get("content").and_then(Value::as_str) {
        events.push(ParsedModelEvent::plain(ModelEvent::TextDelta {
            text: text.into(),
        }));
    }
    if let Some(text) = delta.get("reasoning_content").and_then(Value::as_str) {
        events.push(ParsedModelEvent::plain(ModelEvent::ReasoningDelta {
            text: text.into(),
        }));
    }
    if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            let index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
            if !tool_ids.contains_key(&index) && tool_ids.len() >= MAX_TRACKED_TOOL_CALLS {
                return Err(ProviderError::ToolCallLimit {
                    limit: MAX_TRACKED_TOOL_CALLS,
                });
            }
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
                events.push(ParsedModelEvent {
                    event: ModelEvent::ToolCallDelta {
                        call_id: existing.0.clone(),
                        name: existing.1.clone(),
                        json_delta: arguments.into(),
                    },
                    tool_index: Some(index),
                });
            }
        }
    }
    if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
        events.push(ParsedModelEvent::plain(ModelEvent::Completed {
            stop_reason: match reason {
                "stop" => StopReason::EndTurn,
                "length" => StopReason::MaxTokens,
                "tool_calls" | "function_call" => StopReason::ToolUse,
                "content_filter" => StopReason::ContentFilter,
                other => StopReason::Other(other.into()),
            },
        }));
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

fn decode_sse_block(bytes: &[u8]) -> Result<Option<String>, ProviderError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|error| ProviderError::InvalidStream(format!("SSE is not UTF-8: {error}")))?;
    let mut data = String::new();
    let mut has_data = false;
    for line in text.lines() {
        let Some(line) = line.strip_prefix("data:") else {
            continue;
        };
        if has_data {
            data.push('\n');
        }
        data.push_str(line.trim_start());
        has_data = true;
    }
    Ok(has_data.then_some(data))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::{CredentialError, InMemoryCredentialBroker};
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
    fn streaming_redactor_covers_every_text_delta_split_and_overlap() {
        for secret in ["split-bearer-token", "abab", "REDACTED", "["] {
            for split in 0..=secret.len() {
                let mut redactor = CredentialStreamRedactor::new(Some(secret));
                let mut events = Vec::new();
                events.extend(
                    redactor
                        .redact_event(ParsedModelEvent::plain(ModelEvent::TextDelta {
                            text: format!("before {}", &secret[..split]),
                        }))
                        .unwrap(),
                );
                events.extend(
                    redactor
                        .redact_event(ParsedModelEvent::plain(ModelEvent::TextDelta {
                            text: format!("{} after", &secret[split..]),
                        }))
                        .unwrap(),
                );
                events.extend(
                    redactor
                        .redact_event(ParsedModelEvent::plain(ModelEvent::Completed {
                            stop_reason: StopReason::EndTurn,
                        }))
                        .unwrap(),
                );

                let text = events
                    .iter()
                    .filter_map(|event| match event {
                        ModelEvent::TextDelta { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>();
                assert!(!text.contains(secret), "secret={secret:?}, split={split}");
                assert!(text.starts_with("before "));
                assert!(text.ends_with(" after"));
                // A one-character token such as `[` is indistinguishable from
                // JSON/Debug framing, so inspect the event payload above. For
                // ordinary tokens, also prove the full serialized/debug view.
                if secret.len() > 1 {
                    assert!(!serde_json::to_string(&events).unwrap().contains(secret));
                    assert!(!format!("{events:?}").contains(secret));
                }
            }
        }

        let pattern = SecretPattern::new("abab").unwrap();
        let mut state = SecretStreamState::default();
        let mut output = state.redact_chunk(&pattern, "xxab", 128).unwrap();
        output.push_str(&state.redact_chunk(&pattern, "ababa", 128).unwrap());
        output.push_str(&state.finish_redacted(&pattern).unwrap());
        assert!(!output.contains("abab"));
    }

    #[test]
    fn streaming_redactor_keeps_logical_channels_independent() {
        let secret = "credential-token";
        let mut redactor = CredentialStreamRedactor::new(Some(secret));
        let mut events = Vec::new();
        for parsed in [
            ParsedModelEvent::plain(ModelEvent::TextDelta {
                text: "answer credential-".into(),
            }),
            ParsedModelEvent::plain(ModelEvent::ReasoningDelta {
                text: "thought credential-".into(),
            }),
            ParsedModelEvent {
                event: ModelEvent::ToolCallDelta {
                    call_id: "call-1".into(),
                    name: "lookup".into(),
                    json_delta: "{\"key\":\"credential-".into(),
                },
                tool_index: Some(7),
            },
            ParsedModelEvent::plain(ModelEvent::TextDelta {
                text: "token done".into(),
            }),
            ParsedModelEvent::plain(ModelEvent::ReasoningDelta {
                text: "token done".into(),
            }),
            ParsedModelEvent {
                event: ModelEvent::ToolCallDelta {
                    call_id: "call-1".into(),
                    name: "lookup".into(),
                    json_delta: "token\"}".into(),
                },
                tool_index: Some(7),
            },
            ParsedModelEvent::plain(ModelEvent::Completed {
                stop_reason: StopReason::EndTurn,
            }),
        ] {
            events.extend(redactor.redact_event(parsed).unwrap());
        }

        let serialized = serde_json::to_string(&events).unwrap();
        assert!(!serialized.contains(secret));
        assert!(!format!("{events:?}").contains(secret));

        let text = events
            .iter()
            .filter_map(|event| match event {
                ModelEvent::TextDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        let reasoning = events
            .iter()
            .filter_map(|event| match event {
                ModelEvent::ReasoningDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        let tool_json = events
            .iter()
            .filter_map(|event| match event {
                ModelEvent::ToolCallDelta { json_delta, .. } => Some(json_delta.as_str()),
                _ => None,
            })
            .collect::<String>();
        assert!(text.contains("answer ") && text.ends_with(" done"));
        assert!(reasoning.contains("thought ") && reasoning.ends_with(" done"));
        assert!(serde_json::from_str::<Value>(&tool_json).is_ok());
    }

    #[test]
    fn closed_metadata_terminal_fields_and_internal_debug_hide_secret_prefixes() {
        let secret = "metadata-secret";
        let mut redactor = CredentialStreamRedactor::new(Some(secret));
        let first = redactor
            .redact_event(ParsedModelEvent {
                event: ModelEvent::ToolCallDelta {
                    call_id: "metadata-".into(),
                    name: "metadata-".into(),
                    json_delta: "{}".into(),
                },
                tool_index: Some(1),
            })
            .unwrap();
        let second = redactor
            .redact_event(ParsedModelEvent {
                event: ModelEvent::ToolCallDelta {
                    call_id: "secret".into(),
                    name: "secret".into(),
                    json_delta: String::new(),
                },
                tool_index: Some(1),
            })
            .unwrap();
        let failed = redactor
            .redact_event(ParsedModelEvent::plain(ModelEvent::Failed {
                code: secret.into(),
                message: format!("failed with {secret}"),
                retryable: false,
            }))
            .unwrap();
        let events = first
            .into_iter()
            .chain(second)
            .chain(failed)
            .collect::<Vec<_>>();
        let serialized = serde_json::to_string(&events).unwrap();
        assert!(!serialized.contains(secret));
        assert!(!serialized.contains("metadata-"));

        let raw = ParsedModelEvent::plain(ModelEvent::TextDelta {
            text: secret.into(),
        });
        assert!(!format!("{raw:?}").contains(secret));
    }

    #[tokio::test]
    async fn sse_transport_and_model_delta_boundaries_cannot_reassemble_secret() {
        #[derive(Default)]
        struct CapturingSink(Vec<ModelEvent>);

        #[async_trait]
        impl ModelEventSink for CapturingSink {
            async fn emit(&mut self, event: ModelEvent) -> Result<(), PortError> {
                self.0.push(event);
                Ok(())
            }
        }

        let secret = "transport-model-secret";
        let first = json!({"choices": [{"delta": {"content": "before transport-model-"}}]});
        let second = json!({"choices": [{"delta": {"content": "secret after"}}]});
        let completed = json!({"choices": [{"delta": {}, "finish_reason": "stop"}]});
        let wire =
            format!("data: {first}\n\ndata: {second}\n\ndata: {completed}\n\ndata: [DONE]\n\n");

        let mut decoder = SseDecoder::default();
        let mut sink = CapturingSink::default();
        let mut tool_ids = BTreeMap::new();
        let mut accounting = StreamAccounting::default();
        let mut is_completed = false;
        let mut redactor = CredentialStreamRedactor::new(Some(secret));
        // Three-byte chunks force transport boundaries inside both the JSON
        // framing and the credential, independently of the ModelEvent split.
        for chunk in wire.as_bytes().chunks(3) {
            for data in decoder.push(chunk).unwrap() {
                process_sse_data(
                    data,
                    &mut sink,
                    &mut tool_ids,
                    &mut accounting,
                    &mut is_completed,
                    &mut redactor,
                )
                .await
                .unwrap();
            }
        }
        for data in decoder.finish().unwrap() {
            process_sse_data(
                data,
                &mut sink,
                &mut tool_ids,
                &mut accounting,
                &mut is_completed,
                &mut redactor,
            )
            .await
            .unwrap();
        }

        assert!(is_completed);
        let serialized = serde_json::to_string(&sink.0).unwrap();
        assert!(!serialized.contains(secret));
        let text = sink
            .0
            .iter()
            .filter_map(|event| match event {
                ModelEvent::TextDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        assert!(!text.contains(secret));
        assert!(text.starts_with("before ") && text.ends_with(" after"));
    }

    #[test]
    fn streaming_redactor_enforces_output_bound() {
        let pattern = SecretPattern::new("x").unwrap();
        assert!(!pattern.replacement.contains('x'));
        let mut state = SecretStreamState::default();
        let error = state.redact_chunk(&pattern, "xxxx", 3).unwrap_err();
        assert!(matches!(
            error,
            ProviderError::OutputBytesLimit { limit: 3 }
        ));
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
    fn stream_accounting_enforces_hard_limits_with_stable_error_codes() {
        let mut accounting = StreamAccounting {
            stream_bytes: MAX_SSE_STREAM_BYTES,
            ..Default::default()
        };
        let error = accounting.record_stream_bytes(1).unwrap_err();
        assert_eq!(error.port_error().code, "provider_stream_bytes_limit");
        assert!(error
            .to_string()
            .contains(&MAX_SSE_STREAM_BYTES.to_string()));

        let mut accounting = StreamAccounting {
            sse_events: MAX_SSE_EVENTS,
            ..Default::default()
        };
        let error = accounting.record_sse_event().unwrap_err();
        assert_eq!(error.port_error().code, "provider_stream_event_limit");
        assert!(error.to_string().contains(&MAX_SSE_EVENTS.to_string()));

        let mut accounting = StreamAccounting {
            model_events: MAX_MODEL_EVENTS,
            ..Default::default()
        };
        let error = accounting
            .record_model_event(&ModelEvent::Completed {
                stop_reason: StopReason::EndTurn,
            })
            .unwrap_err();
        assert_eq!(error.port_error().code, "provider_model_event_limit");
        assert!(error.to_string().contains(&MAX_MODEL_EVENTS.to_string()));

        let mut accounting = StreamAccounting {
            output_bytes: MAX_ACCUMULATED_OUTPUT_BYTES,
            ..Default::default()
        };
        let error = accounting
            .record_model_event(&ModelEvent::TextDelta { text: "x".into() })
            .unwrap_err();
        assert_eq!(error.port_error().code, "provider_output_bytes_limit");
        assert!(error
            .to_string()
            .contains(&MAX_ACCUMULATED_OUTPUT_BYTES.to_string()));
    }

    #[tokio::test]
    async fn bounded_error_reader_stops_at_limit_and_preserves_utf8() {
        use std::{cell::Cell, task::Poll};

        let polls = Cell::new(0);
        let stream = futures_util::stream::poll_fn(|_| {
            let poll = polls.get() + 1;
            polls.set(poll);
            assert_eq!(poll, 1, "bounded reader polled after observing truncation");
            Poll::Ready(Some(Ok::<_, ()>(
                "界".repeat(MAX_PROVIDER_ERROR_BYTES).into_bytes(),
            )))
        });
        let body = read_bounded_body(stream, MAX_PROVIDER_ERROR_BYTES).await;
        assert_eq!(polls.get(), 1);
        assert_eq!(body.bytes.len(), MAX_PROVIDER_ERROR_BYTES);
        assert!(body.truncated);

        let summary = summarize_error_body(body, None);
        assert!(!summary.contains('\u{fffd}'));
        assert!(summary.contains("provider error body truncated"));
        assert!(summary.is_char_boundary(summary.len()));
    }

    #[tokio::test]
    async fn split_and_truncated_error_bodies_and_debug_never_render_secret() {
        let secret = "error-body-secret";
        let stream = futures_util::stream::iter([
            Ok::<_, ()>(b"unauthorized: error-body-".to_vec()),
            Ok::<_, ()>(b"secret".to_vec()),
        ]);
        let body = read_bounded_body(stream, MAX_PROVIDER_ERROR_BYTES).await;
        let summary = summarize_error_body(body, Some(secret));
        assert!(!summary.contains(secret));
        assert!(summary.contains("[REDACTED]"));

        let truncated = summarize_error_body(
            BoundedBody {
                bytes: b"unauthorized: error-body-".to_vec(),
                truncated: true,
                read_failed: false,
            },
            Some(secret),
        );
        assert!(!truncated.contains(secret));
        assert!(!truncated.contains("error-body-"));
        assert!(truncated.contains("provider error body truncated"));

        let raw_error = ProviderError::HttpStatus {
            status: StatusCode::UNAUTHORIZED,
            body: format!("provider echoed {secret}"),
        };
        assert!(!format!("{raw_error:?}").contains(secret));
        let sanitized = redact_provider_error(raw_error, Some(secret));
        assert!(!sanitized.to_string().contains(secret));
        assert!(!format!("{sanitized:?}").contains(secret));
    }

    #[tokio::test]
    async fn non_success_response_body_is_bounded_before_summarizing() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(400)
                .header("content-type", "text/plain; charset=utf-8")
                .body("界".repeat(MAX_PROVIDER_ERROR_BYTES));
        });
        let provider = OpenAiCompatibleProvider::without_credentials(config(&server)).unwrap();
        let error = provider.collect(request()).await.unwrap_err();
        let ProviderError::HttpStatus { status, body } = error else {
            panic!("expected bounded HTTP status error");
        };
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(!body.contains('\u{fffd}'));
        assert!(body.contains("provider error body truncated"));
        assert!(body.len() < MAX_PROVIDER_ERROR_BYTES + 128);
    }

    #[test]
    fn rejects_excessive_tool_call_state() {
        let mut tool_ids = (0..MAX_TRACKED_TOOL_CALLS as u64)
            .map(|index| (index, (String::new(), String::new())))
            .collect();
        let value = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{"index": MAX_TRACKED_TOOL_CALLS, "id": "overflow"}]
                }
            }]
        });
        let error = parse_chunk(&value, &mut tool_ids).unwrap_err();
        assert_eq!(error.port_error().code, "provider_tool_call_limit");
        assert!(error
            .to_string()
            .contains(&MAX_TRACKED_TOOL_CALLS.to_string()));
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

    #[tokio::test]
    async fn broker_injects_opaque_token_only_at_request_boundary() {
        let server = MockServer::start();
        let token = "success-secret-token";
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .header("authorization", format!("Bearer {token}"));
            then.status(200)
                .header("content-type", "text/event-stream")
                .body(concat!(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n",
                    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                    "data: [DONE]\n\n"
                ));
        });
        let broker = Arc::new(InMemoryCredentialBroker::default());
        broker.insert("provider/test", token);
        let mut provider_config = config(&server);
        provider_config.credential_handle = Some("provider/test".into());
        let provider =
            OpenAiCompatibleProvider::with_credential_broker(provider_config, broker).unwrap();

        let events = provider.collect(request()).await.unwrap();
        mock.assert();
        let serialized_events = serde_json::to_string(&events).unwrap();
        assert!(!serialized_events.contains(token));
        assert!(!format!("{provider:?}").contains(token));
    }

    #[tokio::test]
    async fn missing_broker_handle_fails_before_network() {
        let mut provider_config = base_config(Url::parse("http://127.0.0.1:1/v1/").unwrap());
        provider_config.credential_handle = Some("provider/missing".into());
        let provider = OpenAiCompatibleProvider::with_credential_broker(
            provider_config,
            Arc::new(crate::credentials::NoCredentials),
        )
        .unwrap();
        let error = provider.collect(request()).await.unwrap_err();
        assert!(!error.to_string().contains("provider/missing"));
        assert!(!format!("{error:?}").contains("provider/missing"));
        assert!(matches!(
            error,
            ProviderError::CredentialUnavailable(handle) if handle == "provider/missing"
        ));
    }

    #[tokio::test]
    async fn broker_rotation_is_resolved_for_each_request_without_cache() {
        let server = MockServer::start();
        let old_token = "old-rotating-token";
        let new_token = "new-rotating-token";
        let old_mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .header("authorization", format!("Bearer {old_token}"));
            then.status(200)
                .header("content-type", "text/event-stream")
                .body(concat!(
                    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                    "data: [DONE]\n\n"
                ));
        });
        let new_mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .header("authorization", format!("Bearer {new_token}"));
            then.status(200)
                .header("content-type", "text/event-stream")
                .body(concat!(
                    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                    "data: [DONE]\n\n"
                ));
        });
        let broker = Arc::new(InMemoryCredentialBroker::default());
        broker.insert("provider/rotate", old_token);
        let mut provider_config = config(&server);
        provider_config.credential_handle = Some("provider/rotate".into());
        let provider = OpenAiCompatibleProvider::with_credential_broker(
            provider_config,
            Arc::clone(&broker) as Arc<dyn CredentialBroker>,
        )
        .unwrap();

        let old_events = provider.collect(request()).await.unwrap();
        broker.insert("provider/rotate", new_token);
        let new_events = provider.collect(request()).await.unwrap();

        old_mock.assert_hits(1);
        new_mock.assert_hits(1);
        let serialized = serde_json::to_string(&(old_events, new_events)).unwrap();
        assert!(!serialized.contains(old_token));
        assert!(!serialized.contains(new_token));
    }

    #[tokio::test]
    async fn broker_and_provider_diagnostics_redact_secret_values() {
        struct LeakyBroker;

        #[async_trait]
        impl CredentialBroker for LeakyBroker {
            async fn resolve(&self, _handle: &str) -> Result<CredentialLease, CredentialError> {
                Err(CredentialError::Unavailable(
                    "leaked-secret-from-broker".into(),
                ))
            }
        }

        let mut provider_config = base_config(Url::parse("http://127.0.0.1:1/v1/").unwrap());
        provider_config.credential_handle = Some("provider/safe-handle".into());
        let provider = OpenAiCompatibleProvider::with_credential_broker(
            provider_config,
            Arc::new(LeakyBroker),
        )
        .unwrap();
        let error = provider.collect(request()).await.unwrap_err();
        let rendered = error.to_string();
        assert!(!rendered.contains("leaked-secret-from-broker"));
        assert!(!rendered.contains("provider/safe-handle"));

        let server = MockServer::start();
        let token = "echoed-http-secret";
        server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(401)
                .header("content-type", "text/plain")
                .body(format!("unauthorized: {token}"));
        });
        let broker = Arc::new(InMemoryCredentialBroker::default());
        broker.insert("provider/http", token);
        let mut provider_config = config(&server);
        provider_config.credential_handle = Some("provider/http".into());
        let provider =
            OpenAiCompatibleProvider::with_credential_broker(provider_config, broker).unwrap();
        let error = provider.collect(request()).await.unwrap_err();
        assert!(!error.to_string().contains(token));
        assert!(!format!("{error:?}").contains(token));
        assert!(error.to_string().contains("[REDACTED]"));
    }

    #[test]
    fn credential_handle_validation_rejects_secret_like_unbounded_inputs() {
        let mut config = base_config(Url::parse("https://example.test/v1/").unwrap());
        config.credential_handle = Some(String::new());
        assert!(matches!(
            OpenAiCompatibleProvider::without_credentials(config),
            Err(ProviderError::Configuration(_))
        ));

        let mut config = base_config(Url::parse("https://example.test/v1/").unwrap());
        config.credential_handle = Some("contains whitespace".into());
        assert!(matches!(
            OpenAiCompatibleProvider::without_credentials(config),
            Err(ProviderError::Configuration(_))
        ));

        let mut config = base_config(Url::parse("https://example.test/v1/").unwrap());
        config.credential_handle = Some("x".repeat(MAX_CREDENTIAL_HANDLE_BYTES + 1));
        assert!(matches!(
            OpenAiCompatibleProvider::without_credentials(config),
            Err(ProviderError::Configuration(_))
        ));

        let config =
            base_config(Url::parse("https://user:embedded-secret@example.test/v1/").unwrap());
        let rendered = format!("{config:?}");
        assert!(!rendered.contains("embedded-secret"));
    }

    #[test]
    fn credential_error_debug_is_redacted() {
        let error = ProviderError::CredentialUnavailable("provider/secret-handle".into());
        assert!(!format!("{error:?}").contains("secret-handle"));
        assert!(!error.to_string().contains("secret-handle"));
    }
}
