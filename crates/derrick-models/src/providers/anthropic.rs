//! Anthropic Messages API provider.
//!
//! Calls `POST /v1/messages` on `api.anthropic.com` with streaming SSE.
//! Auth (D12): `ANTHROPIC_API_KEY` env var first, falling back to
//! `AuthStore` overrides under the `anthropic` provider key.

use std::time::Duration;

use async_trait::async_trait;
use derrick_config::ModelDef;
use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::{
    builtin_cost_hint, AuthStore, CompletionEvent, CompletionRequest, CompletionStream, CostHint,
    FinishReason, Model, ModelError,
};

const PROVIDER: &str = "anthropic";
const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const API_VERSION: &str = "2023-06-01";
const ENV_KEY: &str = "ANTHROPIC_API_KEY";
const DEFAULT_MAX_TOKENS: u32 = 4096;

pub(crate) fn build(model_def: &ModelDef, auth: &AuthStore) -> Result<Box<dyn Model>, ModelError> {
    let api_key = auth
        .get(PROVIDER, ENV_KEY)
        .map(|secret| secret.expose().to_owned())
        .ok_or_else(|| ModelError::MissingCredential {
            provider: PROVIDER.to_owned(),
            env_var: ENV_KEY.to_owned(),
        })?;

    let model_name = model_def.model().to_owned();
    let cost_hint = builtin_cost_hint(&model_name);

    Ok(Box::new(AnthropicModel {
        name: model_name,
        api_key,
        base_url: model_def
            .base_url()
            .unwrap_or(DEFAULT_BASE_URL)
            .trim_end_matches('/')
            .to_owned(),
        max_tokens: model_def.max_tokens().unwrap_or(DEFAULT_MAX_TOKENS),
        temperature: model_def.temperature().map(|t| t as f32),
        cost_hint,
    }))
}

struct AnthropicModel {
    name: String,
    api_key: String,
    base_url: String,
    max_tokens: u32,
    temperature: Option<f32>,
    cost_hint: Option<CostHint>,
}

#[async_trait]
impl Model for AnthropicModel {
    fn name(&self) -> &str {
        &self.name
    }

    fn provider(&self) -> &str {
        PROVIDER
    }

    fn cost_hint(&self) -> Option<&CostHint> {
        self.cost_hint.as_ref()
    }

    fn host_delegated_auth(&self) -> bool {
        false
    }

    async fn stream(&self, request: CompletionRequest) -> Result<CompletionStream, ModelError> {
        let url = format!("{}/v1/messages", self.base_url);
        let body = build_body(
            &self.name,
            request.max_tokens.unwrap_or(self.max_tokens),
            request.temperature.or(self.temperature),
            request.system.as_deref(),
            request.cached_prefix.as_deref(),
            &request.prompt,
        );

        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert("anthropic-version", HeaderValue::from_static(API_VERSION));
        let mut key_header =
            HeaderValue::from_str(&self.api_key).map_err(|error| ModelError::Provider {
                provider: PROVIDER.to_owned(),
                message: format!("invalid api key header: {error}"),
                retryable: false,
            })?;
        key_header.set_sensitive(true);
        headers.insert("x-api-key", key_header);

        let client = reqwest::Client::builder()
            .timeout(request.timeout)
            .build()
            .map_err(|error| ModelError::Provider {
                provider: PROVIDER.to_owned(),
                message: format!("failed to build http client: {error}"),
                retryable: false,
            })?;

        debug!(target: "derrick_models::anthropic", model = %self.name, "POST /v1/messages");
        let response = client
            .post(&url)
            .headers(headers)
            .json(&body)
            .send()
            .await
            .map_err(|error| classify_reqwest_error(&error, request.timeout))?;

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(ModelError::Provider {
                provider: PROVIDER.to_owned(),
                message: format!("HTTP {status}: {text}"),
                retryable: status.is_server_error() || status.as_u16() == 429,
            });
        }

        let byte_stream = response.bytes_stream();
        Ok(Box::pin(sse_to_events(byte_stream)))
    }
}

fn classify_reqwest_error(error: &reqwest::Error, timeout: Duration) -> ModelError {
    if error.is_timeout() {
        ModelError::Timeout {
            provider: PROVIDER.to_owned(),
            seconds: timeout.as_secs(),
        }
    } else {
        ModelError::Provider {
            provider: PROVIDER.to_owned(),
            message: format!("request failed: {error}"),
            retryable: error.is_connect() || error.is_request(),
        }
    }
}

#[derive(Serialize)]
struct Message<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Serialize)]
struct RequestBody<'a> {
    model: &'a str,
    max_tokens: u32,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<&'a str>,
    messages: Vec<Message<'a>>,
}

fn build_body<'a>(
    model: &'a str,
    max_tokens: u32,
    temperature: Option<f32>,
    system: Option<&'a str>,
    cached_prefix: Option<&'a str>,
    prompt: &'a str,
) -> RequestBody<'a> {
    let mut messages = Vec::with_capacity(2);
    if let Some(prefix) = cached_prefix {
        if !prefix.is_empty() {
            messages.push(Message {
                role: "user",
                content: prefix,
            });
        }
    }
    messages.push(Message {
        role: "user",
        content: prompt,
    });
    RequestBody {
        model,
        max_tokens,
        stream: true,
        temperature,
        system,
        messages,
    }
}

#[derive(Deserialize)]
struct SsePayload {
    #[serde(rename = "type")]
    kind: String,
    delta: Option<SseDelta>,
    message: Option<SseMessage>,
    usage: Option<SseUsage>,
}

#[derive(Deserialize)]
struct SseDelta {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    stop_reason: Option<String>,
}

#[derive(Deserialize)]
struct SseMessage {
    #[serde(default)]
    usage: Option<SseUsage>,
}

#[derive(Deserialize, Clone, Default)]
struct SseUsage {
    #[serde(default)]
    input_tokens: Option<u32>,
    #[serde(default)]
    output_tokens: Option<u32>,
}

fn sse_to_events<S, B>(
    stream: S,
) -> impl futures::Stream<Item = Result<CompletionEvent, ModelError>>
where
    S: futures::Stream<Item = Result<B, reqwest::Error>> + Send + 'static,
    B: AsRef<[u8]>,
{
    use futures::stream;

    let state = SseState::default();
    stream::unfold(
        (Box::pin(stream), state, false),
        |(mut stream, mut state, mut done)| async move {
            if done {
                return None;
            }
            loop {
                if let Some(event) = state.pop_event() {
                    return Some((Ok(event), (stream, state, done)));
                }
                if state.terminated {
                    if let Some(event) = state.flush_end() {
                        done = true;
                        return Some((Ok(event), (stream, state, done)));
                    }
                    return None;
                }
                match stream.next().await {
                    Some(Ok(bytes)) => {
                        state.push_bytes(bytes.as_ref());
                    }
                    Some(Err(error)) => {
                        return Some((
                            Err(ModelError::Provider {
                                provider: PROVIDER.to_owned(),
                                message: format!("stream read failed: {error}"),
                                retryable: true,
                            }),
                            (stream, state, true),
                        ));
                    }
                    None => {
                        state.terminated = true;
                    }
                }
            }
        },
    )
}

#[derive(Default)]
struct SseState {
    buffer: String,
    pending: std::collections::VecDeque<CompletionEvent>,
    tokens_in: u32,
    tokens_out: u32,
    finish_reason: Option<FinishReason>,
    saw_end: bool,
    terminated: bool,
}

impl SseState {
    fn pop_event(&mut self) -> Option<CompletionEvent> {
        self.pending.pop_front()
    }

    fn flush_end(&mut self) -> Option<CompletionEvent> {
        if self.saw_end {
            return None;
        }
        self.saw_end = true;
        Some(CompletionEvent::End {
            tokens_in: self.tokens_in,
            tokens_out: self.tokens_out,
            finish_reason: self.finish_reason.unwrap_or(FinishReason::Stop),
        })
    }

    fn push_bytes(&mut self, bytes: &[u8]) {
        let chunk = String::from_utf8_lossy(bytes);
        self.buffer.push_str(&chunk);

        while let Some(idx) = self.buffer.find("\n\n") {
            let raw_event: String = self.buffer.drain(..=idx + 1).collect();
            self.process_event(&raw_event);
        }
    }

    fn process_event(&mut self, raw: &str) {
        let mut data_lines: Vec<&str> = Vec::new();
        for line in raw.lines() {
            if let Some(rest) = line.strip_prefix("data:") {
                data_lines.push(rest.trim_start());
            }
        }
        if data_lines.is_empty() {
            return;
        }
        let payload = data_lines.join("\n");
        if payload == "[DONE]" || payload.is_empty() {
            return;
        }

        let parsed: SsePayload = match serde_json::from_str(&payload) {
            Ok(payload) => payload,
            Err(_) => return,
        };

        match parsed.kind.as_str() {
            "content_block_delta" => {
                if let Some(delta) = parsed.delta {
                    if let Some(text) = delta.text {
                        if !text.is_empty() {
                            self.pending.push_back(CompletionEvent::Content { text });
                        }
                    }
                }
            }
            "message_delta" => {
                if let Some(delta) = parsed.delta {
                    if let Some(reason) = delta.stop_reason {
                        self.finish_reason = Some(map_stop_reason(&reason));
                    }
                }
                if let Some(usage) = parsed.usage {
                    if let Some(out) = usage.output_tokens {
                        self.tokens_out = out;
                    }
                    if let Some(input) = usage.input_tokens {
                        self.tokens_in = input;
                    }
                }
            }
            "message_start" => {
                if let Some(msg) = parsed.message {
                    if let Some(usage) = msg.usage {
                        if let Some(input) = usage.input_tokens {
                            self.tokens_in = input;
                        }
                        if let Some(out) = usage.output_tokens {
                            self.tokens_out = out;
                        }
                    }
                }
            }
            "message_stop" => {
                if let Some(event) = self.flush_end() {
                    self.pending.push_back(event);
                }
            }
            _ => {}
        }
    }
}

fn map_stop_reason(reason: &str) -> FinishReason {
    match reason {
        "end_turn" | "stop_sequence" => FinishReason::Stop,
        "max_tokens" => FinishReason::Length,
        _ => FinishReason::Stop,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_state_emits_content_then_end() {
        let mut state = SseState::default();
        state.push_bytes(
            b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10}}}\n\n",
        );
        state.push_bytes(
            b"data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
        );
        state.push_bytes(
            b"data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\n",
        );
        state.push_bytes(b"data: {\"type\":\"message_stop\"}\n\n");

        let mut events = Vec::new();
        while let Some(e) = state.pop_event() {
            events.push(e);
        }
        assert!(matches!(events[0], CompletionEvent::Content { .. }));
        match events.last().expect("end event") {
            CompletionEvent::End {
                tokens_in,
                tokens_out,
                finish_reason,
            } => {
                assert_eq!(*tokens_in, 10);
                assert_eq!(*tokens_out, 5);
                assert_eq!(*finish_reason, FinishReason::Stop);
            }
            other => panic!("expected End, got {other:?}"),
        }
    }
}
