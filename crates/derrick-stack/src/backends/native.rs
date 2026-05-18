//! Native stacking backend: shells out to `git` and `gh`.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use async_trait::async_trait;
use derrick_config::ForcePush;
use tokio::process::Command;
use tracing::{debug, info, warn};

use crate::{OpenPrParams, PrInfo, RestackOutcome, RestackParams, StackBackend, StackError};

/// Native stack backend. Always force-pushes with `--force-with-lease`
/// regardless of the configured `force_push` policy when the policy
/// authorises any force push at all.
#[derive(Clone, Debug)]
pub struct NativeStackBackend {
    repo_root: PathBuf,
    force_push_flag: ForcePush,
}

impl NativeStackBackend {
    /// Construct a backend rooted at `repo_root`.
    pub fn new(repo_root: PathBuf, force_push_flag: ForcePush) -> Self {
        Self {
            repo_root,
            force_push_flag,
        }
    }

    /// Return the configured force-push policy. Exposed for tests/CLI
    /// surface area that needs to inspect it.
    pub fn force_push_policy(&self) -> ForcePush {
        self.force_push_flag
    }
}

#[async_trait]
impl StackBackend for NativeStackBackend {
    fn kind(&self) -> &'static str {
        "native"
    }

    async fn open_pr(&self, params: OpenPrParams) -> Result<PrInfo, StackError> {
        let mut command = Command::new("gh");
        command
            .arg("pr")
            .arg("create")
            .arg("--title")
            .arg(&params.title)
            .arg("--body")
            .arg(&params.body)
            .arg("--base")
            .arg(&params.parent_branch)
            .arg("--head")
            .arg(&params.branch)
            .current_dir(&params.repo_root)
            .stdin(Stdio::null());
        if params.draft {
            command.arg("--draft");
        }

        let output = command.output().await?;
        if !output.status.success() {
            return Err(StackError::Gh {
                message: format!(
                    "gh pr create failed for {}: {}",
                    params.branch,
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            });
        }

        let url = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let number = parse_pr_number(&url).ok_or_else(|| StackError::Gh {
            message: format!("could not parse PR number from gh output: {url:?}"),
        })?;
        let head_sha = git_rev_parse(&params.repo_root, &params.branch).await?;
        info!(branch = %params.branch, %url, "opened pr");
        Ok(PrInfo {
            number,
            url,
            head_sha,
        })
    }

    async fn restack(&self, params: RestackParams) -> Result<RestackOutcome, StackError> {
        // Sync refs first so the local rebase sees up-to-date parents.
        let fetch = Command::new("git")
            .arg("-C")
            .arg(&params.repo_root)
            .arg("fetch")
            .arg("origin")
            .stdin(Stdio::null())
            .output()
            .await?;
        if !fetch.status.success() {
            warn!(
                stderr = %String::from_utf8_lossy(&fetch.stderr).trim(),
                "git fetch origin failed; continuing with local refs",
            );
        }

        let rebase = Command::new("git")
            .arg("-C")
            .arg(&params.repo_root)
            .arg("rebase")
            .arg("--onto")
            .arg(&params.new_parent)
            .arg(&params.old_parent)
            .arg(&params.branch)
            .stdin(Stdio::null())
            .output()
            .await?;
        if rebase.status.success() {
            debug!(branch = %params.branch, "restacked");
            return Ok(RestackOutcome::Restacked);
        }

        // Conflict: abort the in-progress rebase so the working tree is
        // restored, then surface the recipe.
        let abort = Command::new("git")
            .arg("-C")
            .arg(&params.repo_root)
            .arg("rebase")
            .arg("--abort")
            .stdin(Stdio::null())
            .output()
            .await?;
        if !abort.status.success() {
            return Err(StackError::Git {
                message: format!(
                    "rebase conflict and abort failed: {}",
                    String::from_utf8_lossy(&abort.stderr).trim()
                ),
            });
        }
        let recipe = format!(
            "git rebase --onto {} {} {}",
            params.new_parent, params.old_parent, params.branch
        );
        warn!(branch = %params.branch, recipe = %recipe, "restack conflict; aborted");
        Ok(RestackOutcome::Conflict { recipe })
    }

    async fn force_push(&self, branch: &str, repo_root: &Path) -> Result<(), StackError> {
        if matches!(self.force_push_flag, ForcePush::Off) {
            return Err(StackError::NotSupported {
                backend: "native",
                reason: "force_push policy is off",
            });
        }
        let _ = self.repo_root.as_path(); // keep field meaningfully owned
        let output = Command::new("git")
            .arg("-C")
            .arg(repo_root)
            .arg("push")
            .arg("origin")
            .arg(branch)
            .arg("--force-with-lease")
            .stdin(Stdio::null())
            .output()
            .await?;
        if !output.status.success() {
            return Err(StackError::Git {
                message: format!(
                    "git push --force-with-lease failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            });
        }
        Ok(())
    }
}

async fn git_rev_parse(repo_root: &Path, branch: &str) -> Result<String, StackError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .arg("rev-parse")
        .arg(branch)
        .stdin(Stdio::null())
        .output()
        .await?;
    if !output.status.success() {
        return Err(StackError::Git {
            message: format!(
                "git rev-parse {branch} failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Parse the PR number from a GitHub PR URL like
/// `https://github.com/org/repo/pull/123`.
fn parse_pr_number(url: &str) -> Option<u64> {
    let trimmed = url.trim_end_matches('/');
    let tail = trimmed.rsplit('/').next()?;
    tail.parse::<u64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pr_number_extracts_trailing_number() {
        assert_eq!(
            parse_pr_number("https://github.com/foo/bar/pull/42"),
            Some(42),
        );
        assert_eq!(
            parse_pr_number("https://github.com/foo/bar/pull/42/"),
            Some(42),
        );
        assert_eq!(parse_pr_number("https://github.com/foo/bar/pull/"), None);
        assert_eq!(parse_pr_number("nonsense"), None);
    }
}
