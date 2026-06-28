//! `derrick spec import <source>` — normalize an external spec/PRD into a
//! canonical `specs/<NNN>-<slug>/spec.md` and stop (spec-provider seam, D85).
//!
//! This calls the same import core the pipeline's `import` provider uses
//! ([`derrick_flow::import_specify`]), so the on-disk result is identical to a
//! `derrick drill --spec <source>` run that halts after the specify phase. It
//! exists so an operator can inspect (and hand-edit) the normalized spec before
//! committing to plan/tasks.

use std::path::Path;

use derrick_flow::{ImportSpecifyRequest, import_specify};
use derrick_tools::HostRegistry;

use crate::commands::{SpecArgs, SpecCommand, SpecImportArgs};
use crate::exit_code::CliExitCode;
use crate::{current_repo_root, message, read_config};

/// Executes the `derrick spec` subcommand (import specification sources).
pub(crate) async fn execute(args: SpecArgs) -> Result<CliExitCode, crate::CliError> {
    match args.command {
        SpecCommand::Import(import) => run_import(import).await,
    }
}

async fn run_import(args: SpecImportArgs) -> Result<CliExitCode, crate::CliError> {
    let repo_root = current_repo_root()?;
    let config = read_config(&repo_root)?;

    // The prompt grounds the feature-dir slug and the normalization call. Default
    // to the source file stem so a bare `derrick spec import docs/PRD.md` still
    // produces a sensible directory name.
    let prompt = args.prompt.unwrap_or_else(|| default_prompt(&args.source));

    let hosts = HostRegistry::with_defaults();
    let request = ImportSpecifyRequest {
        config: &config,
        hosts: &hosts,
        repo_root: &repo_root,
        working_dir: &repo_root,
        raw_prompt: &prompt,
        source: &args.source,
    };

    let imported = import_specify(&request)
        .await
        .map_err(|error| message(error.to_string()))?;

    let spec_path = imported.feature_dir.join("spec.md");
    let how = if imported.passthrough {
        "passed through verbatim"
    } else {
        "normalized via one model call"
    };
    println!("Imported {} ({how}).", args.source);
    println!("  spec: {}", spec_path.display());
    println!();
    println!("Review the spec, then run the pipeline against it:");
    println!(
        "  derrick drill --spec {} \"<feature prompt>\"",
        args.source
    );
    Ok(CliExitCode::Success)
}

/// Derives a default feature prompt from a source path's file stem, e.g.
/// `docs/product-requirements.md` → `product requirements`.
fn default_prompt(source: &str) -> String {
    let stem = Path::new(source)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(source);
    let words: String = stem
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if words.is_empty() {
        "imported spec".to_owned()
    } else {
        words
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_prompt_from_stem() {
        assert_eq!(
            default_prompt("docs/product-requirements.md"),
            "product requirements"
        );
        assert_eq!(default_prompt("/abs/PRD.md"), "PRD");
        assert_eq!(default_prompt("???.md"), "imported spec");
    }
}
