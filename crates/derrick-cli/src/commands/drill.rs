//! `derrick drill` — positional-prompt shorthand for `derrick run drill`.
//!
//! All flags are identical; the only difference is that the feature description
//! is a positional argument rather than `--prompt "..."`, making one-liners
//! feel natural:
//!
//! ```text
//! derrick drill "build a webhook ingest endpoint with idempotent dedupe"
//! ```
//!
//! ## Auto-resume
//!
//! When a prompt is given and no explicit `--resume-from` or `--force` is
//! passed, `drill` normalises the prompt to a stable SHA256 key and scans
//! `.derrick/runs/` for an incomplete run with the same key.  If one is
//! found the run is resumed from its last successful step rather than
//! starting fresh (which would cause ticket-ID collisions in bridge).
//!
//! ## Force restart
//!
//! `--force` skips the key scan and starts a brand-new run.  The new
//! manifest records `resume_of: <old_run_id>` so the lineage is traceable.
//!
//! ## No-prompt fallback
//!
//! If no prompt or `--resume-from` is given, any incomplete run is printed
//! and the user is directed to `derrick run resume`.

use std::path::Path;

use owo_colors::OwoColorize;

use crate::commands::{DrillArgs, DrillRunArgs, RunArgs, RunCommand};
use crate::exit_code::CliExitCode;
use crate::{CliError, current_repo_root, read_config};

pub(crate) async fn execute(args: DrillArgs) -> Result<CliExitCode, CliError> {
    let repo_root = current_repo_root()?;

    // Resolve the prompt from the positional string, `--prompt-file`, or stdin
    // exactly once (stdin can only be read once).  When nothing is supplied and
    // stdin is a terminal this returns `None`, preserving the no-prompt
    // fallback below.
    let prompt =
        crate::commands::prompt_input::resolve_prompt_from_env(args.prompt, args.prompt_file)?;

    // ── No-prompt fallback ────────────────────────────────────────────────
    if prompt.is_none() && args.resume_from.is_none() {
        if let Ok(Some(run_id)) = find_incomplete_run(&repo_root) {
            eprintln!(
                "Incomplete or failed run detected: {run_id}\n\
                 Use `derrick run resume` to resume it, or provide a prompt to start a new run."
            );
            return Ok(CliExitCode::Refused);
        }
    }

    // ── Prompt-key auto-resume ────────────────────────────────────────────
    let (auto_resume, run_id_override, force_prior_run_id) = if args.resume_from.is_none()
        && args.run_id.is_none()
    {
        if let Some(ref prompt) = prompt {
            if args.force {
                // Force restart: look for a prior run to record as lineage.
                let prior = find_incomplete_run_for_prompt(prompt, &repo_root).unwrap_or_default();
                (false, None, prior)
            } else {
                // Normal path: auto-resume if an incomplete run matches.
                match find_incomplete_run_for_prompt(prompt, &repo_root) {
                    Ok(Some(run_id)) => {
                        eprintln!(
                            "  {} Resuming incomplete run {} for this feature…",
                            "\u{21bb}".cyan(),
                            run_id.bright_black()
                        );
                        (true, Some(run_id), None)
                    }
                    _ => (false, None, None),
                }
            }
        } else {
            (false, None, None)
        }
    } else {
        (false, None, None)
    };

    let drill_run = DrillRunArgs {
        prompt,
        // Already resolved above; do not let `run::execute` re-read stdin/file.
        prompt_file: None,
        resume_from: args.resume_from,
        run_id: run_id_override.or(args.run_id),
        skip: args.skip,
        unskip: args.unskip,
        dry_run: args.dry_run,
        no_clarify: args.no_clarify,
        no_assay: args.no_assay,
        no_github_issues: args.no_github_issues,
        profile: args.profile,
        spec: args.spec,
        auto_resume,
        force_prior_run_id,
    };
    super::run::execute(RunArgs {
        command: RunCommand::Drill(drill_run),
    })
    .await
}

// ── Run-scanning helpers ──────────────────────────────────────────────────────

/// Scan `.derrick/runs/` and return the most recent run_id that is incomplete
/// (no `finished_at` or `status` is `"failed"` / `"halted"`) **and** whose
/// `prompt_key` matches the normalised key derived from `prompt`.
///
/// Returns `None` if no matching incomplete run exists.
fn find_incomplete_run_for_prompt(
    prompt: &str,
    repo_root: &Path,
) -> Result<Option<String>, CliError> {
    use derrick_flow::compute_prompt_key;
    let key = compute_prompt_key(prompt);
    scan_runs(repo_root, |value| {
        let run_prompt_key = value
            .get("prompt_key")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        run_prompt_key == key
    })
}

/// Find the latest run that is incomplete (no `finished_at`) or failed/halted,
/// regardless of prompt.  Used when no prompt is given.
fn find_incomplete_run(repo_root: &Path) -> Result<Option<String>, CliError> {
    scan_runs(repo_root, |_| true)
}

/// Walk `.derrick/runs/` newest-first; for each manifest that is incomplete
/// and passes `predicate`, return the run_id.  Returns `None` when no match
/// is found or when a `success` run is encountered first (meaning the feature
/// already completed).
fn scan_runs(
    repo_root: &Path,
    predicate: impl Fn(&serde_json::Value) -> bool,
) -> Result<Option<String>, CliError> {
    let config = read_config(repo_root)?;
    let runs_dir = repo_root.join(config.state().dir()).join("runs");
    if !runs_dir.exists() {
        return Ok(None);
    }
    let mut entries: Vec<_> = std::fs::read_dir(&runs_dir)
        .map_err(|source| CliError::Io {
            path: runs_dir.clone(),
            source,
        })?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| e.file_name().to_str().map(|s| s.to_owned()))
        .collect();
    entries.sort();
    // Walk most recent first
    for run_id in entries.into_iter().rev() {
        let manifest_path = runs_dir.join(&run_id).join("manifest.json");
        if !manifest_path.exists() {
            continue;
        }
        let contents = match std::fs::read_to_string(&manifest_path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let value: serde_json::Value = match serde_json::from_str(&contents) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let finished_at = value.get("finished_at").and_then(|v| v.as_str());
        let status = value.get("status").and_then(|v| v.as_str()).unwrap_or("");
        let is_incomplete = finished_at.is_none() || status == "failed" || status == "halted";
        if is_incomplete && predicate(&value) {
            return Ok(Some(run_id));
        }
        if status == "success" && predicate(&value) {
            // Matching run already completed successfully — no resume needed.
            return Ok(None);
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use crate::commands::DrillArgs;

    fn default_args() -> DrillArgs {
        DrillArgs {
            prompt: None,
            prompt_file: None,
            resume_from: None,
            run_id: None,
            skip: vec![],
            unskip: vec![],
            dry_run: false,
            no_clarify: false,
            no_assay: false,
            no_github_issues: false,
            profile: None,
            force: false,
            spec: None,
        }
    }

    #[test]
    fn drill_args_converts_prompt() {
        let args = DrillArgs {
            prompt: Some("build a webhook endpoint".to_owned()),
            ..default_args()
        };
        assert_eq!(args.prompt.as_deref(), Some("build a webhook endpoint"));
    }

    #[test]
    fn drill_args_skip_flags_independent() {
        let args = DrillArgs {
            no_clarify: true,
            no_assay: true,
            ..default_args()
        };
        assert!(args.no_clarify);
        assert!(args.no_assay);
    }

    // ── compute_prompt_key (via derrick_flow) ────────────────────────────

    #[test]
    fn prompt_key_is_stable_across_case_and_whitespace() {
        use derrick_flow::compute_prompt_key;
        let a = compute_prompt_key("Build a webhook endpoint");
        let b = compute_prompt_key("build a webhook endpoint");
        let c = compute_prompt_key("  build  a  webhook  endpoint  ");
        assert_eq!(a, b, "case should not affect key");
        assert_eq!(
            b, c,
            "leading/trailing/internal whitespace should not affect key"
        );
    }

    #[test]
    fn prompt_key_differs_for_different_prompts() {
        use derrick_flow::compute_prompt_key;
        let a = compute_prompt_key("build a webhook endpoint");
        let b = compute_prompt_key("add rate limiting to the API");
        assert_ne!(a, b);
    }

    #[test]
    fn prompt_key_is_12_hex_chars() {
        use derrick_flow::compute_prompt_key;
        let key = compute_prompt_key("some feature prompt");
        assert_eq!(key.len(), 12, "key should be 12 hex chars");
        assert!(
            key.chars().all(|c| c.is_ascii_hexdigit()),
            "key should be hex"
        );
    }

    // ── scan_runs / find_incomplete_run_for_prompt ───────────────────────

    /// Create a manifest under `<repo_root>/.derrick/runs/<run_id>/manifest.json`.
    fn make_manifest(
        repo_root: &std::path::Path,
        run_id: &str,
        prompt_key: &str,
        status: &str,
        finished: bool,
    ) {
        let run_dir = repo_root.join(".derrick").join("runs").join(run_id);
        std::fs::create_dir_all(&run_dir).unwrap();
        let finished_at = if finished {
            r#""2026-05-24T10:00:00Z""#
        } else {
            "null"
        };
        let json = format!(
            r#"{{"run_id":"{run_id}","pipeline_id":"drill","prompt":"x","prompt_key":"{prompt_key}","flags":{{"skip":[],"unskip":[],"dry_run":false}},"config_hash":"sha256:abc","started_at":"2026-05-24T09:00:00Z","finished_at":{finished_at},"status":"{status}","steps":[]}}"#,
        );
        std::fs::write(run_dir.join("manifest.json"), json).unwrap();
    }

    // Minimal derrick.yaml accepted by `read_config` (mirrors the `minimal_yaml()`
    // fixture in `derrick-config`).
    const TEST_CONFIG: &str = "\
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
    enabled: false
    version: \">=1.0.0\"
  assay:
    enabled: false
    role: drafter
    reviewers: []
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
";

    #[test]
    fn find_incomplete_run_for_prompt_returns_matching_run() {
        use derrick_flow::compute_prompt_key;
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("derrick.yaml"), TEST_CONFIG).unwrap();

        let prompt = "build a webhook endpoint";
        let key = compute_prompt_key(prompt);
        make_manifest(tmp.path(), "20260524T090000Z", &key, "failed", false);

        let result = super::find_incomplete_run_for_prompt(prompt, tmp.path()).unwrap();
        assert_eq!(result, Some("20260524T090000Z".to_owned()));
    }

    #[test]
    fn find_incomplete_run_for_prompt_ignores_different_prompt() {
        use derrick_flow::compute_prompt_key;
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("derrick.yaml"), TEST_CONFIG).unwrap();

        let other_key = compute_prompt_key("add rate limiting to the API");
        // Incomplete run with a DIFFERENT prompt key
        make_manifest(tmp.path(), "20260524T090000Z", &other_key, "failed", false);

        let result =
            super::find_incomplete_run_for_prompt("build a webhook endpoint", tmp.path()).unwrap();
        assert_eq!(result, None, "should not match a different prompt");
    }

    #[test]
    fn find_incomplete_run_for_prompt_returns_none_when_completed() {
        use derrick_flow::compute_prompt_key;
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("derrick.yaml"), TEST_CONFIG).unwrap();

        let prompt = "build a webhook endpoint";
        let key = compute_prompt_key(prompt);
        // Completed run with matching key — should not offer resume
        make_manifest(tmp.path(), "20260524T090000Z", &key, "success", true);

        let result = super::find_incomplete_run_for_prompt(prompt, tmp.path()).unwrap();
        assert_eq!(result, None, "completed run should not trigger resume");
    }

    #[test]
    fn find_incomplete_run_for_prompt_returns_most_recent_when_multiple() {
        use derrick_flow::compute_prompt_key;
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("derrick.yaml"), TEST_CONFIG).unwrap();

        let prompt = "build a webhook endpoint";
        let key = compute_prompt_key(prompt);

        // Two incomplete runs — should return the newer one
        make_manifest(tmp.path(), "20260524T080000Z", &key, "halted", false);
        make_manifest(tmp.path(), "20260524T090000Z", &key, "failed", false);

        let result = super::find_incomplete_run_for_prompt(prompt, tmp.path()).unwrap();
        assert_eq!(result, Some("20260524T090000Z".to_owned()));
    }
}
