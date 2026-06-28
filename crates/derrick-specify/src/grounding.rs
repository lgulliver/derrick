//! Survey pre-pass: derrick-authored grounding for the native spec provider.
//!
//! Before any model call, [`gather`] opens the `.derrick/index.db` survey index
//! **iff it exists**, runs a `context` query for the raw prompt, caps and trims
//! the symbols, renders a compact `path:line identifier` block, and compresses
//! it with `caveman::compress(_, Full)` (which preserves `path:line` and
//! identifier spans byte-for-byte). The resulting [`GroundingResult`] carries
//! both the structured [`schema::Grounding`] front-matter (derrick writes this,
//! never the model) and a human-facing context block for the draft prompt.
//!
//! Graceful degradation: with no index, `index_fresh` is `false`, `symbols` is
//! empty, and the draft prompt is told there is no index and not to invent
//! symbol names.

use std::path::Path;

use derrick_caveman::Intensity;
use derrick_survey::{Survey, SurveyConfig, SymbolHit};

use crate::schema::Grounding;

/// Maximum number of grounding symbols carried into the spec.
const MAX_SYMBOLS: usize = 12;
/// Signatures longer than this are trimmed (with an ellipsis) before rendering.
const MAX_SIGNATURE_CHARS: usize = 100;

/// Result of the grounding pre-pass.
#[derive(Clone, Debug)]
pub struct GroundingResult {
    /// Structured front-matter derrick injects into `spec.md` verbatim.
    pub front_matter: Grounding,
    /// A compact, caveman-compressed context block for the draft prompt.
    pub context_block: String,
    /// Repo paths referenced by the grounding symbols (for plan path checks).
    pub indexed_paths: Vec<String>,
    /// Bytes of the rendered block before caveman compression.
    pub bytes_raw: u32,
    /// Bytes saved by caveman compression (>= 0).
    pub bytes_saved: u32,
}

impl GroundingResult {
    /// The degraded result used when no index is present.
    fn degraded() -> Self {
        Self {
            front_matter: Grounding {
                index_fresh: false,
                symbols: Vec::new(),
            },
            context_block: "No survey index is available. Do not invent symbol names, file \
                            paths, or APIs — describe behaviour only, in terms the operator \
                            supplied."
                .to_owned(),
            indexed_paths: Vec::new(),
            bytes_raw: 0,
            bytes_saved: 0,
        }
    }
}

/// Runs the grounding pre-pass for `prompt` against the index under
/// `working_dir/.derrick/index.db`, degrading gracefully if it is absent.
///
/// `working_dir` is the repository working tree (worktree or repo root). No
/// model is invoked; this is pure derrick logic.
pub async fn gather(working_dir: &Path, prompt: &str) -> GroundingResult {
    let db_path = working_dir.join(".derrick").join("index.db");
    if !db_path.exists() {
        tracing::debug!(
            target: "derrick_specify::grounding",
            "no survey index at {}; grounding degraded", db_path.display()
        );
        return GroundingResult::degraded();
    }

    let config = SurveyConfig {
        db_path,
        repo_root: working_dir.to_path_buf(),
        reader_pool: SurveyConfig::DEFAULT_READER_POOL,
    };
    let survey = match Survey::open(config).await {
        Ok(survey) => survey,
        Err(error) => {
            tracing::warn!(
                target: "derrick_specify::grounding",
                %error, "failed to open survey index; grounding degraded"
            );
            return GroundingResult::degraded();
        }
    };

    let context = match survey.context(prompt, MAX_SYMBOLS).await {
        Ok(context) => context,
        Err(error) => {
            tracing::warn!(
                target: "derrick_specify::grounding",
                %error, "survey context query failed; grounding degraded"
            );
            return GroundingResult::degraded();
        }
    };

    let mut hits: Vec<SymbolHit> = Vec::new();
    hits.extend(context.entry_points);
    hits.extend(context.related);

    // Enrich the top hit with its direct callers/callees (impact), which gives
    // the drafter the immediate blast radius without a model-side fan-out.
    if let Some(top) = hits.first() {
        if let Ok(Some(impact)) = survey.impact(&top.name).await {
            hits.extend(impact.callers);
            hits.extend(impact.callees);
        }
    }

    // De-duplicate by (path, start_line, name) and cap.
    let mut seen = std::collections::HashSet::new();
    hits.retain(|h| seen.insert((h.path.clone(), h.start_line, h.name.clone())));
    hits.truncate(MAX_SYMBOLS);

    if hits.is_empty() {
        // Index present but no relevant symbols — still mark fresh so the model
        // knows it may rely on the absence as signal.
        return GroundingResult {
            front_matter: Grounding {
                index_fresh: true,
                symbols: Vec::new(),
            },
            context_block: "Survey index is present but returned no symbols relevant to this \
                            request. Treat this area as greenfield; do not invent symbol names."
                .to_owned(),
            indexed_paths: Vec::new(),
            bytes_raw: 0,
            bytes_saved: 0,
        };
    }

    let lines: Vec<String> = hits.iter().map(render_symbol_line).collect();
    let indexed_paths: Vec<String> = {
        let mut paths: Vec<String> = hits.iter().map(|h| h.path.clone()).collect();
        paths.sort_unstable();
        paths.dedup();
        paths
    };

    let rendered = lines.join("\n");
    let bytes_raw = u32::try_from(rendered.len()).unwrap_or(u32::MAX);
    let compressed = derrick_caveman::compress(&rendered, Intensity::Full);
    let bytes_out = u32::try_from(compressed.text.len()).unwrap_or(u32::MAX);
    let bytes_saved = bytes_raw.saturating_sub(bytes_out);

    GroundingResult {
        front_matter: Grounding {
            index_fresh: true,
            symbols: lines,
        },
        context_block: format!(
            "Survey-grounded symbols (path:line identifier — these are real, indexed names; \
             prefer them and do not invent others):\n{}",
            compressed.text
        ),
        indexed_paths,
        bytes_raw,
        bytes_saved,
    }
}

/// Renders one symbol hit as a compact `path:line identifier — signature` line.
fn render_symbol_line(hit: &SymbolHit) -> String {
    let mut line = format!("{}:{} {}", hit.path, hit.start_line, hit.name);
    if let Some(sig) = &hit.signature {
        let sig = sig.trim();
        if !sig.is_empty() {
            let trimmed = trim_signature(sig);
            line.push_str(" — ");
            line.push_str(&trimmed);
        }
    }
    line
}

/// Trims a signature to [`MAX_SIGNATURE_CHARS`], appending an ellipsis when cut.
fn trim_signature(sig: &str) -> String {
    if sig.chars().count() <= MAX_SIGNATURE_CHARS {
        return sig.to_owned();
    }
    let truncated: String = sig.chars().take(MAX_SIGNATURE_CHARS).collect();
    format!("{truncated}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use derrick_survey::SymbolKind;

    fn hit(name: &str, path: &str, line: u32, sig: Option<&str>) -> SymbolHit {
        SymbolHit {
            name: name.to_owned(),
            kind: SymbolKind::Function,
            path: path.to_owned(),
            start_line: line,
            end_line: line + 5,
            signature: sig.map(str::to_owned),
        }
    }

    #[test]
    fn renders_path_line_identifier() {
        let line = render_symbol_line(&hit(
            "export_widget",
            "src/lib.rs",
            42,
            Some("fn export_widget()"),
        ));
        assert!(line.starts_with("src/lib.rs:42 export_widget"));
        assert!(line.contains("fn export_widget()"));
    }

    #[test]
    fn trims_long_signatures() {
        let long = "x".repeat(200);
        let trimmed = trim_signature(&long);
        assert!(trimmed.chars().count() <= MAX_SIGNATURE_CHARS + 1);
        assert!(trimmed.ends_with('…'));
    }

    #[tokio::test]
    async fn degrades_without_index() {
        let dir = tempfile::tempdir().expect("tempdir");
        let result = gather(dir.path(), "anything").await;
        assert!(!result.front_matter.index_fresh);
        assert!(result.front_matter.symbols.is_empty());
        assert!(result.context_block.contains("Do not invent"));
    }
}
