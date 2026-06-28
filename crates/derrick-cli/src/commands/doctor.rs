use std::collections::BTreeSet;
use std::path::Path;

use derrick_config::{
    Config, Host, ModelDef, Runner, SpecProviderKind, StackBackendKind, SubstrateBackendKind,
};
use derrick_substrate_native::NativeSubstrate;
use serde_json::Value;
use serde_json::json;

use crate::commands::DoctorArgs;
use crate::exit_code::CliExitCode;
use crate::output::OutputFormat;
use crate::{current_repo_root, native_paths};

pub(crate) async fn execute(args: DoctorArgs) -> Result<CliExitCode, crate::CliError> {
    let repo_root = current_repo_root()?;
    let checks = run_checks(&repo_root).await;
    print_checks(&checks, args.format)?;
    Ok(CliExitCode::DoctorFailures(
        checks
            .iter()
            .filter(|check| check.status == CheckStatus::Fail)
            .count(),
    ))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CheckStatus {
    Pass,
    Warn,
    Fail,
}

impl CheckStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Warn => "warn",
            Self::Fail => "fail",
        }
    }
}

struct Check {
    name: String,
    status: CheckStatus,
    message: String,
    remediation: Option<String>,
}

async fn run_checks(repo_root: &Path) -> Vec<Check> {
    let mut checks = Vec::new();
    checks.push(binary_check("git", true));

    let config_path = repo_root.join("derrick.yaml");
    if !config_path.exists() {
        checks.push(Check::fail(
            "derrick.yaml",
            format!("{} does not exist", config_path.display()),
            "run `derrick init --greenfield` in a fresh repo",
        ));
        return checks;
    }

    let config = match Config::load_from_path(&config_path) {
        Ok(config) => {
            checks.push(Check::pass(
                "derrick.yaml",
                format!("{} parses and validates", config_path.display()),
            ));
            config
        }
        Err(error) => {
            checks.push(Check::fail(
                "derrick.yaml",
                error.to_string(),
                "fix derrick.yaml and rerun `derrick doctor`",
            ));
            return checks;
        }
    };

    add_config_driven_checks(repo_root, &config, &mut checks).await;
    checks
}

async fn add_config_driven_checks(repo_root: &Path, config: &Config, checks: &mut Vec<Check>) {
    let requirements = derive_requirements(config);
    for binary in requirements.binaries {
        checks.push(binary_check(&binary, true));
    }

    // D65: validate every role's model/host binding against the curated
    // catalogue using the same core that backs `derrick models check`.
    for model_check in crate::commands::models::models_check_core(config) {
        checks.push(Check::from_model_check(model_check));
    }

    match config.tools().substrate().backend() {
        SubstrateBackendKind::Native => check_native_substrate(repo_root, config, checks).await,
        SubstrateBackendKind::None => checks.push(Check::pass(
            "substrate",
            "substrate checks skipped because backend is none",
        )),
    }

    add_spec_provider_checks(repo_root, config, checks);

    checks.push(claude_hook_check(repo_root));
    checks.push(codex_instructions_check(repo_root));

    let stack_backend = config.tools().git().stacking().backend();
    if stack_backend != StackBackendKind::None {
        // The native backend (D72) is derrick's only stacking engine; it drives
        // plain `git` plus `gh pr create`. `git` is already checked above, so we
        // only need to ensure `gh` is present here.
        checks.push(binary_check("gh", true));
        check_squash_merge_policy(repo_root, checks).await;
    }

    if config.tools().copilot().enabled() {
        // T013: the Copilot hand dispatches via `gh issue create` +
        // `gh issue edit --add-assignee @copilot`. Both rely on the
        // `gh` CLI being installed and authenticated, and on the
        // repository having the Copilot coding agent enabled.
        checks.push(binary_check("gh", true));
        checks.push(Check::warn(
            "copilot coding agent",
            "tools.copilot.enabled is true; derrick cannot confirm the repo has the Copilot coding agent enabled without an API call",
            "verify Copilot is reachable on this repo via the GitHub UI (Settings → Copilot → Coding agent) before running `derrick foreman start`",
        ));
    }
}

/// The bare spec step ids the seam routes through `tools.specify.provider`
/// (mirrors `derrick_flow::spec_provider::SpecPhase::from_step_id`).
const SPEC_STEP_IDS: [&str; 3] = ["specify", "plan", "tasks"];

/// Reports the active spec provider and runs provider-scoped health checks
/// (DESIGN.md §5.3 / Phase 4).
///
/// * Always emits a `spec provider` line mirroring the substrate/stack lines.
/// * The speckit-on-PATH check only runs when `provider == Speckit` *or* a
///   pipeline step explicitly pins a `/speckit.*` command. Under `native`/
///   `import`, speckit absence is never a failure.
/// * `native` verifies the bare spec steps' roles resolve to a model.
/// * `import` validates `tools.specify.import.source`.
fn add_spec_provider_checks(repo_root: &Path, config: &Config, checks: &mut Vec<Check>) {
    let provider = config.tools().specify().provider();
    let provider_label = match provider {
        SpecProviderKind::Speckit => "speckit",
        SpecProviderKind::Native => "native",
        SpecProviderKind::Import => "import",
    };
    checks.push(Check::pass(
        "spec provider",
        format!("tools.specify.provider = {provider_label}"),
    ));

    // A pipeline step that explicitly pins a `/speckit.*` command still needs
    // speckit regardless of the provider (the explicit path bypasses the seam).
    let pins_speckit_command = config.pipeline().iter().any(|step| {
        step.command()
            .is_some_and(|command| command.contains("/speckit."))
    });

    if provider == SpecProviderKind::Speckit || pins_speckit_command {
        checks.push(speckit_path_check(config));
    } else {
        checks.push(Check::pass(
            "speckit",
            format!(
                "speckit PATH check skipped — provider is {provider_label} and no step pins a /speckit.* command"
            ),
        ));
    }

    match provider {
        SpecProviderKind::Native => add_native_spec_checks(config, checks),
        SpecProviderKind::Import => add_import_spec_checks(repo_root, config, checks),
        SpecProviderKind::Speckit => {}
    }
}

/// Checks that the speckit CLI (`specify` or `speckit`) is on PATH. Mirrors
/// `binary_check` but accepts either binary name (init installs `specify-cli`,
/// which provides the `specify` binary).
fn speckit_path_check(config: &Config) -> Check {
    let bins = ["specify", "speckit"];
    if let Some(path) = bins.iter().find_map(|bin| which::which(bin).ok()) {
        return Check::pass("speckit", format!("found {}", path.display()));
    }
    let version = config.tools().speckit().version();
    // A warning, not a failure: the rest of the install can be healthy, and the
    // user may install speckit at any time (or switch to native/import). This
    // matches how doctor treats other recoverable, environment-dependent checks.
    Check::warn(
        "speckit",
        format!("speckit CLI not found on PATH (expected version {version})"),
        "install speckit with `uv tool install specify-cli`, or set \
         tools.specify.provider to `native`/`import` if you do not use speckit",
    )
}

/// `native` provider: verify the bare spec steps' roles resolve to a model so
/// the native generator has a model to invoke.
fn add_native_spec_checks(config: &Config, checks: &mut Vec<Check>) {
    let mut roles: BTreeSet<String> = BTreeSet::new();
    for step in config.pipeline() {
        if SPEC_STEP_IDS.contains(&step.id()) {
            if let Some(role) = step.role() {
                roles.insert(role.to_owned());
            }
        }
    }

    let mut unresolved: Vec<String> = Vec::new();
    for role in &roles {
        let resolves = config
            .roles()
            .get(role)
            .and_then(|model_name| config.models().get(model_name))
            .is_some();
        if !resolves {
            unresolved.push(role.clone());
        }
    }

    if unresolved.is_empty() {
        checks.push(Check::pass(
            "native spec roles",
            "spec step roles resolve to a configured model",
        ));
    } else {
        checks.push(Check::warn(
            "native spec roles",
            format!(
                "native spec provider: role(s) {} do not resolve to a configured model",
                unresolved.join(", ")
            ),
            "bind these roles under `roles:` to a model defined in `models:`, \
             or run `derrick models check`",
        ));
    }
}

/// `import` provider: validate `tools.specify.import.source`. An unset source
/// is a note (it can be supplied via `--spec`); a set plain file path that does
/// not exist is a warning.
fn add_import_spec_checks(repo_root: &Path, config: &Config, checks: &mut Vec<Check>) {
    match config.tools().specify().import().source() {
        None => checks.push(Check::warn(
            "import source",
            "tools.specify.import.source is unset",
            "set tools.specify.import.source, or pass `--spec <path>` on each run",
        )),
        Some(source) => {
            // Only validate plain filesystem paths; anything that looks like a
            // URL/locator is left to the import provider to interpret at run time.
            if looks_like_url(source) {
                checks.push(Check::pass(
                    "import source",
                    format!(
                        "import source {source:?} is a non-file locator (validated at run time)"
                    ),
                ));
                return;
            }
            let candidate = repo_root.join(source);
            if candidate.exists() {
                checks.push(Check::pass(
                    "import source",
                    format!("import source {source:?} exists"),
                ));
            } else {
                checks.push(Check::warn(
                    "import source",
                    format!("import source {source:?} does not exist"),
                    "create the spec file, fix tools.specify.import.source, \
                     or pass `--spec <path>` on each run",
                ));
            }
        }
    }
}

/// Heuristic: treat values with a `scheme://` prefix as locators, not files.
fn looks_like_url(source: &str) -> bool {
    source.split_once("://").is_some_and(|(scheme, _)| {
        !scheme.is_empty()
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    })
}

async fn check_native_substrate(repo_root: &Path, config: &Config, checks: &mut Vec<Check>) {
    let state_dir = repo_root.join(config.state().dir());
    if state_dir.is_dir() {
        checks.push(Check::pass(
            ".derrick",
            format!("{} is accessible", state_dir.display()),
        ));
    } else {
        checks.push(Check::fail(
            ".derrick",
            format!("{} is missing", state_dir.display()),
            "run `derrick init --greenfield`",
        ));
        return;
    }

    let native_config = native_paths(repo_root, config);
    if !native_config.db_path.exists() {
        checks.push(Check::fail(
            "native substrate",
            format!("{} is missing", native_config.db_path.display()),
            "run `derrick init --greenfield`",
        ));
        return;
    }

    if let Err(message) = verify_sqlite_header(&native_config.db_path) {
        checks.push(Check::fail(
            "native substrate",
            message,
            "inspect .derrick/derrick.db",
        ));
        return;
    }

    match NativeSubstrate::open(native_config, config.site().clone()).await {
        Ok(substrate) => {
            let close_result = substrate.close().await;
            if let Err(error) = close_result {
                checks.push(Check::fail(
                    "native substrate",
                    error.to_string(),
                    "inspect .derrick/derrick.db",
                ));
            } else {
                checks.push(Check::pass(
                    "native substrate",
                    "opened .derrick/derrick.db",
                ));
            }
        }
        Err(error) => checks.push(Check::fail(
            "native substrate",
            error.to_string(),
            "inspect .derrick/derrick.db",
        )),
    }
}

fn verify_sqlite_header(path: &Path) -> Result<(), String> {
    let bytes = std::fs::read(path)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    if bytes.starts_with(b"SQLite format 3\0") {
        Ok(())
    } else {
        Err(format!("{} is not a SQLite database", path.display()))
    }
}

#[derive(Default)]
struct Requirements {
    binaries: BTreeSet<String>,
}

fn derive_requirements(config: &Config) -> Requirements {
    let mut requirements = Requirements::default();
    let mut roles = BTreeSet::new();

    for step in config.pipeline() {
        if let Some(role) = step.role() {
            roles.insert(role.to_owned());
        }
        if let Some(role) = step.executor_role() {
            roles.insert(role.to_owned());
        }
        if let Some(host) = step.host() {
            requirements.binaries.insert(host_binary(host).to_owned());
        }
        if let Some(runner) = step.runner() {
            if let Some(binary) = runner_binary(runner) {
                requirements.binaries.insert(binary.to_owned());
            }
        }
        if step.id() == "assay" {
            roles.insert(config.tools().assay().role().to_owned());
            for reviewer in config.tools().assay().reviewers() {
                roles.insert(reviewer.clone());
            }
        }
    }

    if config.tools().assay().enabled() {
        roles.insert(config.tools().assay().role().to_owned());
        for reviewer in config.tools().assay().reviewers() {
            roles.insert(reviewer.clone());
        }
    }
    if config.tools().copilot().enabled() {
        requirements.binaries.insert("copilot".to_owned());
    }

    for role in roles {
        if let Some(model_name) = config.roles().get(&role) {
            if let Some(model) = config.models().get(model_name) {
                add_model_requirement(model, &mut requirements);
            }
        }
    }

    requirements
}

fn add_model_requirement(model: &ModelDef, requirements: &mut Requirements) {
    // D79: the runtime determines the invocation path. A `*-cli` runtime needs
    // its host binary on PATH; the `shell` runtime needs its `cli` command's
    // binary. API and local runtimes (anthropic-api, ollama, …) shell out to no
    // managed binary, so they contribute no PATH requirement here.
    let runtime = model.resolved_runtime();
    if runtime == "shell" {
        if let Some(cli) = model.cli() {
            if let Some(binary) = cli.split_whitespace().next() {
                requirements.binaries.insert(binary.to_owned());
            }
        }
    } else if let Some(host) = derrick_config::cli_host_for_runtime(&runtime) {
        requirements.binaries.insert(host.to_owned());
    }
}

fn host_binary(host: Host) -> &'static str {
    match host {
        Host::Claude => "claude",
        Host::Codex => "codex",
        Host::Copilot => "copilot",
        Host::Opencode => "opencode",
        Host::Aider => "aider",
    }
}

fn runner_binary(runner: Runner) -> Option<&'static str> {
    match runner {
        Runner::Claude => Some("claude"),
        Runner::Codex => Some("codex"),
        Runner::Copilot => Some("copilot"),
        Runner::Derrick | Runner::Human | Runner::Bash => None,
    }
}

async fn check_squash_merge_policy(repo_root: &Path, checks: &mut Vec<Check>) {
    // Resolve origin URL → owner/repo.
    let origin_url = match std::process::Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(repo_root)
        .output()
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_owned(),
        _ => {
            checks.push(Check::warn(
                "git merge policy (D21)",
                "could not read origin remote URL",
                "ensure `git remote get-url origin` works in this repo",
            ));
            return;
        }
    };

    let (owner, repo_name) = match parse_github_owner_repo(&origin_url) {
        Some(pair) => pair,
        None => {
            checks.push(Check::warn(
                "git merge policy (D21)",
                format!("origin does not look like a GitHub URL: {origin_url}"),
                "stacking relies on GitHub PRs; ensure origin points to github.com",
            ));
            return;
        }
    };

    let api_path = format!("repos/{owner}/{repo_name}");
    let api_output = match std::process::Command::new("gh")
        .args(["api", &api_path])
        .current_dir(repo_root)
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            checks.push(Check::warn(
                "git merge policy (D21)",
                format!("could not run `gh api {api_path}`: {e}"),
                "install and authenticate the `gh` CLI",
            ));
            return;
        }
    };

    if !api_output.status.success() {
        let stderr = String::from_utf8_lossy(&api_output.stderr);
        checks.push(Check::warn(
            "git merge policy (D21)",
            format!("gh api returned non-zero: {}", stderr.trim()),
            "ensure `gh auth login` is complete and you have repo access",
        ));
        return;
    }

    let repo_json: serde_json::Value = match serde_json::from_slice(&api_output.stdout) {
        Ok(v) => v,
        Err(e) => {
            checks.push(Check::warn(
                "git merge policy (D21)",
                format!("could not parse gh api response: {e}"),
                "run `gh api repos/{owner}/{repo}` manually to inspect",
            ));
            return;
        }
    };

    let allow_squash = repo_json["allow_squash_merge"].as_bool().unwrap_or(true);
    let allow_merge = repo_json["allow_merge_commit"].as_bool().unwrap_or(true);
    let allow_rebase = repo_json["allow_rebase_merge"].as_bool().unwrap_or(true);

    if allow_squash && !allow_merge && !allow_rebase {
        checks.push(Check::warn(
            "git merge policy (D21)",
            format!(
                "{owner}/{repo_name}: squash-merge is the ONLY option — stacked PRs will \
                 have their parent SHA rewritten on merge, breaking downstream branches",
            ),
            "enable merge commits or rebase-merge in GitHub repo Settings → \
             General → Pull Requests for derrick-managed PRs",
        ));
    } else if allow_squash {
        checks.push(Check::warn(
            "git merge policy (D21)",
            format!(
                "{owner}/{repo_name}: squash-merge is enabled — if reviewers use it on \
                 stacked PRs the parent SHA will be rewritten, breaking downstream branches",
            ),
            "prefer merge commits or rebase-merge for derrick-managed stacked PRs",
        ));
    } else {
        checks.push(Check::pass(
            "git merge policy (D21)",
            format!("{owner}/{repo_name}: squash-merge is disabled — stacking is safe"),
        ));
    }
}

/// Extract `(owner, repo)` from common GitHub remote URL formats:
/// - `https://github.com/owner/repo.git`
/// - `https://github.com/owner/repo`
/// - `git@github.com:owner/repo.git`
fn parse_github_owner_repo(url: &str) -> Option<(String, String)> {
    let url = url.trim();
    // HTTPS form
    let path = if let Some(rest) = url.strip_prefix("https://github.com/") {
        rest
    } else if let Some(rest) = url.strip_prefix("http://github.com/") {
        rest
    } else {
        url.strip_prefix("git@github.com:")?
    };
    let path = path.trim_end_matches(".git");
    let mut parts = path.splitn(2, '/');
    let owner = parts.next()?.to_owned();
    let repo = parts.next()?.trim_end_matches('/').to_owned();
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some((owner, repo))
}

fn binary_check(binary: &str, required: bool) -> Check {
    match which::which(binary) {
        Ok(path) => Check::pass(binary, format!("found {}", path.display())),
        Err(error) if required => Check::fail(
            binary,
            format!("{binary} not found on PATH: {error}"),
            format!("install {binary} or update PATH"),
        ),
        Err(error) => Check::warn(
            binary,
            format!("{binary} not found on PATH: {error}"),
            format!("install {binary} or update PATH"),
        ),
    }
}

fn claude_hook_check(repo_root: &Path) -> Check {
    let relative = ".claude/settings.json";
    let path = repo_root.join(relative);
    if !path.exists() {
        return Check::warn(
            "Claude Code hooks",
            format!("{relative} is missing"),
            "run `derrick init` to install D29 Claude scrub and caveman hooks",
        );
    }

    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) => {
            return Check::warn(
                "Claude Code hooks",
                format!("failed to read {relative}: {error}"),
                format!("inspect {relative} and rerun `derrick doctor`"),
            );
        }
    };
    let settings: Value = match serde_json::from_str(&content) {
        Ok(settings) => settings,
        Err(error) => {
            return Check::warn(
                "Claude Code hooks",
                format!("{relative} is not valid JSON: {error}"),
                format!("repair {relative} and rerun `derrick doctor`"),
            );
        }
    };

    let scrub_installed = hook_stage_contains_description(&settings, "PreToolUse", "derrick:scrub");
    let caveman_installed =
        hook_stage_contains_description(&settings, "PostToolUse", "derrick:caveman");

    match (scrub_installed, caveman_installed) {
        (true, true) => Check::pass(
            "Claude Code hooks",
            "derrick D29 scrub and caveman hooks are installed",
        ),
        (false, false) => Check::warn(
            "Claude Code hooks",
            "derrick D29 scrub and caveman hooks are missing from .claude/settings.json",
            "run `derrick init` to install the missing Claude hook entries",
        ),
        (false, true) => Check::warn(
            "Claude Code hooks",
            "derrick D29 scrub hook is missing from .claude/settings.json",
            "run `derrick init` to reinstall the missing PreToolUse scrub hook",
        ),
        (true, false) => Check::warn(
            "Claude Code hooks",
            "derrick D29 caveman hook is missing from .claude/settings.json",
            "run `derrick init` to reinstall the missing PostToolUse caveman hook",
        ),
    }
}

fn codex_instructions_check(repo_root: &Path) -> Check {
    let relative = ".codex/instructions.md";
    let path = repo_root.join(relative);
    if path.exists() {
        Check::pass(
            "Codex instructions",
            "Codex host context file is present (D34 hook installation is deferred)",
        )
    } else {
        Check::warn(
            "Codex instructions",
            format!("{relative} is missing"),
            "run `derrick init` to install the Codex context file; Codex hooks remain deferred per D34",
        )
    }
}

fn hook_stage_contains_description(settings: &Value, stage: &str, description: &str) -> bool {
    settings
        .get("hooks")
        .and_then(|hooks| hooks.get(stage))
        .and_then(Value::as_array)
        .is_some_and(|entries| {
            entries.iter().any(|entry| {
                entry
                    .get("hooks")
                    .and_then(Value::as_array)
                    .is_some_and(|hooks| {
                        hooks.iter().any(|hook| {
                            hook.get("description").and_then(Value::as_str) == Some(description)
                        })
                    })
            })
        })
}

fn print_checks(checks: &[Check], format: OutputFormat) -> Result<(), crate::CliError> {
    match format {
        OutputFormat::Human => {
            for check in checks {
                println!(
                    "{:<5} {:<24} {}",
                    check.status.as_str(),
                    check.name,
                    check.message
                );
                if let Some(remediation) = &check.remediation {
                    println!("      remediation: {remediation}");
                }
            }
        }
        OutputFormat::Json => {
            let rows = checks
                .iter()
                .map(|check| {
                    json!({
                        "check": check.name,
                        "status": check.status.as_str(),
                        "message": check.message,
                        "remediation": check.remediation,
                    })
                })
                .collect::<Vec<_>>();
            println!("{}", serde_json::to_string(&rows)?);
        }
    }
    Ok(())
}

impl Check {
    fn pass(name: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: CheckStatus::Pass,
            message: message.into(),
            remediation: None,
        }
    }

    fn warn(
        name: impl Into<String>,
        message: impl Into<String>,
        remediation: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            status: CheckStatus::Warn,
            message: message.into(),
            remediation: Some(remediation.into()),
        }
    }

    fn fail(
        name: impl Into<String>,
        message: impl Into<String>,
        remediation: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            status: CheckStatus::Fail,
            message: message.into(),
            remediation: Some(remediation.into()),
        }
    }

    /// Converts a `derrick models check` finding into a doctor check.
    fn from_model_check(check: crate::commands::models::ModelCheck) -> Self {
        use crate::commands::models::CheckLevel;
        let status = match check.level {
            CheckLevel::Pass => CheckStatus::Pass,
            CheckLevel::Warn => CheckStatus::Warn,
            CheckLevel::Fail => CheckStatus::Fail,
        };
        let remediation = match check.level {
            CheckLevel::Warn | CheckLevel::Fail => Some(
                "run `derrick models check` and align the model/host with the catalogue".to_owned(),
            ),
            CheckLevel::Pass => None,
        };
        Self {
            name: check.subject,
            status,
            message: check.message,
            remediation,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEMPLATE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../templates/derrick.yaml.in"
    ));

    /// Builds a valid config from the bundled template, applies the spec-provider
    /// rewrite for `provider` (so native/import bare the steps exactly as
    /// `derrick init` would), then writes/loads it from a tempdir whose root is
    /// returned for the import-source existence check.
    fn config_for(
        provider: crate::commands::spec_provider_init::SpecProviderChoice,
    ) -> (tempfile::TempDir, Config) {
        let rendered = derrick_config::render_init_template(
            TEMPLATE,
            derrick_config::InitTemplateVars {
                site_name: "t",
                prefix: "tst",
                mode: "solo",
            },
        );
        let yaml = crate::commands::spec_provider_init::apply_spec_provider(&rendered, provider)
            .expect("spec provider rewrite");
        write_and_load(yaml)
    }

    fn write_and_load(yaml: String) -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("derrick.yaml");
        std::fs::write(&path, yaml).expect("write");
        let config = Config::load_from_path(&path).expect("config loads");
        (dir, config)
    }

    /// Loads the import-provider config and overrides `import.source` to `source`
    /// (the wizard leaves it as a comment, so tests inject one explicitly).
    fn import_config_with_source(source: Option<&str>) -> (tempfile::TempDir, Config) {
        let rendered = derrick_config::render_init_template(
            TEMPLATE,
            derrick_config::InitTemplateVars {
                site_name: "t",
                prefix: "tst",
                mode: "solo",
            },
        );
        let yaml = crate::commands::spec_provider_init::apply_spec_provider(
            &rendered,
            crate::commands::spec_provider_init::SpecProviderChoice::Import,
        )
        .expect("spec provider rewrite");
        // Inject a concrete source under the emitted `import:` block when asked.
        let yaml = match source {
            Some(source) => {
                let mut value: serde_yaml::Value = serde_yaml::from_str(&yaml).expect("parse");
                let import = value
                    .get_mut("tools")
                    .and_then(|t| t.get_mut("specify"))
                    .and_then(|s| s.get_mut("import"))
                    .and_then(serde_yaml::Value::as_mapping_mut)
                    .expect("import mapping");
                import.insert(
                    serde_yaml::Value::String("source".to_owned()),
                    serde_yaml::Value::String(source.to_owned()),
                );
                serde_yaml::to_string(&value).expect("serialize")
            }
            None => yaml,
        };
        write_and_load(yaml)
    }

    fn find<'a>(checks: &'a [Check], name: &str) -> &'a Check {
        checks
            .iter()
            .find(|check| check.name == name)
            .unwrap_or_else(|| panic!("no `{name}` check; have {:?}", names(checks)))
    }

    fn names(checks: &[Check]) -> Vec<&str> {
        checks.iter().map(|check| check.name.as_str()).collect()
    }

    use crate::commands::spec_provider_init::SpecProviderChoice;

    #[test]
    fn provider_line_renders_for_speckit() {
        let (dir, config) = config_for(SpecProviderChoice::Speckit);
        let mut checks = Vec::new();
        add_spec_provider_checks(dir.path(), &config, &mut checks);
        assert!(find(&checks, "spec provider").message.contains("speckit"));
    }

    #[test]
    fn provider_line_renders_for_native() {
        let (dir, config) = config_for(SpecProviderChoice::Native);
        let mut checks = Vec::new();
        add_spec_provider_checks(dir.path(), &config, &mut checks);
        assert!(find(&checks, "spec provider").message.contains("native"));
    }

    #[test]
    fn provider_line_renders_for_import() {
        let (dir, config) = config_for(SpecProviderChoice::Import);
        let mut checks = Vec::new();
        add_spec_provider_checks(dir.path(), &config, &mut checks);
        assert!(find(&checks, "spec provider").message.contains("import"));
    }

    #[test]
    fn speckit_check_runs_for_speckit_provider() {
        // The default template keeps explicit /speckit.* steps, so the PATH
        // check must run (pass or fail by host) — never "skipped".
        let (dir, config) = config_for(SpecProviderChoice::Speckit);
        let mut checks = Vec::new();
        add_spec_provider_checks(dir.path(), &config, &mut checks);
        assert!(!find(&checks, "speckit").message.contains("skipped"));
    }

    /// Drops any pipeline step that still pins a `/speckit.*` command (e.g. the
    /// template's `analyze` skill, which is outside the seam). A genuinely
    /// speckit-free native/import project would not keep these steps; removing
    /// them here exercises the "no step pins speckit" branch.
    fn strip_speckit_pinned_steps(yaml: &str) -> String {
        let mut value: serde_yaml::Value = serde_yaml::from_str(yaml).expect("parse");
        if let Some(steps) = value
            .get_mut("pipeline")
            .and_then(serde_yaml::Value::as_sequence_mut)
        {
            steps.retain(|step| {
                step.get("command")
                    .and_then(serde_yaml::Value::as_str)
                    .is_none_or(|command| !command.contains("/speckit."))
            });
        }
        serde_yaml::to_string(&value).expect("serialize")
    }

    /// Renders + rewrites for `provider`, then strips any remaining
    /// `/speckit.*`-pinned step, producing a genuinely speckit-free config.
    fn speckit_free_config(provider: SpecProviderChoice) -> (tempfile::TempDir, Config) {
        let rendered = derrick_config::render_init_template(
            TEMPLATE,
            derrick_config::InitTemplateVars {
                site_name: "t",
                prefix: "tst",
                mode: "solo",
            },
        );
        let yaml = crate::commands::spec_provider_init::apply_spec_provider(&rendered, provider)
            .expect("rewrite");
        write_and_load(strip_speckit_pinned_steps(&yaml))
    }

    #[test]
    fn speckit_check_skipped_for_native_without_pinned_command() {
        let (dir, config) = speckit_free_config(SpecProviderChoice::Native);
        let mut checks = Vec::new();
        add_spec_provider_checks(dir.path(), &config, &mut checks);
        // Whatever the host environment, the speckit check must not be a failure.
        assert_ne!(find(&checks, "speckit").status, CheckStatus::Fail);
        assert!(find(&checks, "speckit").message.contains("skipped"));
    }

    #[test]
    fn speckit_check_skipped_for_import_without_pinned_command() {
        let (dir, config) = speckit_free_config(SpecProviderChoice::Import);
        let mut checks = Vec::new();
        add_spec_provider_checks(dir.path(), &config, &mut checks);
        assert_ne!(find(&checks, "speckit").status, CheckStatus::Fail);
        assert!(find(&checks, "speckit").message.contains("skipped"));
    }

    #[test]
    fn speckit_check_runs_for_native_when_analyze_step_pins_speckit() {
        // The default template keeps an `analyze` step pinned to /speckit.analyze
        // even under native; that explicit step legitimately still needs speckit,
        // so the PATH check runs (not skipped).
        let (dir, config) = config_for(SpecProviderChoice::Native);
        let mut checks = Vec::new();
        add_spec_provider_checks(dir.path(), &config, &mut checks);
        assert!(!find(&checks, "speckit").message.contains("skipped"));
    }

    #[test]
    fn speckit_check_runs_when_step_pins_speckit_command_under_native() {
        // A native provider but a step still explicitly pins a /speckit.*
        // command: the speckit PATH check must run (not be skipped). Start from
        // the native rewrite, then add an explicit speckit command back to one
        // step.
        let rendered = derrick_config::render_init_template(
            TEMPLATE,
            derrick_config::InitTemplateVars {
                site_name: "t",
                prefix: "tst",
                mode: "solo",
            },
        );
        let yaml = crate::commands::spec_provider_init::apply_spec_provider(
            &rendered,
            SpecProviderChoice::Native,
        )
        .expect("rewrite");
        let mut value: serde_yaml::Value = serde_yaml::from_str(&yaml).expect("parse");
        let steps = value
            .get_mut("pipeline")
            .and_then(serde_yaml::Value::as_sequence_mut)
            .expect("pipeline");
        for step in steps.iter_mut() {
            if step.get("id").and_then(serde_yaml::Value::as_str) == Some("tasks") {
                let map = step.as_mapping_mut().expect("step mapping");
                map.insert(
                    serde_yaml::Value::String("host".to_owned()),
                    serde_yaml::Value::String("claude".to_owned()),
                );
                map.insert(
                    serde_yaml::Value::String("command".to_owned()),
                    serde_yaml::Value::String("/speckit.tasks".to_owned()),
                );
            }
        }
        let (dir, config) = write_and_load(serde_yaml::to_string(&value).expect("serialize"));
        let mut checks = Vec::new();
        add_spec_provider_checks(dir.path(), &config, &mut checks);
        assert!(!find(&checks, "speckit").message.contains("skipped"));
    }

    #[test]
    fn native_spec_roles_pass_when_resolvable() {
        let (dir, config) = config_for(SpecProviderChoice::Native);
        let mut checks = Vec::new();
        add_spec_provider_checks(dir.path(), &config, &mut checks);
        assert_eq!(find(&checks, "native spec roles").status, CheckStatus::Pass);
    }

    #[test]
    fn import_missing_source_file_warns() {
        let (dir, config) = import_config_with_source(Some("does-not-exist.md"));
        let mut checks = Vec::new();
        add_spec_provider_checks(dir.path(), &config, &mut checks);
        let check = find(&checks, "import source");
        assert_eq!(check.status, CheckStatus::Warn);
        assert!(check.message.contains("does not exist"));
    }

    #[test]
    fn import_existing_source_file_passes() {
        let (dir, config) = import_config_with_source(Some("spec.md"));
        std::fs::write(dir.path().join("spec.md"), "# spec").expect("write spec");
        let mut checks = Vec::new();
        add_spec_provider_checks(dir.path(), &config, &mut checks);
        assert_eq!(find(&checks, "import source").status, CheckStatus::Pass);
    }

    #[test]
    fn import_unset_source_warns_with_spec_hint() {
        let (dir, config) = import_config_with_source(None);
        let mut checks = Vec::new();
        add_spec_provider_checks(dir.path(), &config, &mut checks);
        let check = find(&checks, "import source");
        assert_eq!(check.status, CheckStatus::Warn);
        assert!(check.message.contains("unset"));
    }

    #[test]
    fn looks_like_url_distinguishes_locators_from_paths() {
        assert!(looks_like_url("https://example.com/spec.md"));
        assert!(looks_like_url("git+ssh://host/repo"));
        assert!(!looks_like_url("docs/spec.md"));
        assert!(!looks_like_url("./spec.md"));
    }
}
