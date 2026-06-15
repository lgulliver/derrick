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

use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

const CONFIG_VERSION: u32 = 1;

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
            ModelDef {
                provider: "claude".to_owned(),
                model: "claude-opus-4-8".to_owned(),
                cli: None,
                max_tokens: None,
                temperature: None,
                cache: None,
                timeout: None,
                rate_limit: None,
                cost_hint: None,
            },
        );
        models.insert(
            "claude-sonnet".to_owned(),
            ModelDef {
                provider: "claude".to_owned(),
                model: "claude-sonnet-4-6".to_owned(),
                cli: None,
                max_tokens: None,
                temperature: None,
                cache: None,
                timeout: None,
                rate_limit: None,
                cost_hint: None,
            },
        );
        models.insert(
            "claude-haiku".to_owned(),
            ModelDef {
                provider: "claude".to_owned(),
                model: "claude-haiku-4-5".to_owned(),
                cli: None,
                max_tokens: None,
                temperature: None,
                cache: None,
                timeout: None,
                rate_limit: None,
                cost_hint: None,
            },
        );
        models.insert(
            "codex-gpt5".to_owned(),
            ModelDef {
                provider: "codex".to_owned(),
                model: "gpt-5.5".to_owned(),
                cli: None,
                max_tokens: None,
                temperature: None,
                cache: None,
                timeout: None,
                rate_limit: None,
                cost_hint: None,
            },
        );
        models.insert(
            "copilot".to_owned(),
            ModelDef {
                provider: "copilot".to_owned(),
                // `auto` (D67): the foreman selects the best model within the
                // copilot host per ticket by complexity.
                model: "auto".to_owned(),
                cli: None,
                max_tokens: None,
                temperature: None,
                cache: None,
                timeout: None,
                rate_limit: None,
                cost_hint: None,
            },
        );

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

/// A model provider entry from the `models` section.
///
/// Post-D65 the inference path is host-delegated, so the direct-API fields
/// (`endpoint`/`region`/`deployment`/`base_url`) are gone and `cli` is
/// deprecated — it is still parsed (and used by the `shell` escape hatch) but
/// ignored for host providers.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelDef {
    provider: String,
    model: String,
    cli: Option<String>,
    max_tokens: Option<u32>,
    temperature: Option<f64>,
    cache: Option<bool>,
    timeout: Option<String>,
    rate_limit: Option<String>,
    cost_hint: Option<String>,
}

impl ModelDef {
    /// Returns the provider identifier.
    pub fn provider(&self) -> &str {
        &self.provider
    }

    /// Returns the provider-specific model name.
    pub fn model(&self) -> &str {
        &self.model
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

    serde_yaml::from_str(&source).map_err(|source: serde_yaml::Error| {
        let line = source.location().map_or(0, |location| location.line());
        ConfigError::Syntax {
            path: path.to_path_buf(),
            line,
            message: source.to_string(),
        }
    })
}

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

fn validate_pipeline(steps: &[PipelineStep], roles: &RoleBindings) -> Result<(), ConfigError> {
    for step in steps {
        match (&step.role, step.runner) {
            (Some(_), Some(_)) => {
                return validation(format!(
                    "pipeline.{}: role and runner are mutually exclusive",
                    step.id
                ));
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

fn parse_force_push(value: &str) -> Result<ForcePush, ConfigError> {
    match value {
        "with-lease" => Ok(ForcePush::WithLease),
        "off" => Ok(ForcePush::Off),
        other => validation(format!(
            "tools.git.stacking.force_push: {other:?} must be one of with-lease | off"
        )),
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigLayer {
    version: Option<u32>,
    site: Option<SiteLayer>,
    models: Option<HashMap<String, ModelDefLayer>>,
    roles: Option<HashMap<String, String>>,
    tools: Option<ToolsLayer>,
    pipeline: Option<Vec<PipelineStepLayer>>,
    guardrails: Option<GuardrailsLayer>,
    parallelism: Option<ParallelismLayer>,
    state: Option<StateLayer>,
}

impl ConfigLayer {
    fn merge(&mut self, other: Self) {
        if other.version.is_some() {
            self.version = other.version;
        }
        merge_nested(&mut self.site, other.site, SiteLayer::merge);
        merge_map(&mut self.models, other.models);
        merge_map(&mut self.roles, other.roles);
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
    }

    fn finalize(self) -> Result<Config, ConfigError> {
        let tools = self.tools.unwrap_or_default().finalize()?;
        let guardrails = required(self.guardrails, "guardrails")?.finalize();
        let pipeline = self
            .pipeline
            .unwrap_or_default()
            .into_iter()
            .map(PipelineStepLayer::finalize)
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Config {
            version: required(self.version, "version")?,
            site: required(self.site, "site")?.finalize()?,
            models: ModelRegistry(
                required(self.models, "models")?
                    .into_iter()
                    .map(|(name, model)| Ok((name, model.finalize()?)))
                    .collect::<Result<HashMap<_, _>, ConfigError>>()?,
            ),
            roles: RoleBindings(required(self.roles, "roles")?),
            tools,
            pipeline,
            guardrails,
            parallelism: required(self.parallelism, "parallelism")?.finalize()?,
            state: required(self.state, "state")?.finalize()?,
        })
    }
}

impl From<Config> for ConfigLayer {
    fn from(config: Config) -> Self {
        Self {
            version: Some(config.version),
            site: Some(config.site.into()),
            models: Some(
                config
                    .models
                    .0
                    .into_iter()
                    .map(|(name, model)| (name, model.into()))
                    .collect(),
            ),
            roles: Some(config.roles.0),
            tools: Some(config.tools.into()),
            pipeline: Some(config.pipeline.into_iter().map(Into::into).collect()),
            guardrails: Some(config.guardrails.into()),
            parallelism: Some(config.parallelism.into()),
            state: Some(config.state.into()),
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

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelDefLayer {
    provider: Option<String>,
    model: Option<String>,
    cli: Option<String>,
    // Removed in D65 (direct-API fields). Still accepted on the wire so that
    // pre-D65 `derrick.yaml` files keep loading; dropped at finalize with a
    // one-line warning. No CONFIG_VERSION bump.
    #[serde(default)]
    endpoint: Option<String>,
    #[serde(default)]
    region: Option<String>,
    #[serde(default)]
    deployment: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    max_tokens: Option<u32>,
    temperature: Option<f64>,
    cache: Option<bool>,
    timeout: Option<String>,
    rate_limit: Option<String>,
    cost_hint: Option<String>,
}

/// Maps a legacy provider name to its D65 host-delegated equivalent.
///
/// One-release compatibility shim so pinned user `derrick.yaml` files that
/// still name the pre-D65 providers continue to load. Returns the input
/// unchanged when it is not a known legacy alias.
fn canonical_provider(provider: &str) -> &str {
    match provider {
        "copilot-cli" => "copilot",
        "anthropic" => "claude",
        "openai-cli" => "codex",
        other => other,
    }
}

impl ModelDefLayer {
    fn finalize(self) -> Result<ModelDef, ConfigError> {
        let raw_provider = required(self.provider, "models.*.provider")?;
        let provider = canonical_provider(&raw_provider).to_owned();
        if provider != raw_provider {
            tracing::warn!(
                target: "derrick_config",
                "provider `{raw_provider}` is a pre-D65 alias; treating it as `{provider}`. \
                 Update your config to the host name."
            );
        }
        if self.endpoint.is_some()
            || self.region.is_some()
            || self.deployment.is_some()
            || self.base_url.is_some()
        {
            tracing::warn!(
                target: "derrick_config",
                "models.*.{{endpoint,region,deployment,base_url}} are removed since D65 \
                 (host-CLI-only routing); ignoring them. Remove these fields from your config."
            );
        }
        Ok(ModelDef {
            provider,
            model: required(self.model, "models.*.model")?,
            cli: self.cli,
            max_tokens: self.max_tokens,
            temperature: self.temperature,
            cache: self.cache,
            timeout: self.timeout,
            rate_limit: self.rate_limit,
            cost_hint: self.cost_hint,
        })
    }
}

impl From<ModelDef> for ModelDefLayer {
    fn from(model: ModelDef) -> Self {
        Self {
            provider: Some(model.provider),
            model: Some(model.model),
            cli: model.cli,
            endpoint: None,
            region: None,
            deployment: None,
            base_url: None,
            max_tokens: model.max_tokens,
            temperature: model.temperature,
            cache: model.cache,
            timeout: model.timeout,
            rate_limit: model.rate_limit,
            cost_hint: model.cost_hint,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolsLayer {
    speckit: Option<SpeckitLayer>,
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
        assert_eq!(model.provider(), "azure-openai");
        assert_eq!(model.model(), "gpt-5");
        assert_eq!(model.cli(), Some("az ai"));
        // endpoint/region/deployment/base_url are still accepted on the wire
        // (the YAML above sets them) but are dropped at finalize per D65, so
        // they are no longer queryable on ModelDef.
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
}
