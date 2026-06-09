//! Shared helpers for stacking backends that shell out to an external CLI
//! (Graphite's `gt`, git-spice's `gs`).
//!
//! These backends do not re-implement the rebase/force-push logic the way the
//! native backend does; they delegate to the third-party tool. What they share
//! is process-spawning, binary-presence detection, and the discipline of
//! mapping a non-zero exit into an actionable [`StackError`].

use std::ffi::OsStr;
use std::path::Path;

use tokio::process::Command;
use tracing::debug;

use crate::StackError;

/// Outcome of running an external stacking CLI command.
pub(crate) struct CliRun {
    /// Whether the process exited 0.
    pub(crate) success: bool,
    /// Captured stdout, trimmed.
    pub(crate) stdout: String,
    /// Captured stderr, trimmed.
    pub(crate) stderr: String,
}

impl CliRun {
    /// Combined stdout+stderr, useful for conflict/diagnostic detection where
    /// the tool may write to either stream.
    pub(crate) fn combined(&self) -> String {
        let mut combined = self.stdout.clone();
        if !self.stderr.is_empty() {
            if !combined.is_empty() {
                combined.push('\n');
            }
            combined.push_str(&self.stderr);
        }
        combined
    }
}

/// Verify the backend's CLI binary is on `PATH`, returning an actionable
/// [`StackError::NotSupported`] when it is missing so the failure surfaces
/// early (at construction or first call) rather than as an opaque I/O error.
pub(crate) fn ensure_binary(
    binary: &'static str,
    backend: &'static str,
    install_hint: &'static str,
) -> Result<(), StackError> {
    if which::which(binary).is_ok() {
        return Ok(());
    }
    Err(StackError::NotSupported {
        backend,
        reason: install_hint,
    })
}

/// Run `binary` with `args` in `repo_root`, capturing output. stdin is closed
/// so the external tool never blocks waiting on an interactive prompt.
pub(crate) async fn run<I, S>(
    binary: &str,
    args: I,
    repo_root: &Path,
) -> Result<CliRun, StackError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = Command::new(binary);
    command
        .args(args)
        .current_dir(repo_root)
        .stdin(std::process::Stdio::null());
    debug!(binary, ?command, "running stacking cli");
    let output = command.output().await?;
    Ok(CliRun {
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    })
}

/// Heuristic: does the tool's combined output indicate a merge/rebase conflict?
///
/// Both `gt` and `gs` surface conflicts in human-readable text rather than a
/// distinct exit code, so we sniff for the usual conflict vocabulary. This is
/// deliberately broad: per D19 we must never silently treat a conflict as a
/// clean restack, so a false positive (bailing with a recipe) is far safer than
/// a false negative.
pub(crate) fn looks_like_conflict(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("conflict")
        || lower.contains("merge conflict")
        || lower.contains("could not apply")
        || lower.contains("needs resolution")
        || lower.contains("fix conflicts")
}
