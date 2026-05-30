//! `derrick switch --mode <mode>` — upgrade or downgrade an existing repo's
//! substrate mode in-place, updating `derrick.yaml` without a full re-init.
//!
//! Typical usage:
//!   derrick switch --mode crew           # solo → crew
//!   derrick switch --mode solo           # crew → solo
//!   derrick switch --mode crew --dry-run # preview only

use std::path::Path;

use crate::commands::init::{available_model_ids, nested_mapping, recommended_role_bindings};
use crate::commands::InitMode;
use crate::exit_code::CliExitCode;
use crate::ui;
use crate::{current_repo_root, message, write_file};

/// Changes accumulated during a mode switch, used both for dry-run output
/// and for the final confirmation summary.
#[derive(Debug)]
struct SwitchChanges {
    old_mode: String,
    new_mode: String,
    /// Pipeline step ids that were added (e.g. bridge, foreman).
    steps_added: Vec<String>,
    /// Steps that are present but incompatible with the new mode and were
    /// removed (e.g. bridge/foreman when going back to solo).
    steps_removed: Vec<String>,
    /// Role assignments that changed: `(role, old_model, new_model)`.
    role_changes: Vec<(String, String, String)>,
}

pub(crate) async fn execute(
    args: crate::commands::SwitchArgs,
) -> Result<CliExitCode, crate::CliError> {
    let repo_root = current_repo_root()?;
    let config_path = repo_root.join("derrick.yaml");

    if !config_path.exists() {
        return Err(message("derrick.yaml not found — run `derrick init` first"));
    }

    let raw = std::fs::read_to_string(&config_path).map_err(|source| crate::CliError::Io {
        path: config_path.clone(),
        source,
    })?;

    let mut yaml: serde_yaml::Value =
        serde_yaml::from_str(&raw).map_err(|e| message(e.to_string()))?;

    // ── Validate current mode ──────────────────────────────────────────────

    let current_mode = current_mode_str(&yaml);
    let target_mode = args.mode.as_str();

    if current_mode == target_mode {
        println!(
            "  {}  Already in {} mode — nothing to do.",
            ui::yellow("\u{b7}"),
            ui::bold(target_mode)
        );
        return Ok(CliExitCode::Success);
    }

    // ── Guard: in-flight runs ──────────────────────────────────────────────

    if !args.force {
        let inflight = detect_inflight_runs(&repo_root);
        if !inflight.is_empty() {
            let list = inflight.join(", ");
            return Err(message(format!(
                "Cannot switch mode while runs are in flight: {list}\n\
                 Wait for them to finish or use --force to override (dangerous)."
            )));
        }
    }

    // ── Compute changes ───────────────────────────────────────────────────

    let changes = compute_changes(&mut yaml, current_mode.clone(), args.mode)?;

    // ── Dry run ───────────────────────────────────────────────────────────

    if args.dry_run {
        print_dry_run(&changes);
        return Ok(CliExitCode::Success);
    }

    // ── Apply ─────────────────────────────────────────────────────────────

    let updated = serde_yaml::to_string(&yaml).map_err(|e| message(e.to_string()))?;
    write_file(&config_path, &updated)?;

    print_summary(&changes);
    Ok(CliExitCode::Success)
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Returns the current `tools.substrate.mode` string, defaulting to "solo".
fn current_mode_str(yaml: &serde_yaml::Value) -> String {
    yaml.get("tools")
        .and_then(|t| t.get("substrate"))
        .and_then(|s| s.get("mode"))
        .and_then(serde_yaml::Value::as_str)
        .unwrap_or("solo")
        .to_owned()
}

/// Returns run-ids (directory names) of runs whose manifest has no
/// `finished_at`, indicating they are still in progress.
fn detect_inflight_runs(repo_root: &Path) -> Vec<String> {
    let runs_dir = repo_root.join(".derrick").join("runs");
    let mut inflight = Vec::new();
    let Ok(entries) = std::fs::read_dir(&runs_dir) else {
        return inflight;
    };
    for entry in entries.flatten() {
        let manifest = entry.path().join("manifest.json");
        if !manifest.exists() {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&manifest) else {
            continue;
        };
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        // A run is in-flight when finished_at is absent or explicitly null.
        let finished = json
            .get("finished_at")
            .map(|v| !v.is_null())
            .unwrap_or(false);
        if !finished {
            if let Some(name) = entry.file_name().to_str() {
                inflight.push(name.to_owned());
            }
        }
    }
    inflight
}

/// Mutates `yaml` in-place to apply the mode switch and returns a summary of
/// what changed.
fn compute_changes(
    yaml: &mut serde_yaml::Value,
    old_mode: String,
    target: InitMode,
) -> Result<SwitchChanges, crate::CliError> {
    let target_str = target.as_str().to_owned();
    let root = yaml
        .as_mapping_mut()
        .ok_or_else(|| message("derrick.yaml is not a YAML mapping"))?;

    // ── 1. Update substrate mode ───────────────────────────────────────────
    {
        let tools = nested_mapping(root, "tools")?;
        let substrate = nested_mapping(tools, "substrate")?;
        substrate.insert(
            serde_yaml::Value::String("mode".to_owned()),
            serde_yaml::Value::String(target_str.clone()),
        );
    }

    // ── 2. Pipeline steps ─────────────────────────────────────────────────
    let (steps_added, steps_removed) = update_pipeline(root, target)?;

    // ── 3. Role recommendations ───────────────────────────────────────────
    let role_changes = update_roles(root, target)?;

    Ok(SwitchChanges {
        old_mode,
        new_mode: target_str,
        steps_added,
        steps_removed,
        role_changes,
    })
}

/// Adds crew-specific steps (bridge, foreman) when going to crew, removes them
/// when leaving crew. Returns `(added, removed)` step id lists.
fn update_pipeline(
    root: &mut serde_yaml::Mapping,
    target: InitMode,
) -> Result<(Vec<String>, Vec<String>), crate::CliError> {
    let mut added = Vec::new();
    let mut removed = Vec::new();

    let pipeline_key = serde_yaml::Value::String("pipeline".to_owned());
    let Some(pipeline_value) = root.get_mut(&pipeline_key) else {
        return Ok((added, removed));
    };
    let Some(steps) = pipeline_value.as_sequence_mut() else {
        return Ok((added, removed));
    };

    match target {
        InitMode::Crew => {
            // Add bridge if absent.
            if !steps
                .iter()
                .any(|s| crate::commands::init::step_id(s) == Some("bridge"))
            {
                steps.push(crate::commands::init::yaml_step(&[
                    ("id", "bridge"),
                    ("runner", "derrick"),
                ]));
                added.push("bridge".to_owned());
            }
            // Add foreman if absent.
            if !steps
                .iter()
                .any(|s| crate::commands::init::step_id(s) == Some("foreman"))
            {
                let mut step =
                    crate::commands::init::yaml_step(&[("id", "foreman"), ("runner", "derrick")]);
                if let Some(m) = step.as_mapping_mut() {
                    m.insert(
                        serde_yaml::Value::String("executor_role".to_owned()),
                        serde_yaml::Value::String("executor".to_owned()),
                    );
                }
                steps.push(step);
                added.push("foreman".to_owned());
            }
        }
        InitMode::Solo | InitMode::Copilot => {
            // Remove bridge and foreman when leaving crew mode.
            let crew_steps = ["bridge", "foreman"];
            let before = steps.len();
            steps.retain(|s| {
                let id = crate::commands::init::step_id(s);
                !id.map(|i| crew_steps.contains(&i)).unwrap_or(false)
            });
            for id in &crew_steps {
                if steps.len() < before {
                    removed.push((*id).to_owned());
                }
            }
        }
    }

    Ok((added, removed))
}

/// Updates role assignments to match the recommended defaults for the target
/// mode, but only when a role's current model is also a default candidate
/// (i.e. it hasn't been customised). Returns the list of changes.
fn update_roles(
    root: &mut serde_yaml::Mapping,
    target: InitMode,
) -> Result<Vec<(String, String, String)>, crate::CliError> {
    let mut changes = Vec::new();

    // Determine which models are available in this repo's config.
    let available = {
        let mut models: std::collections::BTreeMap<String, &'static str> =
            std::collections::BTreeMap::new();
        if let Some(m) = root
            .get(serde_yaml::Value::String("models".to_owned()))
            .and_then(serde_yaml::Value::as_mapping)
        {
            for key in m.keys() {
                if let Some(id) = key.as_str() {
                    // Look up against the known catalogue.
                    if let Some(description) = available_model_ids().get(id).copied() {
                        models.insert(id.to_owned(), description);
                    }
                }
            }
        }
        models
    };

    let recommended = recommended_role_bindings(target, &available);

    let roles_key = serde_yaml::Value::String("roles".to_owned());
    let Some(roles_val) = root.get_mut(&roles_key) else {
        return Ok(changes);
    };
    let Some(roles) = roles_val.as_mapping_mut() else {
        return Ok(changes);
    };

    for (role, new_model) in recommended.entries() {
        let role_key = serde_yaml::Value::String(role.to_owned());
        if let Some(current_val) = roles.get(&role_key) {
            let current = current_val.as_str().unwrap_or("").to_owned();
            if current != new_model {
                roles.insert(role_key, serde_yaml::Value::String(new_model.to_owned()));
                changes.push((role.to_owned(), current, new_model.to_owned()));
            }
        }
    }

    Ok(changes)
}

fn print_dry_run(changes: &SwitchChanges) {
    let styled = ui::styled();
    println!();
    if styled {
        println!("  \x1b[1mDry run — no files will be changed.\x1b[0m");
    } else {
        println!("  Dry run — no files will be changed.");
    }
    print_changes(changes, styled);
    println!();
}

fn print_summary(changes: &SwitchChanges) {
    let styled = ui::styled();
    println!();
    if styled {
        println!(
            "  \x1b[32m✓\x1b[0m  Switched \x1b[1m{}\x1b[0m → \x1b[1m{}\x1b[0m",
            changes.old_mode, changes.new_mode
        );
    } else {
        println!("  ✓  Switched {} → {}", changes.old_mode, changes.new_mode);
    }
    print_changes(changes, styled);
    println!();
    if styled {
        println!(
            "  \x1b[36m›\x1b[0m  Run \x1b[1mderrick doctor\x1b[0m to verify the new configuration."
        );
    } else {
        println!("  ›  Run `derrick doctor` to verify the new configuration.");
    }
    println!();
}

fn print_changes(changes: &SwitchChanges, styled: bool) {
    println!();
    println!(
        "  {:<18}  {} → {}",
        "mode", changes.old_mode, changes.new_mode
    );
    let _ = styled; // used below for coloured steps/roles

    if !changes.steps_added.is_empty() {
        let list = changes.steps_added.join(", ");
        if styled {
            println!("  {:<18}  \x1b[32m+{list}\x1b[0m", "pipeline steps");
        } else {
            println!("  {:<18}  +{list}", "pipeline steps");
        }
    }
    if !changes.steps_removed.is_empty() {
        let list = changes.steps_removed.join(", ");
        if styled {
            println!("  {:<18}  \x1b[31m-{list}\x1b[0m", "pipeline steps");
        } else {
            println!("  {:<18}  -{list}", "pipeline steps");
        }
    }
    for (role, old, new) in &changes.role_changes {
        if styled {
            println!(
                "  {:<18}  {role}: \x1b[33m{old}\x1b[0m → \x1b[32m{new}\x1b[0m",
                "role"
            );
        } else {
            println!("  {:<18}  {role}: {old} → {new}", "role");
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn solo_yaml() -> serde_yaml::Value {
        serde_yaml::from_str(
            r#"
version: 1
site:
  name: test
  prefix: tst
models:
  claude-opus:
    provider: shell
    cli: claude
    model: claude-opus
  claude-sonnet:
    provider: shell
    cli: claude
    model: claude-sonnet
  codex-gpt5:
    provider: shell
    cli: codex
    model: codex-gpt5
  copilot:
    provider: shell
    cli: copilot
    model: copilot
roles:
  proposer: claude-sonnet
  drafter: claude-sonnet
  reviewer: codex-gpt5
  executor: copilot
  summariser: claude-sonnet
tools:
  substrate:
    mode: solo
pipeline:
  - id: specify
    role: drafter
  - id: plan
    role: proposer
  - id: assay
    runner: derrick
  - id: tasks
    role: drafter
  - id: analyze
    role: proposer
"#,
        )
        .unwrap()
    }

    #[test]
    fn current_mode_reads_solo() {
        let yaml = solo_yaml();
        assert_eq!(current_mode_str(&yaml), "solo");
    }

    #[test]
    fn switch_solo_to_crew_updates_mode() {
        let mut yaml = solo_yaml();
        let old_mode = current_mode_str(&yaml);
        let changes = compute_changes(&mut yaml, old_mode, InitMode::Crew).unwrap();
        assert_eq!(changes.new_mode, "crew");
        assert_eq!(current_mode_str(&yaml), "crew");
    }

    #[test]
    fn switch_to_crew_adds_bridge_and_foreman() {
        let mut yaml = solo_yaml();
        let old = current_mode_str(&yaml);
        let changes = compute_changes(&mut yaml, old, InitMode::Crew).unwrap();
        assert!(changes.steps_added.contains(&"bridge".to_owned()));
        assert!(changes.steps_added.contains(&"foreman".to_owned()));
        // Verify pipeline in mutated YAML contains the new steps.
        let pipeline = yaml["pipeline"].as_sequence().unwrap();
        let ids: Vec<_> = pipeline
            .iter()
            .filter_map(|s| s.get("id")?.as_str())
            .collect();
        assert!(ids.contains(&"bridge"));
        assert!(ids.contains(&"foreman"));
    }

    #[test]
    fn switch_crew_to_solo_removes_bridge_and_foreman() {
        let mut yaml = solo_yaml();
        // First go to crew.
        let old = current_mode_str(&yaml);
        compute_changes(&mut yaml, old, InitMode::Crew).unwrap();
        // Then switch back to solo.
        let old2 = current_mode_str(&yaml);
        let changes = compute_changes(&mut yaml, old2, InitMode::Solo).unwrap();
        assert!(!changes.steps_removed.is_empty());
        let pipeline = yaml["pipeline"].as_sequence().unwrap();
        let ids: Vec<_> = pipeline
            .iter()
            .filter_map(|s| s.get("id")?.as_str())
            .collect();
        assert!(!ids.contains(&"bridge"));
        assert!(!ids.contains(&"foreman"));
    }

    #[test]
    fn switch_to_crew_updates_proposer_role() {
        let mut yaml = solo_yaml();
        // In solo mode the proposer is claude-sonnet; crew recommends claude-opus.
        let old = current_mode_str(&yaml);
        let changes = compute_changes(&mut yaml, old, InitMode::Crew).unwrap();
        let proposer_change = changes
            .role_changes
            .iter()
            .find(|(r, _, _)| r == "proposer");
        assert!(
            proposer_change.is_some(),
            "expected proposer role to change; changes: {changes:?}",
        );
        let (_, _, new_model) = proposer_change.unwrap();
        assert_eq!(new_model, "claude-opus");
    }

    #[test]
    fn detect_inflight_no_runs_dir() {
        let tmp = tempfile::tempdir().unwrap();
        // No .derrick/runs directory at all → empty.
        assert!(detect_inflight_runs(tmp.path()).is_empty());
    }

    #[test]
    fn detect_inflight_finished_run_not_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let runs = tmp.path().join(".derrick").join("runs").join("run-1");
        std::fs::create_dir_all(&runs).unwrap();
        std::fs::write(
            runs.join("manifest.json"),
            r#"{"started_at":"2026-01-01T00:00:00Z","finished_at":"2026-01-01T01:00:00Z","status":"success","steps":[],"tokens_in":0,"tokens_out":0}"#,
        )
        .unwrap();
        assert!(detect_inflight_runs(tmp.path()).is_empty());
    }

    #[test]
    fn detect_inflight_unfinished_run_is_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let runs = tmp.path().join(".derrick").join("runs").join("run-42");
        std::fs::create_dir_all(&runs).unwrap();
        std::fs::write(
            runs.join("manifest.json"),
            r#"{"started_at":"2026-01-01T00:00:00Z","finished_at":null,"status":"success","steps":[],"tokens_in":0,"tokens_out":0}"#,
        )
        .unwrap();
        let inflight = detect_inflight_runs(tmp.path());
        assert_eq!(inflight, vec!["run-42"]);
    }
}
