//! `derrick uninstall` — reverse the effects of `derrick init`.
//!
//! Removes `derrick.yaml`, the `.derrick/` state directory, and the derrick
//! block from `.codex/instructions.md`. Does not touch `.claude/settings.json`
//! (the hook config) in v1 — a note is printed instead.

use std::path::Path;

use crate::commands::UninstallArgs;
use crate::exit_code::CliExitCode;
use crate::{current_repo_root, message};

pub(crate) async fn execute(args: UninstallArgs) -> Result<CliExitCode, crate::CliError> {
    let _ = args.purge; // reserved for future use; accepted for forward-compat.
    let repo_root = current_repo_root()?;
    let config_path = repo_root.join("derrick.yaml");
    let state_dir = repo_root.join(".derrick");
    let codex_instructions = repo_root.join(".codex/instructions.md");

    // Summarise what will be removed.
    let mut will_remove: Vec<String> = Vec::new();
    if config_path.exists() {
        will_remove.push(config_path.display().to_string());
    }
    if state_dir.exists() {
        will_remove.push(state_dir.display().to_string());
    }
    if codex_instructions.exists() {
        will_remove.push(format!(
            "{} (derrick block stripped)",
            codex_instructions.display()
        ));
    }

    if will_remove.is_empty() {
        println!("nothing to remove — no derrick installation found");
        return Ok(CliExitCode::Success);
    }

    println!("will remove:");
    for item in &will_remove {
        println!("  {item}");
    }
    println!(
        "note: .claude/settings.json hook entries are not removed automatically in v1;\n\
         remove the derrick hooks block manually if desired."
    );

    if !args.yes {
        print!("proceed? [y/N] ");
        use std::io::Write as _;
        let _ = std::io::stdout().flush();
        let mut answer = String::new();
        std::io::stdin()
            .read_line(&mut answer)
            .map_err(|e| message(e.to_string()))?;
        if !answer.trim().eq_ignore_ascii_case("y") {
            println!("aborted");
            return Ok(CliExitCode::Success);
        }
    }

    remove_items(&repo_root, &config_path, &state_dir)?;
    Ok(CliExitCode::Success)
}

fn remove_items(
    repo_root: &Path,
    config_path: &Path,
    state_dir: &Path,
) -> Result<(), crate::CliError> {
    if config_path.exists() {
        std::fs::remove_file(config_path).map_err(|e| crate::CliError::Io {
            path: config_path.to_path_buf(),
            source: e,
        })?;
        println!("removed  {}", config_path.display());
    }

    if state_dir.exists() {
        std::fs::remove_dir_all(state_dir).map_err(|e| crate::CliError::Io {
            path: state_dir.to_path_buf(),
            source: e,
        })?;
        println!("removed  {}", state_dir.display());
    }

    derrick_adopt::remove_codex_instructions(repo_root).map_err(|e| message(e.to_string()))?;
    let codex_instructions = repo_root.join(".codex/instructions.md");
    if !codex_instructions.exists() {
        println!("removed  {}", codex_instructions.display());
    } else {
        println!("stripped {}", codex_instructions.display());
    }

    println!("done");
    Ok(())
}
