//! `derrick models check` — validate configured models and role bindings
//! against the curated host catalogue (D65).
//!
//! The shared [`models_check_core`] is reused by `derrick doctor` and by the
//! soft (WARN-only) checks emitted at `derrick init` and `derrick run`, so the
//! three never drift.

use derrick_config::Config;
use derrick_tools::{HostRegistry, ModelChoice, catalogue, parse_model_choice};
use serde_json::json;

use crate::commands::ModelsArgs;
use crate::commands::ModelsCommand;
use crate::exit_code::CliExitCode;
use crate::output::OutputFormat;

/// The five host CLIs every inference model must route through (D65).
const HOSTS: [&str; 5] = ["claude", "codex", "copilot", "opencode", "aider"];

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

/// Validates every configured model AND every role binding against the host
/// catalogue.
///
/// Two passes (D65):
///
/// 1. **Every configured model** (`config.models()`) is validated on its own,
///    whether or not a role binds it:
///    - host not installed → FAIL;
///    - model id not in the curated catalogue → WARN (never FAIL);
///    - provider is not one of the five hosts (after the finalize alias remap)
///      and is not `shell` → FAIL;
///    - opencode/aider model id lacks a `/` → WARN;
///    - `shell` → WARN: a legitimate, approved escape hatch, but not a managed
///      host CLI, so derrick cannot validate its auth/model.
/// 2. **Every role binding** is checked to resolve to a known model; an unknown
///    model reference → FAIL. The resolved model itself is not re-validated
///    here — pass 1 already covered it — so output is not duplicated.
pub(crate) fn models_check_core(config: &Config) -> Vec<ModelCheck> {
    let registry = HostRegistry::with_defaults();
    models_check_core_with(config, &|host| {
        registry
            .get(host)
            .is_some_and(derrick_tools::HostAdapter::is_available)
    })
}

/// Catalogue validation with an injectable host-availability probe.
///
/// Split out from [`models_check_core`] so tests can supply a deterministic
/// availability function instead of depending on what is installed on PATH.
fn models_check_core_with(
    config: &Config,
    host_available: &dyn Fn(&str) -> bool,
) -> Vec<ModelCheck> {
    let mut checks = Vec::new();

    // Pass 1: validate every configured model in stable (name-sorted) order.
    let mut models: Vec<(&str, &derrick_config::ModelDef)> = config
        .models()
        .as_map()
        .iter()
        .map(|(name, def)| (name.as_str(), def))
        .collect();
    models.sort_unstable_by_key(|(name, _)| *name);

    for (model_name, model_def) in models {
        let subject = format!("model {model_name}");
        checks.push(check_model(
            subject,
            model_def.provider(),
            model_def.model(),
            host_available,
        ));
    }

    // Pass 2: every role binding must resolve to a known model. The resolved
    // model is already validated in pass 1, so only the reference is checked
    // here (no duplicate per-model finding).
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

    checks
}

/// Validates a single configured model's provider + id against the catalogue.
fn check_model(
    subject: String,
    provider: &str,
    model_id: &str,
    host_available: &dyn Fn(&str) -> bool,
) -> ModelCheck {
    // `shell` is an approved escape hatch, but it is not one of the five
    // managed host CLIs, so derrick cannot validate its auth or model.
    if provider == "shell" {
        return ModelCheck::warn(
            subject,
            "shell: unmanaged escape-hatch provider (not a host CLI) — \
             derrick cannot validate its auth/model",
        );
    }

    if !HOSTS.contains(&provider) {
        return ModelCheck::fail(
            subject,
            format!(
                "provider `{provider}` is not one of the five hosts \
                 (claude, codex, copilot, opencode, aider)"
            ),
        );
    }

    // Rule 1: host binary installed. This runs FIRST — even `auto` requires the
    // host CLI present, since the foreman dispatches through it (D67). Only the
    // catalogue model-id validation below is skipped for `auto`.
    if !host_available(provider) {
        return ModelCheck::fail(
            subject,
            format!("host `{provider}` is not installed on PATH"),
        );
    }

    // `auto` (and `auto:<tier>`) is foreman-selected per ticket within the
    // host, so there is no single id to validate against the catalogue (D67).
    // The host is present (checked above); only the model-id checks are skipped.
    if matches!(parse_model_choice(model_id), ModelChoice::Auto { .. }) {
        return ModelCheck::pass(
            subject,
            format!("auto: foreman selects per-ticket within host `{provider}`"),
        );
    }

    // Validate the pinned id against the catalogue using the same trimmed form
    // the dispatch path forwards (parse_model_choice trims), so a quoted,
    // space-padded pin does not produce a spurious WARN.
    let model_id = model_id.trim();

    // Rule 4: opencode/aider expect provider/model.
    if (provider == "opencode" || provider == "aider") && !model_id.contains('/') {
        return ModelCheck::warn(
            subject,
            format!("host `{provider}` expects a `provider/model` id; `{model_id}` has no `/`"),
        );
    }

    // Rule 2: catalogue membership (WARN-only).
    let normalized = catalogue::normalize(provider, model_id);
    if catalogue::is_known(provider, &normalized) {
        ModelCheck::pass(
            subject,
            format!("`{model_id}` is a known `{provider}` model"),
        )
    } else {
        ModelCheck::warn(
            subject,
            format!(
                "`{model_id}` is not in the curated `{provider}` catalogue; \
                 passing it through unverified"
            ),
        )
    }
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
        // `azure-openai` is not a host and is not aliased to one.
        let config = config_with_model("azure-openai", "gpt-5");
        let checks = models_check_core_with(&config, &|_| true);
        assert!(checks.iter().any(
            |c| c.level == CheckLevel::Fail && c.message.contains("not one of the five hosts")
        ));
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
                && c.message.contains("not one of the five hosts")),
            "an unbound model with a non-host provider must FAIL"
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
}
