//! Branch creation helper.
//!
//! `BranchCreator` is responsible for the git-side work that must happen
//! before Copilot can be dispatched: create the `derrick/<batch>/<ticket-id>`
//! branch (or no-op if it already exists locally) and push it to the remote
//! so Copilot can target it.
//!
//! The trait is split out so dispatcher tests can swap in an in-memory
//! recorder without spawning real git subprocesses.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use async_trait::async_trait;
use thiserror::Error;
use tokio::process::Command;
use tracing::{debug, warn};

/// Errors returned by [`BranchCreator`] implementations.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum BranchError {
    /// I/O error spawning or waiting on the `git` subprocess.
    #[error("git io error in {cwd}: {source}")]
    Io {
        /// Working directory used for the git command.
        cwd: PathBuf,
        /// Source I/O error.
        source: std::io::Error,
    },
    /// `git` exited non-zero for an operation that is not safely
    /// idempotent (anything other than "branch already exists").
    #[error("git {operation} failed (exit {exit_code}): {stderr}")]
    NonZeroExit {
        /// Operation we attempted (e.g. `"checkout -b"`, `"push"`).
        operation: String,
        /// Exit code reported by git, or `-1` when no code was reported.
        exit_code: i32,
        /// Captured stderr (lossy UTF-8).
        stderr: String,
    },
}

/// Creates and publishes a branch for a Copilot dispatch.
#[async_trait]
pub trait BranchCreator: Send + Sync {
    /// Ensures `branch` exists locally (no-op if it already does) and
    /// pushes it to the configured remote so Copilot can target it.
    async fn ensure_branch(&self, branch: &str, base_branch: &str) -> Result<(), BranchError>;
}

/// Real [`BranchCreator`] that shells to `git`.
pub struct GitBranchCreator {
    repo_root: PathBuf,
    remote: String,
}

impl GitBranchCreator {
    /// Construct a creator rooted at `repo_root`, pushing to `origin`.
    pub fn new(repo_root: PathBuf) -> Self {
        Self {
            repo_root,
            remote: "origin".to_owned(),
        }
    }

    /// Override the remote (defaults to `origin`).
    pub fn with_remote(mut self, remote: impl Into<String>) -> Self {
        self.remote = remote.into();
        self
    }

    async fn run_git(
        &self,
        operation: &str,
        args: &[&str],
        ignore_exit_code: bool,
    ) -> Result<std::process::Output, BranchError> {
        debug!(
            operation,
            cwd = %self.repo_root.display(),
            args = ?args,
            "running git"
        );
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.repo_root)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|source| BranchError::Io {
                cwd: self.repo_root.clone(),
                source,
            })?;
        if !output.status.success() && !ignore_exit_code {
            return Err(BranchError::NonZeroExit {
                operation: operation.to_owned(),
                exit_code: output.status.code().unwrap_or(-1),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }
        Ok(output)
    }
}

#[async_trait]
impl BranchCreator for GitBranchCreator {
    async fn ensure_branch(&self, branch: &str, base_branch: &str) -> Result<(), BranchError> {
        // `git show-ref --verify --quiet refs/heads/<branch>` exits 0 iff
        // the branch already exists locally; treat any non-zero as "create
        // it" and treat the create-failure as a fatal error.
        let exists = self
            .run_git(
                "show-ref",
                &[
                    "show-ref",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/{branch}"),
                ],
                true,
            )
            .await?;
        if !exists.status.success() {
            // Create from base.
            self.run_git(
                "checkout -b",
                &["checkout", "-b", branch, base_branch],
                false,
            )
            .await?;
        } else {
            // Branch already exists — leave the working tree alone and just
            // make sure the remote knows about it.
            warn!(
                branch,
                "branch already exists locally; skipping create and reusing"
            );
        }

        // Push (idempotent: -u origin <branch>). Tolerate non-zero only when
        // the branch is already up to date; rely on the dispatcher to log.
        self.run_git("push", &["push", "-u", &self.remote, branch], false)
            .await?;
        Ok(())
    }
}

/// Convenience: return a default branch name following the
/// `derrick/<batch>/<ticket-id>` pattern (D19/§8.3). `batch` is the batch
/// name when the ticket is in one, otherwise `"ad-hoc"`.
pub fn default_branch_name(batch: Option<&str>, ticket_id: &str) -> String {
    let batch = batch.unwrap_or("ad-hoc");
    format!("derrick/{batch}/{ticket_id}")
}

/// Returns the repo root passed to this `GitBranchCreator`. Useful in tests.
impl GitBranchCreator {
    /// Path to the repo this creator operates against.
    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    #[derive(Default)]
    struct FakeBranchCreator {
        calls: Arc<Mutex<Vec<(String, String)>>>,
    }

    #[async_trait]
    impl BranchCreator for FakeBranchCreator {
        async fn ensure_branch(&self, branch: &str, base_branch: &str) -> Result<(), BranchError> {
            self.calls
                .lock()
                .await
                .push((branch.to_owned(), base_branch.to_owned()));
            Ok(())
        }
    }

    #[tokio::test]
    async fn default_branch_name_includes_batch_and_ticket() {
        assert_eq!(
            default_branch_name(Some("batch-1"), "drk-001"),
            "derrick/batch-1/drk-001"
        );
        assert_eq!(
            default_branch_name(None, "drk-002"),
            "derrick/ad-hoc/drk-002"
        );
    }

    #[tokio::test]
    async fn fake_branch_creator_records_calls() {
        let fake = FakeBranchCreator::default();
        fake.ensure_branch("derrick/b1/t1", "main")
            .await
            .expect("ensure ok");
        fake.ensure_branch("derrick/b1/t1", "main")
            .await
            .expect("ensure idempotent");
        let calls = fake.calls.lock().await.clone();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0], ("derrick/b1/t1".to_owned(), "main".to_owned()));
    }
}
