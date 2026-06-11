//! git-spice stacking backend: shells out to the `gs` CLI.
//!
//! Same shape as the Graphite adapter (DESIGN.md §8.5): derrick owns branch
//! naming and parent computation (D20); git-spice owns the rebase/submit
//! mechanics. This is the abhinav/git-spice tool whose binary is `gs`.
//!
//! Commands used (stable, non-interactive `gs` surface):
//! - `open_pr`  → `gs branch submit --fill` from the branch (fills PR title /
//!   body from the commit, no editor).
//! - `restack`  → `gs upstack restack` (rebases this branch and everything
//!   stacked above it onto the updated bases).
//! - `force_push` → handled by `gs branch submit`; exposed as a no-op for
//!   uniform mixed-mode callers. git-spice force-pushes with lease itself.
//!
//! On a restack conflict we honour D19: bail immediately, abort the in-progress
//! rebase, and surface the exact manual recipe. We never auto-resolve.

use std::path::Path;

use async_trait::async_trait;
use tracing::{info, warn};

use super::cli::{ensure_binary, looks_like_conflict, run};
use crate::{OpenPrParams, PrInfo, RestackOutcome, RestackParams, StackBackend, StackError};

const BACKEND: &str = "git-spice";
const BINARY: &str = "gs";
const INSTALL_HINT: &str = "gs not found on PATH — install git-spice from https://abhinav.github.io/git-spice/ \
     (e.g. `brew install git-spice`) and run `gs auth login`";

/// git-spice stack backend. Delegates rebase/submit to the `gs` CLI.
#[derive(Clone, Copy, Debug, Default)]
pub struct GitSpiceStackBackend;

impl GitSpiceStackBackend {
    /// Construct a git-spice backend, verifying the `gs` binary is present.
    ///
    /// Returns [`StackError::NotSupported`] with an install hint when `gs` is
    /// not on `PATH`, so the failure surfaces at construction rather than as an
    /// opaque I/O error on first use.
    pub fn new() -> Result<Self, StackError> {
        ensure_binary(BINARY, BACKEND, INSTALL_HINT)?;
        Ok(Self)
    }
}

#[async_trait]
impl StackBackend for GitSpiceStackBackend {
    fn kind(&self) -> &'static str {
        BACKEND
    }

    async fn open_pr(&self, params: OpenPrParams) -> Result<PrInfo, StackError> {
        ensure_binary(BINARY, BACKEND, INSTALL_HINT)?;
        // git-spice submits the checked-out branch, so ensure it is current.
        super::checkout_branch(BINARY, BACKEND, &params.branch, &params.repo_root).await?;

        // `--fill` populates the PR title/body from the commit so no editor
        // opens. `gs branch submit` is the single-branch verb (`gs stack
        // submit` would submit the whole stack).
        let result = run(BINARY, ["branch", "submit", "--fill"], &params.repo_root).await?;
        if !result.success {
            return Err(StackError::Gh {
                message: format!(
                    "gs branch submit failed for {}: {}",
                    params.branch,
                    nonempty(&result.stderr, &result.stdout),
                ),
            });
        }

        let combined = result.combined();
        let (url, number) = match super::find_pr_url_and_number(&combined) {
            Some(found) => found,
            None => super::lookup_pr_via_gh(&params.branch, &params.repo_root).await?,
        };
        let head_sha = super::git_rev_parse(&params.repo_root, &params.branch).await?;
        info!(branch = %params.branch, %url, "opened pr via gs");
        Ok(PrInfo {
            number,
            url,
            head_sha,
        })
    }

    async fn restack(&self, params: RestackParams) -> Result<RestackOutcome, StackError> {
        ensure_binary(BINARY, BACKEND, INSTALL_HINT)?;
        super::checkout_branch(BINARY, BACKEND, &params.branch, &params.repo_root).await?;

        // `gs upstack restack` rebases this branch and everything above it onto
        // the updated bases.
        let result = run(BINARY, ["upstack", "restack"], &params.repo_root).await?;
        if result.success {
            info!(branch = %params.branch, "restacked via gs");
            return Ok(RestackOutcome::Restacked);
        }

        // D19: never auto-resolve a conflict. Abort the rebase git-spice left
        // in progress, then surface the manual recipe.
        let combined = result.combined();
        if looks_like_conflict(&combined) {
            let _ = run(BINARY, ["rebase", "abort"], &params.repo_root).await;
            let recipe = format!(
                "git rebase --onto {} {} {}  # then: gs upstack restack && gs branch submit",
                params.new_parent, params.old_parent, params.branch
            );
            warn!(branch = %params.branch, recipe = %recipe, "gs restack conflict; aborted");
            return Ok(RestackOutcome::Conflict { recipe });
        }

        Err(StackError::Git {
            message: format!(
                "gs upstack restack failed for {}: {}",
                params.branch,
                nonempty(&result.stderr, &result.stdout),
            ),
        })
    }

    async fn force_push(&self, branch: &str, _repo_root: &Path) -> Result<(), StackError> {
        // git-spice force-pushes (with lease) as part of `gs branch submit`.
        info!(
            branch,
            "force_push is a no-op for git-spice; gs branch submit handles it"
        );
        Ok(())
    }
}

/// Return `primary` if non-empty, else `fallback`, else a placeholder.
fn nonempty<'a>(primary: &'a str, fallback: &'a str) -> &'a str {
    if !primary.is_empty() {
        primary
    } else if !fallback.is_empty() {
        fallback
    } else {
        "(no output captured)"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_is_git_spice() {
        assert_eq!(GitSpiceStackBackend.kind(), "git-spice");
    }

    #[test]
    fn install_hint_names_the_binary() {
        assert!(INSTALL_HINT.contains("gs"));
    }
}
