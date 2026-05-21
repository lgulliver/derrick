//! OpenAI provider with two backends:
//!
//! * CLI mode (default): shells to the `codex` binary (or whatever
//!   `model_def.cli()` points at) and streams its stdout. This is the
//!   path used when the host already owns the OpenAI session.
//! * Direct API mode: if `OPENAI_API_KEY` or an `openai-cli` AuthStore
//!   override is present *and* the model definition has no `cli`
//!   override (i.e. the caller did not explicitly force CLI), we POST
//!   to `https://api.openai.com/v1/chat/completions` with SSE.
//!
//! D12: env var first, AuthStore fallback. D13: hosts own their own
//! context — we do not inject system prompts into the CLI args.

use std::time::Duration;

use async_trait::async_trait;
use derrick_config::ModelDef;
use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::providers::subprocess::{parse_argv, stream_subprocess, SubprocessSpec};
use crate::{
    builtin_cost_hint, AuthStore, CompletionEvent, CompletionRequest, CompletionStream, CostHint,
    FinishReason, Model, ModelError,
};

const PROVIDER: &str = "openai-cli";
const DEFAULT_BASE_URL: &str = "https://api.openai.com";
const DEFAULT_CLI: &str = "codex exec";
const ENV_KEY: &str = "OPENAI_API_KEY";
const DEFAULT_MAX_TOKENS: u32 = 4096;

pub(crate) fn build(model_def: &ModelDef, auth: &AuthStore) -> Result<Box<dyn Model>, ModelError> {
    // Force CLI mode when the model explicitly sets `cli`. Otherwise, prefer
    // direct API if a key is available.
    let cli_override = model_def.cli().map(str::to_owned);
    let api_key = auth.get(PROVIDER, ENV_KEY).map(|s| s.expose().to_owned());

    let mode = match (cli_override, api_key) {
        (Some(cli), _) => OpenAiMode::Cli { cli },
        (None, Some(key)) => OpenAiMode::Api {
            api_key: key,
            base_url: model_def
                .base_url()
                .unwrap_or(DEFAULT_BASE_URL)
                .trim_end_matches('/')
                .to_owned(),
        },
        (None, None) => OpenAiMode::Cli {
            cli: DEFAULT_CLI.to_owned(),
        },
    };

    let model_name = model_def.model().to_owned();
    let cost_hint = builtin_cost_hint(&model_name);

    Ok(Box::new(OpenAiCliModel {
        name: model_name,
        mode,
        max_tokens: model_def.max_tokens().unwrap_or(DEFAULT_MAX_TOKENS),
        temperature: model_def.temperature().map(|t| t as f32),
        cost_hint,
    }))
}

enum OpenAiMode {
    Cli { cli: String },
    Api { api_key: String, base_url: String },
}

struct OpenAiCliModel {
    name: String,
    mode: OpenAiMode,
    max_tokens: u32,
    temperature: Option<f32>,
    cost_hint: Option<CostHint>,
}

#[async_trait]
impl Model for OpenAiCliModel {
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
        matches!(self.mode, OpenAiMode::Cli { .. })
    }

    async fn stream(&self, request: CompletionRequest) -> Result<CompletionStream, ModelError> {
        match &self.mode {
            OpenAiMode::Cli { cli } => {
                let argv = parse_argv(PROVIDER, &self.name, cli)?;
                let payload = render_prompt(&request);
                stream_subprocess(SubprocessSpec {
                    provider: PROVIDER,
                    argv,
                    stdin_payload: payload,
                })
                .await
            }
            OpenAiMode::Api { api_key, base_url } => {
                stream_api(
                    &self.name,
                    api_key,
                    base_url,
                    request,
                    self.max_tokens,
                    self.temperature,
                )
                .await
            }
        }
    }
}

fn render_prompt(request: &CompletionRequest) -> String {
    let mut payload = String::new();
    if let Some(system) = &request.system {
        payload.push_str(system);
        payload.push_str("\n\n");
    }
    if let Some(prefix) = &request.cached_prefix {
        payload.push_str(prefix);
        payload.push_str("\n\n");
    }
    payload.push_str(&request.prompt);
    payload
}

async fn stream_api(
    model: &str,
    api_key: &str,
    base_url: &str,
    request: CompletionRequest,
    default_max_tokens: u32,
    default_temperature: Option<f32>,
) -> Result<CompletionStream, ModelError> {
    let url = format!("{base_url}/v1/chat/completions");

    let mut messages = Vec::new();
    if let Some(system) = request.system.as_deref() {
        messages.push(ChatMessage {
            role: "system",
            content: system.to_owned(),
        });
    }
    if let Some(prefix) = request.cached_prefix.as_deref() {
        if !prefix.is_empty() {
            messages.push(ChatMessage {
                role: "user",
                content: prefix.to_owned(),
            });
        }
    }
    messages.push(ChatMessage {
        role: "user",
        content: request.prompt.clone(),
    });

    let body = ChatRequest {
        model,
        messages,
        stream: true,
        max_tokens: request.max_tokens.unwrap_or(default_max_tokens),
        temperature: request.temperature.or(default_temperature),
        stream_options: StreamOptions {
            include_usage: true,
        },
    };

    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    let bearer = format!("Bearer {api_key}");
    let mut auth_header = HeaderValue::from_str(&bearer).map_err(|error| ModelError::Provider {
        provider: PROVIDER.to_owned(),
        message: format!("invalid api key header: {error}"),
        retryable: false,
    })?;
    auth_header.set_sensitive(true);
    headers.insert(AUTHORIZATION, auth_header);

    let client = reqwest::Client::builder()
        .timeout(request.timeout)
        .build()
        .map_err(|error| ModelError::Provider {
            provider: PROVIDER.to_owned(),
            message: format!("failed to build http client: {error}"),
            retryable: false,
        })?;

    debug!(target: "derrick_models::openai_cli", model = %model, "POST /v1/chat/completions");
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
    Ok(Box::pin(openai_sse_to_events(byte_stream)))
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
struct ChatMessage<'a> {
    role: &'a str,
    content: String,
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage<'a>>,
    stream: bool,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    stream_options: StreamOptions,
}

#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

#[derive(Deserialize)]
struct ChatChunk {
    #[serde(default)]
    choices: Vec<ChatChoice>,
    #[serde(default)]
    usage: Option<ChatUsage>,
}

#[derive(Deserialize)]
struct ChatChoice {
    #[serde(default)]
    delta: Option<ChatDelta>,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct ChatDelta {
    #[serde(default)]
    content: Option<String>,
}

#[derive(Deserialize)]
struct ChatUsage {
    #[serde(default)]
    prompt_tokens: Option<u32>,
    #[serde(default)]
    completion_tokens: Option<u32>,
}

fn openai_sse_to_events<S, B>(
    stream: S,
) -> impl futures::Stream<Item = Result<CompletionEvent, ModelError>>
where
    S: futures::Stream<Item = Result<B, reqwest::Error>> + Send + 'static,
    B: AsRef<[u8]>,
{
    use futures::stream;

    let state = OpenAiSseState::default();
    stream::unfold(
        (Box::pin(stream), state, false),
        |(mut stream, mut state, mut done)| async move {
            if done {
                return None;
            }
            loop {
                if let Some(event) = state.pending.pop_front() {
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
                    Some(Ok(bytes)) => state.push_bytes(bytes.as_ref()),
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
                    None => state.terminated = true,
                }
            }
        },
    )
}

#[derive(Default)]
struct OpenAiSseState {
    buffer: String,
    pending: std::collections::VecDeque<CompletionEvent>,
    tokens_in: u32,
    tokens_out: u32,
    finish_reason: Option<FinishReason>,
    saw_end: bool,
    terminated: bool,
}

impl OpenAiSseState {
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
        for line in raw.lines() {
            let Some(rest) = line.strip_prefix("data:") else {
                continue;
            };
            let payload = rest.trim_start();
            if payload == "[DONE]" {
                if let Some(end) = self.flush_end() {
                    self.pending.push_back(end);
                }
                continue;
            }
            if payload.is_empty() {
                continue;
            }
            let Ok(chunk): Result<ChatChunk, _> = serde_json::from_str(payload) else {
                continue;
            };
            for choice in chunk.choices {
                if let Some(delta) = choice.delta {
                    if let Some(text) = delta.content {
                        if !text.is_empty() {
                            self.pending.push_back(CompletionEvent::Content { text });
                        }
                    }
                }
                if let Some(reason) = choice.finish_reason {
                    self.finish_reason = Some(map_finish_reason(&reason));
                }
            }
            if let Some(usage) = chunk.usage {
                if let Some(p) = usage.prompt_tokens {
                    self.tokens_in = p;
                }
                if let Some(c) = usage.completion_tokens {
                    self.tokens_out = c;
                }
            }
        }
    }
}

fn map_finish_reason(reason: &str) -> FinishReason {
    match reason {
        "stop" => FinishReason::Stop,
        "length" => FinishReason::Length,
        _ => FinishReason::Stop,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_sse_state_parses_chunks_and_usage() {
        let mut state = OpenAiSseState::default();
        state.push_bytes(b"data: {\"choices\":[{\"delta\":{\"content\":\"he\"}}]}\n\n");
        state.push_bytes(
            b"data: {\"choices\":[{\"delta\":{\"content\":\"llo\"},\"finish_reason\":\"stop\"}]}\n\n",
        );
        state.push_bytes(
            b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3}}\n\n",
        );
        state.push_bytes(b"data: [DONE]\n\n");

        let events: Vec<_> = state.pending.drain(..).collect();
        let content: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                CompletionEvent::Content { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(content, vec!["he", "llo"]);
        match events.last().expect("end event") {
            CompletionEvent::End {
                tokens_in,
                tokens_out,
                finish_reason,
            } => {
                assert_eq!(*tokens_in, 7);
                assert_eq!(*tokens_out, 3);
                assert_eq!(*finish_reason, FinishReason::Stop);
            }
            other => panic!("expected End, got {other:?}"),
        }
    }
}
