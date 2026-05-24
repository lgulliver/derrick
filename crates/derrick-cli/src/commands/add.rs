//! `derrick add` — positional-prompt shorthand for `derrick run add-feature`.
//!
//! All flags are identical; the only difference is that the feature description
//! is a positional argument rather than `--prompt "..."`, making one-liners
//! feel natural:
//!
//! ```text
//! derrick add "build a webhook ingest endpoint with idempotent dedupe"
//! ```
//!
//! If no prompt or `--resume-from` is given, checks for an incomplete or failed
//! run and suggests using `derrick run resume`.

use std::path::Path;

use crate::commands::{AddArgs, AddFeatureArgs, RunArgs, RunCommand};
use crate::exit_code::CliExitCode;
use crate::{current_repo_root, read_config, CliError};

pub(crate) async fn execute(args: AddArgs) -> Result<CliExitCode, CliError> {
    if args.prompt.is_none() && args.resume_from.is_none() {
        if let Ok(Some(run_id)) = find_incomplete_run(&current_repo_root()?) {
            eprintln!(
                "Incomplete or failed run detected: {run_id}
Use `derrick run resume` to resume it, or provide a prompt to start a new run."
            );
            return Ok(CliExitCode::Refused);
        }
    }

    let add_feature = AddFeatureArgs {
        prompt: args.prompt,
        resume_from: args.resume_from,
        run_id: args.run_id,
        skip: args.skip,
        unskip: args.unskip,
        dry_run: args.dry_run,
        no_clarify: args.no_clarify,
        no_assay: args.no_assay,
        no_github_issues: args.no_github_issues,
    };
    super::run::execute(RunArgs {
        command: RunCommand::AddFeature(add_feature),
    })
    .await
}

/// Find the latest run that is incomplete (no `finished_at`) or failed/halted.
fn find_incomplete_run(repo_root: &Path) -> Result<Option<String>, CliError> {
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
        if finished_at.is_none() || status == "failed" || status == "halted" {
            return Ok(Some(run_id));
        }
        if status == "success" {
            return Ok(None);
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use crate::commands::AddArgs;

    fn default_args() -> AddArgs {
        AddArgs {
            prompt: None,
            resume_from: None,
            run_id: None,
            skip: vec![],
            unskip: vec![],
            dry_run: false,
            no_clarify: false,
            no_assay: false,
            no_github_issues: false,
        }
    }

    #[test]
    fn add_args_converts_prompt() {
        let args = AddArgs {
            prompt: Some("build a webhook endpoint".to_owned()),
            ..default_args()
        };
        assert_eq!(args.prompt.as_deref(), Some("build a webhook endpoint"));
    }

    #[test]
    fn add_args_skip_flags_independent() {
        let args = AddArgs {
            no_clarify: true,
            no_assay: true,
            ..default_args()
        };
        assert!(args.no_clarify);
        assert!(args.no_assay);
    }
}
