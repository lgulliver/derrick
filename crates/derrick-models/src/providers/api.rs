//! API and local-runtime models (D79).
//!
//! A single HTTP-backed [`Model`] that speaks one of three wire dialects — the
//! OpenAI chat-completions protocol (used by `openai-api`, `openai-compatible`,
//! and OpenAI-protocol servers such as OpenRouter / LiteLLM / LM Studio / vLLM),
//! the Anthropic messages protocol (`anthropic-api`), and the Ollama chat
//! protocol (`ollama`). These runtimes are opt-in; the default path remains the
//! CLI runtimes (see [`crate::providers::host_delegated`]).
//!
//! Construction is synchronous (it only resolves config + auth); the HTTP call
//! happens inside [`Model::stream`]. Auth keys come from the env var named by
//! `auth_env`, read through the [`AuthStore`] so tests can inject them.

use std::collections::{BTreeMap, VecDeque};
use std::pin::Pin;

use async_trait::async_trait;
use derrick_config::ModelDef;
use futures::StreamExt;
use serde_json::{Map, Value, json};

use crate::{
    AuthStore, CompletionEvent, CompletionRequest, CompletionStream, CostHint, FinishReason, Model,
    ModelError, RuntimeError, builtin_cost_hint,
};

/// The wire protocol an API/local runtime speaks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ApiDialect {
    /// OpenAI chat-completions, hosted at api.openai.com by default.
    OpenAi,
    /// OpenAI chat-completions against an arbitrary `base_url` (required).
    OpenAiCompatible,
    /// Anthropic messages protocol.
    Anthropic,
    /// Local Ollama chat protocol.
    Ollama,
}

impl ApiDialect {
    fn runtime_id(self) -> &'static str {
        match self {
            Self::OpenAi => "openai-api",
            Self::OpenAiCompatible => "openai-compatible",
            Self::Anthropic => "anthropic-api",
            Self::Ollama => "ollama",
        }
    }

    fn default_provider(self) -> &'static str {
        match self {
            Self::OpenAi | Self::OpenAiCompatible => "openai",
            Self::Anthropic => "anthropic",
            Self::Ollama => "ollama",
        }
    }

    /// Default base URL, or `None` when the runtime requires an explicit one.
    fn default_base_url(self) -> Option<&'static str> {
        match self {
            Self::OpenAi => Some("https://api.openai.com/v1"),
            Self::Anthropic => Some("https://api.anthropic.com/v1"),
            Self::Ollama => Some("http://localhost:11434"),
            Self::OpenAiCompatible => None,
        }
    }

    /// Whether this runtime requires an API key.
    fn requires_auth(self) -> bool {
        matches!(self, Self::OpenAi | Self::Anthropic)
    }
}

/// How the API key is presented on the wire (D79 `auth_mode`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AuthMode {
    /// `Authorization: Bearer <key>` (OpenAI-style default).
    Bearer,
    /// `x-api-key: <key>` (Anthropic-style).
    ApiKeyHeader,
}

/// Resolves the configured `auth_mode`, defaulting per dialect, and rejecting
/// unsupported values early so a typo doesn't silently fall back to bearer.
#[allow(clippy::result_large_err)]
fn resolve_auth_mode(raw: Option<&str>, dialect: ApiDialect) -> Result<AuthMode, RuntimeError> {
    match raw {
        None => Ok(match dialect {
            ApiDialect::Anthropic => AuthMode::ApiKeyHeader,
            _ => AuthMode::Bearer,
        }),
        Some("bearer") => Ok(AuthMode::Bearer),
        Some("x-api-key" | "api-key") => Ok(AuthMode::ApiKeyHeader),
        Some(other) => Err(RuntimeError::new(
            dialect.runtime_id(),
            format!("unsupported auth_mode `{other}` (expected `bearer` or `x-api-key`)"),
        )),
    }
}

/// An HTTP-backed model for an API or local runtime.
pub(crate) struct HttpApiModel {
    dialect: ApiDialect,
    provider: String,
    base_url: String,
    model: String,
    api_key: Option<String>,
    auth_mode: AuthMode,
    params: BTreeMap<String, serde_yaml::Value>,
    cost_hint: Option<CostHint>,
    client: reqwest::Client,
}

/// Builds an [`HttpApiModel`] for `dialect` from a model definition.
pub(crate) fn build(
    dialect: ApiDialect,
    model_def: &ModelDef,
    auth: &AuthStore,
) -> Result<Box<dyn Model>, ModelError> {
    let runtime = dialect.runtime_id();
    let provider = model_def
        .provider()
        .unwrap_or_else(|| dialect.default_provider())
        .to_owned();

    let base_url = model_def
        .base_url()
        .or_else(|| model_def.endpoint())
        .map(str::to_owned)
        .or_else(|| dialect.default_base_url().map(str::to_owned))
        .ok_or_else(|| {
            ModelError::from(
                RuntimeError::new(
                    runtime,
                    format!("runtime `{runtime}` requires `base_url` (no default endpoint)"),
                )
                .with_provider(Some(provider.clone())),
            )
        })?;

    // Resolve the API key from the env var named by `auth_env`, via AuthStore.
    // The key is only ever attached as a request header (never serialised into a
    // body, error message, or log) and `HttpApiModel` deliberately derives no
    // `Debug`, so it cannot leak through a `{:?}` of the model (D79).
    let api_key = model_def.auth_env().and_then(|env_var| {
        auth.get(&provider, env_var)
            .map(|secret| secret.expose().to_owned())
    });

    let auth_mode = resolve_auth_mode(model_def.auth_mode(), dialect)
        .map_err(|error| ModelError::from(error.with_provider(Some(provider.clone()))))?;

    Ok(Box::new(HttpApiModel {
        dialect,
        provider,
        base_url: base_url.trim_end_matches('/').to_owned(),
        model: model_def.model().to_owned(),
        api_key,
        auth_mode,
        params: model_def.params().clone(),
        cost_hint: builtin_cost_hint(model_def.model()),
        client: reqwest::Client::new(),
    }))
}

impl HttpApiModel {
    fn endpoint(&self) -> String {
        match self.dialect {
            ApiDialect::OpenAi | ApiDialect::OpenAiCompatible => {
                format!("{}/chat/completions", self.base_url)
            }
            ApiDialect::Anthropic => format!("{}/messages", self.base_url),
            ApiDialect::Ollama => format!("{}/api/chat", self.base_url),
        }
    }

    /// Builds the JSON request body for the configured dialect. When `stream` is
    /// set, the dialect's streaming flags are added (and, for OpenAI, usage is
    /// requested in the final SSE chunk).
    fn request_body(&self, request: &CompletionRequest, stream: bool) -> Value {
        let mut body = Map::new();
        body.insert("model".to_owned(), json!(self.model));

        match self.dialect {
            ApiDialect::Anthropic => {
                if let Some(system) = combined_system(request) {
                    body.insert("system".to_owned(), json!(system));
                }
                body.insert(
                    "messages".to_owned(),
                    json!([{ "role": "user", "content": request.prompt }]),
                );
                body.insert(
                    "max_tokens".to_owned(),
                    json!(request.max_tokens.unwrap_or(4096)),
                );
                if stream {
                    body.insert("stream".to_owned(), json!(true));
                }
            }
            ApiDialect::Ollama => {
                body.insert("messages".to_owned(), json!(chat_messages(request)));
                body.insert("stream".to_owned(), json!(stream));
            }
            ApiDialect::OpenAi | ApiDialect::OpenAiCompatible => {
                body.insert("messages".to_owned(), json!(chat_messages(request)));
                if let Some(max_tokens) = request.max_tokens {
                    body.insert("max_tokens".to_owned(), json!(max_tokens));
                }
                if stream {
                    body.insert("stream".to_owned(), json!(true));
                    // Ask for a usage block in the terminal chunk.
                    body.insert(
                        "stream_options".to_owned(),
                        json!({ "include_usage": true }),
                    );
                }
            }
        }

        if let Some(temperature) = request.temperature {
            body.insert("temperature".to_owned(), json!(temperature));
        }

        // Overlay user-supplied passthrough params last so they win.
        for (key, value) in &self.params {
            if let Ok(json_value) = serde_json::to_value(value) {
                body.insert(key.clone(), json_value);
            }
        }

        Value::Object(body)
    }

    // `RuntimeError` is the D79 normalised error; boxing it here would just
    // complicate the `?`-into-`ModelError` conversion in `stream`.
    #[allow(clippy::result_large_err)]
    fn build_http_request(
        &self,
        request: &CompletionRequest,
    ) -> Result<reqwest::RequestBuilder, RuntimeError> {
        let mut builder = self
            .client
            .post(self.endpoint())
            .timeout(request.timeout)
            .json(&self.request_body(request, true));

        // Anthropic always needs its API-version header regardless of auth mode.
        if self.dialect == ApiDialect::Anthropic {
            builder = builder.header("anthropic-version", "2023-06-01");
        }

        if let Some(key) = &self.api_key {
            builder = match self.auth_mode {
                AuthMode::Bearer => builder.bearer_auth(key),
                AuthMode::ApiKeyHeader => builder.header("x-api-key", key),
            };
        }

        if self.dialect.requires_auth() && self.api_key.is_none() {
            return Err(self
                .error("missing API key (set the env var named by `auth_env`)")
                .retryable(false));
        }

        Ok(builder)
    }

    fn error(&self, message: impl Into<String>) -> RuntimeError {
        RuntimeError::new(self.dialect.runtime_id(), message)
            .with_provider(Some(self.provider.clone()))
    }
}

/// One semantic piece decoded from a streaming chunk.
#[derive(Clone, Debug, Eq, PartialEq)]
enum StreamPiece {
    /// A delta of generated text.
    Delta(String),
    /// Token-usage update (`tokens_in`, `tokens_out`); either may be `None`.
    Usage(Option<u32>, Option<u32>),
}

/// Incremental decoder for a dialect's streaming wire format (SSE for the
/// OpenAI/Anthropic dialects, newline-delimited JSON for Ollama).
///
/// Pure and synchronous: [`push`](StreamParser::push) is fed raw response bytes
/// (which may split a line or a frame mid-chunk) and returns the text deltas
/// decoded so far, accumulating token usage internally for the final `End`.
struct StreamParser {
    dialect: ApiDialect,
    buf: Vec<u8>,
    /// Accumulated `data:` payload for the in-progress SSE frame.
    sse_data: String,
    /// `event:` type for the in-progress SSE frame (Anthropic).
    sse_event: Option<String>,
    tokens_in: u32,
    tokens_out: u32,
}

impl StreamParser {
    fn new(dialect: ApiDialect) -> Self {
        Self {
            dialect,
            buf: Vec::new(),
            sse_data: String::new(),
            sse_event: None,
            tokens_in: 0,
            tokens_out: 0,
        }
    }

    fn is_sse(&self) -> bool {
        !matches!(self.dialect, ApiDialect::Ollama)
    }

    /// Feeds a chunk of response bytes and returns any text deltas decoded.
    fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(chunk);
        let mut deltas = Vec::new();
        for line in take_lines(&mut self.buf) {
            let pieces = self.consume_line(&line);
            self.ingest(pieces, &mut deltas);
        }
        deltas
    }

    /// Flushes any buffered partial line / dangling SSE frame at end-of-stream,
    /// returning the final deltas. Servers may close without a trailing newline
    /// or SSE blank-line terminator, so the last delta/usage would otherwise be
    /// lost (and token totals left incomplete).
    fn finish(&mut self) -> Vec<String> {
        let mut deltas = Vec::new();
        // A trailing partial line (no terminating `\n`).
        if !self.buf.is_empty() {
            let raw = std::mem::take(&mut self.buf);
            let line = String::from_utf8_lossy(&raw)
                .trim_end_matches(['\n', '\r'])
                .to_owned();
            let pieces = self.consume_line(&line);
            self.ingest(pieces, &mut deltas);
        }
        // A dangling SSE frame whose terminating blank line never arrived.
        if self.is_sse() && !self.sse_data.is_empty() {
            let data = std::mem::take(&mut self.sse_data);
            let event = self.sse_event.take();
            let pieces = self.interpret_sse(event.as_deref(), &data);
            self.ingest(pieces, &mut deltas);
        }
        deltas
    }

    /// Routes decoded pieces: text deltas are collected, usage updates the
    /// running token totals.
    fn ingest(&mut self, pieces: Vec<StreamPiece>, deltas: &mut Vec<String>) {
        for piece in pieces {
            match piece {
                StreamPiece::Delta(text) => deltas.push(text),
                StreamPiece::Usage(tin, tout) => {
                    if let Some(tin) = tin {
                        self.tokens_in = tin;
                    }
                    if let Some(tout) = tout {
                        self.tokens_out = tout;
                    }
                }
            }
        }
    }

    /// Processes one decoded line, returning the pieces it yields. For SSE this
    /// buffers `data:`/`event:` fields and only dispatches on a blank line.
    fn consume_line(&mut self, line: &str) -> Vec<StreamPiece> {
        if !self.is_sse() {
            // NDJSON (Ollama): each non-blank line is a complete JSON object.
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return Vec::new();
            }
            return match serde_json::from_str::<Value>(trimmed) {
                Ok(json) => interpret_ollama(&json),
                Err(error) => {
                    tracing::warn!(
                        target: "derrick_models::api",
                        runtime = self.dialect.runtime_id(),
                        %error,
                        "dropping unparseable NDJSON stream line"
                    );
                    Vec::new()
                }
            };
        }

        if line.is_empty() {
            // SSE frame boundary: dispatch the accumulated frame.
            let data = std::mem::take(&mut self.sse_data);
            let event = self.sse_event.take();
            if data.is_empty() {
                return Vec::new();
            }
            return self.interpret_sse(event.as_deref(), &data);
        }
        if let Some(rest) = line.strip_prefix("data:") {
            if !self.sse_data.is_empty() {
                self.sse_data.push('\n');
            }
            self.sse_data
                .push_str(rest.strip_prefix(' ').unwrap_or(rest));
        } else if let Some(rest) = line.strip_prefix("event:") {
            self.sse_event = Some(rest.strip_prefix(' ').unwrap_or(rest).to_owned());
        }
        // Any other field (`:` comments, `id:`, `retry:`) is ignored.
        Vec::new()
    }

    fn interpret_sse(&self, event: Option<&str>, data: &str) -> Vec<StreamPiece> {
        if data == "[DONE]" {
            return Vec::new();
        }
        let json = match serde_json::from_str::<Value>(data) {
            Ok(json) => json,
            Err(error) => {
                // Not fatal — providers occasionally emit non-JSON keepalive
                // frames — but log it so a truncated/error payload isn't lost
                // silently (the HTTP status guard already rejects error pages).
                tracing::warn!(
                    target: "derrick_models::api",
                    runtime = self.dialect.runtime_id(),
                    %error,
                    "dropping unparseable SSE data frame"
                );
                return Vec::new();
            }
        };
        match self.dialect {
            ApiDialect::Anthropic => interpret_anthropic(event, &json),
            _ => interpret_openai(&json),
        }
    }

    /// Final token counts for the `End` event once the byte stream is exhausted.
    fn totals(&self) -> (u32, u32) {
        (self.tokens_in, self.tokens_out)
    }
}

/// Drains complete `\n`-terminated lines from `buf`, leaving any partial tail.
/// Trailing `\r`/`\n` are stripped; the returned line may be empty (an SSE frame
/// boundary).
fn take_lines(buf: &mut Vec<u8>) -> Vec<String> {
    let mut lines = Vec::new();
    while let Some(pos) = buf.iter().position(|&byte| byte == b'\n') {
        let raw: Vec<u8> = buf.drain(..=pos).collect();
        let mut line = String::from_utf8_lossy(&raw).into_owned();
        while line.ends_with('\n') || line.ends_with('\r') {
            line.pop();
        }
        lines.push(line);
    }
    lines
}

/// Interprets one OpenAI streaming chunk (`choices[].delta.content` + `usage`).
fn interpret_openai(json: &Value) -> Vec<StreamPiece> {
    let mut pieces = Vec::new();
    if let Some(text) = json
        .pointer("/choices/0/delta/content")
        .and_then(Value::as_str)
    {
        if !text.is_empty() {
            pieces.push(StreamPiece::Delta(text.to_owned()));
        }
    }
    if json.get("usage").is_some_and(|usage| !usage.is_null()) {
        pieces.push(StreamPiece::Usage(
            json.pointer("/usage/prompt_tokens")
                .and_then(Value::as_u64)
                .map(|value| value as u32),
            json.pointer("/usage/completion_tokens")
                .and_then(Value::as_u64)
                .map(|value| value as u32),
        ));
    }
    pieces
}

/// Interprets one Anthropic streaming event by its `event:` type.
fn interpret_anthropic(event: Option<&str>, json: &Value) -> Vec<StreamPiece> {
    match event {
        Some("content_block_delta") => json
            .pointer("/delta/text")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(|text| vec![StreamPiece::Delta(text.to_owned())])
            .unwrap_or_default(),
        Some("message_start") => vec![StreamPiece::Usage(
            json.pointer("/message/usage/input_tokens")
                .and_then(Value::as_u64)
                .map(|value| value as u32),
            None,
        )],
        Some("message_delta") => vec![StreamPiece::Usage(
            None,
            json.pointer("/usage/output_tokens")
                .and_then(Value::as_u64)
                .map(|value| value as u32),
        )],
        _ => Vec::new(),
    }
}

/// Interprets one Ollama NDJSON chunk (`message.content` + final eval counts).
fn interpret_ollama(json: &Value) -> Vec<StreamPiece> {
    let mut pieces = Vec::new();
    if let Some(text) = json.pointer("/message/content").and_then(Value::as_str) {
        if !text.is_empty() {
            pieces.push(StreamPiece::Delta(text.to_owned()));
        }
    }
    if json.get("done").and_then(Value::as_bool) == Some(true) {
        pieces.push(StreamPiece::Usage(
            json.get("prompt_eval_count")
                .and_then(Value::as_u64)
                .map(|value| value as u32),
            json.get("eval_count")
                .and_then(Value::as_u64)
                .map(|value| value as u32),
        ));
    }
    pieces
}

/// Joins the system + cached-prefix portions of a request into one system blob.
fn combined_system(request: &CompletionRequest) -> Option<String> {
    let parts: Vec<&str> = [request.system.as_deref(), request.cached_prefix.as_deref()]
        .into_iter()
        .flatten()
        .filter(|part| !part.is_empty())
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    }
}

/// Builds OpenAI/Ollama-style `messages` array from a request.
fn chat_messages(request: &CompletionRequest) -> Vec<Value> {
    let mut messages = Vec::new();
    if let Some(system) = combined_system(request) {
        messages.push(json!({ "role": "system", "content": system }));
    }
    messages.push(json!({ "role": "user", "content": request.prompt }));
    messages
}

/// Classifies an HTTP status as retryable (429 + 5xx) or not.
fn status_is_retryable(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

#[async_trait]
impl Model for HttpApiModel {
    fn name(&self) -> &str {
        &self.model
    }

    fn provider(&self) -> &str {
        &self.provider
    }

    fn cost_hint(&self) -> Option<&CostHint> {
        self.cost_hint.as_ref()
    }

    async fn stream(&self, request: CompletionRequest) -> Result<CompletionStream, ModelError> {
        // The request timeout is applied on the builder in `build_http_request`.
        let builder = self.build_http_request(&request)?;

        let response = builder.send().await.map_err(|error| {
            self.error(format!("HTTP request failed: {error}"))
                .retryable(error.is_timeout() || error.is_connect())
        })?;

        let status = response.status();
        if !status.is_success() {
            let retryable = status_is_retryable(status);
            let detail = response.text().await.unwrap_or_default();
            return Err(ModelError::from(
                self.error(format!("HTTP {status}: {}", truncate(&detail, 500)))
                    .retryable(retryable),
            ));
        }

        // Decode the response body incrementally: emit a `Content` event per
        // text delta as it arrives, then a single `End` once the byte stream is
        // exhausted (carrying the accumulated token usage).
        let state = StreamState {
            // Map to `Vec<u8>` so the boxed stream's item type doesn't name
            // `bytes::Bytes` (not a direct dependency).
            bytes: Box::pin(
                response
                    .bytes_stream()
                    .map(|chunk| chunk.map(|b| b.to_vec())),
            ),
            parser: StreamParser::new(self.dialect),
            queue: VecDeque::new(),
            ended: false,
            err: self.error("streaming body error"),
        };

        let stream = futures::stream::unfold(state, |mut state| async move {
            loop {
                if let Some(event) = state.queue.pop_front() {
                    return Some((Ok(event), state));
                }
                if state.ended {
                    return None;
                }
                match state.bytes.next().await {
                    Some(Ok(chunk)) => {
                        for text in state.parser.push(&chunk) {
                            state.queue.push_back(CompletionEvent::Content { text });
                        }
                        // Loop to drain the queue (or pull more bytes).
                    }
                    Some(Err(error)) => {
                        state.ended = true;
                        // Carry the underlying error detail (safe — reqwest's
                        // Display does not include request headers).
                        let mut err = state.err.clone();
                        err.message = format!("streaming body error: {error}");
                        err.retryable = error.is_timeout() || error.is_connect();
                        return Some((Err(ModelError::from(err)), state));
                    }
                    None => {
                        // Byte stream finished — flush any buffered partial
                        // line/frame, then synthesise the terminal event. Queue
                        // both so the loop drains them in order.
                        if !state.ended {
                            for text in state.parser.finish() {
                                state.queue.push_back(CompletionEvent::Content { text });
                            }
                            let (tokens_in, tokens_out) = state.parser.totals();
                            state.queue.push_back(CompletionEvent::End {
                                tokens_in,
                                tokens_out,
                                finish_reason: FinishReason::Stop,
                            });
                            state.ended = true;
                        }
                    }
                }
            }
        });

        Ok(Box::pin(stream))
    }
}

/// Carried state for the streaming `unfold` over the response byte stream.
struct StreamState {
    bytes: Pin<Box<dyn futures::Stream<Item = reqwest::Result<Vec<u8>>> + Send>>,
    parser: StreamParser,
    queue: VecDeque<CompletionEvent>,
    ended: bool,
    /// Pre-built error template (carries runtime + provider) for byte-stream IO.
    err: RuntimeError,
}

/// Truncates `text` to at most `max` bytes on a char boundary, for error detail.
fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn request(prompt: &str) -> CompletionRequest {
        CompletionRequest {
            cached_prefix: None,
            prompt: prompt.to_owned(),
            system: Some("be terse".to_owned()),
            max_tokens: Some(256),
            temperature: Some(0.2),
            timeout: Duration::from_secs(30),
        }
    }

    fn model(dialect: ApiDialect) -> HttpApiModel {
        HttpApiModel {
            dialect,
            provider: dialect.default_provider().to_owned(),
            base_url: dialect
                .default_base_url()
                .unwrap_or("http://localhost:9999")
                .to_owned(),
            model: "test-model".to_owned(),
            api_key: Some("k".to_owned()),
            auth_mode: resolve_auth_mode(None, dialect).unwrap(),
            params: BTreeMap::new(),
            cost_hint: None,
            client: reqwest::Client::new(),
        }
    }

    #[test]
    fn auth_mode_defaults_per_dialect() {
        assert_eq!(
            resolve_auth_mode(None, ApiDialect::OpenAi).unwrap(),
            AuthMode::Bearer
        );
        assert_eq!(
            resolve_auth_mode(None, ApiDialect::Anthropic).unwrap(),
            AuthMode::ApiKeyHeader
        );
    }

    #[test]
    fn auth_mode_honours_explicit_value_and_rejects_unknown() {
        assert_eq!(
            resolve_auth_mode(Some("x-api-key"), ApiDialect::OpenAi).unwrap(),
            AuthMode::ApiKeyHeader
        );
        assert_eq!(
            resolve_auth_mode(Some("bearer"), ApiDialect::Anthropic).unwrap(),
            AuthMode::Bearer
        );
        assert!(resolve_auth_mode(Some("oauth"), ApiDialect::OpenAi).is_err());
    }

    #[test]
    fn openai_body_has_messages_and_model() {
        let body = model(ApiDialect::OpenAi).request_body(&request("hello"), false);
        assert_eq!(body["model"], json!("test-model"));
        assert_eq!(body["messages"][0]["role"], json!("system"));
        assert_eq!(body["messages"][1]["content"], json!("hello"));
        assert_eq!(body["max_tokens"], json!(256));
        // temperature originates as f32, so compare with tolerance.
        assert!((body["temperature"].as_f64().unwrap() - 0.2).abs() < 1e-6);
    }

    #[test]
    fn anthropic_body_uses_system_and_max_tokens() {
        let body = model(ApiDialect::Anthropic).request_body(&request("hi"), false);
        assert_eq!(body["system"], json!("be terse"));
        assert_eq!(body["messages"][0]["content"], json!("hi"));
        assert_eq!(body["max_tokens"], json!(256));
    }

    #[test]
    fn ollama_body_stream_flag_tracks_arg() {
        let m = model(ApiDialect::Ollama);
        assert_eq!(
            m.request_body(&request("yo"), false)["stream"],
            json!(false)
        );
        assert_eq!(m.request_body(&request("yo"), true)["stream"], json!(true));
    }

    #[test]
    fn streaming_body_requests_openai_usage() {
        let body = model(ApiDialect::OpenAi).request_body(&request("x"), true);
        assert_eq!(body["stream"], json!(true));
        assert_eq!(body["stream_options"]["include_usage"], json!(true));
    }

    #[test]
    fn params_override_defaults() {
        let mut m = model(ApiDialect::OpenAi);
        m.params.insert(
            "temperature".to_owned(),
            serde_yaml::Value::Number(serde_yaml::Number::from(0.9)),
        );
        let body = m.request_body(&request("x"), false);
        assert_eq!(body["temperature"], json!(0.9));
    }

    /// Drains a parser over a sequence of byte chunks into `(text, in, out)`,
    /// then flushes — mirroring how `stream()` drives the parser at end-of-stream.
    fn drain(dialect: ApiDialect, chunks: &[&str]) -> (String, u32, u32) {
        let mut parser = StreamParser::new(dialect);
        let mut text = String::new();
        for chunk in chunks {
            for delta in parser.push(chunk.as_bytes()) {
                text.push_str(&delta);
            }
        }
        for delta in parser.finish() {
            text.push_str(&delta);
        }
        let (tin, tout) = parser.totals();
        (text, tin, tout)
    }

    #[test]
    fn stream_without_trailing_newline_is_flushed() {
        // Server closes mid-frame with no terminating blank line: the final
        // delta and usage must still be emitted via `finish()`.
        let (text, tin, tout) = drain(
            ApiDialect::OpenAi,
            &[
                "data: {\"choices\":[{\"delta\":{\"content\":\"tail\"}}]}\n\n",
                "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}",
            ],
        );
        assert_eq!(text, "tail");
        assert_eq!((tin, tout), (1, 1));
    }

    #[test]
    fn openai_sse_stream_accumulates_deltas_and_usage() {
        let (text, tin, tout) = drain(
            ApiDialect::OpenAi,
            &[
                "data: {\"choices\":[{\"delta\":{\"content\":\"hi \"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"there\"}}]}\n\n",
                "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2}}\n\n",
                "data: [DONE]\n\n",
            ],
        );
        assert_eq!(text, "hi there");
        assert_eq!((tin, tout), (5, 2));
    }

    #[test]
    fn sse_frame_split_across_chunks_is_reassembled() {
        // A single data frame delivered in two byte chunks must decode once.
        let (text, _, _) = drain(
            ApiDialect::OpenAi,
            &[
                "data: {\"choices\":[{\"delta\":{\"con",
                "tent\":\"split\"}}]}\n\n",
            ],
        );
        assert_eq!(text, "split");
    }

    #[test]
    fn anthropic_sse_stream_uses_event_types() {
        let (text, tin, tout) = drain(
            ApiDialect::Anthropic,
            &[
                "event: message_start\ndata: {\"message\":{\"usage\":{\"input_tokens\":9}}}\n\n",
                "event: content_block_delta\ndata: {\"delta\":{\"text\":\"ok\"}}\n\n",
                "event: message_delta\ndata: {\"usage\":{\"output_tokens\":1}}\n\n",
                "event: message_stop\ndata: {}\n\n",
            ],
        );
        assert_eq!(text, "ok");
        assert_eq!((tin, tout), (9, 1));
    }

    #[test]
    fn ollama_ndjson_stream_accumulates_and_reads_final_counts() {
        let (text, tin, tout) = drain(
            ApiDialect::Ollama,
            &[
                "{\"message\":{\"content\":\"local \"},\"done\":false}\n",
                "{\"message\":{\"content\":\"says hi\"},\"done\":false}\n",
                "{\"message\":{\"content\":\"\"},\"done\":true,\"prompt_eval_count\":3,\"eval_count\":7}\n",
            ],
        );
        assert_eq!(text, "local says hi");
        assert_eq!((tin, tout), (3, 7));
    }

    #[test]
    fn take_lines_keeps_partial_tail() {
        let mut buf = b"a\nb\nc".to_vec();
        let lines = take_lines(&mut buf);
        assert_eq!(lines, vec!["a".to_owned(), "b".to_owned()]);
        assert_eq!(buf, b"c"); // partial line retained
    }

    #[test]
    fn status_retry_classification() {
        assert!(status_is_retryable(reqwest::StatusCode::TOO_MANY_REQUESTS));
        assert!(status_is_retryable(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR
        ));
        assert!(!status_is_retryable(reqwest::StatusCode::BAD_REQUEST));
        assert!(!status_is_retryable(reqwest::StatusCode::UNAUTHORIZED));
    }
}
