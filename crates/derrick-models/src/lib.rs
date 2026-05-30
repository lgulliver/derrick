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
        let mut finish_reason = FinishReason::Stop;

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

type ProviderConstructor =
    Arc<dyn Fn(&ModelDef, &AuthStore) -> Result<Box<dyn Model>, ModelError> + Send + Sync>;

/// Registry mapping provider names to model constructors.
#[derive(Clone, Default)]
pub struct ProviderRegistry {
    constructors: HashMap<String, ProviderConstructor>,
}

impl ProviderRegistry {
    /// Returns a registry pre-populated with the default providers.
    ///
    /// Per D65, inference is host-delegated: each of the five host CLIs is
    /// registered as a provider whose name equals the host name. The `shell`
    /// provider survives as a bespoke-envelope escape hatch. There is no
    /// direct-API path.
    pub fn with_defaults() -> Self {
        let mut registry = Self::default();
        registry.register("shell", providers::shell::build);
        register_host(&mut registry, "claude", || {
            Arc::new(derrick_tools::ClaudeHost::new())
        });
        register_host(&mut registry, "codex", || {
            Arc::new(derrick_tools::CodexHost::new())
        });
        register_host(&mut registry, "copilot", || {
            Arc::new(derrick_tools::CopilotHost::new())
        });
        register_host(&mut registry, "opencode", || {
            Arc::new(derrick_tools::OpencodeHost::new())
        });
        register_host(&mut registry, "aider", || {
            Arc::new(derrick_tools::AiderHost::new())
        });
        registry
    }

    /// Adds or replaces a constructor for a provider name.
    pub fn register<F>(&mut self, name: &str, constructor: F)
    where
        F: Fn(&ModelDef, &AuthStore) -> Result<Box<dyn Model>, ModelError> + Send + Sync + 'static,
    {
        self.constructors
            .insert(name.to_owned(), Arc::new(constructor));
    }

    /// Builds a model from a model definition.
    pub fn build(
        &self,
        model_def: &ModelDef,
        auth: &AuthStore,
    ) -> Result<Box<dyn Model>, ModelError> {
        let provider = model_def.provider();
        let constructor = self
            .constructors
            .get(provider)
            .ok_or_else(|| ModelError::UnknownProvider(provider.to_owned()))?;

        constructor(model_def, auth)
    }
}

/// Registers a host-delegated provider for `host` in the registry.
///
/// `make_adapter` constructs a fresh `Arc<dyn HostAdapter>` per built model
/// (the adapter's `run` takes `&self`; the registry is not `Clone` and owns no
/// adapter references). The provider name equals the host name.
fn register_host(
    registry: &mut ProviderRegistry,
    host: &'static str,
    make_adapter: fn() -> Arc<dyn derrick_tools::HostAdapter>,
) {
    registry.register(host, move |model_def, auth| {
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
