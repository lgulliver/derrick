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

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use derrick_config::ModelDef;
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

/// An HTTP-backed model for an API or local runtime.
pub(crate) struct HttpApiModel {
    dialect: ApiDialect,
    provider: String,
    base_url: String,
    model: String,
    api_key: Option<String>,
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
    let api_key = model_def.auth_env().and_then(|env_var| {
        auth.get(&provider, env_var)
            .map(|secret| secret.expose().to_owned())
    });

    Ok(Box::new(HttpApiModel {
        dialect,
        provider,
        base_url: base_url.trim_end_matches('/').to_owned(),
        model: model_def.model().to_owned(),
        api_key,
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

    /// Builds the JSON request body for the configured dialect.
    fn request_body(&self, request: &CompletionRequest) -> Value {
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
            }
            ApiDialect::Ollama => {
                body.insert("messages".to_owned(), json!(chat_messages(request)));
                body.insert("stream".to_owned(), json!(false));
            }
            ApiDialect::OpenAi | ApiDialect::OpenAiCompatible => {
                body.insert("messages".to_owned(), json!(chat_messages(request)));
                if let Some(max_tokens) = request.max_tokens {
                    body.insert("max_tokens".to_owned(), json!(max_tokens));
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
            .json(&self.request_body(request));

        if self.dialect == ApiDialect::Anthropic {
            builder = builder.header("anthropic-version", "2023-06-01");
            if let Some(key) = &self.api_key {
                builder = builder.header("x-api-key", key);
            }
        } else if let Some(key) = &self.api_key {
            builder = builder.bearer_auth(key);
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

    /// Parses a dialect-specific response body into `(text, tokens_in, tokens_out)`.
    #[allow(clippy::result_large_err)]
    fn parse_response(&self, body: &Value) -> Result<(String, u32, u32), RuntimeError> {
        let (text, tokens_in, tokens_out) = match self.dialect {
            ApiDialect::Anthropic => (
                body.pointer("/content/0/text").and_then(Value::as_str),
                body.pointer("/usage/input_tokens").and_then(Value::as_u64),
                body.pointer("/usage/output_tokens").and_then(Value::as_u64),
            ),
            ApiDialect::Ollama => (
                body.pointer("/message/content").and_then(Value::as_str),
                body.pointer("/prompt_eval_count").and_then(Value::as_u64),
                body.pointer("/eval_count").and_then(Value::as_u64),
            ),
            ApiDialect::OpenAi | ApiDialect::OpenAiCompatible => (
                body.pointer("/choices/0/message/content")
                    .and_then(Value::as_str),
                body.pointer("/usage/prompt_tokens").and_then(Value::as_u64),
                body.pointer("/usage/completion_tokens")
                    .and_then(Value::as_u64),
            ),
        };

        let text = text
            .ok_or_else(|| self.error("response did not contain completion text"))?
            .to_owned();
        Ok((
            text,
            tokens_in.unwrap_or(0) as u32,
            tokens_out.unwrap_or(0) as u32,
        ))
    }
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
        let timeout_secs = request.timeout.max(Duration::from_secs(0)).as_secs();
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

        let body: Value = response.json().await.map_err(|error| {
            self.error(format!("invalid JSON response: {error}"))
                .retryable(false)
        })?;

        let (text, tokens_in, tokens_out) = self.parse_response(&body)?;
        let _ = timeout_secs;

        let events = vec![
            Ok(CompletionEvent::Content { text }),
            Ok(CompletionEvent::End {
                tokens_in,
                tokens_out,
                finish_reason: FinishReason::Stop,
            }),
        ];
        Ok(Box::pin(futures::stream::iter(events)))
    }
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
            params: BTreeMap::new(),
            cost_hint: None,
            client: reqwest::Client::new(),
        }
    }

    #[test]
    fn openai_body_has_messages_and_model() {
        let body = model(ApiDialect::OpenAi).request_body(&request("hello"));
        assert_eq!(body["model"], json!("test-model"));
        assert_eq!(body["messages"][0]["role"], json!("system"));
        assert_eq!(body["messages"][1]["content"], json!("hello"));
        assert_eq!(body["max_tokens"], json!(256));
        // temperature originates as f32, so compare with tolerance.
        assert!((body["temperature"].as_f64().unwrap() - 0.2).abs() < 1e-6);
    }

    #[test]
    fn anthropic_body_uses_system_and_max_tokens() {
        let body = model(ApiDialect::Anthropic).request_body(&request("hi"));
        assert_eq!(body["system"], json!("be terse"));
        assert_eq!(body["messages"][0]["content"], json!("hi"));
        assert_eq!(body["max_tokens"], json!(256));
    }

    #[test]
    fn ollama_body_sets_stream_false() {
        let body = model(ApiDialect::Ollama).request_body(&request("yo"));
        assert_eq!(body["stream"], json!(false));
        assert_eq!(body["messages"][1]["content"], json!("yo"));
    }

    #[test]
    fn params_override_defaults() {
        let mut m = model(ApiDialect::OpenAi);
        m.params.insert(
            "temperature".to_owned(),
            serde_yaml::Value::Number(serde_yaml::Number::from(0.9)),
        );
        let body = m.request_body(&request("x"));
        assert_eq!(body["temperature"], json!(0.9));
    }

    #[test]
    fn parse_openai_response() {
        let body = json!({
            "choices": [{ "message": { "content": "hi there" } }],
            "usage": { "prompt_tokens": 5, "completion_tokens": 2 }
        });
        let (text, tin, tout) = model(ApiDialect::OpenAi).parse_response(&body).unwrap();
        assert_eq!(text, "hi there");
        assert_eq!((tin, tout), (5, 2));
    }

    #[test]
    fn parse_anthropic_response() {
        let body = json!({
            "content": [{ "type": "text", "text": "ok" }],
            "usage": { "input_tokens": 9, "output_tokens": 1 }
        });
        let (text, tin, tout) = model(ApiDialect::Anthropic).parse_response(&body).unwrap();
        assert_eq!(text, "ok");
        assert_eq!((tin, tout), (9, 1));
    }

    #[test]
    fn parse_ollama_response() {
        let body = json!({
            "message": { "content": "local says hi" },
            "prompt_eval_count": 3, "eval_count": 7
        });
        let (text, tin, tout) = model(ApiDialect::Ollama).parse_response(&body).unwrap();
        assert_eq!(text, "local says hi");
        assert_eq!((tin, tout), (3, 7));
    }

    #[test]
    fn missing_text_is_error() {
        let body = json!({ "choices": [] });
        assert!(model(ApiDialect::OpenAi).parse_response(&body).is_err());
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
