//! `derrick models check` — validate configured models, role bindings, and
//! stage capability requirements against the runtime registry and host
//! catalogue (D79, generalising D65).
//!
//! The shared [`models_check_core`] is reused by `derrick doctor` and by the
//! soft (WARN-only) checks emitted at `derrick init` and `derrick run`, so the
//! three never drift.

use derrick_config::{Config, KNOWN_RUNTIMES, ModelDef, cli_host_for_runtime};
use derrick_tools::{HostRegistry, ModelChoice, catalogue, parse_model_choice};
use serde_json::json;

use crate::commands::ModelsArgs;
use crate::commands::ModelsCommand;
use crate::exit_code::CliExitCode;
use crate::output::OutputFormat;

/// Severity of a single model-check finding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CheckLevel {
    Pass,
    Warn,
    Fail,
}

impl CheckLevel {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Warn => "warn",
            Self::Fail => "fail",
        }
    }
}

/// One finding produced by [`models_check_core`].
pub(crate) struct ModelCheck {
    /// Subject of the check, e.g. `role drafter → claude-opus`.
    pub(crate) subject: String,
    pub(crate) level: CheckLevel,
    pub(crate) message: String,
}

impl ModelCheck {
    fn pass(subject: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            subject: subject.into(),
            level: CheckLevel::Pass,
            message: message.into(),
        }
    }

    fn warn(subject: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            subject: subject.into(),
            level: CheckLevel::Warn,
            message: message.into(),
        }
    }

    fn fail(subject: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            subject: subject.into(),
            level: CheckLevel::Fail,
            message: message.into(),
        }
    }
}

/// Validates every configured model, role binding, and stage requirement
/// against the runtime registry and host catalogue (D79).
///
/// Three passes:
///
/// 1. **Every configured model** is validated on its own (whether or not a role
///    binds it). The runtime decides the rules: a `*-cli` runtime checks host
///    availability + catalogue (WARN-only on unknown ids); `shell` WARNs; API
///    and local runtimes check `auth_env`/`base_url`. An unknown runtime FAILs.
/// 2. **Every role binding** must resolve to a known model (FAIL otherwise).
/// 3. **Every stage `requires:`** is checked against the bound model's declared
///    capabilities: explicitly `false` → FAIL; declared `true` → PASS;
///    undeclared → WARN (never blocks).
pub(crate) fn models_check_core(config: &Config) -> Vec<ModelCheck> {
    let registry = HostRegistry::with_defaults();
    models_check_core_with(config, &|host| {
        registry
            .get(host)
            .is_some_and(derrick_tools::HostAdapter::is_available)
    })
}

/// Validation with an injectable host-availability probe (real env auth probe).
fn models_check_core_with(
    config: &Config,
    host_available: &dyn Fn(&str) -> bool,
) -> Vec<ModelCheck> {
    models_check_core_with_probes(config, host_available, &|var| {
        std::env::var_os(var).is_some()
    })
}

/// Validation with injectable host-availability AND env-presence probes, so
/// tests need not depend on PATH or process environment.
fn models_check_core_with_probes(
    config: &Config,
    host_available: &dyn Fn(&str) -> bool,
    env_present: &dyn Fn(&str) -> bool,
) -> Vec<ModelCheck> {
    let mut checks = Vec::new();

    // Pass 1: validate every configured model in stable (name-sorted) order.
    let mut models: Vec<(&str, &ModelDef)> = config
        .models()
        .as_map()
        .iter()
        .map(|(name, def)| (name.as_str(), def))
        .collect();
    models.sort_unstable_by_key(|(name, _)| *name);

    for (model_name, model_def) in models {
        let subject = format!("model {model_name}");
        checks.push(check_model(subject, model_def, host_available, env_present));
    }

    // Pass 2: every role binding must resolve to a known model.
    let mut roles: Vec<(&str, &str)> = config
        .roles()
        .as_map()
        .iter()
        .map(|(role, model)| (role.as_str(), model.as_str()))
        .collect();
    roles.sort_unstable();

    for (role, model_name) in roles {
        if config.models().get(model_name).is_none() {
            checks.push(ModelCheck::fail(
                format!("role {role} → {model_name}"),
                format!("role `{role}` binds unknown model `{model_name}`"),
            ));
        }
    }

    // Pass 3: stage capability requirements (D79).
    for (stage, requires) in config.stage_requirements() {
        let Some(model_name) = config.roles().get(stage) else {
            continue;
        };
        let Some(model_def) = config.models().get(model_name) else {
            continue; // pass 2 already FAILed the missing binding.
        };
        for capability in requires {
            let subject = format!("stage {stage} requires {capability}");
            match model_def
                .capabilities()
                .and_then(|caps| caps.declared(capability))
            {
                Some(true) => checks.push(ModelCheck::pass(
                    subject,
                    format!("model `{model_name}` supports `{capability}`"),
                )),
                Some(false) => checks.push(ModelCheck::fail(
                    subject,
                    format!(
                        "model `{model_name}` declares `{capability}=false` but stage \
                         `{stage}` requires it"
                    ),
                )),
                None => checks.push(ModelCheck::warn(
                    subject,
                    format!(
                        "model `{model_name}` does not declare `{capability}`; \
                         passing through unverified"
                    ),
                )),
            }
        }
    }

    checks
}

/// Validates a single configured model, dispatching on its resolved runtime.
fn check_model(
    subject: String,
    model_def: &ModelDef,
    host_available: &dyn Fn(&str) -> bool,
    env_present: &dyn Fn(&str) -> bool,
) -> ModelCheck {
    let runtime = model_def.resolved_runtime();

    // An unknown runtime is a genuine blocker (typo or missing registration).
    if !KNOWN_RUNTIMES.contains(&runtime.as_str()) {
        return ModelCheck::fail(subject, format!("runtime `{runtime}` does not exist"));
    }

    // `shell` is an approved escape hatch, not a managed runtime — can't verify.
    if runtime == "shell" {
        return ModelCheck::warn(
            subject,
            "shell: unmanaged escape-hatch runtime — derrick cannot validate its auth/model",
        );
    }

    match cli_host_for_runtime(&runtime) {
        Some(host) => check_cli_model(subject, host, &runtime, model_def.model(), host_available),
        None => check_api_model(subject, &runtime, model_def, env_present),
    }
}

/// Validates a CLI-runtime model against host availability + the catalogue.
fn check_cli_model(
    subject: String,
    host: &str,
    runtime: &str,
    model_id: &str,
    host_available: &dyn Fn(&str) -> bool,
) -> ModelCheck {
    // Host binary installed — required even for `auto` (the foreman dispatches
    // through it, D67).
    if !host_available(host) {
        return ModelCheck::fail(
            subject,
            format!("runtime `{runtime}` host `{host}` is not installed on PATH"),
        );
    }

    // `auto`/`auto:<tier>` is foreman-selected per ticket; no single id to check.
    if matches!(parse_model_choice(model_id), ModelChoice::Auto { .. }) {
        return ModelCheck::pass(
            subject,
            format!("auto: foreman selects per-ticket within runtime `{runtime}`"),
        );
    }

    let model_id = model_id.trim();
    if (host == "opencode" || host == "aider") && !model_id.contains('/') {
        return ModelCheck::warn(
            subject,
            format!("runtime `{runtime}` expects a `provider/model` id; `{model_id}` has no `/`"),
        );
    }

    let normalized = catalogue::normalize(host, model_id);
    if catalogue::is_known(host, &normalized) {
        ModelCheck::pass(subject, format!("`{model_id}` is a known `{host}` model"))
    } else {
        ModelCheck::warn(
            subject,
            format!(
                "`{model_id}` is not in the curated `{host}` catalogue; \
                 passing it through unverified"
            ),
        )
    }
}

/// Validates an API/local-runtime model: auth + endpoint preconditions (D79).
fn check_api_model(
    subject: String,
    runtime: &str,
    model_def: &ModelDef,
    env_present: &dyn Fn(&str) -> bool,
) -> ModelCheck {
    let requires_auth = matches!(runtime, "anthropic-api" | "openai-api");
    match model_def.auth_env() {
        Some(env) if !env_present(env) => {
            return ModelCheck::fail(
                subject,
                format!("auth env `{env}` is not set for runtime `{runtime}`"),
            );
        }
        None if requires_auth => {
            return ModelCheck::fail(
                subject,
                format!("runtime `{runtime}` requires an `auth_env` naming its API-key env var"),
            );
        }
        _ => {}
    }

    if runtime == "openai-compatible"
        && model_def.base_url().is_none()
        && model_def.endpoint().is_none()
    {
        return ModelCheck::fail(
            subject,
            "runtime `openai-compatible` requires a `base_url` (or `endpoint`)".to_owned(),
        );
    }

    ModelCheck::pass(
        subject,
        format!("runtime `{runtime}` configured; model id passed through unverified"),
    )
}

/// Number of FAIL-level findings.
pub(crate) fn fail_count(checks: &[ModelCheck]) -> usize {
    checks
        .iter()
        .filter(|check| check.level == CheckLevel::Fail)
        .count()
}

/// Emits WARN/FAIL findings to the log as a soft pre-flight check.
///
/// Used by `derrick init` and `derrick run` (D15): issues surface early but
/// never block.
pub(crate) fn emit_soft_warnings(config: &Config) {
    for check in models_check_core(config) {
        match check.level {
            CheckLevel::Fail => tracing::warn!(
                target: "derrick_cli::models",
                "models check FAIL: {} — {}", check.subject, check.message
            ),
            CheckLevel::Warn => tracing::warn!(
                target: "derrick_cli::models",
                "models check warning: {} — {}", check.subject, check.message
            ),
            CheckLevel::Pass => {}
        }
    }
}

pub(crate) async fn execute(args: ModelsArgs) -> Result<CliExitCode, crate::CliError> {
    match args.command {
        ModelsCommand::Check(check_args) => {
            let repo_root = crate::current_repo_root()?;
            let config = Config::load_layered(&repo_root)
                .map_err(|error| crate::message(error.to_string()))?;
            let checks = models_check_core(&config);
            print_checks(&checks, check_args.format)?;
            Ok(CliExitCode::DoctorFailures(fail_count(&checks)))
        }
    }
}

fn print_checks(checks: &[ModelCheck], format: OutputFormat) -> Result<(), crate::CliError> {
    match format {
        OutputFormat::Human => {
            for check in checks {
                println!(
                    "{:<5} {:<32} {}",
                    check.level.as_str(),
                    check.subject,
                    check.message
                );
            }
        }
        OutputFormat::Json => {
            let rows = checks
                .iter()
                .map(|check| {
                    json!({
                        "subject": check.subject,
                        "status": check.level.as_str(),
                        "message": check.message,
                    })
                })
                .collect::<Vec<_>>();
            println!("{}", serde_json::to_string(&rows)?);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn config_with_model(provider: &str, model: &str) -> Config {
        let yaml = format!(
            r#"
version: 1
site:
  name: test
  prefix: tst
models:
  m:
    provider: {provider}
    model: {model}
roles:
  drafter: m
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
        );
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("derrick.yaml");
        let mut file = std::fs::File::create(&path).expect("create");
        file.write_all(yaml.as_bytes()).expect("write");
        Config::load_from_path(&path).expect("config should load")
    }

    /// Builds a config whose role binds a valid host model `bound`, plus an
    /// extra model `extra` that NO role references. Used to prove pass 1
    /// validates configured-but-unbound models.
    fn config_with_unbound_model(
        bound_provider: &str,
        bound_model: &str,
        extra_provider: &str,
        extra_model: &str,
    ) -> Config {
        let yaml = format!(
            r#"
version: 1
site:
  name: test
  prefix: tst
models:
  bound:
    provider: {bound_provider}
    model: {bound_model}
  extra:
    provider: {extra_provider}
    model: {extra_model}
roles:
  drafter: bound
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
        );
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("derrick.yaml");
        let mut file = std::fs::File::create(&path).expect("create");
        file.write_all(yaml.as_bytes()).expect("write");
        Config::load_from_path(&path).expect("config should load")
    }

    #[test]
    fn host_missing_is_fail() {
        let config = config_with_model("claude", "claude-opus-4-8");
        let checks = models_check_core_with(&config, &|_| false);
        assert!(
            checks
                .iter()
                .any(|c| c.level == CheckLevel::Fail && c.message.contains("not installed"))
        );
    }

    #[test]
    fn unknown_model_is_warn_not_fail() {
        // A host that IS installed but with an off-catalogue model id must WARN.
        let config = config_with_model("claude", "claude-opus-4-7");
        let checks = models_check_core_with(&config, &|_| true);
        assert!(checks.iter().any(|c| c.level == CheckLevel::Warn));
        assert!(
            !checks.iter().any(|c| c.level == CheckLevel::Fail),
            "an unknown model id must never FAIL (hybrid validation)"
        );
    }

    #[test]
    fn known_model_is_pass() {
        let config = config_with_model("codex", "gpt-5.5");
        let checks = models_check_core_with(&config, &|_| true);
        assert!(checks.iter().any(|c| c.level == CheckLevel::Pass));
        assert_eq!(fail_count(&checks), 0);
    }

    #[test]
    fn auto_model_is_pass_via_parse_model_choice() {
        // `auto` is foreman-selected per ticket: detected via parse_model_choice
        // (not a raw starts_with) and reported as a PASS, with no FAIL/WARN.
        for model in ["auto", "auto:light", "auto:standard", "auto:heavy"] {
            let config = config_with_model("copilot", model);
            let checks = models_check_core_with(&config, &|_| true);
            assert!(
                checks
                    .iter()
                    .any(|c| c.level == CheckLevel::Pass && c.message.contains("auto: foreman")),
                "{model} should PASS as foreman-selected"
            );
            assert_eq!(fail_count(&checks), 0, "{model} must not FAIL");
        }
    }

    #[test]
    fn auto_lookalike_is_not_treated_as_auto() {
        // `auto-foo` is a pin, not Auto: it goes through normal catalogue
        // validation (unknown -> WARN, never the auto PASS message).
        let config = config_with_model("copilot", "auto-foo");
        let checks = models_check_core_with(&config, &|_| true);
        assert!(!checks.iter().any(|c| c.message.contains("auto: foreman")));
    }

    #[test]
    fn unknown_provider_is_fail() {
        // `azure-openai` is neither a host alias nor a known runtime, so its
        // derived runtime does not exist — a genuine blocker (D79).
        let config = config_with_model("azure-openai", "gpt-5");
        let checks = models_check_core_with(&config, &|_| true);
        assert!(
            checks
                .iter()
                .any(|c| c.level == CheckLevel::Fail && c.message.contains("does not exist"))
        );
    }

    #[test]
    fn opencode_without_slash_is_warn() {
        let config = config_with_model("opencode", "sonnet");
        let checks = models_check_core_with(&config, &|_| true);
        assert!(
            checks
                .iter()
                .any(|c| c.level == CheckLevel::Warn && c.message.contains("provider/model"))
        );
        assert_eq!(fail_count(&checks), 0);
    }

    #[test]
    fn shell_provider_is_warn_not_pass_or_fail() {
        // `shell` is an approved escape hatch but not a managed host CLI, so it
        // must surface as WARN — never PASS (it cannot be validated) and never
        // FAIL (it stays legitimate).
        let config = config_with_model("shell", "echo");
        let checks = models_check_core_with(&config, &|_| true);
        assert!(
            checks
                .iter()
                .any(|c| c.level == CheckLevel::Warn && c.message.contains("escape-hatch")),
            "shell must produce a WARN"
        );
        assert!(
            !checks.iter().any(|c| c.level == CheckLevel::Pass),
            "shell must not PASS"
        );
        assert_eq!(fail_count(&checks), 0, "shell must not FAIL");
    }

    #[test]
    fn unbound_model_with_unknown_provider_is_fail() {
        // `extra` is configured but bound by no role; its `azure-openai`
        // provider is not a host and is not aliased to one, so pass 1 must FAIL
        // it even though pass 2 sees no role referencing it.
        let config =
            config_with_unbound_model("claude", "claude-opus-4-8", "azure-openai", "gpt-5");
        let checks = models_check_core_with(&config, &|_| true);
        assert!(
            checks.iter().any(|c| c.subject == "model extra"
                && c.level == CheckLevel::Fail
                && c.message.contains("does not exist")),
            "an unbound model with an unknown runtime must FAIL"
        );
    }

    #[test]
    fn role_binding_not_duplicated_for_configured_model() {
        // A model that is both configured and role-bound must be reported once
        // (pass 1), not again by pass 2.
        let config = config_with_model("codex", "gpt-5.5");
        let checks = models_check_core_with(&config, &|_| true);
        let model_findings = checks.iter().filter(|c| c.subject == "model m").count();
        assert_eq!(model_findings, 1, "the model must be reported exactly once");
        // No `role ... → ...` finding because the binding resolved cleanly.
        assert!(
            !checks.iter().any(|c| c.subject.starts_with("role ")),
            "a cleanly-resolving role binding must not add a finding"
        );
    }

    #[test]
    fn legacy_alias_provider_resolves_to_host() {
        // `anthropic` is remapped to `claude` at finalize; with the host
        // available and a known id, it should PASS, not FAIL on the provider.
        let config = config_with_model("anthropic", "claude-sonnet-4-6");
        let checks = models_check_core_with(&config, &|_| true);
        assert_eq!(fail_count(&checks), 0);
        assert!(checks.iter().any(|c| c.level == CheckLevel::Pass));
    }

    /// Loads a config from a custom `models`/`roles`/`stages` top section.
    fn config_top(top: &str) -> Config {
        let yaml = format!(
            r#"
version: 1
site:
  name: test
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
        );
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("derrick.yaml");
        let mut file = std::fs::File::create(&path).expect("create");
        file.write_all(yaml.as_bytes()).expect("write");
        Config::load_from_path(&path).expect("config should load")
    }

    #[test]
    fn d79_unknown_runtime_is_fail() {
        let config = config_top(
            "models:\n  m:\n    runtime: bogus-runtime\n    model: x\nroles:\n  drafter: m",
        );
        let checks = models_check_core_with(&config, &|_| true);
        assert!(
            checks
                .iter()
                .any(|c| c.level == CheckLevel::Fail && c.message.contains("does not exist"))
        );
    }

    #[test]
    fn d79_api_runtime_without_auth_env_is_fail() {
        let config = config_top(
            "models:\n  m:\n    runtime: openai-api\n    model: gpt-5.5\nroles:\n  drafter: m",
        );
        let checks = models_check_core_with_probes(&config, &|_| true, &|_| true);
        assert!(
            checks.iter().any(
                |c| c.level == CheckLevel::Fail && c.message.contains("requires an `auth_env`")
            )
        );
    }

    #[test]
    fn d79_api_runtime_auth_env_unset_is_fail_but_set_is_pass() {
        let config = config_top(
            "models:\n  m:\n    runtime: openai-api\n    model: gpt-5.5\n    auth_env: OPENAI_API_KEY\nroles:\n  drafter: m",
        );
        let unset = models_check_core_with_probes(&config, &|_| true, &|_| false);
        assert!(
            unset
                .iter()
                .any(|c| c.level == CheckLevel::Fail && c.message.contains("is not set"))
        );
        let set = models_check_core_with_probes(&config, &|_| true, &|_| true);
        assert_eq!(fail_count(&set), 0);
        assert!(set.iter().any(|c| c.level == CheckLevel::Pass));
    }

    #[test]
    fn d79_ollama_runtime_needs_no_auth() {
        let config = config_top(
            "models:\n  m:\n    runtime: ollama\n    model: qwen2.5-coder:32b\nroles:\n  drafter: m",
        );
        let checks = models_check_core_with_probes(&config, &|_| false, &|_| false);
        assert_eq!(fail_count(&checks), 0);
        assert!(checks.iter().any(|c| c.level == CheckLevel::Pass));
    }

    #[test]
    fn d79_openai_compatible_requires_base_url() {
        let missing = config_top(
            "models:\n  m:\n    runtime: openai-compatible\n    model: x\nroles:\n  drafter: m",
        );
        let checks = models_check_core_with_probes(&missing, &|_| true, &|_| true);
        assert!(
            checks
                .iter()
                .any(|c| c.level == CheckLevel::Fail && c.message.contains("requires a `base_url`"))
        );

        let present = config_top(
            "models:\n  m:\n    runtime: openai-compatible\n    base_url: http://localhost:8000/v1\n    model: x\nroles:\n  drafter: m",
        );
        let checks = models_check_core_with_probes(&present, &|_| true, &|_| true);
        assert_eq!(fail_count(&checks), 0);
    }

    #[test]
    fn d79_stage_requirement_checks() {
        // `tools=false` declared but required → FAIL; `vision` undeclared but
        // required → WARN; `streaming=true` declared and required → PASS.
        let config = config_top(
            "models:\n  m:\n    runtime: anthropic-api\n    model: claude-opus-4-8\n    auth_env: ANTHROPIC_API_KEY\n    capabilities:\n      tools: false\n      streaming: true\nroles:\n  drafter: m\nstages:\n  a:\n    model: m\n    requires: [tools]\n  b:\n    model: m\n    requires: [vision]\n  c:\n    model: m\n    requires: [streaming]",
        );
        let checks = models_check_core_with_probes(&config, &|_| true, &|_| true);
        assert!(
            checks.iter().any(
                |c| c.subject.contains("stage a requires tools") && c.level == CheckLevel::Fail
            )
        );
        assert!(
            checks
                .iter()
                .any(|c| c.subject.contains("stage b requires vision")
                    && c.level == CheckLevel::Warn)
        );
        assert!(checks.iter().any(
            |c| c.subject.contains("stage c requires streaming") && c.level == CheckLevel::Pass
        ));
    }
}
