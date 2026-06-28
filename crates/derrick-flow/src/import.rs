//! The `import` spec provider (DESIGN.md §5.3, D85 / Phase 3).
//!
//! Brings an externally-authored specification/PRD into derrick's pipeline. The
//! Specify phase resolves a source, scaffolds the feature directory (reusing the
//! same `derrick_assay::io` primitives every provider uses), reads the source,
//! and writes a canonical `spec.md`:
//!
//!   * **Passthrough** — the source already validates as a `derrick.spec/v1`
//!     document ([`derrick_specify::schema::looks_like_spec`]); it is written
//!     through byte-for-byte with a trailing provenance comment, no model call.
//!   * **Normalize** — otherwise, one in-process drafter-tier model call rewrites
//!     the source into the spec schema/template
//!     ([`derrick_specify::NativeSpecProvider::normalize_to_spec`]), with one
//!     bounded repair pass.
//!
//! `verify_spec_written` then fails loudly if the resulting `spec.md` is still the
//! pre-scaffolded stub, so a resume never treats an empty import as done.
//!
//! **v1 supports a local file path only.** A source carrying a non-file scheme
//! (e.g. `github:`/`notion:`) returns a clear "not supported yet" error rather
//! than failing obscurely — remote sources are a documented, deferred limitation
//! (derrick's Rust cannot call agent-side MCP tools).
//!
//! The downstream `plan`/`tasks` routing (per `import.{plan,tasks}`) lives in
//! [`crate::spec_provider`], which dispatches to the native or speckit path.

use std::path::{Path, PathBuf};

use derrick_config::Config;
use derrick_specify::{NativeOutcome, NativeRequest, NativeSpecProvider, schema};
use derrick_tools::HostRegistry;

use derrick_assay::types::RunError;

/// Everything the import-specify core needs. Built by both the pipeline seam
/// ([`crate::spec_provider`]) and the `derrick spec import` CLI subcommand.
pub struct ImportSpecifyRequest<'a> {
    /// The effective configuration (role → model → host resolution + the
    /// `tools.specify.import` block).
    pub config: &'a Config,
    /// The registered host adapters (the normalization model call dispatches
    /// through these in-process, mirroring the native path).
    pub hosts: &'a HostRegistry,
    /// The repository root.
    pub repo_root: &'a Path,
    /// The working directory (worktree or repo root) where `specs/<NNN>-<slug>/`
    /// is allocated and the survey index is read.
    pub working_dir: &'a Path,
    /// The originating feature prompt (drives the feature-dir slug + grounding).
    pub raw_prompt: &'a str,
    /// The import source. A local file path for v1; a non-file scheme is a clear
    /// error. Resolved from `tools.specify.import.source` or a `--spec` override.
    pub source: &'a str,
}

/// Outcome of an import-specify run: the produced artifacts/accounting plus the
/// allocated feature directory (relative to `working_dir`).
pub struct ImportSpecifyOutcome {
    /// The native-shaped accounting + artifact list.
    pub outcome: NativeOutcome,
    /// The feature directory allocated for this import (relative to `working_dir`).
    pub feature_dir: PathBuf,
    /// Whether the source was written through verbatim (`true`) or normalized by
    /// a model call (`false`). Telemetry / test signal.
    pub passthrough: bool,
}

/// Runs the import Specify phase: resolve source → scaffold → read → passthrough
/// or normalize → write `spec.md` → verify.
///
/// Reuses `derrick_assay::io::prescaffold_feature_dir` (feature dir + stub +
/// `.specify/feature.json`) and `verify_spec_written` so the on-disk contract is
/// identical to every other provider.
pub async fn import_specify(
    req: &ImportSpecifyRequest<'_>,
) -> Result<ImportSpecifyOutcome, RunError> {
    // v1: a local file path only. Reject a non-file scheme up front with a clear,
    // documented limitation rather than a confusing "file not found". A relative
    // path is resolved against the working directory (worktree or repo root), so
    // a config `source: docs/PRD.md` points inside the repo regardless of the
    // process cwd.
    let source_path = resolve_file_source(req.source, req.working_dir)?;

    // Scaffold the feature dir exactly as the other providers do.
    let feature_dir = derrick_assay::io::prescaffold_feature_dir(req.working_dir, req.raw_prompt)?;

    // Read the source. Only NotFound is a clean "missing source" RunError; any
    // other IO error (permissions, not-a-file, …) is surfaced, never swallowed
    // (Phase-2 lesson: do not `.ok()`-drop IO errors).
    let source_text = match std::fs::read_to_string(&source_path) {
        Ok(text) => text,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            return Err(RunError::Config(format!(
                "import source not found: {} \
                 (set tools.specify.import.source or pass --spec <path>)",
                source_path.display()
            )));
        }
        Err(source) => {
            return Err(RunError::Io {
                path: source_path,
                source,
            });
        }
    };

    let provider = NativeSpecProvider::new();
    let native_req = NativeRequest {
        raw_prompt: req.raw_prompt,
        repo_root: req.repo_root,
        working_dir: req.working_dir,
        hosts: req.hosts,
        config: req.config,
        interactive: false,
        feature_dir: &feature_dir,
    };

    let (outcome, passthrough) = if schema::looks_like_spec(&source_text) {
        // Structural passthrough: the source is already a valid spec. Write it
        // through with a short provenance note and no model call.
        tracing::info!(
            target: "derrick_flow::import",
            source = %source_path.display(),
            "import source matches the spec schema; passing through verbatim"
        );
        // Append the provenance as a trailing comment so the leading `---`
        // front-matter fence (which `split_front_matter` requires to be the very
        // first line) stays intact and the document still validates.
        let mut spec_md = source_text.clone();
        if !spec_md.ends_with('\n') {
            spec_md.push('\n');
        }
        spec_md.push_str(&format!(
            "\n<!-- derrick: imported verbatim from {} -->\n",
            source_path.display()
        ));
        let spec_path = req.working_dir.join(&feature_dir).join("spec.md");
        std::fs::write(&spec_path, &spec_md).map_err(|source| RunError::Io {
            path: spec_path,
            source,
        })?;
        let mut outcome = NativeOutcome {
            bytes_raw: u32::try_from(source_text.len()).unwrap_or(u32::MAX),
            ..NativeOutcome::default()
        };
        outcome.artifacts.push(feature_dir.join("spec.md"));
        outcome
            .artifacts
            .push(PathBuf::from(".specify/feature.json"));
        (outcome, true)
    } else {
        // One model normalization call into the native schema, with the same
        // validate + one-repair contract the native path uses.
        tracing::info!(
            target: "derrick_flow::import",
            source = %source_path.display(),
            "import source does not match the spec schema; normalizing via one model call"
        );
        let outcome = provider
            .normalize_to_spec(&native_req, &source_text)
            .await
            .map_err(map_specify_err)?;
        (outcome, false)
    };

    // Fail loudly if the spec is still the scaffolded stub (so resume never
    // treats a no-op import as done).
    derrick_assay::io::verify_spec_written(req.working_dir, &feature_dir)?;

    Ok(ImportSpecifyOutcome {
        outcome,
        feature_dir,
        passthrough,
    })
}

/// Maps a [`derrick_specify::SpecifyError`] onto a [`RunError::StepFailed`],
/// matching `verify_spec_written` failure semantics on the import path.
fn map_specify_err(error: derrick_specify::SpecifyError) -> RunError {
    RunError::StepFailed {
        id: "import".to_owned(),
        message: error.to_string(),
    }
}

/// Resolves an import `source` to a local file path, rejecting non-file schemes.
///
/// v1 supports only a local filesystem path. A source that begins with a URI
/// scheme other than `file:` (e.g. `github:`, `notion:`, `https:`) is a clear,
/// documented "not supported yet" error — remote sources are deferred because
/// derrick's Rust cannot call agent-side MCP tools. A `file:` prefix is accepted
/// and stripped. A relative path is resolved against `working_dir`; an absolute
/// path is used as-is.
fn resolve_file_source(source: &str, working_dir: &Path) -> Result<PathBuf, RunError> {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        return Err(RunError::Config(
            "import source is empty; set tools.specify.import.source or pass --spec <path>"
                .to_owned(),
        ));
    }
    let rel = if let Some(scheme) = uri_scheme(trimmed) {
        if scheme == "file" {
            // Accept only `file:path` and `file:///abs/path` (empty authority).
            // A `file://<authority>/...` with a non-empty authority (e.g.
            // `file://localhost/tmp/spec.md`) would otherwise strip to a relative
            // `localhost/tmp/spec.md` and resolve under working_dir — surprising
            // and almost certainly not what the operator meant. Reject it.
            let rest = &trimmed[scheme.len() + 1..];
            if let Some(after_slashes) = rest.strip_prefix("//") {
                // `rest` is `//<authority><path>`; the authority is up to the
                // next `/`. It must be empty (the `file:///abs` form).
                let authority_end = after_slashes.find('/').unwrap_or(after_slashes.len());
                let authority = &after_slashes[..authority_end];
                if !authority.is_empty() {
                    return Err(RunError::Config(format!(
                        "import source {trimmed:?} is a file:// URL with an authority \
                         ({authority:?}), which is not supported; use file:///absolute/path \
                         or a plain local path"
                    )));
                }
                &after_slashes[authority_end..]
            } else {
                rest
            }
        } else {
            return Err(RunError::Config(format!(
                "import source {trimmed:?} uses the {scheme:?} scheme, which is not supported yet \
                 (v1 supports a local file path only). Remote sources (GitHub issues, Notion, \
                 Confluence) are a documented, deferred limitation — export the document to a \
                 local file and point the import source at that path."
            )));
        }
    } else {
        trimmed
    };
    // `file:` / `file://` with no path strips `rel` to empty, which would
    // otherwise resolve to `working_dir` and surface later as a confusing
    // directory read error. Reject it as a clear config error instead.
    if rel.is_empty() {
        return Err(RunError::Config(format!(
            "import source {trimmed:?} does not include a local file path"
        )));
    }
    let path = Path::new(rel);
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(working_dir.join(path))
    }
}

/// Returns the URI scheme of `source` if it carries one, per RFC 3986
/// (`scheme = ALPHA *( ALPHA / DIGIT / "+" / "-" / "." )` followed by `:`).
///
/// Returns `None` for plain paths. A single-letter scheme followed by a path
/// separator (e.g. `C:\…`, `C:/…`) is treated as a Windows drive, not a scheme,
/// so Windows paths are not misclassified as remote sources.
fn uri_scheme(source: &str) -> Option<&str> {
    let colon = source.find(':')?;
    let scheme = &source[..colon];
    if scheme.is_empty() {
        return None;
    }
    let mut chars = scheme.chars();
    let first = chars.next()?;
    if !first.is_ascii_alphabetic() {
        return None;
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) {
        return None;
    }
    // A single-letter "scheme" immediately followed by a path separator is a
    // Windows drive letter (`C:\` / `C:/`), not a URI scheme.
    if scheme.len() == 1 {
        let after = &source[colon + 1..];
        if after.starts_with('\\') || after.starts_with('/') {
            return None;
        }
    }
    Some(scheme)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_path_has_no_scheme() {
        assert_eq!(uri_scheme("docs/PRD.md"), None);
        assert_eq!(uri_scheme("/abs/path/spec.md"), None);
        assert_eq!(uri_scheme("./relative.md"), None);
    }

    #[test]
    fn remote_schemes_detected() {
        assert_eq!(uri_scheme("github:owner/repo#1"), Some("github"));
        assert_eq!(uri_scheme("notion:abc123"), Some("notion"));
        assert_eq!(uri_scheme("https://example.com/x"), Some("https"));
    }

    #[test]
    fn windows_drive_is_not_a_scheme() {
        assert_eq!(uri_scheme(r"C:\docs\spec.md"), None);
        assert_eq!(uri_scheme("C:/docs/spec.md"), None);
    }

    #[test]
    fn resolve_rejects_remote_scheme() {
        let err = resolve_file_source("github:owner/repo#1", Path::new("/repo"))
            .expect_err("remote should error");
        let msg = err.to_string();
        assert!(msg.contains("not supported yet"), "got: {msg}");
        assert!(msg.contains("github"), "should name the scheme, got: {msg}");
    }

    #[test]
    fn resolve_relative_path_joins_working_dir() {
        let path = resolve_file_source("docs/PRD.md", Path::new("/repo")).expect("plain path ok");
        assert_eq!(path, PathBuf::from("/repo/docs/PRD.md"));
    }

    #[test]
    fn resolve_absolute_path_is_used_as_is() {
        let path = resolve_file_source("/abs/spec.md", Path::new("/repo")).expect("abs path ok");
        assert_eq!(path, PathBuf::from("/abs/spec.md"));
    }

    #[test]
    fn resolve_accepts_file_scheme() {
        let path =
            resolve_file_source("file:///tmp/spec.md", Path::new("/repo")).expect("file scheme ok");
        assert_eq!(path, PathBuf::from("/tmp/spec.md"));
    }

    #[test]
    fn resolve_accepts_file_scheme_without_slashes() {
        let path = resolve_file_source("file:docs/spec.md", Path::new("/repo")).expect("file: ok");
        assert_eq!(path, PathBuf::from("/repo/docs/spec.md"));
    }

    #[test]
    fn resolve_rejects_empty_file_scheme_paths() {
        // `file:` and `file://` strip to an empty path, which must be a clear
        // config error rather than resolving to working_dir.
        for source in ["file:", "file://"] {
            let err = resolve_file_source(source, Path::new("/repo"))
                .expect_err("empty file path should be rejected");
            assert!(
                err.to_string().contains("local file path"),
                "{source:?} → unexpected error: {err}"
            );
        }
    }

    #[test]
    fn resolve_rejects_file_url_with_authority() {
        let err = resolve_file_source("file://localhost/tmp/spec.md", Path::new("/repo"))
            .expect_err("authority should be rejected");
        let msg = err.to_string();
        assert!(msg.contains("authority"), "got: {msg}");
        assert!(
            msg.contains("localhost"),
            "should name the authority, got: {msg}"
        );
    }

    #[test]
    fn resolve_rejects_empty_source() {
        let err = resolve_file_source("   ", Path::new("/repo")).expect_err("empty should error");
        assert!(err.to_string().contains("empty"));
    }
}
