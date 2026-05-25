use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use tempfile::TempDir;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn derrick() -> TestResult<Command> {
    Ok(Command::cargo_bin("derrick")?)
}

fn repo() -> TestResult<TempDir> {
    let dir = tempfile::tempdir()?;
    fs::create_dir(dir.path().join(".git"))?;
    Ok(dir)
}

fn greenfield(dir: &Path) -> TestResult<assert_cmd::assert::Assert> {
    Ok(derrick()?
        .current_dir(dir)
        .env("DERRICK_SKIP_PREREQS", "1")
        .args([
            "init",
            "--greenfield",
            "--site",
            "test",
            "--prefix",
            "tst",
            "--mode",
            "solo",
        ])
        .assert())
}

fn adopted_init(dir: &Path) -> TestResult<assert_cmd::assert::Assert> {
    fs::write(dir.join("AGENTS.md"), "# Agents\n")?;
    fs::write(dir.join("CLAUDE.md"), "# Claude\n")?;
    Ok(derrick()?
        .current_dir(dir)
        .env("DERRICK_SKIP_PREREQS", "1")
        .args(["init", "--site", "test", "--prefix", "tst"])
        .assert())
}

fn mock_path(dir: &Path, names: &[&str]) -> TestResult<PathBuf> {
    let bin_dir = dir.join("bin");
    fs::create_dir(&bin_dir)?;
    for name in names {
        let path = bin_dir.join(name);
        fs::write(&path, "#!/bin/sh\nexit 0\n")?;
        let mut permissions = fs::metadata(&path)?.permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            permissions.set_mode(0o755);
        }

        fs::set_permissions(&path, permissions)?;
    }
    Ok(bin_dir)
}

fn mock_flow_path(dir: &Path) -> TestResult<PathBuf> {
    let bin_dir = dir.join("flow-bin");
    fs::create_dir(&bin_dir)?;
    write_executable(
        &bin_dir.join("claude"),
        r#"#!/bin/sh
# The prompt is always the last argument; flags like --print, --output-format,
# --dangerously-skip-permissions etc. precede it.
prompt=""
for arg in "$@"; do prompt="$arg"; done
# For specify, derrick pre-scaffolds the feature dir and amends the prompt
# with `Write the spec to: specs/<NNN>-<slug>/spec.md`. Extract that path so
# the mock writes to the same location derrick expects.
target=$(printf '%s' "$prompt" | /usr/bin/awk '/Write the spec to:/ { for (i=1;i<=NF;i++) if ($i ~ /^specs\//) { sub(/\/spec\.md$/, "", $i); print $i; exit } }')
if [ -z "$target" ]; then
  # Plan/tasks/etc — read feature.json for the feature dir.
  if [ -f .specify/feature.json ]; then
    target=$(/usr/bin/sed -n 's/.*"feature_directory"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' .specify/feature.json | /usr/bin/head -1)
  fi
fi
if [ -z "$target" ]; then target="specs/001-test"; fi
/bin/mkdir -p "$target" .specify
case "$prompt" in
  *speckit.specify*)
    printf '{"feature_directory":"%s"}' "$target" > .specify/feature.json
    printf 'spec' > "$target/spec.md"
    printf 'ok'
    ;;
  *speckit.plan*)
    printf 'plan' > "$target/plan.md"
    printf 'ok'
    ;;
  *speckit.tasks*)
    printf 'tasks' > "$target/tasks.md"
    printf 'ok'
    ;;
  *Verdict*|*Plan:*)
    # D37 codex-fallback: assay invokes the claude host instead of codex
    # in non-TTY contexts. Mirror the codex mock's verdict output.
    printf '## Verdict\naccept\n'
    ;;
  *)
    printf 'ok'
    ;;
esac
"#,
    )?;
    write_executable(
        &bin_dir.join("codex"),
        r#"#!/bin/sh
printf '## Verdict\naccept\n'
"#,
    )?;
    Ok(bin_dir)
}

fn write_executable(path: &Path, contents: &str) -> TestResult {
    fs::write(path, contents)?;
    let mut permissions = fs::metadata(path)?.permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(0o755);
    }
    fs::set_permissions(path, permissions)?;
    Ok(())
}

fn utf8(bytes: &[u8]) -> TestResult<String> {
    Ok(String::from_utf8(bytes.to_vec())?)
}

fn assert_contains(haystack: &[u8], needle: &str) -> TestResult {
    let text = utf8(haystack)?;
    assert!(
        text.contains(needle),
        "expected output to contain {needle:?}, got {text:?}"
    );
    Ok(())
}

fn assert_not_contains(haystack: &[u8], needle: &str) -> TestResult {
    let text = utf8(haystack)?;
    assert!(
        !text.contains(needle),
        "expected output not to contain {needle:?}, got {text:?}"
    );
    Ok(())
}

fn write_minimal_config(dir: &Path, backend: &str, mode: &str, pipeline: &str) -> TestResult {
    let pipeline_block = if pipeline == "[]" {
        "[]".to_owned()
    } else {
        format!("\n{pipeline}")
    };
    fs::write(
        dir.join("derrick.yaml"),
        format!(
            r#"
version: 1
site:
  name: test
  prefix: tst
models:
  claude-sonnet:
    provider: shell
    cli: "claude"
    model: claude-sonnet
roles:
  drafter: claude-sonnet
tools:
  speckit:
    enabled: true
    version: ">=0.4.0"
  assay:
    enabled: false
    role: drafter
    reviewers: [drafter]
  substrate:
    backend: {backend}
    mode: {mode}
  copilot:
    enabled: false
    agent_identity: derrick-hand
pipeline: {pipeline_block}
guardrails:
  constitution_path: .specify/memory/constitution.md
  forbid_paths: []
  required_labels: []
parallelism:
  batch_max: 8
  step_max: 4
  assay_max: 2
state:
  dir: .derrick
  log_runs: true
  worktree_root: .derrick/worktrees
"#
        ),
    )?;
    Ok(())
}

#[test]
fn bare_init_adopts_brownfield_repo() -> TestResult {
    let dir = repo()?;
    fs::write(dir.path().join("AGENTS.md"), "# Agents\n")?;
    fs::write(dir.path().join("CLAUDE.md"), "# Claude\n")?;

    let output = derrick()?
        .current_dir(dir.path())
        .env("DERRICK_SKIP_PREREQS", "1")
        .args(["init", "--site", "test", "--prefix", "tst"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    assert_contains(&output, "adoption plan")?;
    assert_contains(&output, "AGENTS.md as guardrails.agents_md")?;
    assert_contains(&output, "CLAUDE.md as guardrails.claude_md")?;
    assert!(dir.path().join("derrick.yaml").exists());
    assert!(dir.path().join(".derrick/derrick.db").exists());
    let config = fs::read_to_string(dir.path().join("derrick.yaml"))?;
    assert!(config.contains("# guardrails.agents_md: AGENTS.md"));
    assert!(config.contains("# guardrails.claude_md: CLAUDE.md"));
    Ok(())
}

#[test]
fn greenfield_init_in_empty_repo_creates_files() -> TestResult {
    let dir = repo()?;

    let output = greenfield(dir.path())?
        .success()
        .get_output()
        .stdout
        .clone();

    assert_contains(&output, "test  ready")?;
    assert!(dir.path().join("derrick.yaml").exists());
    assert!(dir.path().join(".derrick/derrick.db").exists());
    Ok(())
}

#[test]
fn greenfield_init_refuses_existing_yaml_without_force() -> TestResult {
    let dir = repo()?;
    fs::write(dir.path().join("derrick.yaml"), "not: overwritten")?;

    let output = greenfield(dir.path())?
        .failure()
        .get_output()
        .stderr
        .clone();

    assert_contains(&output, "--force")?;
    assert_eq!(
        fs::read_to_string(dir.path().join("derrick.yaml"))?,
        "not: overwritten"
    );
    Ok(())
}

#[test]
fn greenfield_init_overwrites_with_force() -> TestResult {
    let dir = repo()?;
    fs::write(dir.path().join("derrick.yaml"), "not: valid")?;

    derrick()?
        .current_dir(dir.path())
        .env("DERRICK_SKIP_PREREQS", "1")
        .args([
            "init",
            "--greenfield",
            "--site",
            "test",
            "--prefix",
            "tst",
            "--mode",
            "solo",
            "--force",
        ])
        .assert()
        .success();

    assert!(fs::read_to_string(dir.path().join("derrick.yaml"))?.contains("name: test"));
    Ok(())
}

#[test]
fn init_refuses_outside_git_repo() -> TestResult {
    let dir = tempfile::tempdir()?;

    let output = derrick()?
        .current_dir(dir.path())
        .env("DERRICK_SKIP_PREREQS", "1")
        .arg("init")
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    assert_contains(&output, "inside a git repo")?;

    let output = derrick()?
        .current_dir(dir.path())
        .env("DERRICK_SKIP_PREREQS", "1")
        .args(["init", "--greenfield"])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    assert_contains(&output, "inside a git repo")?;

    Ok(())
}

#[test]
fn greenfield_init_validates_prefix() -> TestResult {
    let dir = repo()?;

    let output = derrick()?
        .current_dir(dir.path())
        .env("DERRICK_SKIP_PREREQS", "1")
        .args(["init", "--greenfield", "--prefix", "BAD"])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();

    assert_contains(&output, "^[a-z]{1,6}$")?;
    Ok(())
}

#[test]
fn init_rejects_wizard_and_no_wizard_combination() -> TestResult {
    let output = derrick()?
        .args(["init", "--wizard", "--no-wizard"])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    assert_contains(&output, "cannot be used with")?;
    Ok(())
}

#[test]
fn init_accepts_project_alias_for_site() -> TestResult {
    let dir = repo()?;
    derrick()?
        .current_dir(dir.path())
        .env("DERRICK_SKIP_PREREQS", "1")
        .args([
            "init",
            "--greenfield",
            "--project",
            "testproj",
            "--prefix",
            "tst",
        ])
        .assert()
        .success();
    let config = fs::read_to_string(dir.path().join("derrick.yaml"))?;
    assert!(config.contains("name: testproj"));
    Ok(())
}

#[test]
fn greenfield_crew_init_writes_mode_roles_and_crew_steps() -> TestResult {
    let dir = repo()?;
    derrick()?
        .current_dir(dir.path())
        .env("DERRICK_SKIP_PREREQS", "1")
        .args([
            "init",
            "--greenfield",
            "--site",
            "test",
            "--prefix",
            "tst",
            "--mode",
            "crew",
            "--yes",
        ])
        .assert()
        .success();
    let config = fs::read_to_string(dir.path().join("derrick.yaml"))?;
    assert!(config.contains("mode: crew"));
    assert!(config.contains("proposer: claude-opus"));
    assert!(config.contains("id: bridge"));
    assert!(config.contains("id: foreman"));
    assert!(config.contains("executor_role: executor"));
    Ok(())
}

#[test]
fn status_shows_site_after_init() -> TestResult {
    let dir = repo()?;
    greenfield(dir.path())?.success();

    let output = derrick()?
        .current_dir(dir.path())
        .arg("status")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    assert_contains(&output, "test")?;
    assert_contains(&output, "mode: solo")?;
    Ok(())
}

#[test]
fn status_json_round_trips() -> TestResult {
    let dir = repo()?;
    greenfield(dir.path())?.success();

    let output = derrick()?
        .current_dir(dir.path())
        .args(["status", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: serde_json::Value = serde_json::from_slice(&output)?;

    assert_eq!(value["site"], "test");
    assert_eq!(value["mode"], "solo");
    assert_eq!(value["backend"], "native");
    Ok(())
}

#[test]
fn doctor_passes_after_successful_init() -> TestResult {
    let dir = repo()?;
    greenfield(dir.path())?.success();
    let path = mock_path(dir.path(), &["git", "claude", "codex"])?;

    let output = derrick()?
        .current_dir(dir.path())
        .env("PATH", path)
        .arg("doctor")
        .assert()
        .code(0)
        .get_output()
        .stdout
        .clone();

    assert_not_contains(&output, "fail")?;
    Ok(())
}

#[test]
fn doctor_reports_claude_hooks_as_installed_when_markers_exist() -> TestResult {
    let dir = repo()?;
    adopted_init(dir.path())?.success();
    let path = mock_path(dir.path(), &["git", "claude", "codex"])?;

    let output = derrick()?
        .current_dir(dir.path())
        .env("PATH", path)
        .arg("doctor")
        .assert()
        .code(0)
        .get_output()
        .stdout
        .clone();

    assert_contains(&output, "Claude Code hooks")?;
    assert_contains(&output, "derrick D29 scrub and caveman hooks are installed")?;
    Ok(())
}

#[test]
fn doctor_warns_when_claude_hook_markers_are_missing() -> TestResult {
    let dir = repo()?;
    adopted_init(dir.path())?.success();
    fs::write(dir.path().join(".claude/settings.json"), "{\"hooks\":{}}")?;
    let path = mock_path(dir.path(), &["git", "claude", "codex"])?;

    let output = derrick()?
        .current_dir(dir.path())
        .env("PATH", path)
        .arg("doctor")
        .assert()
        .code(0)
        .get_output()
        .stdout
        .clone();

    assert_contains(&output, "warn")?;
    assert_contains(&output, "Claude Code hooks")?;
    assert_contains(
        &output,
        "derrick D29 scrub and caveman hooks are missing from .claude/settings.json",
    )?;
    Ok(())
}

#[test]
fn doctor_warns_when_claude_settings_json_is_invalid() -> TestResult {
    let dir = repo()?;
    adopted_init(dir.path())?.success();
    fs::write(dir.path().join(".claude/settings.json"), "{")?;
    let path = mock_path(dir.path(), &["git", "claude", "codex"])?;

    let output = derrick()?
        .current_dir(dir.path())
        .env("PATH", path)
        .arg("doctor")
        .assert()
        .code(0)
        .get_output()
        .stdout
        .clone();

    assert_contains(&output, "warn")?;
    assert_contains(&output, "Claude Code hooks")?;
    assert_contains(&output, ".claude/settings.json is not valid JSON")?;
    Ok(())
}

#[test]
fn doctor_json_round_trips() -> TestResult {
    let dir = repo()?;
    greenfield(dir.path())?.success();
    let path = mock_path(dir.path(), &["git", "claude", "codex"])?;

    let output = derrick()?
        .current_dir(dir.path())
        .env("PATH", path)
        .args(["doctor", "--format", "json"])
        .assert()
        .code(0)
        .get_output()
        .stdout
        .clone();
    let value: serde_json::Value = serde_json::from_slice(&output)?;

    assert!(value.as_array().is_some_and(|checks| !checks.is_empty()));
    assert_contains(&output, "derrick.yaml")?;
    Ok(())
}

#[test]
fn doctor_fails_when_yaml_missing() -> TestResult {
    let dir = repo()?;
    let path = mock_path(dir.path(), &["git"])?;

    let output = derrick()?
        .current_dir(dir.path())
        .env("PATH", path)
        .arg("doctor")
        .assert()
        .code(1)
        .get_output()
        .stdout
        .clone();

    assert_contains(&output, "derrick.yaml")?;
    assert_contains(&output, "does not exist")?;
    Ok(())
}

#[test]
fn doctor_fails_when_yaml_invalid() -> TestResult {
    let dir = repo()?;
    fs::write(dir.path().join("derrick.yaml"), "not: [valid")?;
    let path = mock_path(dir.path(), &["git"])?;

    let output = derrick()?
        .current_dir(dir.path())
        .env("PATH", path)
        .arg("doctor")
        .assert()
        .code(1)
        .get_output()
        .stdout
        .clone();

    assert_contains(&output, "derrick.yaml")?;
    assert_contains(&output, "fail")?;
    Ok(())
}

#[test]
fn doctor_fails_for_reachable_env_provider() -> TestResult {
    let dir = repo()?;
    fs::write(
        dir.path().join("derrick.yaml"),
        r#"
version: 1
site:
  name: test
  prefix: tst
models:
  claude-sonnet:
    provider: anthropic
    model: claude-sonnet
roles:
  drafter: claude-sonnet
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
    enabled: false
    agent_identity: derrick-hand
pipeline:
  - id: specify
    role: drafter
    host: claude
    command: "/speckit.specify {{prompt}}"
guardrails:
  constitution_path: .specify/memory/constitution.md
  forbid_paths: []
  required_labels: []
parallelism:
  batch_max: 8
  step_max: 4
  assay_max: 2
state:
  dir: .derrick
  log_runs: true
  worktree_root: .derrick/worktrees
"#,
    )?;
    let path = mock_path(dir.path(), &["git", "claude"])?;

    let output = derrick()?
        .current_dir(dir.path())
        .env("PATH", path)
        .env_remove("ANTHROPIC_API_KEY")
        .arg("doctor")
        .assert()
        .code(1)
        .get_output()
        .stdout
        .clone();

    assert_contains(&output, "ANTHROPIC_API_KEY")?;
    Ok(())
}

#[test]
fn doctor_skips_state_for_substrate_none() -> TestResult {
    let dir = repo()?;
    write_minimal_config(dir.path(), "none", "solo", "[]")?;
    let path = mock_path(dir.path(), &["git"])?;

    derrick()?
        .current_dir(dir.path())
        .env("PATH", path)
        .arg("doctor")
        .assert()
        .code(0);

    Ok(())
}

#[test]
fn doctor_fails_when_substrate_corrupt() -> TestResult {
    let dir = repo()?;
    greenfield(dir.path())?.success();
    fs::write(dir.path().join(".derrick/derrick.db"), "not sqlite")?;
    let path = mock_path(dir.path(), &["git", "claude", "codex"])?;

    let output = derrick()?
        .current_dir(dir.path())
        .env("PATH", path)
        .arg("doctor")
        .assert()
        .code(1)
        .get_output()
        .stdout
        .clone();

    assert_contains(&output, "native substrate")?;
    assert_contains(&output, "fail")?;
    Ok(())
}

#[test]
fn status_fails_when_native_db_missing() -> TestResult {
    let dir = repo()?;
    greenfield(dir.path())?.success();
    fs::remove_file(dir.path().join(".derrick/derrick.db"))?;

    let output = derrick()?
        .current_dir(dir.path())
        .arg("status")
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();

    assert_contains(&output, "derrick.db")?;
    Ok(())
}

#[test]
fn status_handles_substrate_none_json() -> TestResult {
    let dir = repo()?;
    write_minimal_config(dir.path(), "none", "crew", "[]")?;

    let output = derrick()?
        .current_dir(dir.path())
        .args(["status", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: serde_json::Value = serde_json::from_slice(&output)?;

    assert_eq!(value["backend"], "none");
    assert_eq!(value["mode"], "crew");
    Ok(())
}

#[test]
fn doctor_exit_code_equals_fail_count() -> TestResult {
    let dir = repo()?;
    greenfield(dir.path())?.success();
    let path = mock_path(dir.path(), &["git"])?;

    let output = derrick()?
        .current_dir(dir.path())
        .env("PATH", path)
        .arg("doctor")
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();

    assert_contains(&output, "claude")?;
    assert_contains(&output, "codex")?;
    Ok(())
}

#[test]
fn run_add_feature_smoke_writes_real_artifacts() -> TestResult {
    let dir = repo()?;
    greenfield(dir.path())?.success();
    fs::create_dir_all(dir.path().join(".specify/memory"))?;
    fs::write(
        dir.path().join(".specify/memory/constitution.md"),
        "constitution",
    )?;
    let path = mock_flow_path(dir.path())?;

    let output = derrick()?
        .current_dir(dir.path())
        .env("PATH", path)
        .args(["run", "add-feature", "--prompt", "hello", "--run", "smoke"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    assert_contains(&output, "smoke")?;
    // derrick now pre-scaffolds the feature dir from the prompt slug
    // ("hello" → 001-hello). The mock claude binary reads the target path
    // from the amended prompt and writes there.
    assert!(dir.path().join("specs/001-hello/spec.md").exists());
    assert!(dir.path().join("specs/001-hello/plan.md").exists());
    assert!(dir.path().join("specs/001-hello/tasks.md").exists());
    assert!(dir.path().join("specs/001-hello/assay/verdict.md").exists());
    assert!(dir
        .path()
        .join(".derrick/runs/smoke/manifest.json")
        .exists());
    Ok(())
}

#[test]
fn completions_emit_for_each_shell() -> TestResult {
    for shell in ["bash", "zsh", "fish", "elvish", "powershell"] {
        let output = derrick()?
            .args(["completions", shell])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        assert_contains(&output, "derrick")?;
    }
    Ok(())
}

#[test]
fn version_matches_cargo_pkg_version() -> TestResult {
    let output = derrick()?
        .arg("--version")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    assert_contains(&output, &format!("derrick {}", env!("CARGO_PKG_VERSION")))?;
    Ok(())
}
