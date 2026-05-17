# T006 — `derrick-models` BYOM provider trait

**Specialist owner**: `integrations-engineer` (opus)
**Crate**: `crates/derrick-models`
**Depends on**: `derrick-config` (for `ModelDef` / `ModelRegistry` types)
**Priority**: P1 — every pipeline step with a `role:` needs a Model to call. Blocks T009 (`derrick-flow`).

## Why

DESIGN.md §6.5 defines BYOM (Bring Your Own Model): providers
serve inference; hosts load context; roles name what work
needs done; pipeline steps name roles. This crate is the
**provider** layer. It exposes one async trait every adapter
implements plus a registry that resolves a role → model →
provider chain.

## Scope (v1)

### Public API

```rust
//! Model provider abstraction. See DESIGN.md §6.5 and D12.

use derrick_config::{ModelDef, ModelRegistry};

/// Lookup a model by role name, walking
/// `Config.roles[role] -> Config.models[model_name]`. Returns
/// a ready-to-call `Box<dyn Model>` or a typed error.
pub async fn resolve_role(
    role: &str,
    roles: &derrick_config::RoleBindings,
    models: &ModelRegistry,
    auth: &AuthStore,
) -> Result<Box<dyn Model>, ModelError>;

/// A model the orchestrator can call. Adapters live under
/// `crates/derrick-models/src/providers/<name>.rs` and one
/// constructor each is registered through `ProviderRegistry`.
///
/// **`stream` is the primitive.** `complete` has a default
/// implementation that drains the stream and assembles a
/// `CompletionResponse`. Adapters should only override
/// `complete` when their provider has a meaningfully cheaper
/// non-streaming code path (e.g. a single API call vs. SSE
/// setup).
#[async_trait::async_trait]
pub trait Model: Send + Sync {
    /// Human-readable name (for logs / `derrick gain`).
    fn name(&self) -> &str;

    /// Which provider family (anthropic, openai, ollama, ...).
    fn provider(&self) -> &str;

    /// Cost hints used by `derrick gain` to convert token counts
    /// into dollar estimates. `None` when the user did not
    /// supply a cost_hint in derrick.yaml.
    fn cost_hint(&self) -> Option<&CostHint>;

    /// Whether this provider supports host-delegated auth
    /// (i.e. shells to a host CLI that owns its own keys).
    /// Default: false. Providers that delegate auth (claude
    /// / codex / copilot-cli) override to true.
    fn host_delegated_auth(&self) -> bool { false }

    /// Primitive: streaming completion. Returns a
    /// `Stream<Item = Result<CompletionEvent, ModelError>>`.
    /// Implementations that don't natively stream emit a single
    /// `Content` event followed by `End`.
    async fn stream(
        &self,
        request: CompletionRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<CompletionEvent, ModelError>> + Send>>, ModelError>;

    /// Convenience: single-shot completion, derived from
    /// `stream` by draining and assembling.
    async fn complete(&self, request: CompletionRequest)
        -> Result<CompletionResponse, ModelError>
    {
        // Default impl: collect stream events into a response.
        // Adapters may override for native non-streaming paths.
        let mut stream = self.stream(request).await?;
        let mut text = String::new();
        let mut tokens_in = 0;
        let mut tokens_out = 0;
        let mut finish_reason = FinishReason::Stop;
        use futures::StreamExt;
        while let Some(event) = stream.next().await {
            match event? {
                CompletionEvent::Content { text: chunk } => text.push_str(&chunk),
                CompletionEvent::End { tokens_in: i, tokens_out: o, finish_reason: f } => {
                    tokens_in = i;
                    tokens_out = o;
                    finish_reason = f;
                }
            }
        }
        Ok(CompletionResponse { text, tokens_in, tokens_out, finish_reason })
    }
}

#[derive(Clone, Debug)]
pub struct CompletionRequest {
    /// The cacheable prefix the caller has assembled
    /// (typically constitution + derrick.yaml + memory seeds).
    /// Always populated by the caller; adapters consult their
    /// own `ModelDef.cache` flag to decide whether to mark the
    /// region cacheable on the wire. If the provider doesn't
    /// support caching or `cache: false`, this string is
    /// concatenated as ordinary prompt content per the
    /// prompt-assembly rules below.
    pub cached_prefix: Option<String>,
    /// The mutable per-call prompt.
    pub prompt: String,
    /// Optional system message; adapters that don't support a
    /// system role concat it as a prefix to `prompt`.
    pub system: Option<String>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub timeout: std::time::Duration,
}

// Prompt assembly: when an adapter cannot honour all four
// fields (`system`, `cached_prefix`, `prompt`, plus the
// provider's wire-level system role), the canonical order
// from highest precedence to lowest is:
//
//   1. provider system role (if available)   <- system
//   2. cached_prefix                          <- cached_prefix
//   3. prompt                                 <- prompt
//
// Adapters without a native system role concatenate
// `system`, `cached_prefix`, and `prompt` in that order with
// "\n\n" separators. Adapters with cache support mark the
// region containing `system` and `cached_prefix` as cacheable;
// `prompt` is always uncached.

#[derive(Clone, Debug)]
pub struct CompletionResponse {
    pub text: String,
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub finish_reason: FinishReason,
}

#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum CompletionEvent {
    /// A chunk of response content.
    Content { text: String },
    /// End-of-stream. Carries final token counts and finish
    /// reason.
    End {
        tokens_in: u32,
        tokens_out: u32,
        finish_reason: FinishReason,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum FinishReason {
    Stop,
    Length,
    Error,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct CostHint {
    /// USD per 1M input tokens.
    pub in_per_mtok: f64,
    /// USD per 1M output tokens.
    pub out_per_mtok: f64,
}

/// Credentials lookup. Wraps env-var-first auth per D12;
/// optional `~/.derrick/credentials.yaml` is a stretch goal
/// implemented in a follow-up ticket.
///
/// Host-delegated providers (those whose `Model::
/// host_delegated_auth()` returns `true`) do **not** look up
/// secrets through this store — they shell to a host CLI that
/// manages its own credentials. Consequently
/// `AuthStore::missing_required(...)` always returns `false`
/// for such providers; `derrick models check` validates them
/// via a different path (presence of the host CLI on PATH).
#[derive(Clone, Debug, Default)]
pub struct AuthStore { /* opaque */ }

impl AuthStore {
    /// Reads from env vars only.
    pub fn from_env() -> Self;

    /// For tests: construct with an explicit map of
    /// `(provider, key)` → secret. Never use in production.
    pub fn for_testing(map: std::collections::HashMap<(String, String), Secret>) -> Self;

    pub fn get(&self, provider: &str, key: &str) -> Option<&Secret>;

    /// Returns the list of `(provider, env_var)` pairs the
    /// caller declared required via `require()` but which are
    /// absent. Host-delegated providers are excluded by
    /// construction. Used by `derrick models check`.
    pub fn missing_required(&self) -> Vec<(String, String)>;

    /// Declare that a non-host-delegated provider needs a
    /// given env var; later surfaced via `missing_required()`.
    pub fn require(&mut self, provider: &str, env_var: &str);
}

/// A wrapper around String that does not Debug-print its content
/// and does not leak via tracing (deliberately implements
/// minimal traits). Use anywhere an API key flows.
#[derive(Clone)]
pub struct Secret(/* opaque */ String);

impl Secret {
    pub fn new<S: Into<String>>(s: S) -> Self;
    pub fn expose(&self) -> &str;
}

impl std::fmt::Debug for Secret { /* prints "Secret(***)" */ }

#[derive(thiserror::Error, Debug)]
#[non_exhaustive]
pub enum ModelError {
    #[error("no such model: {0}")]
    UnknownModel(String),
    #[error("no such role: {0}")]
    UnknownRole(String),
    #[error("no such provider: {0}")]
    UnknownProvider(String),
    #[error("missing credential for provider {provider}: env var {env_var} not set")]
    MissingCredential { provider: String, env_var: String },
    #[error("timeout after {seconds}s calling {provider}")]
    Timeout { provider: String, seconds: u64 },
    #[error("provider error ({provider}): {message}")]
    Provider { provider: String, message: String, retryable: bool },
    #[error("invalid model config for {model}: {message}")]
    InvalidConfig { model: String, message: String },
}

impl ModelError {
    pub fn is_retryable(&self) -> bool;
}
```

### Provider registry

```rust
/// Maps a provider name (anthropic, openai, ollama, shell, ...) to
/// a constructor that takes a `ModelDef` + `AuthStore` and returns
/// a boxed `Model`.
pub struct ProviderRegistry { /* opaque */ }

impl ProviderRegistry {
    /// Returns a registry pre-populated with all v1 providers.
    pub fn with_defaults() -> Self;

    /// Add or replace a constructor for a named provider.
    pub fn register<F>(&mut self, name: &str, ctor: F)
    where
        F: Fn(&ModelDef, &AuthStore) -> Result<Box<dyn Model>, ModelError> + Send + Sync + 'static;

    pub fn build(
        &self,
        model_def: &ModelDef,
        auth: &AuthStore,
    ) -> Result<Box<dyn Model>, ModelError>;
}
```

### v1 provider shipping list

For T006 we ship the **trait + registry + one fully working
provider** so the surface is testable end-to-end without
network. Other providers are follow-up tickets (T006a anthropic,
T006b openai, T006c bedrock, …).

| Provider | T006 status | Tests |
|---|---|---|
| `shell` | **ships** — spawns a configured CLI, writes prompt to stdin, reads response from stdout, parses token counts from the trailing JSON line (documented format) | full unit + integration via tempdir + test scripts |
| `anthropic` | trait shape stub only; constructor returns `ModelError::Provider { retryable: false, message: "not implemented in T006; see T006a" }`. **Phasing note**: DESIGN.md §6.5 and §9.B.4 lean on Anthropic prompt caching in v1; the stub is an explicit phasing step, not a contradiction. T006a (next follow-up after T006 lands) implements the real anthropic adapter and is on the dogfooding-critical path. | one test asserting the stub error message |
| `openai`, `openai-cli`, `google`, `bedrock`, `azure-openai`, `copilot-cli`, `ollama`, `llamacpp` | not registered in `with_defaults()` | n/a — T006[a-h] add these |

The `shell` provider is sufficient to exercise the trait
through every code path: timeouts, retryable errors, prompt
caching marker (ignored at this level), and `derrick gain`
cost-hint roundtrips.

#### `shell` provider contract

Argv-only (no shell metacharacter interpretation), sentinel-
delimited streaming I/O, full envelope on stdin.

```yaml
# derrick.yaml — example shell-provider model
models:
  local-llm:
    provider: shell
    argv: ["ollama", "run", "llama3.3"]   # explicit argv, preferred
    # OR for backwards-compat / convenience:
    # cli: "ollama run llama3.3"   # split via shell-words at load time
    cost_hint: { in_per_mtok: 0.0, out_per_mtok: 0.0 }
    cache: false   # shell provider never marks prefixes cacheable
```

Either `argv` or `cli` is required; if both are present
`argv` wins and a warning is logged. `cli` is parsed via
`shell-words` (argv only — no `&&`, no pipes, no env
interpolation). Anything that requires shell interpretation
must be wrapped in `["bash", "-lc", "..."]` explicitly.

The shell provider:

- Spawns the configured argv; enforces `timeout` from request
  (kill-on-timeout via tokio).
- Writes a single JSON object on stdin (including
  `cached_prefix`, which the shell provider always treats as
  ordinary prompt content since `cache: false`):
  ```json
  {
    "system": "...",
    "cached_prefix": "...",
    "prompt": "...",
    "max_tokens": 2048,
    "temperature": 0.2
  }
  ```
- Reads stdout line by line. Two **reserved sentinel
  prefixes**:
  - Lines beginning with `<<DERRICK-CONTENT>> ` carry response
    text (the rest of the line, plus the line terminator).
    Streamed events emit `CompletionEvent::Content` per such
    line.
  - A single line of the form
    `<<DERRICK-META>> {"tokens_in":N,"tokens_out":N,"finish_reason":"stop"}`
    terminates the response and yields `CompletionEvent::End`.
  - Any other lines are treated as plain content (backwards-
    compatible relaxation: a script that doesn't emit
    sentinels still works; `tokens_in/out` default to 0 and
    finish reason to `Error`).
- stderr is captured and surfaced in `ModelError::Provider`
  on non-zero exit.

The sentinel prefix removes the "what if response text ends
in JSON" ambiguity from round 1: only lines with the explicit
prefix are metadata.

This contract lets a tiny shell script stand in as a model in
tests without dragging in any real provider's SDK.

### Dependencies

```toml
[dependencies]
derrick-config = { path = "../derrick-config" }
async-trait = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
thiserror = { workspace = true }
tokio = { workspace = true }
futures = "0.3"                # add to workspace deps for Stream
shell-words = "1"              # add to workspace deps

[dev-dependencies]
tempfile = { workspace = true }
tokio = { workspace = true, features = ["macros", "rt", "rt-multi-thread"] }
```

Add `futures = "0.3"` and `shell-words = "1"` to root
`[workspace.dependencies]`.

### Tests

All tempfile-based, no network.

Trait-level / registry tests:

- `auth_store_reads_env_vars` — set an env var, build
  `AuthStore::from_env()`, get the value back.
- `auth_store_missing_credential_returns_typed_error`.
- `secret_debug_does_not_leak` — `format!("{:?}", secret)`
  contains "***" and not the secret content.
- `secret_expose_returns_inner`.
- `provider_registry_resolves_known_provider`.
- `provider_registry_unknown_provider_returns_typed_error`.
- `resolve_role_walks_role_to_model_to_provider`.
- `resolve_role_unknown_role_returns_typed_error`.
- `resolve_role_unknown_model_returns_typed_error`.

Shell-provider tests (use small shell scripts in `tests/fixtures/`):

- `shell_provider_completes_simple_prompt` — fixture script
  echoes the prompt back plus a trailing JSON line; the test
  asserts the response and token counts.
- `shell_provider_respects_timeout` — fixture sleeps longer
  than the request's `timeout`; test asserts `ModelError::
  Timeout` within ~timeout + 100ms.
- `shell_provider_nonzero_exit_surfaces_stderr`.
- `shell_provider_missing_trailing_json_falls_back`.
- `shell_provider_streaming_emits_content_then_end`.
- `shell_provider_handles_crlf_in_stdout`.

Other-provider stubs:

- `anthropic_stub_returns_not_implemented_error`.

**Coverage target**: 85% (a bit lower than other crates
because some error branches need network-injection to exercise;
shell-provider tests carry most of the weight).

## Out of scope

- Anthropic, OpenAI, Bedrock, Azure OpenAI, Gemini, Ollama,
  llama.cpp, `copilot-cli`, `codex` adapters. Each gets its
  own follow-up ticket (T006a–T006h). Stubs that return
  "not implemented" are fine for v1 of *this* crate; they
  go to red in `derrick models check` and the user sees a
  clear error.
- `~/.derrick/credentials.yaml` keyfile loader — env-vars-only
  in T006. Keyfile is T006i.
- Rate limiting / retry policy — adapters return
  `ModelError::Provider { retryable: bool }` and the caller
  (typically `derrick-flow`) decides retry semantics.
- Cost telemetry aggregation — that's `derrick gain`'s job.

## Acceptance

- [ ] `cargo check -p derrick-models` passes.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] `cargo test -p derrick-models` (incl. fixture-script
      integration tests), 3× stress green.
- [ ] `cargo llvm-cov -p derrick-models --fail-under-lines 85`.
- [ ] Workspace `cargo llvm-cov --fail-under-lines 80` still passes.
- [ ] No `unwrap`/`expect`/`panic` in non-test code.
- [ ] `Secret::Debug` test confirms no key material leaks.
- [ ] All public types/methods documented.
- [ ] No gastown vocabulary anywhere.

## Reviewer notes (Codex)

Pre-implementation review. Focus on:
- Is the `Model` trait shape enough for both single-shot and
  streaming use? Anything that would force a future breaking
  change?
- Is the `shell` provider contract precise enough for an
  implementer to write a fixture script?
- Are auth + Secret sufficient given D12?
- Is the cached-prefix marker meaningful for providers that
  ignore it, or should the request type just lift the prefix
  into the prompt at the call site?

## Implementer notes (Copilot)

Stay in `crates/derrick-models/`. The only outside edits are
adding `futures = "0.3"` and `shell-words = "1"` to root
`[workspace.dependencies]`. Test fixture scripts go under
`crates/derrick-models/tests/fixtures/` with `chmod +x`. macOS
+ Linux only; treat Windows as TODO with a clear comment.
