use std::collections::BTreeSet;
use std::path::Path;

use derrick_config::{Config, Host, ModelDef, Runner, StackBackendKind, SubstrateBackendKind};
use derrick_models::AuthStore;
use derrick_substrate_native::NativeSubstrate;
use serde_json::json;
use serde_json::Value;

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

    let mut auth = AuthStore::from_env();
    for (provider, env_var) in requirements.env_vars {
        auth.require(&provider, &env_var);
    }
    for (provider, env_var) in auth.missing_required() {
        checks.push(Check::fail(
            format!("{provider} credentials"),
            format!("{env_var} is not set"),
            format!("export {env_var} before running derrick"),
        ));
    }

    match config.tools().substrate().backend() {
        SubstrateBackendKind::Native => check_native_substrate(repo_root, config, checks).await,
        SubstrateBackendKind::None => checks.push(Check::pass(
            "substrate",
            "substrate checks skipped because backend is none",
        )),
    }

    checks.push(claude_hook_check(repo_root));
    checks.push(codex_instructions_check(repo_root));

    if config.tools().git().stacking().backend() != StackBackendKind::None {
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
    env_vars: BTreeSet<(String, String)>,
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
    match model.provider() {
        "shell" => {
            if let Some(cli) = model.cli() {
                if let Some(binary) = cli.split_whitespace().next() {
                    requirements.binaries.insert(binary.to_owned());
                }
            }
        }
        "openai-cli" => {
            requirements.binaries.insert("codex".to_owned());
        }
        "copilot-cli" => {
            requirements.binaries.insert("copilot".to_owned());
        }
        "anthropic" => {
            requirements
                .env_vars
                .insert(("anthropic".to_owned(), "ANTHROPIC_API_KEY".to_owned()));
        }
        "openai" => {
            requirements
                .env_vars
                .insert(("openai".to_owned(), "OPENAI_API_KEY".to_owned()));
        }
        "google" => {
            requirements
                .env_vars
                .insert(("google".to_owned(), "GOOGLE_API_KEY".to_owned()));
        }
        "bedrock" => {
            requirements
                .env_vars
                .insert(("bedrock".to_owned(), "AWS_ACCESS_KEY_ID".to_owned()));
        }
        "azure-openai" => {
            requirements
                .env_vars
                .insert(("azure-openai".to_owned(), "AZURE_OPENAI_API_KEY".to_owned()));
        }
        "ollama" => {
            requirements
                .env_vars
                .insert(("ollama".to_owned(), "OLLAMA_HOST".to_owned()));
        }
        "llamacpp" => {
            requirements
                .env_vars
                .insert(("llamacpp".to_owned(), "LLAMACPP_BASE_URL".to_owned()));
        }
        _ => {}
    }
}

fn host_binary(host: Host) -> &'static str {
    match host {
        Host::Claude => "claude",
        Host::Codex => "codex",
        Host::Copilot => "copilot",
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
}
