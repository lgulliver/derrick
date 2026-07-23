//! Model provider abstraction. See DESIGN.md §6.5 and D12.

use std::collections::HashMap;
use std::env;
use std::fmt;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use derrick_config::{ModelDef, ModelRegistry, RoleBindings};
use futures::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use thiserror::Error;

mod providers;

/// A stream of completion events produced by a model provider.
pub type CompletionStream = Pin<Box<dyn Stream<Item = Result<CompletionEvent, ModelError>> + Send>>;

/// A model the orchestrator can call.
///
/// `stream` is the primitive. `complete` drains the stream and assembles a
/// `CompletionResponse`.
#[async_trait]
pub trait Model: Send + Sync {
    /// Human-readable name for logs and cost accounting.
    fn name(&self) -> &str;

    /// Provider family, such as `anthropic`, `openai`, or `shell`.
    fn provider(&self) -> &str;

    /// Cost hints used by `derrick gain`, when configured.
    fn cost_hint(&self) -> Option<&CostHint>;

    /// Whether this provider delegates authentication to a host CLI.
    fn host_delegated_auth(&self) -> bool {
        false
    }

    /// Streams a completion response.
    async fn stream(&self, request: CompletionRequest) -> Result<CompletionStream, ModelError>;

    /// Completes a request by draining `stream`.
    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse, ModelError> {
        let mut stream = self.stream(request).await?;
        let mut text = String::new();
        let mut tokens_in = 0;
        let mut tokens_out = 0;
        // Default to `Error`: a stream that ends without a terminal `End` event
        // was truncated (connection dropped, host killed mid-generation) and
        // must not read as a clean `Stop`. The real reason is set only if an
        // `End` event actually arrives.
        let mut finish_reason = FinishReason::Error;

        while let Some(event) = stream.next().await {
            match event? {
                CompletionEvent::Content { text: chunk } => text.push_str(&chunk),
                CompletionEvent::End {
                    tokens_in: input,
                    tokens_out: output,
                    finish_reason: reason,
                } => {
                    tokens_in = input;
                    tokens_out = output;
                    finish_reason = reason;
                }
            }
        }

        Ok(CompletionResponse {
            text,
            tokens_in,
            tokens_out,
            finish_reason,
        })
    }
}

/// A completion request assembled by a caller.
#[derive(Clone, Debug)]
pub struct CompletionRequest {
    /// Cacheable prompt prefix assembled by the caller.
    pub cached_prefix: Option<String>,
    /// Mutable per-call prompt.
    pub prompt: String,
    /// Optional system message.
    pub system: Option<String>,
    /// Optional output token budget.
    pub max_tokens: Option<u32>,
    /// Optional sampling temperature.
    pub temperature: Option<f32>,
    /// Provider call timeout.
    pub timeout: Duration,
}

/// A fully assembled completion response.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletionResponse {
    /// Response text.
    pub text: String,
    /// Input token count reported by the provider.
    pub tokens_in: u32,
    /// Output token count reported by the provider.
    pub tokens_out: u32,
    /// Provider finish reason.
    pub finish_reason: FinishReason,
}

/// A streaming completion event.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum CompletionEvent {
    /// A chunk of response content.
    Content {
        /// Content text for this event.
        text: String,
    },
    /// End-of-stream metadata.
    End {
        /// Input token count reported by the provider.
        tokens_in: u32,
        /// Output token count reported by the provider.
        tokens_out: u32,
        /// Provider finish reason.
        finish_reason: FinishReason,
    },
}

/// Reason a provider stopped producing output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum FinishReason {
    /// The provider reached a natural stop.
    Stop,
    /// The provider reached a length limit.
    Length,
    /// The provider ended without a successful metadata line.
    Error,
}

/// Cost hint for converting token counts into dollars.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CostHint {
    /// USD per one million input tokens.
    pub in_per_mtok: f64,
    /// USD per one million output tokens.
    pub out_per_mtok: f64,
}

impl CostHint {
    /// Estimate cost in USD for the given token counts.
    pub fn estimate_usd(&self, tokens_in: u64, tokens_out: u64) -> f64 {
        (tokens_in as f64 / 1_000_000.0) * self.in_per_mtok
            + (tokens_out as f64 / 1_000_000.0) * self.out_per_mtok
    }
}

/// Returns built-in cost hints for well-known model names (substring match).
/// Returns `None` for unknown models.
pub fn builtin_cost_hint(model_name: &str) -> Option<CostHint> {
    let n = model_name.to_ascii_lowercase();
    if n.contains("claude-opus-4") {
        Some(CostHint {
            in_per_mtok: 15.0,
            out_per_mtok: 75.0,
        })
    } else if n.contains("claude-sonnet-4") {
        Some(CostHint {
            in_per_mtok: 3.0,
            out_per_mtok: 15.0,
        })
    } else if n.contains("claude-haiku-4") || n.contains("claude-haiku-3") {
        Some(CostHint {
            in_per_mtok: 0.8,
            out_per_mtok: 4.0,
        })
    } else if n.contains("gpt-5") && n.contains("mini") {
        Some(CostHint {
            in_per_mtok: 0.25,
            out_per_mtok: 2.0,
        })
    } else if n.contains("gpt-5") {
        Some(CostHint {
            in_per_mtok: 1.25,
            out_per_mtok: 10.0,
        })
    } else if n.contains("gpt-4o-mini") {
        Some(CostHint {
            in_per_mtok: 0.15,
            out_per_mtok: 0.60,
        })
    } else if n.contains("gpt-4o") {
        Some(CostHint {
            in_per_mtok: 2.5,
            out_per_mtok: 10.0,
        })
    } else if n.contains("gemini-2.5-pro") {
        Some(CostHint {
            in_per_mtok: 1.25,
            out_per_mtok: 10.0,
        })
    } else if n.contains("gemini-2.0-flash") || n.contains("gemini-flash") {
        Some(CostHint {
            in_per_mtok: 0.10,
            out_per_mtok: 0.40,
        })
    } else {
        None
    }
}

/// Environment passthrough store for host-delegated providers (D65).
///
/// Post-D65 derrick holds no API keys. This store exposes the process
/// environment so host adapters can forward vars such as `GH_TOKEN` and proxy
/// settings to the child CLI, which manages its own auth. The `overrides` map
/// supports test injection.
#[derive(Clone, Debug, Default)]
pub struct AuthStore {
    env: HashMap<String, Secret>,
    overrides: HashMap<(String, String), Secret>,
}

impl AuthStore {
    /// Reads the process environment.
    pub fn from_env() -> Self {
        let env = env::vars()
            .map(|(key, value)| (key, Secret::new(value)))
            .collect();

        Self {
            env,
            overrides: HashMap::new(),
        }
    }

    /// Constructs a store from explicit test secrets.
    pub fn for_testing(map: HashMap<(String, String), Secret>) -> Self {
        Self {
            env: HashMap::new(),
            overrides: map,
        }
    }

    /// Returns the process environment as a plain map for host passthrough.
    ///
    /// Host-delegated providers forward these to the child process so host
    /// CLIs can pick up vars such as `GH_TOKEN` and proxy settings (D65).
    pub fn env_map(&self) -> HashMap<String, String> {
        self.env
            .iter()
            .map(|(key, value)| (key.clone(), value.expose().to_owned()))
            .collect()
    }

    /// Returns a secret by provider and env-var key.
    pub fn get(&self, provider: &str, key: &str) -> Option<&Secret> {
        self.overrides
            .get(&(provider.to_owned(), key.to_owned()))
            .or_else(|| self.env.get(key))
    }
}

/// Secret string wrapper that redacts its `Debug` output.
#[derive(Clone, Eq, PartialEq)]
pub struct Secret(String);

impl Secret {
    /// Creates a new secret.
    pub fn new<S: Into<String>>(secret: S) -> Self {
        Self(secret.into())
    }

    /// Exposes the inner secret for provider calls.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Secret(***)")
    }
}

/// Errors returned by model resolution and provider calls.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum ModelError {
    /// The requested model name is not present in config.
    #[error("no such model: {0}")]
    UnknownModel(String),
    /// The requested role name is not present in config.
    #[error("no such role: {0}")]
    UnknownRole(String),
    /// The requested provider is not registered.
    #[error("no such provider: {0}")]
    UnknownProvider(String),
    /// A provider call exceeded its configured timeout.
    #[error("timeout after {seconds}s calling {provider}")]
    Timeout {
        /// Provider that timed out.
        provider: String,
        /// Timeout in whole seconds.
        seconds: u64,
    },
    /// A provider returned an error.
    #[error("provider error ({provider}): {message}")]
    Provider {
        /// Provider that returned the error.
        provider: String,
        /// Provider error message.
        message: String,
        /// Whether the caller may retry the request.
        retryable: bool,
    },
    /// A configured model is invalid.
    #[error("invalid model config for {model}: {message}")]
    InvalidConfig {
        /// Configured model name.
        model: String,
        /// Validation message.
        message: String,
    },
}

impl ModelError {
    /// Returns whether the error is retryable.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Timeout { .. } => true,
            Self::Provider { retryable, .. } => *retryable,
            Self::UnknownModel(_)
            | Self::UnknownRole(_)
            | Self::UnknownProvider(_)
            | Self::InvalidConfig { .. } => false,
        }
    }
}

/// Normalised error surfaced by any runtime (D79).
///
/// A single shape across CLI, API, and local runtimes so the foreman can make a
/// uniform retry/escalate decision. `retryable` is the load-bearing field;
/// `stdout`/`stderr` capture CLI subprocess output when present. Converts into
/// [`ModelError::Provider`] for the `Model` trait surface.
#[derive(Clone, Debug)]
pub struct RuntimeError {
    /// Runtime that produced the error, e.g. `claude-cli` or `ollama`.
    pub runtime: String,
    /// Provider serving the model, when known.
    pub provider: Option<String>,
    /// Whether the caller may retry the request.
    pub retryable: bool,
    /// Human-readable error message.
    pub message: String,
    /// Captured subprocess stdout, for CLI runtimes.
    pub stdout: Option<String>,
    /// Captured subprocess stderr, for CLI runtimes.
    pub stderr: Option<String>,
}

impl RuntimeError {
    /// Creates a non-retryable runtime error.
    pub fn new(runtime: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            runtime: runtime.into(),
            provider: None,
            retryable: false,
            message: message.into(),
            stdout: None,
            stderr: None,
        }
    }

    /// Sets the serving provider.
    #[must_use]
    pub fn with_provider(mut self, provider: Option<String>) -> Self {
        self.provider = provider;
        self
    }

    /// Marks the error retryable.
    #[must_use]
    pub fn retryable(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "runtime {} error: {}",
            self.runtime, self.message
        )
    }
}

impl std::error::Error for RuntimeError {}

impl From<RuntimeError> for ModelError {
    fn from(error: RuntimeError) -> Self {
        // Preserve the serving provider when it differs from the runtime, so a
        // failure on e.g. `openai-compatible` + provider `openrouter` keeps both
        // identities for diagnosis (D79).
        let provider = match error.provider {
            Some(serving) if serving != error.runtime => {
                format!("{serving} via {}", error.runtime)
            }
            _ => error.runtime,
        };
        ModelError::Provider {
            provider,
            message: error.message,
            retryable: error.retryable,
        }
    }
}

type ProviderConstructor =
    Arc<dyn Fn(&ModelDef, &AuthStore) -> Result<Box<dyn Model>, ModelError> + Send + Sync>;

/// Registry mapping runtime ids to model constructors (D79).
///
/// The five `*-cli` runtimes delegate to the `derrick-tools` host adapters
/// (unchanged D65 behaviour); `shell` is the bespoke-envelope escape hatch; and
/// the opt-in API/local runtimes (`anthropic-api`, `openai-api`,
/// `openai-compatible`, `ollama`) build HTTP-backed models. Adding a runtime is
/// a `register` call — not an architectural change.
#[derive(Clone, Default)]
pub struct RuntimeRegistry {
    constructors: HashMap<String, ProviderConstructor>,
}

/// Backwards-compatible alias for the pre-D79 name.
pub type ProviderRegistry = RuntimeRegistry;

impl RuntimeRegistry {
    /// Returns a registry pre-populated with every built-in runtime.
    pub fn with_defaults() -> Self {
        let mut registry = Self::default();
        registry.register("shell", providers::shell::build);
        register_cli_runtime(&mut registry, "claude-cli", "claude", || {
            Arc::new(derrick_tools::ClaudeHost::new())
        });
        register_cli_runtime(&mut registry, "codex-cli", "codex", || {
            Arc::new(derrick_tools::CodexHost::new())
        });
        register_cli_runtime(&mut registry, "copilot-cli", "copilot", || {
            Arc::new(derrick_tools::CopilotHost::new())
        });
        register_cli_runtime(&mut registry, "opencode-cli", "opencode", || {
            Arc::new(derrick_tools::OpencodeHost::new())
        });
        register_cli_runtime(&mut registry, "aider-cli", "aider", || {
            Arc::new(derrick_tools::AiderHost::new())
        });
        registry.register("anthropic-api", |model_def, auth| {
            providers::api::build(providers::api::ApiDialect::Anthropic, model_def, auth)
        });
        registry.register("openai-api", |model_def, auth| {
            providers::api::build(providers::api::ApiDialect::OpenAi, model_def, auth)
        });
        registry.register("openai-compatible", |model_def, auth| {
            providers::api::build(
                providers::api::ApiDialect::OpenAiCompatible,
                model_def,
                auth,
            )
        });
        registry.register("ollama", |model_def, auth| {
            providers::api::build(providers::api::ApiDialect::Ollama, model_def, auth)
        });
        registry
    }

    /// Adds or replaces a constructor for a runtime id.
    pub fn register<F>(&mut self, name: &str, constructor: F)
    where
        F: Fn(&ModelDef, &AuthStore) -> Result<Box<dyn Model>, ModelError> + Send + Sync + 'static,
    {
        self.constructors
            .insert(name.to_owned(), Arc::new(constructor));
    }

    /// Builds a model from a model definition, dispatching on its resolved
    /// runtime (D79). A legacy `provider:`-only config resolves to the matching
    /// `*-cli` runtime.
    pub fn build(
        &self,
        model_def: &ModelDef,
        auth: &AuthStore,
    ) -> Result<Box<dyn Model>, ModelError> {
        let runtime = model_def.resolved_runtime();
        let constructor = self
            .constructors
            .get(&runtime)
            .ok_or(ModelError::UnknownProvider(runtime))?;

        constructor(model_def, auth)
    }
}

/// Registers a `*-cli` runtime that delegates to a `derrick-tools` host adapter.
///
/// `make_adapter` constructs a fresh `Arc<dyn HostAdapter>` per built model. The
/// model's reported provider stays the host name (e.g. `claude`) for telemetry
/// and the assay family check.
fn register_cli_runtime(
    registry: &mut RuntimeRegistry,
    runtime: &'static str,
    host: &'static str,
    make_adapter: fn() -> Arc<dyn derrick_tools::HostAdapter>,
) {
    registry.register(runtime, move |model_def, auth| {
        providers::host_delegated::build_for_host(host, make_adapter(), model_def, auth)
    });
}

/// Resolves a role name to a ready-to-call model.
pub async fn resolve_role(
    role: &str,
    roles: &RoleBindings,
    models: &ModelRegistry,
    auth: &AuthStore,
) -> Result<Box<dyn Model>, ModelError> {
    let model_name = roles
        .get(role)
        .ok_or_else(|| ModelError::UnknownRole(role.to_owned()))?;
    let model_def = models
        .get(model_name)
        .ok_or_else(|| ModelError::UnknownModel(model_name.to_owned()))?;

    ProviderRegistry::with_defaults().build(model_def, auth)
}

/// Completes a request, retrying on **retryable** errors (transient subprocess
/// failures, HTTP 429/5xx, timeouts) with exponential backoff (D79).
///
/// Up to `max_attempts` total attempts; non-retryable errors return immediately.
/// `max_attempts <= 1` disables retries. Backoff is `200ms * 2^(attempt-1)`.
pub async fn complete_with_retry(
    model: &dyn Model,
    request: CompletionRequest,
    max_attempts: u32,
) -> Result<CompletionResponse, ModelError> {
    let mut attempt: u32 = 1;
    loop {
        match model.complete(request.clone()).await {
            Ok(response) => return Ok(response),
            Err(error) if error.is_retryable() && attempt < max_attempts => {
                // Cap the exponent (overflow-safe) and the total backoff at 30s
                // so a large `max_attempts` can't produce an unbounded sleep.
                let exponent = (attempt - 1).min(8);
                let backoff = Duration::from_millis((200u64 << exponent).min(30_000));
                tokio::time::sleep(backoff).await;
                attempt += 1;
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cost_hint_estimate_usd_zero_for_zero_tokens() {
        let hint = CostHint {
            in_per_mtok: 3.0,
            out_per_mtok: 15.0,
        };
        assert_eq!(hint.estimate_usd(0, 0), 0.0);
    }

    /// A `Model` that fails a fixed number of times before succeeding, used to
    /// exercise `complete_with_retry`.
    struct FlakyModel {
        remaining_failures: std::sync::atomic::AtomicU32,
        retryable: bool,
        attempts: std::sync::atomic::AtomicU32,
    }

    #[async_trait]
    impl Model for FlakyModel {
        fn name(&self) -> &str {
            "flaky"
        }
        fn provider(&self) -> &str {
            "test"
        }
        fn cost_hint(&self) -> Option<&CostHint> {
            None
        }
        async fn stream(
            &self,
            _request: CompletionRequest,
        ) -> Result<CompletionStream, ModelError> {
            unreachable!("complete is overridden")
        }
        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> Result<CompletionResponse, ModelError> {
            use std::sync::atomic::Ordering;
            self.attempts.fetch_add(1, Ordering::SeqCst);
            if self.remaining_failures.load(Ordering::SeqCst) > 0 {
                self.remaining_failures.fetch_sub(1, Ordering::SeqCst);
                return Err(ModelError::Provider {
                    provider: "test".to_owned(),
                    message: "transient".to_owned(),
                    retryable: self.retryable,
                });
            }
            Ok(CompletionResponse {
                text: "ok".to_owned(),
                tokens_in: 0,
                tokens_out: 0,
                finish_reason: FinishReason::Stop,
            })
        }
    }

    fn request() -> CompletionRequest {
        CompletionRequest {
            cached_prefix: None,
            prompt: "hi".to_owned(),
            system: None,
            max_tokens: None,
            temperature: None,
            timeout: Duration::from_secs(1),
        }
    }

    /// A `Model` that yields a fixed sequence of events via the default
    /// `complete` (which drains `stream`), used to check terminal-event
    /// handling.
    struct ScriptedModel {
        events: Vec<CompletionEvent>,
    }

    #[async_trait]
    impl Model for ScriptedModel {
        fn name(&self) -> &str {
            "scripted"
        }
        fn provider(&self) -> &str {
            "test"
        }
        fn cost_hint(&self) -> Option<&CostHint> {
            None
        }
        async fn stream(
            &self,
            _request: CompletionRequest,
        ) -> Result<CompletionStream, ModelError> {
            let events: Vec<Result<CompletionEvent, ModelError>> =
                self.events.iter().cloned().map(Ok).collect();
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    #[tokio::test]
    async fn complete_reports_error_when_stream_ends_without_end_event() {
        // Mid-stream death: content arrives, then the stream ends with no
        // terminal `End`. This must surface as `Error`, not a clean `Stop`.
        let model = ScriptedModel {
            events: vec![CompletionEvent::Content {
                text: "partial".to_owned(),
            }],
        };
        let response = model.complete(request()).await.unwrap();
        assert_eq!(response.text, "partial");
        assert_eq!(response.finish_reason, FinishReason::Error);
    }

    #[tokio::test]
    async fn complete_propagates_terminal_end_reason() {
        let model = ScriptedModel {
            events: vec![
                CompletionEvent::Content {
                    text: "done".to_owned(),
                },
                CompletionEvent::End {
                    tokens_in: 3,
                    tokens_out: 1,
                    finish_reason: FinishReason::Length,
                },
            ],
        };
        let response = model.complete(request()).await.unwrap();
        assert_eq!(response.text, "done");
        assert_eq!(response.tokens_in, 3);
        assert_eq!(response.tokens_out, 1);
        assert_eq!(response.finish_reason, FinishReason::Length);
    }

    #[tokio::test(start_paused = true)]
    async fn retry_succeeds_after_retryable_failures() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let model = FlakyModel {
            remaining_failures: AtomicU32::new(1),
            retryable: true,
            attempts: AtomicU32::new(0),
        };
        let response = complete_with_retry(&model, request(), 3).await.unwrap();
        assert_eq!(response.text, "ok");
        assert_eq!(model.attempts.load(Ordering::SeqCst), 2); // one retry
    }

    #[tokio::test]
    async fn retry_does_not_retry_non_retryable() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let model = FlakyModel {
            remaining_failures: AtomicU32::new(1),
            retryable: false,
            attempts: AtomicU32::new(0),
        };
        let error = complete_with_retry(&model, request(), 3).await.unwrap_err();
        assert!(!error.is_retryable());
        assert_eq!(model.attempts.load(Ordering::SeqCst), 1); // no retry
    }

    #[tokio::test(start_paused = true)]
    async fn retry_gives_up_after_max_attempts() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let model = FlakyModel {
            remaining_failures: AtomicU32::new(10),
            retryable: true,
            attempts: AtomicU32::new(0),
        };
        let error = complete_with_retry(&model, request(), 2).await.unwrap_err();
        assert!(error.is_retryable());
        assert_eq!(model.attempts.load(Ordering::SeqCst), 2); // capped
    }

    #[test]
    fn cost_hint_estimate_usd_rounds_correctly() {
        let hint = CostHint {
            in_per_mtok: 3.0,
            out_per_mtok: 15.0,
        };
        // 1M input + 100k output = $3.00 + $1.50 = $4.50
        let cost = hint.estimate_usd(1_000_000, 100_000);
        assert!((cost - 4.5).abs() < 1e-9, "expected ~$4.50, got {cost}");
    }

    #[test]
    fn builtin_cost_hint_recognises_claude_sonnet() {
        let hint = builtin_cost_hint("claude-sonnet-4-5").unwrap();
        assert_eq!(hint.in_per_mtok, 3.0);
    }

    #[test]
    fn builtin_cost_hint_returns_none_for_unknown() {
        assert!(builtin_cost_hint("my-local-llama").is_none());
    }
}
