//! Load and validate `derrick.yaml`.
//!
//! `Config::load_layered` builds the effective configuration from
//! built-in defaults, then `~/.derrick/config.yaml` when present, then
//! `<repo_root>/derrick.yaml` when present. Higher-precedence layers
//! override lower-precedence layers field-by-field. Maps (`models`,
//! `roles`) merge by key, sequences replace wholesale when present,
//! nested structs merge field-by-field, scalars override when present,
//! and YAML `null` is treated the same as an omitted field.
//!
//! This crate performs structural validation only: schema shape, enum
//! values, and references between sections. Host/provider compatibility is
//! checked by downstream model tooling.

use std::collections::{BTreeMap, HashMap};
use std::env;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde::de::{self, MapAccess, Visitor};
use thiserror::Error;

const CONFIG_VERSION: u32 = 1;

/// Maps a legacy host-provider name to its D79 runtime.
///
/// The five CLI hosts (`claude`/`codex`/`copilot`/`opencode`/`aider`) each have
/// a `*-cli` runtime; everything else (the `shell` escape hatch and the opt-in
/// API/local runtimes such as `ollama`) passes through unchanged. Used to derive
/// the runtime for legacy configs that name only a `provider`.
pub fn runtime_for_provider(provider: &str) -> &str {
    match provider {
        "claude" => "claude-cli",
        "codex" => "codex-cli",
        "copilot" => "copilot-cli",
        "opencode" => "opencode-cli",
        "aider" => "aider-cli",
        other => other,
    }
}

/// Builds a legacy host-CLI [`ModelDef`] (provider-only; runtime derived).
fn host_model(provider: &str, model: &str) -> ModelDef {
    ModelDef {
        runtime: None,
        provider: Some(provider.to_owned()),
        model: model.to_owned(),
        cli: None,
        base_url: None,
        endpoint: None,
        auth_env: None,
        auth_mode: None,
        params: BTreeMap::new(),
        capabilities: None,
        max_tokens: None,
        temperature: None,
        cache: None,
        timeout: None,
        rate_limit: None,
        cost_hint: None,
        estimated: None,
    }
}

/// Returns the host CLI binary backing a `*-cli` runtime, or `None` for API,
/// local, and `shell` runtimes that do not shell out to a managed host.
pub fn cli_host_for_runtime(runtime: &str) -> Option<&'static str> {
    match runtime {
        "claude-cli" => Some("claude"),
        "codex-cli" => Some("codex"),
        "copilot-cli" => Some("copilot"),
        "opencode-cli" => Some("opencode"),
        "aider-cli" => Some("aider"),
        _ => None,
    }
}

/// The runtimes derrick knows how to invoke (D79). Used by `models check` to
/// distinguish a typo from a genuinely-unknown runtime.
pub const KNOWN_RUNTIMES: [&str; 10] = [
    "claude-cli",
    "codex-cli",
    "copilot-cli",
    "opencode-cli",
    "aider-cli",
    "anthropic-api",
    "openai-api",
    "openai-compatible",
    "ollama",
    "shell",
];

/// Init-time values substituted into `templates/derrick.yaml.in`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InitTemplateVars<'a> {
    /// Site name written to `site.name`.
    pub site_name: &'a str,
    /// Ticket prefix written to `site.prefix`.
    pub prefix: &'a str,
    /// Operating mode written to `tools.substrate.mode`.
    pub mode: &'a str,
}

/// Renders an init template with minimal `{{var}}` substitution.
///
/// Unknown placeholders are left intact so runtime pipeline variables such as
/// `{{prompt}}` and `{{feature_dir}}` survive for later flow execution.
pub fn render_init_template(template: &str, vars: InitTemplateVars<'_>) -> String {
    template
        .replace("{{site_name}}", vars.site_name)
        .replace("{{prefix}}", vars.prefix)
        .replace("{{mode}}", vars.mode)
}

/// A loaded and structurally validated derrick configuration.
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    version: u32,
    site: Site,
    models: ModelRegistry,
    roles: RoleBindings,
    tools: Tools,
    pipeline: Vec<PipelineStep>,
    guardrails: Guardrails,
    parallelism: Parallelism,
    state: StateConfig,
    /// Per-stage capability requirements declared under `stages:` (D79). Keyed
    /// by stage/role name; checked by `derrick models check`.
    stage_requirements: BTreeMap<String, Vec<String>>,
    /// User-defined AI profiles (D80). Built-in profiles are merged at lookup.
    profiles: ProfileRegistry,
    /// Optional cost budgets (D80).
    budgets: Option<BudgetConfig>,
    /// Default profile applied when none is requested (D80).
    default_profile: Option<String>,
    /// Profile applied in-memory for this run, if any (D80).
    active_profile: Option<String>,
}

impl Config {
    /// Loads a `derrick.yaml` from `path` and validates the resulting config.
    pub fn load_from_path(path: &Path) -> Result<Self, ConfigError> {
        let layer = read_layer(path)?;
        let config = layer.finalize()?;
        config.validate()?;
        Ok(config)
    }

    /// Returns the baked-in fallback configuration.
    pub fn defaults() -> Self {
        let mut models = HashMap::new();
        models.insert(
            "claude-opus".to_owned(),
            host_model("claude", "claude-opus-4-8"),
        );
        models.insert(
            "claude-sonnet".to_owned(),
            host_model("claude", "claude-sonnet-4-6"),
        );
        models.insert(
            "claude-haiku".to_owned(),
            host_model("claude", "claude-haiku-4-5"),
        );
        models.insert("codex-gpt5".to_owned(), host_model("codex", "gpt-5.5"));
        // `auto` (D67): the foreman selects the best model within the copilot
        // host per ticket by complexity.
        models.insert("copilot".to_owned(), host_model("copilot", "auto"));

        let roles = HashMap::from([
            ("proposer".to_owned(), "claude-opus".to_owned()),
            ("drafter".to_owned(), "claude-sonnet".to_owned()),
            ("reviewer".to_owned(), "codex-gpt5".to_owned()),
            ("executor".to_owned(), "copilot".to_owned()),
            ("summariser".to_owned(), "claude-haiku".to_owned()),
        ]);

        Self {
            version: CONFIG_VERSION,
            site: Site {
                name: "derrick".to_owned(),
                prefix: "drk".to_owned(),
            },
            models: ModelRegistry(models),
            roles: RoleBindings(roles),
            tools: Tools::default(),
            pipeline: Vec::new(),
            guardrails: Guardrails {
                constitution_path: PathBuf::from(".specify/memory/constitution.md"),
                forbid_paths: Vec::new(),
                required_labels: Vec::new(),
            },
            parallelism: Parallelism {
                batch_max: 8,
                step_max: 4,
                assay_max: 2,
            },
            state: StateConfig {
                dir: PathBuf::from(".derrick"),
                log_runs: true,
                worktree_root: PathBuf::from(".derrick/worktrees"),
            },
            stage_requirements: BTreeMap::new(),
            profiles: ProfileRegistry::default(),
            budgets: None,
            default_profile: None,
            active_profile: None,
        }
    }

    /// Loads built-in defaults, optional user config, and optional repo config.
    pub fn load_layered(repo_root: &Path) -> Result<Self, ConfigError> {
        let mut layer = ConfigLayer::from(Self::defaults());
        if let Some(user_path) = user_config_path() {
            if user_path.exists() {
                layer.merge(read_layer(&user_path)?);
            }
        }

        let repo_path = repo_root.join("derrick.yaml");
        if repo_path.exists() {
            layer.merge(read_layer(&repo_path)?);
        }

        let config = layer.finalize()?;
        config.validate()?;
        Ok(config)
    }

    /// Validates structural config invariants.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.version != CONFIG_VERSION {
            return validation(format!(
                "version: unsupported config version {}; this binary speaks v1 only. See DESIGN.md §4.",
                self.version
            ));
        }

        validate_site_prefix(&self.site.prefix)?;
        validate_parallelism(&self.parallelism)?;
        validate_state_dir(&self.state.dir)?;
        validate_roles(&self.roles, &self.models)?;
        validate_assay(&self.tools.assay, &self.roles)?;
        validate_code_review(&self.tools.code_review, &self.roles)?;
        validate_pipeline(&self.pipeline, &self.roles)?;
        Ok(())
    }

    /// Returns the config version.
    pub fn version(&self) -> u32 {
        self.version
    }

    /// Returns the site identity.
    pub fn site(&self) -> &Site {
        &self.site
    }

    /// Returns the configured model registry.
    pub fn models(&self) -> &ModelRegistry {
        &self.models
    }

    /// Returns role-to-model bindings.
    pub fn roles(&self) -> &RoleBindings {
        &self.roles
    }

    /// Returns tool configuration.
    pub fn tools(&self) -> &Tools {
        &self.tools
    }

    /// Forces the `import` spec provider with the given `source` for this run,
    /// overriding `tools.specify.provider`/`tools.specify.import.source` from the
    /// config file.
    ///
    /// This is the highest-precedence entry point for the CLI `--spec <path>`
    /// override: it switches the provider to [`SpecProviderKind::Import`] and
    /// sets the source, leaving the downstream `import.{plan,tasks}` modes (and
    /// every other config field) untouched. It mutates the in-memory config only
    /// — the `derrick.yaml` file is never rewritten.
    pub fn force_import_spec(&mut self, source: String) {
        self.tools.specify.provider = SpecProviderKind::Import;
        self.tools.specify.import.source = Some(source);
    }

    /// Returns the configured pipeline steps.
    pub fn pipeline(&self) -> &[PipelineStep] {
        &self.pipeline
    }

    /// Returns guardrail configuration.
    pub fn guardrails(&self) -> &Guardrails {
        &self.guardrails
    }

    /// Returns parallelism budgets.
    pub fn parallelism(&self) -> &Parallelism {
        &self.parallelism
    }

    /// Returns state storage configuration.
    pub fn state(&self) -> &StateConfig {
        &self.state
    }

    /// Returns per-stage capability requirements declared under `stages:` (D79).
    pub fn stage_requirements(&self) -> &BTreeMap<String, Vec<String>> {
        &self.stage_requirements
    }

    /// Returns the user-defined profile registry (D80).
    pub fn profiles(&self) -> &ProfileRegistry {
        &self.profiles
    }

    /// Returns the cost budgets, if configured (D80).
    pub fn budgets(&self) -> Option<&BudgetConfig> {
        self.budgets.as_ref()
    }

    /// Returns the default profile name, if configured (D80).
    pub fn default_profile(&self) -> Option<&str> {
        self.default_profile.as_deref()
    }

    /// Returns the profile applied in-memory for this run, if any (D80).
    pub fn active_profile(&self) -> Option<&str> {
        self.active_profile.as_deref()
    }

    /// Returns a clone of this config with the named profile applied in-memory.
    /// Built-in profiles are checked if the name is not found among user-defined
    /// profiles. Unknown model aliases in the profile are warned and skipped.
    pub fn with_profile(&self, name: &str) -> Result<Self, ConfigError> {
        let builtin = builtin_profiles();
        let profile = self
            .profiles
            .0
            .get(name)
            .or_else(|| builtin.get(name))
            .ok_or_else(|| {
                ConfigError::Validation(format!(
                    "profile {name:?} is not defined; run `derrick profile list` to see available profiles"
                ))
            })?;

        let mut config = self.clone();
        config.active_profile = Some(name.to_owned());

        for (stage, aliases) in &profile.stages {
            if aliases.is_empty() {
                continue;
            }
            if stage == "assay" && aliases.len() > 1 {
                let mut reviewer_roles: Vec<String> = Vec::new();
                for (i, alias) in aliases.iter().enumerate() {
                    if config.models.contains_key(alias) {
                        let role = format!("assay-reviewer-{}", i + 1);
                        config.roles.0.insert(role.clone(), alias.clone());
                        reviewer_roles.push(role);
                    } else {
                        tracing::warn!(
                            target: "derrick_config",
                            "profile {name:?}: stage `assay` references unknown model alias {alias:?}; skipping"
                        );
                    }
                }
                if !reviewer_roles.is_empty() {
                    config.tools.assay.enabled = true;
                    config.tools.assay.role = reviewer_roles[0].clone();
                    config.tools.assay.reviewers = reviewer_roles;
                }
            } else {
                let alias = &aliases[0];
                if config.models.contains_key(alias) {
                    let role = stage_to_role(stage);
                    config.roles.0.insert(role.to_owned(), alias.clone());
                } else {
                    tracing::warn!(
                        target: "derrick_config",
                        "profile {name:?}: stage {stage:?} references unknown model alias {alias:?}; keeping existing binding"
                    );
                }
            }
        }

        Ok(config)
    }
}

/// Errors returned while loading or validating derrick configuration.
#[derive(Error, Debug)]
pub enum ConfigError {
    /// An operating-system error occurred while reading a config file.
    #[error("IO error reading {path}: {source}")]
    Io {
        /// The path that could not be read.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },

    /// YAML syntax or schema deserialization failed.
    #[error("YAML syntax error in {path} at line {line}: {message}")]
    Syntax {
        /// The file containing the invalid YAML.
        path: PathBuf,
        /// The 1-indexed source line reported by the YAML parser.
        line: usize,
        /// The parser or schema error message.
        message: String,
    },

    /// A structurally valid YAML document violates derrick config rules.
    #[error("Validation failed: {0}")]
    Validation(String),
}

/// Site identity used by the substrate and ticket names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Site {
    name: String,
    prefix: String,
}

impl Site {
    /// Returns the human-readable site name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the lowercase ticket prefix.
    pub fn prefix(&self) -> &str {
        &self.prefix
    }
}

/// Named model definitions keyed by model alias.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelRegistry(HashMap<String, ModelDef>);

impl ModelRegistry {
    /// Returns a model definition by name.
    pub fn get(&self, name: &str) -> Option<&ModelDef> {
        self.0.get(name)
    }

    /// Returns all model definitions.
    pub fn as_map(&self) -> &HashMap<String, ModelDef> {
        &self.0
    }

    fn contains_key(&self, name: &str) -> bool {
        self.0.contains_key(name)
    }
}

/// A model-alias entry from the `models` section.
///
/// D79 makes `runtime` the primary dimension: it selects *how* derrick invokes
/// the model. When `runtime` is omitted, it is derived from `provider` for
/// backward compatibility (e.g. `provider: claude` → `claude-cli`). API and
/// local runtimes re-activate the `base_url`/`endpoint`/`auth_*` fields that
/// D65 had parsed-and-ignored; CLI runtimes continue to ignore them. The `cli`
/// field remains in use by the `shell` escape hatch only.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelDef {
    runtime: Option<String>,
    provider: Option<String>,
    model: String,
    cli: Option<String>,
    base_url: Option<String>,
    endpoint: Option<String>,
    auth_env: Option<String>,
    auth_mode: Option<String>,
    params: BTreeMap<String, serde_yaml::Value>,
    capabilities: Option<ModelCapabilities>,
    max_tokens: Option<u32>,
    temperature: Option<f64>,
    cache: Option<bool>,
    timeout: Option<String>,
    rate_limit: Option<String>,
    cost_hint: Option<String>,
    estimated: Option<ModelEstimate>,
}

impl ModelDef {
    /// Returns the explicitly-configured runtime, if any.
    ///
    /// Most callers want [`ModelDef::resolved_runtime`], which falls back to the
    /// provider-derived runtime when this is `None`.
    pub fn runtime(&self) -> Option<&str> {
        self.runtime.as_deref()
    }

    /// Returns the runtime to invoke, deriving it from the provider when the
    /// `runtime` field is absent (D79 backward compatibility).
    ///
    /// At least one of `runtime`/`provider` is always present after validation,
    /// so the `shell` fallback is unreachable for a finalized config.
    pub fn resolved_runtime(&self) -> String {
        if let Some(runtime) = &self.runtime {
            return runtime.clone();
        }
        self.provider
            .as_deref()
            .map(runtime_for_provider)
            .unwrap_or("shell")
            .to_owned()
    }

    /// Returns the provider identifier, if configured.
    ///
    /// Provider is *who serves the model* (metadata for auth/cost); the runtime
    /// determines the invocation path. For legacy host configs this is the host
    /// name (`claude`, `codex`, …).
    pub fn provider(&self) -> Option<&str> {
        self.provider.as_deref()
    }

    /// Returns the model identifier, forwarded to the runtime untouched.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Returns the API/local-runtime base URL, when set.
    pub fn base_url(&self) -> Option<&str> {
        self.base_url.as_deref()
    }

    /// Returns the API/local-runtime endpoint override, when set.
    pub fn endpoint(&self) -> Option<&str> {
        self.endpoint.as_deref()
    }

    /// Returns the env var name holding the runtime's API key, when set.
    pub fn auth_env(&self) -> Option<&str> {
        self.auth_env.as_deref()
    }

    /// Returns the auth mode (e.g. `bearer`), when set.
    pub fn auth_mode(&self) -> Option<&str> {
        self.auth_mode.as_deref()
    }

    /// Returns runtime-specific passthrough parameters.
    pub fn params(&self) -> &BTreeMap<String, serde_yaml::Value> {
        &self.params
    }

    /// Returns the declared model capabilities, when present.
    pub fn capabilities(&self) -> Option<&ModelCapabilities> {
        self.capabilities.as_ref()
    }

    /// Returns the optional CLI command.
    ///
    /// Deprecated post-D65: only the `shell` provider still reads this. Host
    /// providers ignore it.
    pub fn cli(&self) -> Option<&str> {
        self.cli.as_deref()
    }

    /// Returns the optional maximum token budget.
    pub fn max_tokens(&self) -> Option<u32> {
        self.max_tokens
    }

    /// Returns the optional model temperature.
    pub fn temperature(&self) -> Option<f64> {
        self.temperature
    }

    /// Returns the optional cache setting.
    pub fn cache(&self) -> Option<bool> {
        self.cache
    }

    /// Returns the optional timeout string.
    pub fn timeout(&self) -> Option<&str> {
        self.timeout.as_deref()
    }

    /// Returns the optional rate-limit hint.
    pub fn rate_limit(&self) -> Option<&str> {
        self.rate_limit.as_deref()
    }

    /// Returns the optional cost hint.
    pub fn cost_hint(&self) -> Option<&str> {
        self.cost_hint.as_deref()
    }

    /// Returns the optional estimated performance characteristics (D80).
    pub fn estimated(&self) -> Option<&ModelEstimate> {
        self.estimated.as_ref()
    }
}

/// Declared capabilities for a model alias (D79).
///
/// Each boolean capability is tri-state: `None` means the alias does not
/// declare it (undeclared → a stage requirement WARNs, never FAILs), `Some(true)`
/// means supported, and `Some(false)` means explicitly unsupported (a matching
/// stage requirement FAILs). The two windows are pure metadata used by telemetry
/// and pre-flight checks.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelCapabilities {
    /// Whether the model can stream tokens.
    pub streaming: Option<bool>,
    /// Whether the model supports tool/function calling.
    pub tools: Option<bool>,
    /// Whether the model supports a structured JSON output mode.
    pub json_mode: Option<bool>,
    /// Whether the model accepts image input.
    pub vision: Option<bool>,
    /// Whether the model supports prompt caching.
    pub prompt_cache: Option<bool>,
    /// Maximum context window in tokens, when known.
    pub context_window: Option<u32>,
    /// Maximum output tokens, when known.
    pub max_output_tokens: Option<u32>,
}

impl ModelCapabilities {
    /// Returns the declared value for a named boolean capability, or `None` when
    /// the capability is undeclared or not a boolean capability.
    pub fn declared(&self, capability: &str) -> Option<bool> {
        match capability {
            "streaming" => self.streaming,
            "tools" => self.tools,
            "json_mode" => self.json_mode,
            "vision" => self.vision,
            "prompt_cache" => self.prompt_cache,
            _ => None,
        }
    }
}

/// Returns known-good capability defaults for a well-known model id, matched by
/// family substring (D79). Lets a stage `requires:` check pass for a capable
/// model without the user hand-declaring `capabilities:`. Unknown / local models
/// return all `None` (undeclared → WARN, never FAIL); a model's own declared
/// capabilities always take precedence over these defaults.
pub fn builtin_capabilities(model: &str) -> ModelCapabilities {
    let id = model.to_ascii_lowercase();
    let caps = |tools, vision, prompt_cache| ModelCapabilities {
        streaming: Some(true),
        tools: Some(tools),
        json_mode: Some(true),
        vision: Some(vision),
        prompt_cache: Some(prompt_cache),
        context_window: None,
        max_output_tokens: None,
    };
    if id.contains("claude") {
        caps(true, true, true)
    } else if id.contains("gpt-5")
        || id.contains("gpt-4o")
        || id.contains("gpt-4.1")
        || id.contains("gemini")
    {
        caps(true, true, false)
    } else {
        // Local / unknown models: assume nothing.
        ModelCapabilities::default()
    }
}

/// Estimated performance characteristics for a model alias (D80).
/// All fields are optional; unknown values never prevent execution.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ModelEstimate {
    latency: Option<String>,
    cost: Option<String>,
    quality: Option<String>,
}

impl ModelEstimate {
    /// Latency tier hint: `low` | `medium` | `high`.
    pub fn latency(&self) -> Option<&str> {
        self.latency.as_deref()
    }
    /// Cost tier hint: `very_low` | `low` | `medium` | `high` | `very_high`.
    pub fn cost(&self) -> Option<&str> {
        self.cost.as_deref()
    }
    /// Quality tier hint: `low` | `medium` | `high` | `very_high`.
    pub fn quality(&self) -> Option<&str> {
        self.quality.as_deref()
    }
}

/// A profile maps stage names to model alias overrides (D80). Profiles
/// temporarily override role bindings without modifying `derrick.yaml`.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Profile {
    stages: std::collections::HashMap<String, Vec<String>>,
    ci: bool,
    description: Option<String>,
}

impl Profile {
    /// Returns the stage→alias(es) bindings.
    pub fn stages(&self) -> &std::collections::HashMap<String, Vec<String>> {
        &self.stages
    }
    /// Returns true if this is a CI-safe (non-interactive) profile.
    pub fn ci(&self) -> bool {
        self.ci
    }
    /// Returns a human-readable description.
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }
}

/// Named profile registry. User-defined profiles live here; built-in
/// profiles (`speed`, `balanced`, `quality`, `cheap`, `local`, `ci`) are
/// provided by [`builtin_profiles`] and merged at lookup time.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ProfileRegistry(std::collections::HashMap<String, Profile>);

impl ProfileRegistry {
    /// Returns a profile by name.
    pub fn get(&self, name: &str) -> Option<&Profile> {
        self.0.get(name)
    }
    /// Returns all user-defined profiles.
    pub fn as_map(&self) -> &std::collections::HashMap<String, Profile> {
        &self.0
    }
}

/// Optional per-operation cost budget (D80). Costs are estimates only
/// until provider telemetry lands in a later release.
#[derive(Clone, Debug, PartialEq)]
pub struct Budget {
    max_cost: f64,
}

impl Budget {
    /// Maximum estimated cost (USD) for the operation.
    pub fn max_cost(&self) -> f64 {
        self.max_cost
    }
}

/// Budget configuration (D80). All scopes are optional.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct BudgetConfig {
    per_ticket: Option<Budget>,
    daily: Option<Budget>,
    monthly: Option<Budget>,
}

impl BudgetConfig {
    /// Returns the per-ticket budget, if set.
    pub fn per_ticket(&self) -> Option<&Budget> {
        self.per_ticket.as_ref()
    }
    /// Returns the daily budget, if set.
    pub fn daily(&self) -> Option<&Budget> {
        self.daily.as_ref()
    }
    /// Returns the monthly budget, if set.
    pub fn monthly(&self) -> Option<&Budget> {
        self.monthly.as_ref()
    }
}

/// Returns the built-in profile definitions (D80). These use common alias
/// names (`fast`, `strong`, `cheap`, `local`) as a convention. Bindings for
/// missing aliases are silently skipped by `Config::with_profile`.
fn builtin_profiles() -> std::collections::HashMap<String, Profile> {
    let mut profiles = std::collections::HashMap::new();

    let single_stage = |alias: &str| vec![alias.to_owned()];

    // speed — minimise latency
    let mut stages = std::collections::HashMap::new();
    for s in &["clarify", "plan", "tasks", "execute"] {
        stages.insert(s.to_string(), single_stage("fast"));
    }
    stages.insert("assay".to_owned(), single_stage("fast"));
    profiles.insert(
        "speed".to_owned(),
        Profile {
            stages,
            ci: false,
            description: Some(
                "Optimise for latency: fastest runtime, smallest model, minimum reviewers"
                    .to_owned(),
            ),
        },
    );

    // balanced — default (no stage overrides; effective no-op with a label)
    profiles.insert(
        "balanced".to_owned(),
        Profile {
            stages: std::collections::HashMap::new(),
            ci: false,
            description: Some("Good quality at reasonable speed (default)".to_owned()),
        },
    );

    // quality — maximise reasoning quality
    let mut stages = std::collections::HashMap::new();
    for s in &["clarify", "plan", "tasks", "execute"] {
        stages.insert(s.to_string(), single_stage("strong"));
    }
    stages.insert(
        "assay".to_owned(),
        vec!["strong".to_owned(), "strong".to_owned()],
    );
    profiles.insert(
        "quality".to_owned(),
        Profile {
            stages,
            ci: false,
            description: Some(
                "Maximum reasoning quality: stronger models and multiple reviewers".to_owned(),
            ),
        },
    );

    // cheap — minimise cost
    let mut stages = std::collections::HashMap::new();
    for s in &["clarify", "plan", "tasks", "execute", "assay"] {
        stages.insert(s.to_string(), single_stage("cheap"));
    }
    profiles.insert(
        "cheap".to_owned(),
        Profile {
            stages,
            ci: false,
            description: Some(
                "Optimise for lowest cost: included CLI usage, local models, cheapest APIs"
                    .to_owned(),
            ),
        },
    );

    // local — local runtimes only
    let mut stages = std::collections::HashMap::new();
    for s in &["clarify", "plan", "tasks", "execute", "assay"] {
        stages.insert(s.to_string(), single_stage("local"));
    }
    profiles.insert(
        "local".to_owned(),
        Profile {
            stages,
            ci: false,
            description: Some(
                "Use only local runtimes (Ollama, LM Studio, vLLM, LiteLLM)".to_owned(),
            ),
        },
    );

    // ci — non-interactive, deterministic
    let mut stages = std::collections::HashMap::new();
    for s in &["clarify", "plan", "tasks", "execute"] {
        stages.insert(s.to_string(), single_stage("fast"));
    }
    stages.insert("assay".to_owned(), single_stage("fast"));
    profiles.insert(
        "ci".to_owned(),
        Profile {
            stages,
            ci: true,
            description: Some("Non-interactive, deterministic, suitable for automation".to_owned()),
        },
    );

    profiles
}

/// Returns the names of all built-in profiles (D80).
pub const BUILTIN_PROFILE_NAMES: [&str; 6] =
    ["speed", "balanced", "quality", "cheap", "local", "ci"];

/// Maps a profile stage name to the canonical role key it overrides (D86).
///
/// Built-in profiles use pipeline stage ids (`clarify`, `plan`, `tasks`,
/// `execute`) as their stage keys; the live role map uses semantic role names
/// (`proposer`, `drafter`, `executor`, …). Without this mapping, profile
/// overrides insert orphan entries that the pipeline never reads.
///
/// Stage names that do not appear in the table are returned unchanged so that
/// user-defined profiles can target arbitrary role names directly.
fn stage_to_role(stage: &str) -> &str {
    match stage {
        "clarify" | "plan" | "analyze" => "proposer",
        "specify" | "tasks" => "drafter",
        "execute" => "executor",
        other => other,
    }
}

/// Role bindings keyed by role name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoleBindings(HashMap<String, String>);

impl RoleBindings {
    /// Returns the model name for a role.
    pub fn get(&self, role: &str) -> Option<&str> {
        self.0.get(role).map(String::as_str)
    }

    /// Returns all role bindings.
    pub fn as_map(&self) -> &HashMap<String, String> {
        &self.0
    }

    fn contains_key(&self, role: &str) -> bool {
        self.0.contains_key(role)
    }
}

/// Configuration for external and in-process tools.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tools {
    speckit: Speckit,
    specify: Specify,
    assay: Assay,
    substrate: Substrate,
    copilot: Copilot,
    claude: Claude,
    git: Git,
    foreman: Foreman,
    code_review: CodeReview,
    output_compression: OutputCompression,
    roughneck: Roughneck,
}

impl Tools {
    /// Returns speckit configuration.
    pub fn speckit(&self) -> &Speckit {
        &self.speckit
    }

    /// Returns spec-provider configuration (`tools.specify`).
    pub fn specify(&self) -> &Specify {
        &self.specify
    }

    /// Returns assay configuration.
    pub fn assay(&self) -> &Assay {
        &self.assay
    }

    /// Returns substrate configuration.
    pub fn substrate(&self) -> &Substrate {
        &self.substrate
    }

    /// Returns Copilot dispatch configuration.
    pub fn copilot(&self) -> &Copilot {
        &self.copilot
    }

    /// Returns Claude Code dispatch configuration (T015).
    pub fn claude(&self) -> &Claude {
        &self.claude
    }

    /// Returns git configuration.
    pub fn git(&self) -> &Git {
        &self.git
    }

    /// Returns foreman runtime configuration (T012).
    pub fn foreman(&self) -> &Foreman {
        &self.foreman
    }

    /// Returns code-review configuration.
    pub fn code_review(&self) -> &CodeReview {
        &self.code_review
    }

    /// Returns output-compression configuration.
    pub fn output_compression(&self) -> &OutputCompression {
        &self.output_compression
    }

    /// Returns roughneck (LLM output compression) configuration.
    pub fn roughneck(&self) -> &Roughneck {
        &self.roughneck
    }
}

impl Default for Tools {
    fn default() -> Self {
        Self {
            speckit: Speckit {
                enabled: true,
                version: ">=0.4.0".to_owned(),
            },
            specify: Specify::default(),
            assay: Assay::default(),
            substrate: Substrate {
                backend: SubstrateBackendKind::Native,
                mode: SubstrateMode::Solo,
            },
            copilot: Copilot::default(),
            claude: Claude::default(),
            git: Git::default(),
            foreman: Foreman::default(),
            code_review: CodeReview::default(),
            output_compression: OutputCompression::default(),
            roughneck: Roughneck::default(),
        }
    }
}

/// Code-review configuration for pre-PR adversarial review.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodeReview {
    enabled: bool,
    role: String,
    rounds: u32,
    base_branch: String,
}

impl CodeReview {
    /// Returns whether code review is enabled.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Returns the reviewer role.
    pub fn role(&self) -> &str {
        &self.role
    }

    /// Maximum remediation rounds the hand should attempt.
    pub fn rounds(&self) -> u32 {
        self.rounds
    }

    /// Base branch to diff against.
    pub fn base_branch(&self) -> &str {
        &self.base_branch
    }
}

impl Default for CodeReview {
    fn default() -> Self {
        Self {
            enabled: false,
            role: "reviewer".to_owned(),
            rounds: 2,
            base_branch: "main".to_owned(),
        }
    }
}

/// Output-compression configuration.
///
/// When enabled, Derrick scrubs subprocess stdout/stderr through
/// `derrick-scrub` rules before writing to step logs.  This reduces the
/// bytes that flow into LLM context windows on subsequent steps.
/// Enabled by default.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputCompression {
    enabled: bool,
}

impl OutputCompression {
    /// Returns whether output compression is enabled (default: `true`).
    pub fn enabled(&self) -> bool {
        self.enabled
    }
}

impl Default for OutputCompression {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Roughneck (LLM output compression via prompt injection) configuration.
///
/// When enabled, Derrick prepends terse-response instructions to each
/// pipeline step's prompt to cut output tokens. Three intensity levels are
/// available: `lite`, `full`, `ultra`. See `derrick-roughneck`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Roughneck {
    enabled: bool,
    level: String,
    compress_memory: bool,
}

impl Roughneck {
    /// Returns whether roughneck is enabled (default: `true`).
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Returns the configured intensity level (`lite` | `full` | `ultra`).
    pub fn level(&self) -> &str {
        &self.level
    }

    /// Returns whether memory entries should be compressed on read.
    pub fn compress_memory(&self) -> bool {
        self.compress_memory
    }
}

impl Default for Roughneck {
    fn default() -> Self {
        Self {
            enabled: true,
            level: "full".to_owned(),
            compress_memory: false,
        }
    }
}

/// Foreman runtime configuration (T012). Sourced from `tools.foreman` in
/// `derrick.yaml`; all fields are optional and fall back to compiled-in
/// defaults documented on [`Foreman`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Foreman {
    poll_interval: std::time::Duration,
    in_review_ttl: std::time::Duration,
    hand_ttl: std::time::Duration,
    worktree_ttl: std::time::Duration,
    exit_when_idle: bool,
}

impl Foreman {
    /// Time between `tick()` iterations when running attached. Default 10s.
    pub fn poll_interval(&self) -> std::time::Duration {
        self.poll_interval
    }

    /// Maximum age of an `InReview` ticket before the verifier eagerly
    /// re-checks it. Default 24h.
    pub fn in_review_ttl(&self) -> std::time::Duration {
        self.in_review_ttl
    }

    /// Maximum gap since a hand's last heartbeat before the cleanup pass
    /// releases its tickets. Default 30m.
    pub fn hand_ttl(&self) -> std::time::Duration {
        self.hand_ttl
    }

    /// Maximum age of an open worktree row before the cleanup pass prunes
    /// it. Default 24h.
    pub fn worktree_ttl(&self) -> std::time::Duration {
        self.worktree_ttl
    }

    /// When true, `run_attached` returns after the first tick that produced
    /// no actions. Default `false`.
    pub fn exit_when_idle(&self) -> bool {
        self.exit_when_idle
    }
}

impl Default for Foreman {
    fn default() -> Self {
        Self {
            poll_interval: std::time::Duration::from_secs(10),
            in_review_ttl: std::time::Duration::from_secs(60 * 60 * 24),
            hand_ttl: std::time::Duration::from_secs(60 * 30),
            worktree_ttl: std::time::Duration::from_secs(60 * 60 * 24),
            exit_when_idle: false,
        }
    }
}

/// Speckit integration settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Speckit {
    enabled: bool,
    version: String,
}

impl Speckit {
    /// Returns whether speckit integration is enabled.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Returns the expected speckit version requirement.
    pub fn version(&self) -> &str {
        &self.version
    }
}

/// Which provider produces the spec/plan/tasks artifacts for the `specify`,
/// `plan`, and `tasks` pipeline steps when those steps are declared **bare**
/// (no `host`/`command`/`runner`). Selected via `tools.specify.provider`.
///
/// This is distinct from [`Speckit`] (`tools.speckit`), which governs speckit
/// version detection and PATH checks. `tools.specify` chooses the dispatch
/// path; `tools.speckit` still describes the speckit toolchain itself.
///
/// Default is [`SpecProviderKind::Speckit`], preserving the historical
/// behaviour: bare spec steps delegate to the speckit host CLI exactly as the
/// explicit `host: claude` + `command: "/speckit.specify …"` steps do.
/// `Native` and `Import` are config-accepted in Phase 1 but their dispatch
/// arms return a "not yet available" error until Phases 2/3 land.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum SpecProviderKind {
    /// Delegate to the speckit host CLI (the historical behaviour).
    #[default]
    Speckit,
    /// Derrick-native spec generation (Phase 2 — not yet wired).
    Native,
    /// Import an externally-authored spec (Phase 3 — not yet wired).
    Import,
}

/// Downstream-phase mode for the `import` provider. Controls how `plan` and
/// `tasks` are produced once a spec has been imported. Defaults to
/// [`DownstreamMode::Native`].
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum DownstreamMode {
    /// Produce the artifact with derrick-native generation.
    #[default]
    Native,
    /// Delegate the artifact to the speckit host CLI.
    Speckit,
    /// Import the artifact from an external source.
    Import,
}

/// Spec-provider configuration block (`tools.specify`).
///
/// An omitted `tools.specify` block finalizes to `provider: Speckit` via
/// [`Default`], which is the back-compat path — existing configs that never
/// mention `tools.specify` keep dispatching bare spec steps through speckit.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Specify {
    provider: SpecProviderKind,
    import: ImportConfig,
}

impl Specify {
    /// Returns the configured spec provider.
    pub fn provider(&self) -> SpecProviderKind {
        self.provider
    }

    /// Returns the import-provider sub-configuration.
    pub fn import(&self) -> &ImportConfig {
        &self.import
    }
}

/// Import-provider sub-configuration (`tools.specify.import`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImportConfig {
    source: Option<String>,
    plan: DownstreamMode,
    tasks: DownstreamMode,
}

impl ImportConfig {
    /// Returns the optional import source (path or locator). Interpreted by the
    /// import provider in Phase 3.
    pub fn source(&self) -> Option<&str> {
        self.source.as_deref()
    }

    /// Returns the downstream mode for the `plan` phase.
    pub fn plan(&self) -> DownstreamMode {
        self.plan
    }

    /// Returns the downstream mode for the `tasks` phase.
    pub fn tasks(&self) -> DownstreamMode {
        self.tasks
    }
}

/// Assay review configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Assay {
    enabled: bool,
    role: String,
    reviewers: Vec<String>,
    rounds: String,
    strict: bool,
    auto_execute: bool,
    on_split: OnSplit,
}

impl Assay {
    /// Returns whether assay is enabled.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Returns the role that runs assay.
    pub fn role(&self) -> &str {
        &self.role
    }

    /// Returns the reviewer role list.
    pub fn reviewers(&self) -> &[String] {
        &self.reviewers
    }

    /// Returns the opaque rounds template.
    pub fn rounds(&self) -> &str {
        &self.rounds
    }

    /// Returns whether strict assay mode is enabled.
    pub fn strict(&self) -> bool {
        self.strict
    }

    /// Returns whether assay automatically proceeds to implementation
    /// without a human verdict gate.
    pub fn auto_execute(&self) -> bool {
        self.auto_execute
    }

    /// Returns the split-verdict policy.
    pub fn on_split(&self) -> OnSplit {
        self.on_split
    }
}

impl Default for Assay {
    fn default() -> Self {
        Self {
            enabled: false,
            role: "reviewer".to_owned(),
            reviewers: vec!["reviewer".to_owned()],
            rounds: "10".to_owned(),
            strict: false,
            auto_execute: false,
            on_split: OnSplit::Reject,
        }
    }
}

/// Split-verdict policy for multi-reviewer assay.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum OnSplit {
    /// Reject the plan when reviewers split.
    Reject,
    /// Ask a human to decide when reviewers split.
    Human,
    /// Use majority vote when reviewers split.
    Majority,
}

/// Substrate execution settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Substrate {
    backend: SubstrateBackendKind,
    mode: SubstrateMode,
}

impl Substrate {
    /// Returns the substrate backend kind.
    pub fn backend(&self) -> SubstrateBackendKind {
        self.backend
    }

    /// Returns the operating mode.
    pub fn mode(&self) -> SubstrateMode {
        self.mode
    }
}

/// Substrate backend kind.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SubstrateBackendKind {
    /// Native SQLite-backed substrate.
    Native,
    /// Disable substrate persistence.
    None,
}

/// Derrick operating mode.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SubstrateMode {
    /// Single-user local operation.
    Solo,
    /// Dispatch work to Copilot while the user drives orchestration.
    Copilot,
    /// Multi-hand execution with a foreman loop.
    Crew,
}

/// Copilot dispatch configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Copilot {
    enabled: bool,
    agent_identity: String,
    poll_interval: std::time::Duration,
    poll_timeout: std::time::Duration,
}

impl Copilot {
    /// Returns whether Copilot dispatch is enabled.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Returns the identity used for Copilot hands.
    pub fn agent_identity(&self) -> &str {
        &self.agent_identity
    }

    /// Interval between successive PR polls. Default 30s.
    pub fn poll_interval(&self) -> std::time::Duration {
        self.poll_interval
    }

    /// Maximum wall-clock duration the poll loop will wait for a PR before
    /// giving up. Default 10 minutes.
    pub fn poll_timeout(&self) -> std::time::Duration {
        self.poll_timeout
    }
}

impl Default for Copilot {
    fn default() -> Self {
        Self {
            enabled: false,
            agent_identity: "derrick-hand".to_owned(),
            poll_interval: std::time::Duration::from_secs(30),
            poll_timeout: std::time::Duration::from_secs(60 * 10),
        }
    }
}

/// Claude Code hand dispatch configuration (T015).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Claude {
    enabled: bool,
    agent_identity: String,
    auto_dispatch: bool,
    poll_interval: std::time::Duration,
    poll_timeout: std::time::Duration,
    queue_dir: String,
}

impl Claude {
    /// Returns whether Claude Code dispatch is enabled.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Returns the identity used for Claude hands.
    pub fn agent_identity(&self) -> &str {
        &self.agent_identity
    }

    /// When true, spawn `claude --print` autonomously. Default false; an
    /// operator must invoke the queue file manually.
    pub fn auto_dispatch(&self) -> bool {
        self.auto_dispatch
    }

    /// Interval between heartbeats while a dispatched ticket runs. Default 60s.
    pub fn poll_interval(&self) -> std::time::Duration {
        self.poll_interval
    }

    /// Timeout before releasing an unresponsive Claude hand. Default 60 minutes.
    pub fn poll_timeout(&self) -> std::time::Duration {
        self.poll_timeout
    }

    /// Directory (relative to repo root) where queue files are written.
    /// Default `.derrick/queue`.
    pub fn queue_dir(&self) -> &str {
        &self.queue_dir
    }
}

impl Default for Claude {
    fn default() -> Self {
        Self {
            enabled: false,
            agent_identity: "derrick-claude-hand".to_owned(),
            auto_dispatch: false,
            poll_interval: std::time::Duration::from_secs(60),
            poll_timeout: std::time::Duration::from_secs(60 * 60),
            queue_dir: ".derrick/queue".to_owned(),
        }
    }
}

/// Git and PR stacking configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Git {
    stacking: Stacking,
    branch_prefix: String,
}

impl Git {
    /// Returns stacking configuration.
    pub fn stacking(&self) -> &Stacking {
        &self.stacking
    }

    /// Returns the branch prefix used by Copilot dispatches.
    ///
    /// Defaults to `"derrick"`, producing branch names of the form
    /// `derrick/<batch>/<ticket-id>` (see D19/§8.3). Override via
    /// `tools.git.branch_prefix` in `derrick.yaml`.
    pub fn branch_prefix(&self) -> &str {
        &self.branch_prefix
    }
}

impl Default for Git {
    fn default() -> Self {
        Self {
            stacking: Stacking::default(),
            branch_prefix: "derrick".to_owned(),
        }
    }
}

/// Pull-request stacking configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stacking {
    backend: StackBackendKind,
    branch_pattern: String,
    auto_restack_on_merge: bool,
    force_push: ForcePush,
    auto_pr: bool,
    draft: bool,
}

impl Stacking {
    /// Returns the configured stacking backend.
    pub fn backend(&self) -> StackBackendKind {
        self.backend
    }

    /// Returns the branch naming pattern.
    pub fn branch_pattern(&self) -> &str {
        &self.branch_pattern
    }

    /// Returns whether derrick should auto-restack after merges.
    pub fn auto_restack_on_merge(&self) -> bool {
        self.auto_restack_on_merge
    }

    /// Returns the force-push policy.
    pub fn force_push(&self) -> ForcePush {
        self.force_push
    }

    /// Returns whether derrick should open PRs automatically.
    pub fn auto_pr(&self) -> bool {
        self.auto_pr
    }

    /// Returns whether automatically opened PRs should be drafts.
    pub fn draft(&self) -> bool {
        self.draft
    }
}

impl Default for Stacking {
    fn default() -> Self {
        Self {
            backend: StackBackendKind::None,
            branch_pattern: "derrick/{{batch}}/{{ticket_id}}".to_owned(),
            auto_restack_on_merge: true,
            force_push: ForcePush::WithLease,
            auto_pr: false,
            draft: false,
        }
    }
}

/// PR stacking backend kind.
///
/// Per D72 derrick owns its stacking technology: the native backend is the only
/// engine. The third-party Graphite (`gt`) and git-spice (`gs`) adapters were
/// removed; the `StackBackend` trait remains as the §8.6 extension seam.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum StackBackendKind {
    /// Disable stacking.
    None,
    /// Use derrick's native stack backend (plain `git` + `gh`).
    Native,
}

/// Force-push safety policy.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ForcePush {
    /// Use `--force-with-lease`.
    WithLease,
    /// Do not force-push.
    Off,
}

/// One pipeline step in the `/drill` flow.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PipelineStep {
    id: String,
    role: Option<String>,
    runner: Option<Runner>,
    host: Option<Host>,
    command: Option<String>,
    inputs: Vec<String>,
    skippable: bool,
    default_skip: bool,
    prompt: Option<String>,
    rounds: Option<String>,
    on_reject: Option<OnReject>,
    on_failure: Option<OnFailure>,
    poll_interval: Option<String>,
    batch: Option<String>,
    executor_role: Option<String>,
    parallel_group: Option<String>,
}

impl PipelineStep {
    /// Returns the step identifier.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Returns the optional role binding.
    pub fn role(&self) -> Option<&str> {
        self.role.as_deref()
    }

    /// Returns the optional in-process or human runner.
    pub fn runner(&self) -> Option<Runner> {
        self.runner
    }

    /// Returns the optional host CLI for role-backed steps.
    pub fn host(&self) -> Option<Host> {
        self.host
    }

    /// Returns the optional command template.
    pub fn command(&self) -> Option<&str> {
        self.command.as_deref()
    }

    /// Returns the opaque input templates.
    pub fn inputs(&self) -> &[String] {
        &self.inputs
    }

    /// Returns whether the step is user-skippable.
    pub fn skippable(&self) -> bool {
        self.skippable
    }

    /// Returns whether the step is skipped by default.
    pub fn default_skip(&self) -> bool {
        self.default_skip
    }

    /// Returns the optional human prompt.
    pub fn prompt(&self) -> Option<&str> {
        self.prompt.as_deref()
    }

    /// Returns the optional assay rounds template.
    pub fn rounds(&self) -> Option<&str> {
        self.rounds.as_deref()
    }

    /// Returns the optional rejection policy.
    pub fn on_reject(&self) -> Option<OnReject> {
        self.on_reject
    }

    /// Returns the optional failure policy.
    pub fn on_failure(&self) -> Option<OnFailure> {
        self.on_failure
    }

    /// Returns the optional poll interval.
    pub fn poll_interval(&self) -> Option<&str> {
        self.poll_interval.as_deref()
    }

    /// Returns the optional batch template.
    pub fn batch(&self) -> Option<&str> {
        self.batch.as_deref()
    }

    /// Returns the optional executor role.
    pub fn executor_role(&self) -> Option<&str> {
        self.executor_role.as_deref()
    }

    /// Returns the optional parallel group.
    pub fn parallel_group(&self) -> Option<&str> {
        self.parallel_group.as_deref()
    }
}

/// Pipeline runner kind for non-role steps.
#[derive(Copy, Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Runner {
    /// Run derrick in-process logic.
    Derrick,
    /// Ask a human to make a decision.
    Human,
    /// Run a bash command.
    Bash,
    /// Run the Claude host.
    Claude,
    /// Run the Codex host.
    Codex,
    /// Run the Copilot host.
    Copilot,
}

/// Host CLI for role-backed steps.
#[derive(Copy, Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Host {
    /// Claude CLI.
    Claude,
    /// Codex CLI.
    Codex,
    /// Copilot CLI.
    Copilot,
    /// OpenCode CLI.
    Opencode,
    /// Aider CLI.
    Aider,
}

/// Assay rejection policy.
#[derive(Copy, Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum OnReject {
    /// Halt the pipeline.
    Halt,
    /// Warn and continue.
    Warn,
}

/// Dispatch failure policy.
#[derive(Copy, Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum OnFailure {
    /// Pause for intervention.
    Pause,
    /// Retry the failed work.
    Retry,
    /// Abort the pipeline.
    Abort,
}

/// Project-specific guardrails surfaced into prompts and checkpoints.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Guardrails {
    constitution_path: PathBuf,
    forbid_paths: Vec<String>,
    required_labels: Vec<String>,
}

impl Guardrails {
    /// Returns the constitution path.
    pub fn constitution_path(&self) -> &Path {
        &self.constitution_path
    }

    /// Returns paths that features may not touch.
    pub fn forbid_paths(&self) -> &[String] {
        &self.forbid_paths
    }

    /// Returns labels every ticket must carry.
    pub fn required_labels(&self) -> &[String] {
        &self.required_labels
    }
}

/// Parallelism budgets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Parallelism {
    batch_max: u32,
    step_max: u32,
    assay_max: u32,
}

impl Parallelism {
    /// Returns the maximum concurrent hands per batch.
    pub fn batch_max(&self) -> u32 {
        self.batch_max
    }

    /// Returns the maximum parallel work inside a pipeline step.
    pub fn step_max(&self) -> u32 {
        self.step_max
    }

    /// Returns the maximum concurrent assay reviewers.
    pub fn assay_max(&self) -> u32 {
        self.assay_max
    }
}

/// Derrick state storage configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateConfig {
    dir: PathBuf,
    log_runs: bool,
    worktree_root: PathBuf,
}

impl StateConfig {
    /// Returns derrick's state directory inside the repo.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Returns whether run logs should be written.
    pub fn log_runs(&self) -> bool {
        self.log_runs
    }

    /// Returns the root directory for per-run worktrees.
    pub fn worktree_root(&self) -> &Path {
        &self.worktree_root
    }
}

fn read_layer(path: &Path) -> Result<ConfigLayer, ConfigError> {
    let source = fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;

    let mut layer: ConfigLayer =
        serde_yaml::from_str(&source).map_err(|source: serde_yaml::Error| {
            let line = source.location().map_or(0, |location| location.line());
            ConfigError::Syntax {
                path: path.to_path_buf(),
                line,
                message: source.to_string(),
            }
        })?;
    // Expand `ai.preset` here, at the declaring layer, so it participates in the
    // cross-layer merge with the right precedence (D79).
    layer.apply_preset()?;
    Ok(layer)
}

/// A preset's generated models, as `(alias, runtime, model)` tuples.
type PresetModels = Vec<(&'static str, &'static str, &'static str)>;
/// A preset's generated role bindings, as `(role, alias)` tuples.
type PresetRoles = Vec<(&'static str, &'static str)>;

/// Returns the `(models, roles)` a named preset generates (D79).
///
/// Presets are only a starting point — they expand into ordinary config the
/// user can edit.
fn preset_definition(preset: &str) -> Result<(PresetModels, PresetRoles), ConfigError> {
    // Common role wiring shared by the single-runtime presets.
    let roles_strong_fast = vec![
        ("proposer", "strong"),
        ("drafter", "fast"),
        ("reviewer", "strong"),
        ("executor", "executor"),
        ("summariser", "fast"),
    ];
    let models = match preset {
        "cli-defaults" => {
            return Ok((
                vec![
                    ("fast", "claude-cli", "claude-sonnet-4-6"),
                    ("strong", "claude-cli", "claude-opus-4-8"),
                    ("reviewer", "codex-cli", "gpt-5.5"),
                    ("executor", "copilot-cli", "auto"),
                ],
                vec![
                    ("proposer", "strong"),
                    ("drafter", "fast"),
                    ("reviewer", "reviewer"),
                    ("executor", "executor"),
                    ("summariser", "fast"),
                ],
            ));
        }
        "claude-only" => vec![
            ("fast", "claude-cli", "claude-sonnet-4-6"),
            ("strong", "claude-cli", "claude-opus-4-8"),
            ("executor", "claude-cli", "claude-opus-4-8"),
        ],
        "codex-only" => vec![
            // gpt-5.4-mini is the codex light-tier id in the curated catalogue;
            // using it keeps freshly generated configs WARN-free.
            ("fast", "codex-cli", "gpt-5.4-mini"),
            ("strong", "codex-cli", "gpt-5.5"),
            ("executor", "codex-cli", "gpt-5.5"),
        ],
        "local-only" => vec![
            ("fast", "ollama", "qwen2.5-coder:32b"),
            ("strong", "ollama", "qwen2.5-coder:32b"),
            ("executor", "ollama", "qwen2.5-coder:32b"),
        ],
        other => {
            return validation(format!(
                "ai.preset: {other:?} must be one of cli-defaults | claude-only | codex-only | local-only"
            ));
        }
    };
    Ok((models, roles_strong_fast))
}

/// The set of presets `ai.preset` accepts (D79). Exposed for the init wizard.
pub const PRESETS: [&str; 4] = ["cli-defaults", "claude-only", "codex-only", "local-only"];

fn user_config_path() -> Option<PathBuf> {
    env::var_os("HOME").map(|home| PathBuf::from(home).join(".derrick/config.yaml"))
}

fn validation<T>(message: String) -> Result<T, ConfigError> {
    Err(ConfigError::Validation(message))
}

fn required<T>(value: Option<T>, path: &str) -> Result<T, ConfigError> {
    value.ok_or_else(|| ConfigError::Validation(format!("{path}: missing required field")))
}

fn validate_roles(roles: &RoleBindings, models: &ModelRegistry) -> Result<(), ConfigError> {
    for (role, model) in roles.as_map() {
        if !models.contains_key(model) {
            return validation(format!(
                "roles.{role}: references unknown model {model:?} in models"
            ));
        }
    }
    Ok(())
}

/// Returns whether `step` is a *bare* spec step — one of `specify`/`plan`/
/// `tasks` with no `role`, `runner`, `host`, or `command`. Such steps are
/// dispatched through the spec-provider seam (`tools.specify.provider`).
fn is_bare_spec_step(step: &PipelineStep) -> bool {
    matches!(step.id.as_str(), "specify" | "plan" | "tasks")
        && step.role.is_none()
        && step.runner.is_none()
        && step.host.is_none()
        && step.command.is_none()
}

fn validate_pipeline(steps: &[PipelineStep], roles: &RoleBindings) -> Result<(), ConfigError> {
    for step in steps {
        match (&step.role, step.runner) {
            (Some(_), Some(_)) => {
                return validation(format!(
                    "pipeline.{}: role and runner are mutually exclusive",
                    step.id
                ));
            }
            // A *bare* spec step (`specify`/`plan`/`tasks` with no
            // role/runner/host/command) is valid: it dispatches through the
            // spec-provider seam (`tools.specify.provider`). See DESIGN.md §5.3.
            (None, None) if is_bare_spec_step(step) => {
                // Accepted — dispatched by the spec-provider seam.
            }
            (None, None) => {
                return validation(format!(
                    "pipeline.{}: either role or runner is required",
                    step.id
                ));
            }
            (Some(role), None) if !roles.contains_key(role) => {
                return validation(format!(
                    "pipeline.{}.role: references unknown role {role:?} in roles",
                    step.id
                ));
            }
            (None, Some(Runner::Human)) if step.prompt.is_none() => {
                return validation(format!(
                    "pipeline.{}.prompt: runner human steps require prompt",
                    step.id
                ));
            }
            _ => {}
        }

        if let Some(executor_role) = &step.executor_role {
            if !roles.contains_key(executor_role) {
                return validation(format!(
                    "pipeline.{}.executor_role: references unknown role {executor_role:?} in roles",
                    step.id
                ));
            }
        }
    }
    Ok(())
}

fn validate_code_review(cr: &CodeReview, roles: &RoleBindings) -> Result<(), ConfigError> {
    if cr.enabled && !roles.contains_key(&cr.role) {
        return validation(format!(
            "tools.code_review.role: references unknown role {:?} in roles",
            cr.role
        ));
    }
    Ok(())
}

fn validate_assay(assay: &Assay, roles: &RoleBindings) -> Result<(), ConfigError> {
    if assay.enabled {
        if !roles.contains_key(&assay.role) {
            return validation(format!(
                "tools.assay.role: references unknown role {:?} in roles",
                assay.role
            ));
        }
        if assay.reviewers.is_empty() {
            return validation(
                "tools.assay.reviewers: must be non-empty when assay is enabled".to_owned(),
            );
        }
        for reviewer in &assay.reviewers {
            if !roles.contains_key(reviewer) {
                return validation(format!(
                    "tools.assay.reviewers: references unknown role {reviewer:?} in roles"
                ));
            }
        }
        if assay.on_split == OnSplit::Majority && assay.reviewers.len() % 2 == 0 {
            return validation(
                "tools.assay.on_split: majority requires an odd number of reviewers; use reject as the safe fallback".to_owned(),
            );
        }
    }
    Ok(())
}

fn validate_site_prefix(prefix: &str) -> Result<(), ConfigError> {
    if (1..=6).contains(&prefix.len()) && prefix.bytes().all(|byte| byte.is_ascii_lowercase()) {
        Ok(())
    } else {
        validation("site.prefix: must match ^[a-z]{1,6}$".to_owned())
    }
}

fn validate_parallelism(parallelism: &Parallelism) -> Result<(), ConfigError> {
    if !(1..=64).contains(&parallelism.batch_max) {
        return validation("parallelism.batch_max: must be >= 1 and <= 64".to_owned());
    }
    if !(1..=64).contains(&parallelism.step_max) {
        return validation("parallelism.step_max: must be >= 1 and <= 64".to_owned());
    }
    Ok(())
}

fn validate_state_dir(dir: &Path) -> Result<(), ConfigError> {
    if dir.is_absolute() {
        validation("state.dir: must be a relative path".to_owned())
    } else {
        Ok(())
    }
}

fn parse_substrate_backend(value: &str) -> Result<SubstrateBackendKind, ConfigError> {
    match value {
        "native" => Ok(SubstrateBackendKind::Native),
        "none" => Ok(SubstrateBackendKind::None),
        "gastown" => validation(
            "tools.substrate.backend: \"gastown\" is not allowed in v1; gastown backend ships in a future version behind the Substrate trait — see DESIGN.md §8.5".to_owned(),
        ),
        other => validation(format!(
            "tools.substrate.backend: {other:?} must be one of native | none"
        )),
    }
}

fn parse_substrate_mode(value: &str) -> Result<SubstrateMode, ConfigError> {
    match value {
        "solo" => Ok(SubstrateMode::Solo),
        "copilot" => Ok(SubstrateMode::Copilot),
        "crew" => Ok(SubstrateMode::Crew),
        other => validation(format!(
            "tools.substrate.mode: {other:?} must be one of solo | copilot | crew"
        )),
    }
}

fn parse_on_split(value: &str) -> Result<OnSplit, ConfigError> {
    match value {
        "reject" => Ok(OnSplit::Reject),
        "human" => Ok(OnSplit::Human),
        "majority" => Ok(OnSplit::Majority),
        other => validation(format!(
            "tools.assay.on_split: {other:?} must be one of reject | human | majority"
        )),
    }
}

fn parse_stack_backend(value: &str) -> Result<StackBackendKind, ConfigError> {
    match value {
        "none" => Ok(StackBackendKind::None),
        "native" => Ok(StackBackendKind::Native),
        "graphite" | "git-spice" => validation(format!(
            "tools.git.stacking.backend: stacking backend {value:?} was removed (D72) — \
             derrick's native stacking is the supported engine; \
             set tools.git.stacking.backend: native"
        )),
        other => validation(format!(
            "tools.git.stacking.backend: {other:?} must be one of none | native"
        )),
    }
}

fn parse_spec_provider(value: &str) -> Result<SpecProviderKind, ConfigError> {
    match value {
        "speckit" => Ok(SpecProviderKind::Speckit),
        "native" => Ok(SpecProviderKind::Native),
        "import" => Ok(SpecProviderKind::Import),
        other => validation(format!(
            "tools.specify.provider: {other:?} must be one of speckit | native | import"
        )),
    }
}

fn parse_downstream_mode(value: &str, path: &str) -> Result<DownstreamMode, ConfigError> {
    match value {
        "native" => Ok(DownstreamMode::Native),
        "speckit" => Ok(DownstreamMode::Speckit),
        "import" => Ok(DownstreamMode::Import),
        other => validation(format!(
            "{path}: {other:?} must be one of native | speckit | import"
        )),
    }
}

fn parse_force_push(value: &str) -> Result<ForcePush, ConfigError> {
    match value {
        "with-lease" => Ok(ForcePush::WithLease),
        "off" => Ok(ForcePush::Off),
        other => validation(format!(
            "tools.git.stacking.force_push: {other:?} must be one of with-lease | off"
        )),
    }
}

/// A profile stage binding: a single alias or a list of aliases (D80).
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
enum ProfileBindingLayer {
    Single(String),
    Multi(Vec<String>),
}

/// A profile's stage→binding map (D80). Stage names are user-defined, so this
/// is a bare `HashMap` without `deny_unknown_fields`.
type ProfileLayer = HashMap<String, ProfileBindingLayer>;

/// Deserialize target for the `budgets:` section (D80).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct BudgetConfigLayer {
    per_ticket: Option<BudgetLayer>,
    daily: Option<BudgetLayer>,
    monthly: Option<BudgetLayer>,
}

/// Deserialize target for a single budget scope (D80).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BudgetLayer {
    max_cost: f64,
}

impl BudgetConfigLayer {
    fn merge(&mut self, other: Self) {
        merge_nested(&mut self.per_ticket, other.per_ticket, BudgetLayer::merge);
        merge_nested(&mut self.daily, other.daily, BudgetLayer::merge);
        merge_nested(&mut self.monthly, other.monthly, BudgetLayer::merge);
    }
}

impl BudgetLayer {
    fn merge(&mut self, other: Self) {
        self.max_cost = other.max_cost;
    }
}

fn finalize_profile(layer: ProfileLayer) -> Profile {
    let mut stages = HashMap::new();
    for (stage, binding) in layer {
        match binding {
            ProfileBindingLayer::Single(alias) => {
                stages.insert(stage, vec![alias]);
            }
            ProfileBindingLayer::Multi(aliases) => {
                stages.insert(stage, aliases);
            }
        }
    }
    Profile {
        stages,
        ci: false,
        description: None,
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigLayer {
    version: Option<u32>,
    site: Option<SiteLayer>,
    #[serde(default)]
    ai: Option<AiLayer>,
    models: Option<HashMap<String, ModelDefLayer>>,
    roles: Option<HashMap<String, String>>,
    #[serde(default)]
    stages: Option<HashMap<String, StageBindingLayer>>,
    tools: Option<ToolsLayer>,
    pipeline: Option<Vec<PipelineStepLayer>>,
    guardrails: Option<GuardrailsLayer>,
    parallelism: Option<ParallelismLayer>,
    state: Option<StateLayer>,
    #[serde(default)]
    profiles: Option<HashMap<String, ProfileLayer>>,
    #[serde(default)]
    budgets: Option<BudgetConfigLayer>,
    #[serde(default)]
    default_profile: Option<String>,
}

impl ConfigLayer {
    fn merge(&mut self, other: Self) {
        if other.version.is_some() {
            self.version = other.version;
        }
        merge_nested(&mut self.site, other.site, SiteLayer::merge);
        merge_scalar(&mut self.ai, other.ai);
        merge_map(&mut self.models, other.models);
        merge_map(&mut self.roles, other.roles);
        merge_map(&mut self.stages, other.stages);
        merge_nested(&mut self.tools, other.tools, ToolsLayer::merge);
        if other.pipeline.is_some() {
            self.pipeline = other.pipeline;
        }
        merge_nested(
            &mut self.guardrails,
            other.guardrails,
            GuardrailsLayer::merge,
        );
        merge_nested(
            &mut self.parallelism,
            other.parallelism,
            ParallelismLayer::merge,
        );
        merge_nested(&mut self.state, other.state, StateLayer::merge);
        merge_map(&mut self.profiles, other.profiles);
        merge_nested(&mut self.budgets, other.budgets, BudgetConfigLayer::merge);
        merge_scalar(&mut self.default_profile, other.default_profile);
    }

    /// Expands an `ai.preset` into concrete `models` and `roles` entries (D79).
    ///
    /// Applied at the layer that declares the preset (before cross-layer merge)
    /// so explicitly-configured `models`/`roles` keys always win and preset
    /// `roles` override lower layers via the normal merge. Inserts only keys not
    /// already present in this layer.
    fn apply_preset(&mut self) -> Result<(), ConfigError> {
        let Some(preset) = self.ai.as_ref().and_then(|ai| ai.preset.as_deref()) else {
            return Ok(());
        };
        let (models, roles) = preset_definition(preset)?;
        let model_map = self.models.get_or_insert_with(HashMap::new);
        for (alias, runtime, model) in models {
            model_map
                .entry(alias.to_owned())
                .or_insert_with(|| ModelDefLayer::from_runtime(runtime, model));
        }
        let role_map = self.roles.get_or_insert_with(HashMap::new);
        for (role, alias) in roles {
            role_map
                .entry(role.to_owned())
                .or_insert_with(|| alias.to_owned());
        }
        Ok(())
    }

    fn finalize(self) -> Result<Config, ConfigError> {
        let guardrails = required(self.guardrails, "guardrails")?.finalize();
        let pipeline = self
            .pipeline
            .unwrap_or_default()
            .into_iter()
            .map(PipelineStepLayer::finalize)
            .collect::<Result<Vec<_>, _>>()?;

        let models = ModelRegistry(
            required(self.models, "models")?
                .into_iter()
                .map(|(name, model)| Ok((name, model.finalize()?)))
                .collect::<Result<HashMap<_, _>, ConfigError>>()?,
        );

        // Fold stage bindings into role bindings and, for a multi-model `assay`
        // stage, into the assay reviewer list — so this must run before the
        // tools (assay) layer is finalized.
        let mut roles = required(self.roles, "roles")?;
        let mut tools_layer = self.tools.unwrap_or_default();
        let stage_requirements = apply_stages(
            self.stages.unwrap_or_default(),
            &mut roles,
            &mut tools_layer,
        )?;
        let tools = tools_layer.finalize()?;

        Ok(Config {
            version: required(self.version, "version")?,
            site: required(self.site, "site")?.finalize()?,
            models,
            roles: RoleBindings(roles),
            tools,
            pipeline,
            guardrails,
            parallelism: required(self.parallelism, "parallelism")?.finalize()?,
            state: required(self.state, "state")?.finalize()?,
            stage_requirements,
            profiles: ProfileRegistry(
                self.profiles
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(name, layer)| (name, finalize_profile(layer)))
                    .collect(),
            ),
            budgets: self
                .budgets
                .map(|b| -> Result<BudgetConfig, ConfigError> {
                    fn vb(b: BudgetLayer, scope: &'static str) -> Result<Budget, ConfigError> {
                        if !b.max_cost.is_finite() || b.max_cost < 0.0 {
                            return Err(ConfigError::Validation(format!(
                                "budgets.{scope}.max_cost must be a finite non-negative number \
                                 (got {})",
                                b.max_cost
                            )));
                        }
                        Ok(Budget {
                            max_cost: b.max_cost,
                        })
                    }
                    Ok(BudgetConfig {
                        per_ticket: b.per_ticket.map(|b| vb(b, "per_ticket")).transpose()?,
                        daily: b.daily.map(|b| vb(b, "daily")).transpose()?,
                        monthly: b.monthly.map(|b| vb(b, "monthly")).transpose()?,
                    })
                })
                .transpose()?,
            default_profile: self.default_profile,
            active_profile: None,
        })
    }
}

/// Folds `stages:` entries into role bindings and, for a multi-model `assay`
/// stage, into the assay reviewer list (D79). Returns per-stage capability
/// requirements. A `stages:` entry overrides a role binding from a lower layer.
fn apply_stages(
    stages: HashMap<String, StageBindingLayer>,
    roles: &mut HashMap<String, String>,
    tools: &mut ToolsLayer,
) -> Result<BTreeMap<String, Vec<String>>, ConfigError> {
    let mut requirements = BTreeMap::new();
    for (stage, binding) in stages {
        match binding {
            StageBindingLayer::Alias(alias) => {
                roles.insert(stage, alias);
            }
            StageBindingLayer::Structured { model, requires } => {
                roles.insert(stage.clone(), model);
                if !requires.is_empty() {
                    requirements.insert(stage, requires);
                }
            }
            StageBindingLayer::Multi(aliases) => {
                // A list of models only makes sense for assay (multi-reviewer).
                if stage != "assay" {
                    return validation(format!(
                        "stages.{stage}: a list of models is only supported for the `assay` \
                         stage (multi-reviewer); use a single alias or `{{ model, requires }}`"
                    ));
                }
                if aliases.is_empty() {
                    return validation(
                        "stages.assay: the reviewer list must be non-empty".to_owned(),
                    );
                }
                // Synthesise one reviewer role per alias and point the assay
                // config at them (enabling assay).
                let reviewer_roles: Vec<String> = aliases
                    .into_iter()
                    .enumerate()
                    .map(|(index, alias)| {
                        let role = format!("assay-reviewer-{}", index + 1);
                        roles.insert(role.clone(), alias);
                        role
                    })
                    .collect();
                let assay = tools.assay.get_or_insert_with(AssayLayer::default);
                assay.enabled = Some(true);
                assay.role = Some(reviewer_roles[0].clone());
                assay.reviewers = Some(reviewer_roles);
            }
        }
    }
    Ok(requirements)
}

impl From<Config> for ConfigLayer {
    fn from(config: Config) -> Self {
        Self {
            version: Some(config.version),
            site: Some(config.site.into()),
            ai: None,
            models: Some(
                config
                    .models
                    .0
                    .into_iter()
                    .map(|(name, model)| (name, model.into()))
                    .collect(),
            ),
            roles: Some(config.roles.0),
            stages: None,
            tools: Some(config.tools.into()),
            pipeline: Some(config.pipeline.into_iter().map(Into::into).collect()),
            guardrails: Some(config.guardrails.into()),
            parallelism: Some(config.parallelism.into()),
            state: Some(config.state.into()),
            profiles: if config.profiles.0.is_empty() {
                None
            } else {
                Some(
                    config
                        .profiles
                        .0
                        .into_iter()
                        .map(|(name, profile)| {
                            let layer: ProfileLayer = profile
                                .stages
                                .into_iter()
                                .map(|(stage, aliases)| {
                                    let binding = if aliases.len() == 1 {
                                        ProfileBindingLayer::Single(
                                            aliases.into_iter().next().unwrap(),
                                        )
                                    } else {
                                        ProfileBindingLayer::Multi(aliases)
                                    };
                                    (stage, binding)
                                })
                                .collect();
                            (name, layer)
                        })
                        .collect(),
                )
            },
            budgets: config.budgets.map(|b| BudgetConfigLayer {
                per_ticket: b.per_ticket.map(|b| BudgetLayer {
                    max_cost: b.max_cost,
                }),
                daily: b.daily.map(|b| BudgetLayer {
                    max_cost: b.max_cost,
                }),
                monthly: b.monthly.map(|b| BudgetLayer {
                    max_cost: b.max_cost,
                }),
            }),
            default_profile: config.default_profile,
        }
    }
}

fn merge_nested<T>(current: &mut Option<T>, other: Option<T>, merge: impl FnOnce(&mut T, T)) {
    if let Some(other_value) = other {
        if let Some(current_value) = current {
            merge(current_value, other_value);
        } else {
            *current = Some(other_value);
        }
    }
}

fn merge_map<T>(current: &mut Option<HashMap<String, T>>, other: Option<HashMap<String, T>>) {
    if let Some(other_map) = other {
        current.get_or_insert_with(HashMap::new).extend(other_map);
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SiteLayer {
    name: Option<String>,
    prefix: Option<String>,
}

impl SiteLayer {
    fn merge(&mut self, other: Self) {
        merge_scalar(&mut self.name, other.name);
        merge_scalar(&mut self.prefix, other.prefix);
    }

    fn finalize(self) -> Result<Site, ConfigError> {
        Ok(Site {
            name: required(self.name, "site.name")?,
            prefix: required(self.prefix, "site.prefix")?,
        })
    }
}

impl From<Site> for SiteLayer {
    fn from(site: Site) -> Self {
        Self {
            name: Some(site.name),
            prefix: Some(site.prefix),
        }
    }
}

/// Deserialize target for the structured (mapping) form of a model alias.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelDefSpec {
    #[serde(default)]
    runtime: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    model: Option<String>,
    #[serde(default)]
    cli: Option<String>,
    // D79 re-activates these for API/local runtimes; ignored (with a warning)
    // for CLI runtimes.
    #[serde(default)]
    endpoint: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    auth_env: Option<String>,
    #[serde(default)]
    auth_mode: Option<String>,
    #[serde(default)]
    params: Option<BTreeMap<String, serde_yaml::Value>>,
    #[serde(default)]
    capabilities: Option<ModelCapabilities>,
    // Retained for the deprecation warning only; never used.
    #[serde(default)]
    region: Option<String>,
    #[serde(default)]
    deployment: Option<String>,
    max_tokens: Option<u32>,
    temperature: Option<f64>,
    cache: Option<bool>,
    timeout: Option<String>,
    rate_limit: Option<String>,
    cost_hint: Option<String>,
    #[serde(default)]
    estimated: Option<ModelEstimateSpec>,
}

/// Deserialize target for `models.*.estimated` (D80).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelEstimateSpec {
    latency: Option<String>,
    cost: Option<String>,
    quality: Option<String>,
}

/// The optional `ai:` section (D79). Currently just a preset selector.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct AiLayer {
    #[serde(default)]
    preset: Option<String>,
}

/// A `stages:` entry (D79): a bare model alias, a list of aliases (multi-model —
/// only meaningful for the `assay` stage, where it drives multi-reviewer assay),
/// or a mapping with an explicit `model` alias and optional capability
/// `requires:` list.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
enum StageBindingLayer {
    /// `stage: alias` — bind the stage to a model alias.
    Alias(String),
    /// `stage: [alias, …]` — multiple model aliases (assay reviewers).
    Multi(Vec<String>),
    /// `stage: { model: alias, requires: [tools, …] }`.
    Structured {
        /// Model alias this stage binds to.
        model: String,
        /// Capability names this stage requires.
        #[serde(default)]
        requires: Vec<String>,
    },
}

/// A model-alias layer entry. Accepts either the structured mapping form or the
/// D79 short syntax `runtime:model` (e.g. `claude-cli:claude-sonnet-4-6`).
#[derive(Clone, Debug, Default)]
struct ModelDefLayer {
    spec: ModelDefSpec,
}

impl<'de> Deserialize<'de> for ModelDefLayer {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct ModelDefLayerVisitor;

        impl<'de> Visitor<'de> for ModelDefLayerVisitor {
            type Value = ModelDefLayer;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a `runtime:model` string or a model mapping")
            }

            fn visit_str<E>(self, value: &str) -> Result<ModelDefLayer, E>
            where
                E: de::Error,
            {
                // Split on the FIRST colon only, so model ids that contain a
                // colon (e.g. `qwen2.5-coder:32b`) survive intact.
                let (runtime, model) = value
                    .split_once(':')
                    .filter(|(r, m)| !r.trim().is_empty() && !m.trim().is_empty())
                    .ok_or_else(|| {
                        E::custom(format!(
                            "short model syntax must be `runtime:model` (got `{value}`)"
                        ))
                    })?;
                Ok(ModelDefLayer {
                    spec: ModelDefSpec {
                        runtime: Some(runtime.trim().to_owned()),
                        model: Some(model.trim().to_owned()),
                        ..ModelDefSpec::default()
                    },
                })
            }

            fn visit_map<A>(self, map: A) -> Result<ModelDefLayer, A::Error>
            where
                A: MapAccess<'de>,
            {
                let spec = ModelDefSpec::deserialize(de::value::MapAccessDeserializer::new(map))?;
                Ok(ModelDefLayer { spec })
            }
        }

        deserializer.deserialize_any(ModelDefLayerVisitor)
    }
}

/// Maps a legacy provider name to its canonical host name.
///
/// Compatibility shim so pinned `derrick.yaml` files that still name the
/// pre-D65 providers continue to load. Returns the input unchanged when it is
/// not a known legacy alias. Applied only when no explicit `runtime` is set
/// (D79): with a runtime present, `provider` is preserved verbatim as metadata.
fn canonical_provider(provider: &str) -> &str {
    match provider {
        "copilot-cli" => "copilot",
        "openai-cli" => "codex",
        // Pre-D65 `anthropic` meant the Anthropic host; with no explicit runtime
        // it still resolves to the claude CLI. Use `runtime: anthropic-api` for
        // the direct-API path (D79).
        "anthropic" => "claude",
        other => other,
    }
}

impl ModelDefLayer {
    /// Builds a runtime-keyed layer entry (used by preset expansion).
    fn from_runtime(runtime: &str, model: &str) -> Self {
        Self {
            spec: ModelDefSpec {
                runtime: Some(runtime.to_owned()),
                model: Some(model.to_owned()),
                ..ModelDefSpec::default()
            },
        }
    }

    fn finalize(self) -> Result<ModelDef, ConfigError> {
        let spec = self.spec;
        let model = required(spec.model, "models.*.model")?;

        // Legacy provider aliases apply only when no explicit runtime is set.
        let provider = match (&spec.runtime, spec.provider) {
            (None, Some(raw)) => {
                let canon = canonical_provider(&raw).to_owned();
                if canon != raw {
                    tracing::warn!(
                        target: "derrick_config",
                        "provider `{raw}` is a legacy alias; treating it as `{canon}`. \
                         Prefer the explicit `runtime:` key (D79)."
                    );
                }
                Some(canon)
            }
            (Some(_), provider) => provider,
            (None, None) => None,
        };

        if spec.runtime.is_none() && provider.is_none() {
            return validation("models.*: requires `runtime` or `provider` (D79)".to_owned());
        }

        if spec.region.is_some() || spec.deployment.is_some() {
            tracing::warn!(
                target: "derrick_config",
                "models.*.{{region,deployment}} are not used; remove them from your config."
            );
        }

        // Resolve the runtime to decide whether endpoint/base_url are meaningful.
        let runtime_id = spec.runtime.clone().unwrap_or_else(|| {
            provider
                .as_deref()
                .map(runtime_for_provider)
                .unwrap_or("shell")
                .to_owned()
        });
        let is_cli_runtime = cli_host_for_runtime(&runtime_id).is_some() || runtime_id == "shell";
        if is_cli_runtime && (spec.endpoint.is_some() || spec.base_url.is_some()) {
            tracing::warn!(
                target: "derrick_config",
                "models.*.{{endpoint,base_url}} are ignored for CLI runtime `{runtime_id}`."
            );
        }

        Ok(ModelDef {
            runtime: spec.runtime,
            provider,
            model,
            cli: spec.cli,
            base_url: spec.base_url,
            endpoint: spec.endpoint,
            auth_env: spec.auth_env,
            auth_mode: spec.auth_mode,
            params: spec.params.unwrap_or_default(),
            capabilities: spec.capabilities,
            max_tokens: spec.max_tokens,
            temperature: spec.temperature,
            cache: spec.cache,
            timeout: spec.timeout,
            rate_limit: spec.rate_limit,
            cost_hint: spec.cost_hint,
            estimated: spec.estimated.map(|e| {
                const VALID_LATENCY: &[&str] = &["low", "medium", "high"];
                const VALID_COST: &[&str] = &["very_low", "low", "medium", "high", "very_high"];
                const VALID_QUALITY: &[&str] = &["low", "medium", "high", "very_high"];
                ModelEstimate {
                    latency: e.latency.filter(|v| VALID_LATENCY.contains(&v.as_str())),
                    cost: e.cost.filter(|v| VALID_COST.contains(&v.as_str())),
                    quality: e.quality.filter(|v| VALID_QUALITY.contains(&v.as_str())),
                }
            }),
        })
    }
}

impl From<ModelDef> for ModelDefLayer {
    fn from(model: ModelDef) -> Self {
        Self {
            spec: ModelDefSpec {
                runtime: model.runtime,
                provider: model.provider,
                model: Some(model.model),
                cli: model.cli,
                endpoint: model.endpoint,
                base_url: model.base_url,
                auth_env: model.auth_env,
                auth_mode: model.auth_mode,
                params: if model.params.is_empty() {
                    None
                } else {
                    Some(model.params)
                },
                capabilities: model.capabilities,
                region: None,
                deployment: None,
                max_tokens: model.max_tokens,
                temperature: model.temperature,
                cache: model.cache,
                timeout: model.timeout,
                rate_limit: model.rate_limit,
                cost_hint: model.cost_hint,
                estimated: model.estimated.map(|e| ModelEstimateSpec {
                    latency: e.latency,
                    cost: e.cost,
                    quality: e.quality,
                }),
            },
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolsLayer {
    speckit: Option<SpeckitLayer>,
    specify: Option<SpecifyLayer>,
    assay: Option<AssayLayer>,
    substrate: Option<SubstrateLayer>,
    copilot: Option<CopilotLayer>,
    claude: Option<ClaudeLayer>,
    git: Option<GitLayer>,
    foreman: Option<ToolsForemanLayer>,
    code_review: Option<CodeReviewLayer>,
    output_compression: Option<OutputCompressionLayer>,
    roughneck: Option<RoughneckLayer>,
}

impl ToolsLayer {
    fn merge(&mut self, other: Self) {
        merge_nested(&mut self.speckit, other.speckit, SpeckitLayer::merge);
        merge_nested(&mut self.specify, other.specify, SpecifyLayer::merge);
        merge_nested(&mut self.assay, other.assay, AssayLayer::merge);
        merge_nested(&mut self.substrate, other.substrate, SubstrateLayer::merge);
        merge_nested(&mut self.copilot, other.copilot, CopilotLayer::merge);
        merge_nested(&mut self.claude, other.claude, ClaudeLayer::merge);
        merge_nested(&mut self.git, other.git, GitLayer::merge);
        merge_nested(&mut self.foreman, other.foreman, ToolsForemanLayer::merge);
        merge_nested(
            &mut self.code_review,
            other.code_review,
            CodeReviewLayer::merge,
        );
        merge_nested(
            &mut self.output_compression,
            other.output_compression,
            OutputCompressionLayer::merge,
        );
        merge_nested(&mut self.roughneck, other.roughneck, RoughneckLayer::merge);
    }

    fn finalize(self) -> Result<Tools, ConfigError> {
        Ok(Tools {
            speckit: required(self.speckit, "tools.speckit")?.finalize()?,
            specify: self.specify.unwrap_or_default().finalize()?,
            assay: self.assay.unwrap_or_default().finalize()?,
            substrate: required(self.substrate, "tools.substrate")?.finalize()?,
            copilot: self.copilot.unwrap_or_default().finalize()?,
            claude: self.claude.unwrap_or_default().finalize(),
            git: self.git.unwrap_or_default().finalize()?,
            foreman: self.foreman.unwrap_or_default().finalize(),
            code_review: self.code_review.unwrap_or_default().finalize(),
            output_compression: self.output_compression.unwrap_or_default().finalize(),
            roughneck: self.roughneck.unwrap_or_default().finalize(),
        })
    }
}

impl From<Tools> for ToolsLayer {
    fn from(tools: Tools) -> Self {
        Self {
            speckit: Some(tools.speckit.into()),
            specify: Some(tools.specify.into()),
            assay: Some(tools.assay.into()),
            substrate: Some(tools.substrate.into()),
            copilot: Some(tools.copilot.into()),
            claude: Some(tools.claude.into()),
            git: Some(tools.git.into()),
            foreman: Some(tools.foreman.into()),
            code_review: Some(tools.code_review.into()),
            output_compression: Some(tools.output_compression.into()),
            roughneck: Some(tools.roughneck.into()),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct CodeReviewLayer {
    enabled: Option<bool>,
    role: Option<String>,
    rounds: Option<u32>,
    base_branch: Option<String>,
}

impl CodeReviewLayer {
    fn merge(&mut self, other: Self) {
        merge_scalar(&mut self.enabled, other.enabled);
        merge_scalar(&mut self.role, other.role);
        merge_scalar(&mut self.rounds, other.rounds);
        merge_scalar(&mut self.base_branch, other.base_branch);
    }

    fn finalize(self) -> CodeReview {
        let d = CodeReview::default();
        CodeReview {
            enabled: self.enabled.unwrap_or(d.enabled),
            role: self.role.unwrap_or(d.role),
            rounds: self.rounds.unwrap_or(d.rounds),
            base_branch: self.base_branch.unwrap_or(d.base_branch),
        }
    }
}

impl From<CodeReview> for CodeReviewLayer {
    fn from(cr: CodeReview) -> Self {
        Self {
            enabled: Some(cr.enabled),
            role: Some(cr.role),
            rounds: Some(cr.rounds),
            base_branch: Some(cr.base_branch),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct OutputCompressionLayer {
    enabled: Option<bool>,
}

impl OutputCompressionLayer {
    fn merge(&mut self, other: Self) {
        merge_scalar(&mut self.enabled, other.enabled);
    }

    fn finalize(self) -> OutputCompression {
        OutputCompression {
            enabled: self.enabled.unwrap_or(true),
        }
    }
}

impl From<OutputCompression> for OutputCompressionLayer {
    fn from(oc: OutputCompression) -> Self {
        Self {
            enabled: Some(oc.enabled),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RoughneckLayer {
    enabled: Option<bool>,
    level: Option<String>,
    compress_memory: Option<bool>,
}

impl RoughneckLayer {
    fn merge(&mut self, other: Self) {
        merge_scalar(&mut self.enabled, other.enabled);
        merge_scalar(&mut self.level, other.level);
        merge_scalar(&mut self.compress_memory, other.compress_memory);
    }

    fn finalize(self) -> Roughneck {
        Roughneck {
            enabled: self.enabled.unwrap_or(true),
            level: self.level.unwrap_or_else(|| "full".to_owned()),
            compress_memory: self.compress_memory.unwrap_or(false),
        }
    }
}

impl From<Roughneck> for RoughneckLayer {
    fn from(r: Roughneck) -> Self {
        Self {
            enabled: Some(r.enabled),
            level: Some(r.level),
            compress_memory: Some(r.compress_memory),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolsForemanLayer {
    #[serde(default, with = "humantime_serde")]
    poll_interval: Option<std::time::Duration>,
    #[serde(default, with = "humantime_serde")]
    in_review_ttl: Option<std::time::Duration>,
    #[serde(default, with = "humantime_serde")]
    hand_ttl: Option<std::time::Duration>,
    #[serde(default, with = "humantime_serde")]
    worktree_ttl: Option<std::time::Duration>,
    exit_when_idle: Option<bool>,
}

impl ToolsForemanLayer {
    fn merge(&mut self, other: Self) {
        merge_scalar(&mut self.poll_interval, other.poll_interval);
        merge_scalar(&mut self.in_review_ttl, other.in_review_ttl);
        merge_scalar(&mut self.hand_ttl, other.hand_ttl);
        merge_scalar(&mut self.worktree_ttl, other.worktree_ttl);
        merge_scalar(&mut self.exit_when_idle, other.exit_when_idle);
    }

    fn finalize(self) -> Foreman {
        let defaults = Foreman::default();
        Foreman {
            poll_interval: self.poll_interval.unwrap_or(defaults.poll_interval),
            in_review_ttl: self.in_review_ttl.unwrap_or(defaults.in_review_ttl),
            hand_ttl: self.hand_ttl.unwrap_or(defaults.hand_ttl),
            worktree_ttl: self.worktree_ttl.unwrap_or(defaults.worktree_ttl),
            exit_when_idle: self.exit_when_idle.unwrap_or(defaults.exit_when_idle),
        }
    }
}

impl From<Foreman> for ToolsForemanLayer {
    fn from(foreman: Foreman) -> Self {
        Self {
            poll_interval: Some(foreman.poll_interval),
            in_review_ttl: Some(foreman.in_review_ttl),
            hand_ttl: Some(foreman.hand_ttl),
            worktree_ttl: Some(foreman.worktree_ttl),
            exit_when_idle: Some(foreman.exit_when_idle),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SpeckitLayer {
    enabled: Option<bool>,
    version: Option<String>,
}

impl SpeckitLayer {
    fn merge(&mut self, other: Self) {
        merge_scalar(&mut self.enabled, other.enabled);
        merge_scalar(&mut self.version, other.version);
    }

    fn finalize(self) -> Result<Speckit, ConfigError> {
        Ok(Speckit {
            enabled: required(self.enabled, "tools.speckit.enabled")?,
            version: required(self.version, "tools.speckit.version")?,
        })
    }
}

impl From<Speckit> for SpeckitLayer {
    fn from(speckit: Speckit) -> Self {
        Self {
            enabled: Some(speckit.enabled),
            version: Some(speckit.version),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SpecifyLayer {
    provider: Option<String>,
    import: Option<ImportLayer>,
}

impl SpecifyLayer {
    fn merge(&mut self, other: Self) {
        merge_scalar(&mut self.provider, other.provider);
        merge_nested(&mut self.import, other.import, ImportLayer::merge);
    }

    fn finalize(self) -> Result<Specify, ConfigError> {
        Ok(Specify {
            // Omitted `provider` defaults to speckit — the back-compat path.
            provider: match self.provider {
                Some(value) => parse_spec_provider(&value)?,
                None => SpecProviderKind::default(),
            },
            import: self.import.unwrap_or_default().finalize()?,
        })
    }
}

impl From<Specify> for SpecifyLayer {
    fn from(specify: Specify) -> Self {
        Self {
            provider: Some(
                match specify.provider {
                    SpecProviderKind::Speckit => "speckit",
                    SpecProviderKind::Native => "native",
                    SpecProviderKind::Import => "import",
                }
                .to_owned(),
            ),
            import: Some(specify.import.into()),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportLayer {
    source: Option<String>,
    plan: Option<String>,
    tasks: Option<String>,
}

impl ImportLayer {
    fn merge(&mut self, other: Self) {
        merge_scalar(&mut self.source, other.source);
        merge_scalar(&mut self.plan, other.plan);
        merge_scalar(&mut self.tasks, other.tasks);
    }

    fn finalize(self) -> Result<ImportConfig, ConfigError> {
        Ok(ImportConfig {
            source: self.source,
            plan: match self.plan {
                Some(value) => parse_downstream_mode(&value, "tools.specify.import.plan")?,
                None => DownstreamMode::default(),
            },
            tasks: match self.tasks {
                Some(value) => parse_downstream_mode(&value, "tools.specify.import.tasks")?,
                None => DownstreamMode::default(),
            },
        })
    }
}

impl From<ImportConfig> for ImportLayer {
    fn from(import: ImportConfig) -> Self {
        let mode_str = |mode: DownstreamMode| {
            match mode {
                DownstreamMode::Native => "native",
                DownstreamMode::Speckit => "speckit",
                DownstreamMode::Import => "import",
            }
            .to_owned()
        };
        Self {
            source: import.source,
            plan: Some(mode_str(import.plan)),
            tasks: Some(mode_str(import.tasks)),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct AssayLayer {
    enabled: Option<bool>,
    role: Option<String>,
    reviewers: Option<Vec<String>>,
    #[serde(default, deserialize_with = "option_string_or_number")]
    rounds: Option<String>,
    strict: Option<bool>,
    auto_execute: Option<bool>,
    on_split: Option<String>,
}

impl AssayLayer {
    fn merge(&mut self, other: Self) {
        merge_scalar(&mut self.enabled, other.enabled);
        merge_scalar(&mut self.role, other.role);
        merge_scalar(&mut self.reviewers, other.reviewers);
        merge_scalar(&mut self.rounds, other.rounds);
        merge_scalar(&mut self.strict, other.strict);
        merge_scalar(&mut self.auto_execute, other.auto_execute);
        merge_scalar(&mut self.on_split, other.on_split);
    }

    fn finalize(self) -> Result<Assay, ConfigError> {
        Ok(Assay {
            enabled: self.enabled.unwrap_or(false),
            role: required(self.role, "tools.assay.role")?,
            reviewers: self.reviewers.unwrap_or_default(),
            rounds: self.rounds.unwrap_or_else(|| "10".to_owned()),
            strict: self.strict.unwrap_or(false),
            auto_execute: self.auto_execute.unwrap_or(false),
            on_split: parse_on_split(&self.on_split.unwrap_or_else(|| "reject".to_owned()))?,
        })
    }
}

impl From<Assay> for AssayLayer {
    fn from(assay: Assay) -> Self {
        Self {
            enabled: Some(assay.enabled),
            role: Some(assay.role),
            reviewers: Some(assay.reviewers),
            rounds: Some(assay.rounds),
            strict: Some(assay.strict),
            auto_execute: Some(assay.auto_execute),
            on_split: Some(
                match assay.on_split {
                    OnSplit::Reject => "reject",
                    OnSplit::Human => "human",
                    OnSplit::Majority => "majority",
                }
                .to_owned(),
            ),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SubstrateLayer {
    backend: Option<String>,
    mode: Option<String>,
}

impl SubstrateLayer {
    fn merge(&mut self, other: Self) {
        merge_scalar(&mut self.backend, other.backend);
        merge_scalar(&mut self.mode, other.mode);
    }

    fn finalize(self) -> Result<Substrate, ConfigError> {
        Ok(Substrate {
            backend: parse_substrate_backend(&required(self.backend, "tools.substrate.backend")?)?,
            mode: parse_substrate_mode(&required(self.mode, "tools.substrate.mode")?)?,
        })
    }
}

impl From<Substrate> for SubstrateLayer {
    fn from(substrate: Substrate) -> Self {
        Self {
            backend: Some(
                match substrate.backend {
                    SubstrateBackendKind::Native => "native",
                    SubstrateBackendKind::None => "none",
                }
                .to_owned(),
            ),
            mode: Some(
                match substrate.mode {
                    SubstrateMode::Solo => "solo",
                    SubstrateMode::Copilot => "copilot",
                    SubstrateMode::Crew => "crew",
                }
                .to_owned(),
            ),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct CopilotLayer {
    enabled: Option<bool>,
    agent_identity: Option<String>,
    #[serde(default, with = "humantime_serde")]
    poll_interval: Option<std::time::Duration>,
    #[serde(default, with = "humantime_serde")]
    poll_timeout: Option<std::time::Duration>,
}

impl CopilotLayer {
    fn merge(&mut self, other: Self) {
        merge_scalar(&mut self.enabled, other.enabled);
        merge_scalar(&mut self.agent_identity, other.agent_identity);
        merge_scalar(&mut self.poll_interval, other.poll_interval);
        merge_scalar(&mut self.poll_timeout, other.poll_timeout);
    }

    fn finalize(self) -> Result<Copilot, ConfigError> {
        let defaults = Copilot::default();
        Ok(Copilot {
            enabled: self.enabled.unwrap_or(false),
            agent_identity: required(self.agent_identity, "tools.copilot.agent_identity")?,
            poll_interval: self.poll_interval.unwrap_or(defaults.poll_interval),
            poll_timeout: self.poll_timeout.unwrap_or(defaults.poll_timeout),
        })
    }
}

impl From<Copilot> for CopilotLayer {
    fn from(copilot: Copilot) -> Self {
        Self {
            enabled: Some(copilot.enabled),
            agent_identity: Some(copilot.agent_identity),
            poll_interval: Some(copilot.poll_interval),
            poll_timeout: Some(copilot.poll_timeout),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClaudeLayer {
    enabled: Option<bool>,
    agent_identity: Option<String>,
    auto_dispatch: Option<bool>,
    #[serde(default, with = "humantime_serde")]
    poll_interval: Option<std::time::Duration>,
    #[serde(default, with = "humantime_serde")]
    poll_timeout: Option<std::time::Duration>,
    queue_dir: Option<String>,
}

impl ClaudeLayer {
    fn merge(&mut self, other: Self) {
        merge_scalar(&mut self.enabled, other.enabled);
        merge_scalar(&mut self.agent_identity, other.agent_identity);
        merge_scalar(&mut self.auto_dispatch, other.auto_dispatch);
        merge_scalar(&mut self.poll_interval, other.poll_interval);
        merge_scalar(&mut self.poll_timeout, other.poll_timeout);
        merge_scalar(&mut self.queue_dir, other.queue_dir);
    }

    fn finalize(self) -> Claude {
        let defaults = Claude::default();
        Claude {
            enabled: self.enabled.unwrap_or(defaults.enabled),
            agent_identity: self.agent_identity.unwrap_or(defaults.agent_identity),
            auto_dispatch: self.auto_dispatch.unwrap_or(defaults.auto_dispatch),
            poll_interval: self.poll_interval.unwrap_or(defaults.poll_interval),
            poll_timeout: self.poll_timeout.unwrap_or(defaults.poll_timeout),
            queue_dir: self.queue_dir.unwrap_or(defaults.queue_dir),
        }
    }
}

impl From<Claude> for ClaudeLayer {
    fn from(claude: Claude) -> Self {
        Self {
            enabled: Some(claude.enabled),
            agent_identity: Some(claude.agent_identity),
            auto_dispatch: Some(claude.auto_dispatch),
            poll_interval: Some(claude.poll_interval),
            poll_timeout: Some(claude.poll_timeout),
            queue_dir: Some(claude.queue_dir),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct GitLayer {
    stacking: Option<StackingLayer>,
    branch_prefix: Option<String>,
}

impl GitLayer {
    fn merge(&mut self, other: Self) {
        merge_nested(&mut self.stacking, other.stacking, StackingLayer::merge);
        merge_scalar(&mut self.branch_prefix, other.branch_prefix);
    }

    fn finalize(self) -> Result<Git, ConfigError> {
        let defaults = Git::default();
        Ok(Git {
            stacking: self.stacking.unwrap_or_default().finalize()?,
            branch_prefix: self.branch_prefix.unwrap_or(defaults.branch_prefix),
        })
    }
}

impl From<Git> for GitLayer {
    fn from(git: Git) -> Self {
        Self {
            stacking: Some(git.stacking.into()),
            branch_prefix: Some(git.branch_prefix),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct StackingLayer {
    backend: Option<String>,
    branch_pattern: Option<String>,
    auto_restack_on_merge: Option<bool>,
    force_push: Option<String>,
    auto_pr: Option<bool>,
    draft: Option<bool>,
}

impl StackingLayer {
    fn merge(&mut self, other: Self) {
        merge_scalar(&mut self.backend, other.backend);
        merge_scalar(&mut self.branch_pattern, other.branch_pattern);
        merge_scalar(&mut self.auto_restack_on_merge, other.auto_restack_on_merge);
        merge_scalar(&mut self.force_push, other.force_push);
        merge_scalar(&mut self.auto_pr, other.auto_pr);
        merge_scalar(&mut self.draft, other.draft);
    }

    fn finalize(self) -> Result<Stacking, ConfigError> {
        let defaults = Stacking::default();
        Ok(Stacking {
            backend: parse_stack_backend(&self.backend.unwrap_or_else(|| "none".to_owned()))?,
            branch_pattern: self.branch_pattern.unwrap_or(defaults.branch_pattern),
            auto_restack_on_merge: self
                .auto_restack_on_merge
                .unwrap_or(defaults.auto_restack_on_merge),
            force_push: parse_force_push(
                &self.force_push.unwrap_or_else(|| "with-lease".to_owned()),
            )?,
            auto_pr: self.auto_pr.unwrap_or(defaults.auto_pr),
            draft: self.draft.unwrap_or(defaults.draft),
        })
    }
}

impl From<Stacking> for StackingLayer {
    fn from(stacking: Stacking) -> Self {
        Self {
            backend: Some(
                match stacking.backend {
                    StackBackendKind::None => "none",
                    StackBackendKind::Native => "native",
                }
                .to_owned(),
            ),
            branch_pattern: Some(stacking.branch_pattern),
            auto_restack_on_merge: Some(stacking.auto_restack_on_merge),
            force_push: Some(
                match stacking.force_push {
                    ForcePush::WithLease => "with-lease",
                    ForcePush::Off => "off",
                }
                .to_owned(),
            ),
            auto_pr: Some(stacking.auto_pr),
            draft: Some(stacking.draft),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PipelineStepLayer {
    id: Option<String>,
    role: Option<String>,
    runner: Option<Runner>,
    host: Option<Host>,
    command: Option<String>,
    inputs: Option<Vec<String>>,
    skippable: Option<bool>,
    default_skip: Option<bool>,
    prompt: Option<String>,
    #[serde(default, deserialize_with = "option_string_or_number")]
    rounds: Option<String>,
    on_reject: Option<OnReject>,
    on_failure: Option<OnFailure>,
    poll_interval: Option<String>,
    batch: Option<String>,
    executor_role: Option<String>,
    parallel_group: Option<String>,
}

impl PipelineStepLayer {
    fn finalize(self) -> Result<PipelineStep, ConfigError> {
        Ok(PipelineStep {
            id: required(self.id, "pipeline[].id")?,
            role: self.role,
            runner: self.runner,
            host: self.host,
            command: self.command,
            inputs: self.inputs.unwrap_or_default(),
            skippable: self.skippable.unwrap_or(false),
            default_skip: self.default_skip.unwrap_or(false),
            prompt: self.prompt,
            rounds: self.rounds,
            on_reject: self.on_reject,
            on_failure: self.on_failure,
            poll_interval: self.poll_interval,
            batch: self.batch,
            executor_role: self.executor_role,
            parallel_group: self.parallel_group,
        })
    }
}

impl From<PipelineStep> for PipelineStepLayer {
    fn from(step: PipelineStep) -> Self {
        Self {
            id: Some(step.id),
            role: step.role,
            runner: step.runner,
            host: step.host,
            command: step.command,
            inputs: Some(step.inputs),
            skippable: Some(step.skippable),
            default_skip: Some(step.default_skip),
            prompt: step.prompt,
            rounds: step.rounds,
            on_reject: step.on_reject,
            on_failure: step.on_failure,
            poll_interval: step.poll_interval,
            batch: step.batch,
            executor_role: step.executor_role,
            parallel_group: step.parallel_group,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct GuardrailsLayer {
    constitution_path: Option<PathBuf>,
    forbid_paths: Option<Vec<String>>,
    required_labels: Option<Vec<String>>,
}

impl GuardrailsLayer {
    fn merge(&mut self, other: Self) {
        merge_scalar(&mut self.constitution_path, other.constitution_path);
        merge_scalar(&mut self.forbid_paths, other.forbid_paths);
        merge_scalar(&mut self.required_labels, other.required_labels);
    }

    fn finalize(self) -> Guardrails {
        Guardrails {
            constitution_path: self
                .constitution_path
                .unwrap_or_else(|| PathBuf::from(".specify/memory/constitution.md")),
            forbid_paths: self.forbid_paths.unwrap_or_default(),
            required_labels: self.required_labels.unwrap_or_default(),
        }
    }
}

impl From<Guardrails> for GuardrailsLayer {
    fn from(guardrails: Guardrails) -> Self {
        Self {
            constitution_path: Some(guardrails.constitution_path),
            forbid_paths: Some(guardrails.forbid_paths),
            required_labels: Some(guardrails.required_labels),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ParallelismLayer {
    batch_max: Option<u32>,
    step_max: Option<u32>,
    assay_max: Option<u32>,
}

impl ParallelismLayer {
    fn merge(&mut self, other: Self) {
        merge_scalar(&mut self.batch_max, other.batch_max);
        merge_scalar(&mut self.step_max, other.step_max);
        merge_scalar(&mut self.assay_max, other.assay_max);
    }

    fn finalize(self) -> Result<Parallelism, ConfigError> {
        Ok(Parallelism {
            batch_max: required(self.batch_max, "parallelism.batch_max")?,
            step_max: required(self.step_max, "parallelism.step_max")?,
            assay_max: required(self.assay_max, "parallelism.assay_max")?,
        })
    }
}

impl From<Parallelism> for ParallelismLayer {
    fn from(parallelism: Parallelism) -> Self {
        Self {
            batch_max: Some(parallelism.batch_max),
            step_max: Some(parallelism.step_max),
            assay_max: Some(parallelism.assay_max),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct StateLayer {
    dir: Option<PathBuf>,
    log_runs: Option<bool>,
    worktree_root: Option<PathBuf>,
}

impl StateLayer {
    fn merge(&mut self, other: Self) {
        merge_scalar(&mut self.dir, other.dir);
        merge_scalar(&mut self.log_runs, other.log_runs);
        merge_scalar(&mut self.worktree_root, other.worktree_root);
    }

    fn finalize(self) -> Result<StateConfig, ConfigError> {
        Ok(StateConfig {
            dir: required(self.dir, "state.dir")?,
            log_runs: required(self.log_runs, "state.log_runs")?,
            worktree_root: required(self.worktree_root, "state.worktree_root")?,
        })
    }
}

impl From<StateConfig> for StateLayer {
    fn from(state: StateConfig) -> Self {
        Self {
            dir: Some(state.dir),
            log_runs: Some(state.log_runs),
            worktree_root: Some(state.worktree_root),
        }
    }
}

fn merge_scalar<T>(current: &mut Option<T>, other: Option<T>) {
    if other.is_some() {
        *current = other;
    }
}

fn option_string_or_number<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_yaml::Value>::deserialize(deserializer)?;
    value.map_or(Ok(None), |value| match value {
        serde_yaml::Value::String(text) => Ok(Some(text)),
        serde_yaml::Value::Number(number) => Ok(Some(number.to_string())),
        other => Err(serde::de::Error::custom(format!(
            "expected string or number, got {other:?}"
        ))),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Tests that mutate the process-global `HOME` env var must run
    // serially. Cargo runs tests in parallel by default; on CI runners
    // with more cores than dev machines, racing tests overwrite each
    // other's HOME and the layered-config merge becomes nondeterministic.
    // Acquire HOME_LOCK at the top of any test that calls
    // `env::set_var("HOME", _)` or `env::remove_var("HOME")`.
    static HOME_LOCK: Mutex<()> = Mutex::new(());

    fn write_file(path: &Path, contents: &str) {
        fs::write(path, contents).unwrap_or_else(|error| {
            panic!("failed to write {}: {error}", path.display());
        });
    }

    fn minimal_yaml() -> String {
        r#"
version: 1
site:
  name: test-site
  prefix: tst
models:
  claude-sonnet:
    provider: anthropic
    model: claude-sonnet-4-6
  codex-gpt5:
    provider: openai-cli
    model: gpt-5
roles:
  drafter: claude-sonnet
  reviewer: codex-gpt5
tools:
  speckit:
    enabled: true
    version: ">=0.4.0"
  assay:
    enabled: false
    role: reviewer
    reviewers: [reviewer]
  substrate:
    backend: native
    mode: solo
  copilot:
    agent_identity: derrick-hand
pipeline: []
guardrails:
  constitution_path: .specify/memory/constitution.md
parallelism:
  batch_max: 8
  step_max: 4
  assay_max: 2
state:
  dir: .derrick
  log_runs: true
  worktree_root: .derrick/worktrees
"#
        .to_owned()
    }

    fn load_yaml(contents: &str) -> Result<Config, ConfigError> {
        let dir = tempfile::tempdir().unwrap_or_else(|error| {
            panic!("failed to create temp dir: {error}");
        });
        let path = dir.path().join("derrick.yaml");
        write_file(&path, contents);
        Config::load_from_path(&path)
    }

    /// Wraps a top section (`models`/`roles`/`ai`/`stages`) in the fixed
    /// scaffolding every config needs, for D79 tests.
    fn assemble(top: &str) -> String {
        format!(
            r#"
version: 1
site:
  name: t
  prefix: tst
{top}
tools:
  speckit:
    enabled: true
    version: ">=0.4.0"
  assay:
    enabled: false
    role: drafter
    reviewers: [drafter]
  substrate:
    backend: none
    mode: solo
  copilot:
    agent_identity: derrick-hand
pipeline: []
guardrails:
  constitution_path: .specify/memory/constitution.md
parallelism:
  batch_max: 8
  step_max: 4
  assay_max: 2
state:
  dir: .derrick
  log_runs: true
  worktree_root: .derrick/worktrees
"#
        )
    }

    #[test]
    fn d79_short_syntax_expands_runtime_and_model() {
        let yaml =
            assemble("models:\n  fast: claude-cli:claude-sonnet-4-6\nroles:\n  drafter: fast");
        let config = load_yaml(&yaml).expect("short syntax should parse");
        let model = config.models().get("fast").expect("fast model");
        assert_eq!(model.runtime(), Some("claude-cli"));
        assert_eq!(model.model(), "claude-sonnet-4-6");
        assert_eq!(model.provider(), None);
        assert_eq!(model.resolved_runtime(), "claude-cli");
    }

    #[test]
    fn d79_short_syntax_keeps_colon_in_model_id() {
        // ollama ids carry their own colon (`qwen2.5-coder:32b`); only the first
        // colon separates runtime from model.
        let yaml =
            assemble("models:\n  local: \"ollama:qwen2.5-coder:32b\"\nroles:\n  drafter: local");
        let config = load_yaml(&yaml).expect("short syntax should parse");
        let model = config.models().get("local").expect("local model");
        assert_eq!(model.runtime(), Some("ollama"));
        assert_eq!(model.model(), "qwen2.5-coder:32b");
    }

    #[test]
    fn d79_runtime_derived_from_legacy_provider() {
        let yaml = assemble(
            "models:\n  m:\n    provider: claude\n    model: claude-opus-4-8\nroles:\n  drafter: m",
        );
        let config = load_yaml(&yaml).expect("legacy provider should parse");
        let model = config.models().get("m").expect("m model");
        assert_eq!(model.runtime(), None);
        assert_eq!(model.resolved_runtime(), "claude-cli");
        assert_eq!(model.provider(), Some("claude"));
    }

    #[test]
    fn d79_missing_runtime_and_provider_is_rejected() {
        let yaml = assemble("models:\n  m:\n    model: foo\nroles:\n  drafter: m");
        assert_validation(&yaml, "requires `runtime` or `provider`");
    }

    #[test]
    fn d79_preset_cli_defaults_generates_models_and_roles() {
        let yaml = assemble("ai:\n  preset: cli-defaults");
        let config = load_yaml(&yaml).expect("preset should expand");
        for alias in ["fast", "strong", "reviewer", "executor"] {
            assert!(config.models().get(alias).is_some(), "{alias} should exist");
        }
        assert_eq!(config.roles().get("proposer"), Some("strong"));
        assert_eq!(config.roles().get("drafter"), Some("fast"));
        assert_eq!(
            config.models().get("strong").unwrap().resolved_runtime(),
            "claude-cli"
        );
        assert_eq!(
            config.models().get("executor").unwrap().resolved_runtime(),
            "copilot-cli"
        );
    }

    #[test]
    fn d79_preset_unknown_is_rejected() {
        let yaml = assemble("ai:\n  preset: bogus");
        assert_validation(&yaml, "must be one of cli-defaults");
    }

    #[test]
    fn d79_explicit_models_override_preset() {
        // An explicit `fast` definition beats the preset's `fast`.
        let yaml = assemble(
            "ai:\n  preset: cli-defaults\nmodels:\n  fast:\n    runtime: ollama\n    model: llama3.2",
        );
        let config = load_yaml(&yaml).expect("preset + override should parse");
        let fast = config.models().get("fast").expect("fast model");
        assert_eq!(fast.resolved_runtime(), "ollama");
        assert_eq!(fast.model(), "llama3.2");
        // The preset still supplies the other aliases.
        assert!(config.models().get("strong").is_some());
    }

    #[test]
    fn d79_stages_bind_to_roles_and_collect_requires() {
        let yaml = assemble(
            "ai:\n  preset: cli-defaults\nstages:\n  plan: strong\n  execute:\n    model: executor\n    requires: [tools]",
        );
        let config = load_yaml(&yaml).expect("stages should parse");
        assert_eq!(config.roles().get("plan"), Some("strong"));
        assert_eq!(config.roles().get("execute"), Some("executor"));
        assert_eq!(
            config.stage_requirements().get("execute"),
            Some(&vec!["tools".to_owned()])
        );
    }

    #[test]
    fn d79_builtin_capabilities_by_family() {
        assert_eq!(builtin_capabilities("claude-opus-4-8").tools, Some(true));
        assert_eq!(
            builtin_capabilities("claude-opus-4-8").prompt_cache,
            Some(true)
        );
        assert_eq!(builtin_capabilities("gpt-5.5").tools, Some(true));
        assert_eq!(builtin_capabilities("gpt-5.5").prompt_cache, Some(false));
        // Unknown / local model: nothing assumed.
        assert_eq!(builtin_capabilities("qwen2.5-coder:32b").tools, None);
    }

    #[test]
    fn d79_multi_model_assay_stage_wires_reviewers() {
        let yaml = assemble(
            "ai:\n  preset: cli-defaults\nstages:\n  assay:\n    - strong\n    - reviewer\n    - fast",
        );
        let config = load_yaml(&yaml).expect("multi-reviewer assay should parse");
        // Each alias becomes a synthesised reviewer role bound to that model.
        assert_eq!(config.roles().get("assay-reviewer-1"), Some("strong"));
        assert_eq!(config.roles().get("assay-reviewer-2"), Some("reviewer"));
        assert_eq!(config.roles().get("assay-reviewer-3"), Some("fast"));
        // Assay is enabled and points at the synthesised reviewers in order.
        let assay = config.tools().assay();
        assert!(assay.enabled());
        assert_eq!(assay.role(), "assay-reviewer-1");
        assert_eq!(
            assay.reviewers(),
            &[
                "assay-reviewer-1".to_owned(),
                "assay-reviewer-2".to_owned(),
                "assay-reviewer-3".to_owned()
            ]
        );
    }

    #[test]
    fn d79_multi_model_non_assay_stage_is_rejected() {
        let yaml =
            assemble("ai:\n  preset: cli-defaults\nstages:\n  plan:\n    - fast\n    - strong");
        assert_validation(&yaml, "only supported for the `assay` stage");
    }

    #[test]
    fn d79_capabilities_parse() {
        let yaml = assemble(
            "models:\n  m:\n    runtime: anthropic-api\n    model: claude-opus-4-8\n    auth_env: ANTHROPIC_API_KEY\n    capabilities:\n      tools: true\n      prompt_cache: false\n      context_window: 200000\nroles:\n  drafter: m",
        );
        let config = load_yaml(&yaml).expect("capabilities should parse");
        let model = config.models().get("m").expect("m model");
        assert_eq!(model.auth_env(), Some("ANTHROPIC_API_KEY"));
        let caps = model.capabilities().expect("capabilities present");
        assert_eq!(caps.declared("tools"), Some(true));
        assert_eq!(caps.declared("prompt_cache"), Some(false));
        assert_eq!(caps.declared("vision"), None);
        assert_eq!(caps.context_window, Some(200_000));
    }

    fn assert_validation(contents: &str, expected: &str) {
        match load_yaml(contents) {
            Err(ConfigError::Validation(message)) => assert!(
                message.contains(expected),
                "expected {message:?} to contain {expected:?}",
            ),
            other => panic!("expected validation error, got {other:?}"),
        }
    }

    fn replace(source: &str, from: &str, to: &str) -> String {
        source.replacen(from, to, 1)
    }

    #[test]
    fn render_init_template_replaces_init_vars_and_preserves_flow_vars() {
        let template = "site: {{site_name}}\nprefix: {{prefix}}\nmode: {{mode}}\nflow: {{prompt}}";

        let rendered = render_init_template(
            template,
            InitTemplateVars {
                site_name: "test",
                prefix: "tst",
                mode: "solo",
            },
        );

        assert_eq!(
            rendered,
            "site: test\nprefix: tst\nmode: solo\nflow: {{prompt}}"
        );
    }

    #[test]
    fn config_defaults_validate() {
        Config::defaults()
            .validate()
            .unwrap_or_else(|error| panic!("defaults should validate: {error}"));
    }

    #[test]
    fn config_parses_minimal_valid_yaml() {
        let config =
            load_yaml(&minimal_yaml()).unwrap_or_else(|error| panic!("yaml should parse: {error}"));

        assert_eq!(config.version(), 1);
        assert_eq!(config.site().prefix(), "tst");
        assert!(config.pipeline().is_empty());
        assert_eq!(
            config.tools().git().stacking().backend(),
            StackBackendKind::None
        );
        assert_eq!(config.tools().git().branch_prefix(), "derrick");
    }

    #[test]
    fn config_parses_full_design_md_example() {
        let yaml = r#"
version: 1
site:
  name: my-project
  prefix: mp
models:
  claude-opus:    { provider: anthropic, model: "claude-opus-4-7" }
  claude-sonnet:  { provider: anthropic, model: "claude-sonnet-4-6" }
  codex-gpt5:     { provider: openai-cli, cli: "codex exec", model: "gpt-5" }
  copilot:        { provider: copilot-cli, cli: "copilot",  model: "gpt-5-codex" }
roles:
  proposer:  claude-opus
  drafter:   claude-sonnet
  reviewer:  codex-gpt5
  executor:  copilot
  summariser: claude-sonnet
tools:
  speckit: { enabled: true, version: ">=0.4.0" }
  assay:
    enabled: true
    role: reviewer
    reviewers: [reviewer]
    rounds: 10

  substrate:
    backend: native
    mode: crew
  copilot:
    enabled: true
    agent_identity: derrick-hand
pipeline:
  - id: specify
    role: drafter
    host: claude
    command: "/speckit.specify {{prompt}}"
  - id: clarify
    runner: derrick
    skippable: true
  - id: plan
    role: proposer
    host: claude
    command: "/speckit.plan"
  - id: assay
    runner: derrick
    inputs: ["{{feature_dir}}/spec.md", "{{feature_dir}}/plan.md"]
    rounds: "{{tools.assay.rounds}}"
    on_reject: halt
  - id: analyze
    role: proposer
    host: claude
    command: "/speckit.analyze"
  - id: tasks
    role: drafter
    host: claude
    command: "/speckit.tasks"
  - id: bridge
    runner: derrick
    inputs: ["{{tasks_md}}"]
    batch: "{{batch}}"
  - id: foreman
    runner: derrick
    executor_role: executor
guardrails:
  constitution_path: .specify/memory/constitution.md
  forbid_paths: []
  required_labels: []
parallelism:
  batch_max: 8
  step_max:   4
  assay_max:  2
state:
  dir: .derrick
  log_runs: true
  worktree_root: .derrick/worktrees
"#;
        let config =
            load_yaml(yaml).unwrap_or_else(|error| panic!("design example should parse: {error}"));

        assert_eq!(config.pipeline().len(), 8);
        assert_eq!(config.tools().assay().rounds(), "10");
        assert_eq!(config.tools().substrate().mode(), SubstrateMode::Crew);
    }

    #[test]
    fn config_role_points_to_missing_model_is_rejected() {
        let yaml = replace(
            &minimal_yaml(),
            "drafter: claude-sonnet",
            "drafter: missing",
        );

        assert_validation(&yaml, "roles.drafter");
    }

    #[test]
    fn config_pipeline_role_points_to_missing_role_is_rejected() {
        let yaml = replace(
            &minimal_yaml(),
            "pipeline: []",
            "pipeline:\n  - id: specify\n    role: missing\n",
        );

        assert_validation(&yaml, "pipeline.specify.role");
    }

    #[test]
    fn config_pipeline_step_with_role_and_runner_is_rejected() {
        let yaml = replace(
            &minimal_yaml(),
            "pipeline: []",
            "pipeline:\n  - id: bad\n    role: drafter\n    runner: human\n    prompt: ok\n",
        );

        assert_validation(&yaml, "mutually exclusive");
    }

    #[test]
    fn config_pipeline_step_without_role_or_runner_is_rejected() {
        let yaml = replace(
            &minimal_yaml(),
            "pipeline: []",
            "pipeline:\n  - id: bad\n    command: ok\n",
        );

        assert_validation(&yaml, "either role or runner");
    }

    #[test]
    fn config_human_runner_without_prompt_is_rejected() {
        let yaml = replace(
            &minimal_yaml(),
            "pipeline: []",
            "pipeline:\n  - id: human-step\n    runner: human\n",
        );

        assert_validation(&yaml, "runner human");
    }

    #[test]
    fn config_assay_role_points_to_missing_role_is_rejected() {
        let yaml = replace(&minimal_yaml(), "role: reviewer", "role: missing");
        let yaml = replace(&yaml, "enabled: false", "enabled: true");

        assert_validation(&yaml, "tools.assay.role");
    }

    #[test]
    fn config_assay_reviewer_points_to_missing_role_is_rejected() {
        let yaml = replace(
            &minimal_yaml(),
            "reviewers: [reviewer]",
            "reviewers: [missing]",
        );
        let yaml = replace(&yaml, "enabled: false", "enabled: true");

        assert_validation(&yaml, "tools.assay.reviewers");
    }

    #[test]
    fn config_gastown_substrate_backend_is_rejected() {
        let yaml = replace(&minimal_yaml(), "backend: native", "backend: gastown");

        assert_validation(&yaml, "gastown backend ships in a future version");
    }

    #[test]
    fn config_invalid_substrate_backend_is_rejected() {
        let yaml = replace(&minimal_yaml(), "backend: native", "backend: invalid");

        assert_validation(&yaml, "tools.substrate.backend");
    }

    #[test]
    fn config_assay_reviewers_empty_when_enabled_is_rejected() {
        let yaml = replace(&minimal_yaml(), "reviewers: [reviewer]", "reviewers: []");
        let yaml = replace(&yaml, "enabled: false", "enabled: true");

        assert_validation(&yaml, "must be non-empty");
    }

    #[test]
    fn config_invalid_on_split_is_rejected() {
        let yaml = replace(
            &minimal_yaml(),
            "reviewers: [reviewer]",
            "reviewers: [reviewer]\n    on_split: invalid",
        );

        assert_validation(&yaml, "tools.assay.on_split");
    }

    #[test]
    fn config_majority_on_split_requires_odd_reviewers() {
        let yaml = replace(
            &minimal_yaml(),
            "reviewers: [reviewer]",
            "reviewers: [reviewer, drafter]\n    on_split: majority",
        );
        let yaml = replace(&yaml, "enabled: false", "enabled: true");

        assert_validation(&yaml, "odd number of reviewers");
    }

    #[test]
    fn config_invalid_stack_backend_is_rejected() {
        let yaml = replace(
            &minimal_yaml(),
            "  substrate:\n    backend: native\n    mode: solo",
            "  substrate:\n    backend: native\n    mode: solo\n  git:\n    stacking:\n      backend: invalid",
        );

        assert_validation(&yaml, "tools.git.stacking.backend");
    }

    #[test]
    fn config_graphite_backend_is_rejected_with_actionable_d72_error() {
        let yaml = replace(
            &minimal_yaml(),
            "  substrate:\n    backend: native\n    mode: solo",
            "  substrate:\n    backend: native\n    mode: solo\n  git:\n    stacking:\n      backend: graphite",
        );

        // Removed (D72): must name the removed value, the decision, and the
        // exact remediation rather than a bare unknown-variant message.
        assert_validation(&yaml, "removed (D72)");
        assert_validation(&yaml, "\"graphite\"");
        assert_validation(&yaml, "set tools.git.stacking.backend: native");
    }

    #[test]
    fn config_git_spice_backend_is_rejected_with_actionable_d72_error() {
        let yaml = replace(
            &minimal_yaml(),
            "  substrate:\n    backend: native\n    mode: solo",
            "  substrate:\n    backend: native\n    mode: solo\n  git:\n    stacking:\n      backend: git-spice",
        );

        assert_validation(&yaml, "removed (D72)");
        assert_validation(&yaml, "\"git-spice\"");
        assert_validation(&yaml, "set tools.git.stacking.backend: native");
    }

    #[test]
    fn config_invalid_site_prefix_is_rejected() {
        let yaml = replace(&minimal_yaml(), "prefix: tst", "prefix: TooLong");

        assert_validation(&yaml, "site.prefix");
    }

    #[test]
    fn config_parallelism_batch_max_out_of_range_is_rejected() {
        let yaml = replace(&minimal_yaml(), "batch_max: 8", "batch_max: 0");

        assert_validation(&yaml, "parallelism.batch_max");
    }

    #[test]
    fn config_parallelism_step_max_out_of_range_is_rejected() {
        let yaml = replace(&minimal_yaml(), "step_max: 4", "step_max: 65");

        assert_validation(&yaml, "parallelism.step_max");
    }

    #[test]
    fn config_absolute_state_dir_is_rejected() {
        let yaml = replace(&minimal_yaml(), "dir: .derrick", "dir: /tmp/.derrick");

        assert_validation(&yaml, "state.dir");
    }

    #[test]
    fn config_foreman_executor_role_points_to_missing_role_is_rejected() {
        let yaml = replace(
            &minimal_yaml(),
            "pipeline: []",
            "pipeline:\n  - id: foreman\n    runner: derrick\n    executor_role: missing\n",
        );

        assert_validation(&yaml, "executor_role");
    }

    #[test]
    fn config_unsupported_version_is_rejected() {
        let yaml = replace(&minimal_yaml(), "version: 1", "version: 2");

        assert_validation(&yaml, "unsupported config version 2");
    }

    #[test]
    fn config_missing_optional_sections_default_correctly() {
        let config =
            load_yaml(&minimal_yaml()).unwrap_or_else(|error| panic!("yaml should parse: {error}"));

        assert_eq!(
            config.tools().git().stacking().backend(),
            StackBackendKind::None
        );
        assert_eq!(config.tools().assay().on_split(), OnSplit::Reject);
        assert!(!config.tools().copilot().enabled());
        assert!(config.guardrails().forbid_paths().is_empty());
        assert!(config.guardrails().required_labels().is_empty());
    }

    #[test]
    fn config_unknown_field_is_rejected() {
        let dir = tempfile::tempdir().unwrap_or_else(|error| {
            panic!("failed to create temp dir: {error}");
        });
        let path = dir.path().join("derrick.yaml");
        let yaml = replace(&minimal_yaml(), "prefix: tst", "prefix: tst\n  typo: nope");
        write_file(&path, &yaml);

        match Config::load_from_path(&path) {
            Err(ConfigError::Syntax { line, message, .. }) => {
                assert_eq!(line, 6);
                assert!(message.contains("typo"));
            }
            other => panic!("expected syntax error, got {other:?}"),
        }
    }

    #[test]
    fn config_yaml_syntax_error_reports_line() {
        let dir = tempfile::tempdir().unwrap_or_else(|error| {
            panic!("failed to create temp dir: {error}");
        });
        let path = dir.path().join("derrick.yaml");
        write_file(&path, "version: 1\nsite:\n  name: [\n");

        match Config::load_from_path(&path) {
            Err(ConfigError::Syntax { line, .. }) => assert_eq!(line, 3),
            other => panic!("expected syntax error, got {other:?}"),
        }
    }

    #[test]
    fn config_io_error_reports_path() {
        let dir = tempfile::tempdir().unwrap_or_else(|error| {
            panic!("failed to create temp dir: {error}");
        });
        let path = dir.path().join("missing.yaml");

        match Config::load_from_path(&path) {
            Err(ConfigError::Io {
                path: error_path, ..
            }) => assert_eq!(error_path, path),
            other => panic!("expected io error, got {other:?}"),
        }
    }

    #[test]
    fn config_load_layered_without_files_returns_defaults() {
        let _guard = HOME_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let old_home = env::var_os("HOME");
        // SAFETY: single-threaded test context serialised by HOME_LOCK.
        unsafe { env::remove_var("HOME") };
        let repo = tempfile::tempdir().unwrap_or_else(|error| {
            panic!("failed to create temp repo: {error}");
        });

        let config = Config::load_layered(repo.path())
            .unwrap_or_else(|error| panic!("defaults should layer: {error}"));
        if let Some(old_home) = old_home {
            // SAFETY: single-threaded test context serialised by HOME_LOCK.
            unsafe { env::set_var("HOME", old_home) };
        }

        assert_eq!(config.site().prefix(), "drk");
        assert_eq!(config.roles().get("executor"), Some("copilot"));
    }

    #[test]
    fn config_layered_load_overrides_correctly() {
        let _guard = HOME_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = tempfile::tempdir().unwrap_or_else(|error| {
            panic!("failed to create temp home: {error}");
        });
        let derrick_dir = home.path().join(".derrick");
        fs::create_dir(&derrick_dir).unwrap_or_else(|error| {
            panic!("failed to create {}: {error}", derrick_dir.display());
        });
        write_file(
            &derrick_dir.join("config.yaml"),
            r#"
models:
  local:
    provider: ollama
    model: llama3.3:70b
roles:
  drafter: local
parallelism:
  batch_max: 12
tools:
  assay:
    reviewers: [reviewer, drafter]
guardrails:
  forbid_paths: [secrets]
"#,
        );
        let repo = tempfile::tempdir().unwrap_or_else(|error| {
            panic!("failed to create temp repo: {error}");
        });
        write_file(
            &repo.path().join("derrick.yaml"),
            r#"
site:
  name: repo-site
roles:
  reviewer: local
parallelism:
  step_max: 9
tools:
  assay:
    reviewers: [reviewer]
guardrails:
  forbid_paths: []
  required_labels: [feature]
"#,
        );

        let old_home = env::var_os("HOME");
        // SAFETY: single-threaded test context serialised by HOME_LOCK.
        unsafe { env::set_var("HOME", home.path()) };
        let config = Config::load_layered(repo.path())
            .unwrap_or_else(|error| panic!("layered config should load: {error}"));
        if let Some(old_home) = old_home {
            // SAFETY: single-threaded test context serialised by HOME_LOCK.
            unsafe { env::set_var("HOME", old_home) };
        } else {
            // SAFETY: single-threaded test context serialised by HOME_LOCK.
            unsafe { env::remove_var("HOME") };
        }

        assert_eq!(config.site().name(), "repo-site");
        assert_eq!(config.roles().get("drafter"), Some("local"));
        assert_eq!(config.roles().get("reviewer"), Some("local"));
        assert!(config.models().get("local").is_some());
        assert_eq!(config.parallelism().batch_max(), 12);
        assert_eq!(config.parallelism().step_max(), 9);
        assert_eq!(config.tools().assay().reviewers(), &["reviewer".to_owned()]);
        assert!(config.guardrails().forbid_paths().is_empty());
        assert_eq!(
            config.guardrails().required_labels(),
            &["feature".to_owned()]
        );
    }

    #[test]
    fn config_public_accessors_cover_full_schema() {
        let yaml = r#"
version: 1
site:
  name: full-site
  prefix: full
models:
  full:
    provider: azure-openai
    model: gpt-5
    cli: "az ai"
    endpoint: "https://example.test"
    region: eu-west-2
    deployment: prod
    base_url: "https://base.example.test"
    max_tokens: 4096
    temperature: 0.2
    cache: true
    timeout: 30s
    rate_limit: 10/s
    cost_hint: high
roles:
  drafter: full
  executor: full
tools:
  speckit:
    enabled: false
    version: ">=1.0.0"
  assay:
    enabled: false
    role: drafter
    reviewers: [drafter]
    rounds: "2"
    strict: true
    on_split: human
  substrate:
    backend: none
    mode: copilot
  copilot:
    enabled: true
    agent_identity: custom-hand
  git:
    branch_prefix: "feature"
    stacking:
      backend: native
      branch_pattern: "stack/{{ticket_id}}"
      auto_restack_on_merge: false
      force_push: off
      auto_pr: true
      draft: true
pipeline:
  - id: draft
    role: drafter
    host: copilot
    command: "draft {{prompt}}"
    inputs: [one, two]
    skippable: true
    default_skip: true
    on_failure: retry
    poll_interval: 30s
    parallel_group: writing
  - id: dispatch
    runner: derrick
    rounds: 3
    on_reject: warn
    batch: "{{batch}}"
    executor_role: executor
guardrails:
  constitution_path: docs/constitution.md
  forbid_paths: [secrets, target]
  required_labels: [feature, safe]
parallelism:
  batch_max: 16
  step_max: 8
  assay_max: 3
state:
  dir: state
  log_runs: false
  worktree_root: state/worktrees
"#;
        let config =
            load_yaml(yaml).unwrap_or_else(|error| panic!("full yaml should parse: {error}"));
        let model = config
            .models()
            .get("full")
            .unwrap_or_else(|| panic!("full model should exist"));
        let draft = &config.pipeline()[0];
        let dispatch = &config.pipeline()[1];

        assert_eq!(config.version(), 1);
        assert_eq!(config.site().name(), "full-site");
        assert_eq!(config.site().prefix(), "full");
        assert_eq!(config.models().as_map().len(), 1);
        assert_eq!(model.provider(), Some("azure-openai"));
        // No explicit runtime → derived from provider; azure-openai is not a
        // legacy host alias, so it passes through as its own runtime (D79).
        assert_eq!(model.runtime(), None);
        assert_eq!(model.resolved_runtime(), "azure-openai");
        assert_eq!(model.model(), "gpt-5");
        assert_eq!(model.cli(), Some("az ai"));
        // endpoint/base_url are retained post-D79 for non-CLI runtimes; only
        // region/deployment remain parsed-and-ignored.
        assert_eq!(model.endpoint(), Some("https://example.test"));
        assert_eq!(model.base_url(), Some("https://base.example.test"));
        assert_eq!(model.max_tokens(), Some(4096));
        assert_eq!(model.temperature(), Some(0.2));
        assert_eq!(model.cache(), Some(true));
        assert_eq!(model.timeout(), Some("30s"));
        assert_eq!(model.rate_limit(), Some("10/s"));
        assert_eq!(model.cost_hint(), Some("high"));
        assert_eq!(config.roles().as_map().len(), 2);
        assert_eq!(config.roles().get("drafter"), Some("full"));

        assert!(!config.tools().speckit().enabled());
        assert_eq!(config.tools().speckit().version(), ">=1.0.0");
        assert!(!config.tools().assay().enabled());
        assert_eq!(config.tools().assay().role(), "drafter");
        assert_eq!(config.tools().assay().reviewers(), &["drafter".to_owned()]);
        assert_eq!(config.tools().assay().rounds(), "2");
        assert!(config.tools().assay().strict());
        assert_eq!(config.tools().assay().on_split(), OnSplit::Human);
        assert_eq!(
            config.tools().substrate().backend(),
            SubstrateBackendKind::None
        );
        assert_eq!(config.tools().substrate().mode(), SubstrateMode::Copilot);
        assert!(config.tools().copilot().enabled());
        assert_eq!(config.tools().copilot().agent_identity(), "custom-hand");
        assert_eq!(
            config.tools().git().stacking().backend(),
            StackBackendKind::Native
        );
        assert_eq!(
            config.tools().git().stacking().branch_pattern(),
            "stack/{{ticket_id}}"
        );
        assert!(!config.tools().git().stacking().auto_restack_on_merge());
        assert_eq!(config.tools().git().stacking().force_push(), ForcePush::Off);
        assert!(config.tools().git().stacking().auto_pr());
        assert!(config.tools().git().stacking().draft());
        assert_eq!(config.tools().git().branch_prefix(), "feature");

        assert_eq!(draft.id(), "draft");
        assert_eq!(draft.role(), Some("drafter"));
        assert_eq!(draft.runner(), None);
        assert_eq!(draft.host(), Some(Host::Copilot));
        assert_eq!(draft.command(), Some("draft {{prompt}}"));
        assert_eq!(draft.inputs(), &["one".to_owned(), "two".to_owned()]);
        assert!(draft.skippable());
        assert!(draft.default_skip());
        assert_eq!(draft.prompt(), None);
        assert_eq!(draft.rounds(), None);
        assert_eq!(draft.on_reject(), None);
        assert_eq!(draft.on_failure(), Some(OnFailure::Retry));
        assert_eq!(draft.poll_interval(), Some("30s"));
        assert_eq!(draft.batch(), None);
        assert_eq!(draft.executor_role(), None);
        assert_eq!(draft.parallel_group(), Some("writing"));

        assert_eq!(dispatch.id(), "dispatch");
        assert_eq!(dispatch.runner(), Some(Runner::Derrick));
        assert_eq!(dispatch.rounds(), Some("3"));
        assert_eq!(dispatch.on_reject(), Some(OnReject::Warn));
        assert_eq!(dispatch.batch(), Some("{{batch}}"));
        assert_eq!(dispatch.executor_role(), Some("executor"));
        assert_eq!(
            config.guardrails().constitution_path(),
            Path::new("docs/constitution.md")
        );
        assert_eq!(
            config.guardrails().forbid_paths(),
            &["secrets".to_owned(), "target".to_owned()]
        );
        assert_eq!(
            config.guardrails().required_labels(),
            &["feature".to_owned(), "safe".to_owned()]
        );
        assert_eq!(config.parallelism().batch_max(), 16);
        assert_eq!(config.parallelism().step_max(), 8);
        assert_eq!(config.parallelism().assay_max(), 3);
        assert_eq!(config.state().dir(), Path::new("state"));
        assert!(!config.state().log_runs());
        assert_eq!(config.state().worktree_root(), Path::new("state/worktrees"));
    }

    #[test]
    fn output_compression_defaults_to_enabled() {
        let tools = Tools::default();
        assert!(
            tools.output_compression().enabled(),
            "output_compression should be enabled by default"
        );
    }

    #[test]
    fn roughneck_defaults() {
        let tools = Tools::default();
        assert!(tools.roughneck().enabled());
        assert_eq!(tools.roughneck().level(), "full");
        assert!(!tools.roughneck().compress_memory());
    }

    #[test]
    fn roughneck_can_be_configured_via_yaml() {
        let _guard = HOME_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tmp: {e}"));
        let path = dir.path().join("derrick.yaml");
        let yaml = r#"
version: 1
site:
  name: test-site
  prefix: tst
models:
  claude-sonnet:
    provider: anthropic
    model: claude-sonnet-4-6
roles:
  drafter: claude-sonnet
tools:
  speckit:
    enabled: true
    version: ">=0.4.0"
  assay:
    enabled: false
    role: drafter
    reviewers: []
  substrate:
    backend: native
    mode: solo
  copilot:
    agent_identity: derrick-hand
  roughneck:
    enabled: false
    level: lite
    compress_memory: true
pipeline: []
guardrails:
  constitution_path: .specify/memory/constitution.md
parallelism:
  batch_max: 8
  step_max: 4
  assay_max: 2
state:
  dir: .derrick
  log_runs: true
  worktree_root: .derrick/worktrees
"#;
        write_file(&path, yaml);
        let config = Config::load_from_path(&path).expect("load config");
        assert!(!config.tools().roughneck().enabled());
        assert_eq!(config.tools().roughneck().level(), "lite");
        assert!(config.tools().roughneck().compress_memory());
    }

    #[test]
    fn output_compression_can_be_disabled_via_yaml() {
        let _guard = HOME_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("derrick.yaml");
        // Write a minimal config with output_compression.enabled: false nested
        // correctly under tools:.
        let yaml = r#"
version: 1
site:
  name: test-site
  prefix: tst
models:
  claude-sonnet:
    provider: anthropic
    model: claude-sonnet-4-6
roles:
  drafter: claude-sonnet
tools:
  speckit:
    enabled: true
    version: ">=0.4.0"
  assay:
    enabled: false
    role: drafter
    reviewers: []
  substrate:
    backend: native
    mode: solo
  copilot:
    agent_identity: derrick-hand
  output_compression:
    enabled: false
pipeline: []
guardrails:
  constitution_path: .specify/memory/constitution.md
parallelism:
  batch_max: 8
  step_max: 4
  assay_max: 2
state:
  dir: .derrick
  log_runs: true
  worktree_root: .derrick/worktrees
"#;
        write_file(&path, yaml);
        let config = Config::load_from_path(&path).expect("load config");
        assert!(
            !config.tools().output_compression().enabled(),
            "output_compression.enabled should be false"
        );
    }

    // ---- spec provider (tools.specify) ----

    #[test]
    fn default_tools_use_speckit_spec_provider() {
        let tools = Tools::default();
        assert_eq!(tools.specify().provider(), SpecProviderKind::Speckit);
        assert_eq!(tools.specify().import().plan(), DownstreamMode::Native);
        assert_eq!(tools.specify().import().tasks(), DownstreamMode::Native);
        assert_eq!(tools.specify().import().source(), None);
    }

    #[test]
    fn omitting_specify_block_loads_as_speckit() {
        // `minimal_yaml()` carries no `tools.specify` — the back-compat path.
        let config = load_yaml(&minimal_yaml()).expect("load minimal config");
        assert_eq!(
            config.tools().specify().provider(),
            SpecProviderKind::Speckit,
            "an omitted tools.specify must default to the speckit provider"
        );
    }

    #[test]
    fn specify_provider_native_round_trips() {
        let yaml = minimal_yaml().replace(
            "  speckit:\n    enabled: true\n    version: \">=0.4.0\"\n",
            "  speckit:\n    enabled: true\n    version: \">=0.4.0\"\n  specify:\n    provider: native\n",
        );
        let config = load_yaml(&yaml).expect("load native provider");
        assert_eq!(
            config.tools().specify().provider(),
            SpecProviderKind::Native
        );
    }

    #[test]
    fn specify_import_block_round_trips() {
        let yaml = minimal_yaml().replace(
            "  speckit:\n    enabled: true\n    version: \">=0.4.0\"\n",
            "  speckit:\n    enabled: true\n    version: \">=0.4.0\"\n  \
             specify:\n    provider: import\n    import:\n      source: ./docs/spec.md\n      \
             plan: speckit\n      tasks: native\n",
        );
        let config = load_yaml(&yaml).expect("load import provider");
        let specify = config.tools().specify();
        assert_eq!(specify.provider(), SpecProviderKind::Import);
        assert_eq!(specify.import().source(), Some("./docs/spec.md"));
        assert_eq!(specify.import().plan(), DownstreamMode::Speckit);
        assert_eq!(specify.import().tasks(), DownstreamMode::Native);
    }

    #[test]
    fn specify_import_defaults_to_native_downstream() {
        // An `import` provider with no explicit plan/tasks modes defaults both
        // to native.
        let yaml = minimal_yaml().replace(
            "  speckit:\n    enabled: true\n    version: \">=0.4.0\"\n",
            "  speckit:\n    enabled: true\n    version: \">=0.4.0\"\n  specify:\n    provider: import\n",
        );
        let config = load_yaml(&yaml).expect("load import provider");
        assert_eq!(
            config.tools().specify().import().plan(),
            DownstreamMode::Native
        );
        assert_eq!(
            config.tools().specify().import().tasks(),
            DownstreamMode::Native
        );
    }

    #[test]
    fn force_import_spec_overrides_provider_and_source() {
        // The `--spec <path>` CLI override forces the import provider + source
        // in memory, leaving the downstream plan/tasks modes untouched.
        let yaml = minimal_yaml().replace(
            "  speckit:\n    enabled: true\n    version: \">=0.4.0\"\n",
            "  speckit:\n    enabled: true\n    version: \">=0.4.0\"\n  specify:\n    \
             provider: speckit\n    import:\n      plan: speckit\n      tasks: native\n",
        );
        let mut config = load_yaml(&yaml).expect("load speckit provider");
        assert_eq!(
            config.tools().specify().provider(),
            SpecProviderKind::Speckit
        );
        config.force_import_spec("docs/PRD.md".to_owned());
        let specify = config.tools().specify();
        assert_eq!(specify.provider(), SpecProviderKind::Import);
        assert_eq!(specify.import().source(), Some("docs/PRD.md"));
        // Downstream modes are preserved from the config file.
        assert_eq!(specify.import().plan(), DownstreamMode::Speckit);
        assert_eq!(specify.import().tasks(), DownstreamMode::Native);
    }

    #[test]
    fn unknown_spec_provider_is_actionable_error() {
        let yaml = minimal_yaml().replace(
            "  speckit:\n    enabled: true\n    version: \">=0.4.0\"\n",
            "  speckit:\n    enabled: true\n    version: \">=0.4.0\"\n  specify:\n    provider: bogus\n",
        );
        match load_yaml(&yaml) {
            Err(ConfigError::Validation(message)) => {
                assert!(
                    message.contains("tools.specify.provider")
                        && message.contains("speckit | native | import"),
                    "expected actionable provider error, got: {message}"
                );
            }
            other => panic!("expected validation error, got {other:?}"),
        }
    }

    #[test]
    fn unknown_downstream_mode_is_actionable_error() {
        let yaml = minimal_yaml().replace(
            "  speckit:\n    enabled: true\n    version: \">=0.4.0\"\n",
            "  speckit:\n    enabled: true\n    version: \">=0.4.0\"\n  \
             specify:\n    provider: import\n    import:\n      plan: nonsense\n",
        );
        match load_yaml(&yaml) {
            Err(ConfigError::Validation(message)) => {
                assert!(
                    message.contains("tools.specify.import.plan")
                        && message.contains("native | speckit | import"),
                    "expected actionable downstream-mode error, got: {message}"
                );
            }
            other => panic!("expected validation error, got {other:?}"),
        }
    }

    #[test]
    fn specify_unknown_field_is_rejected() {
        // `deny_unknown_fields` on the specify block guards against typos.
        let yaml = minimal_yaml().replace(
            "  speckit:\n    enabled: true\n    version: \">=0.4.0\"\n",
            "  speckit:\n    enabled: true\n    version: \">=0.4.0\"\n  specify:\n    providr: speckit\n",
        );
        assert!(
            load_yaml(&yaml).is_err(),
            "an unknown field under tools.specify must be rejected"
        );
    }

    // D86 — profile stage names must be folded to canonical role keys so that
    // `--profile speed` actually updates the roles the pipeline reads.
    #[test]
    fn d86_profile_stage_names_fold_to_canonical_roles() {
        // Use `cli-defaults` preset: it wires `proposer → strong`, `drafter → fast`,
        // `executor → executor`, and provides the `fast` / `strong` model aliases.
        let yaml = assemble("ai:\n  preset: cli-defaults");
        let config = load_yaml(&yaml).expect("preset should parse");

        // Baseline: proposer uses the `strong` alias, drafter uses `fast`.
        assert_eq!(config.roles().get("proposer"), Some("strong"));
        assert_eq!(config.roles().get("drafter"), Some("fast"));

        // Applying the `quality` profile should update proposer → strong (no change)
        // and drafter → strong (the change that proves folding works).
        let quality = config
            .with_profile("quality")
            .expect("quality profile should apply");
        assert_eq!(
            quality.roles().get("proposer"),
            Some("strong"),
            "plan/analyze stages should fold to the `proposer` role"
        );
        assert_eq!(
            quality.roles().get("drafter"),
            Some("strong"),
            "tasks/specify stages should fold to the `drafter` role"
        );

        // Applying the `speed` profile should update both proposer and drafter → fast.
        let speed = config
            .with_profile("speed")
            .expect("speed profile should apply");
        assert_eq!(
            speed.roles().get("proposer"),
            Some("fast"),
            "plan stage should fold to the `proposer` role"
        );
        assert_eq!(
            speed.roles().get("drafter"),
            Some("fast"),
            "tasks stage should fold to the `drafter` role"
        );
        assert_eq!(speed.active_profile(), Some("speed"));
    }

    // D86 — user-defined stage names that are not in the fold table pass through
    // unchanged so custom pipeline roles can be targeted directly.
    #[test]
    fn d86_unknown_stage_name_passes_through_as_role_key() {
        let yaml = assemble("ai:\n  preset: cli-defaults");
        let config = load_yaml(&yaml).expect("preset should parse");
        // `proposer` is not a built-in stage name — it IS a role name.  A
        // user-defined profile targeting `proposer` directly should keep working.
        let yaml_with_profile =
            assemble("ai:\n  preset: cli-defaults\nprofiles:\n  custom:\n    proposer: fast\n");
        let cfg = load_yaml(&yaml_with_profile).expect("user-defined profile should parse");
        let applied = cfg
            .with_profile("custom")
            .expect("custom profile should apply");
        assert_eq!(
            applied.roles().get("proposer"),
            Some("fast"),
            "targeting a role name directly in a user-defined profile should work"
        );
        let _ = config; // silence unused warning
    }
}
