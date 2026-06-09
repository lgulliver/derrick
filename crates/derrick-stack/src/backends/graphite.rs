//! Graphite stacking backend: shells out to the `gt` CLI.
//!
//! Unlike the native backend, this adapter delegates the heavy lifting to
//! Graphite. derrick still owns branch *naming* and parent computation (D20);
//! Graphite owns the rebase/submit mechanics. See DESIGN.md §8.5.
//!
//! Commands used (all stable, non-interactive `gt` surface):
//! - `open_pr`  → `gt submit --no-interactive --no-edit` from the branch.
//! - `restack`  → `gt restack` (rebases the current stack onto updated parents).
//! - `force_push` → handled by `gt submit`; exposed as a no-op so mixed-mode
//!   callers can invoke it uniformly. Graphite force-pushes with lease itself.
//!
//! On a restack conflict we honour D19: bail immediately, abort any in-progress
//! rebase, and surface the exact manual recipe. We never auto-resolve.

use std::path::Path;

use async_trait::async_trait;
use tracing::{info, warn};

use super::cli::{ensure_binary, looks_like_conflict, run};
use crate::{OpenPrParams, PrInfo, RestackOutcome, RestackParams, StackBackend, StackError};

const BACKEND: &str = "graphite";
const BINARY: &str = "gt";
const INSTALL_HINT: &str =
    "gt not found on PATH — install Graphite via `npm install -g @withgraphite/graphite-cli` \
     (or https://graphite.dev/docs/install-the-cli) and run `gt auth`";

/// Graphite stack backend. Delegates rebase/submit to the `gt` CLI.
#[derive(Clone, Copy, Debug, Default)]
pub struct GraphiteStackBackend;

impl GraphiteStackBackend {
    /// Construct a Graphite backend, verifying the `gt` binary is present.
    ///
    /// Returns [`StackError::NotSupported`] with an install hint when `gt` is
    /// not on `PATH`, so the failure surfaces at construction rather than as an
    /// opaque I/O error on first use.
    pub fn new() -> Result<Self, StackError> {
        ensure_binary(BINARY, BACKEND, INSTALL_HINT)?;
        Ok(Self)
    }
}

#[async_trait]
impl StackBackend for GraphiteStackBackend {
    fn kind(&self) -> &'static str {
        BACKEND
    }

    async fn open_pr(&self, params: OpenPrParams) -> Result<PrInfo, StackError> {
        ensure_binary(BINARY, BACKEND, INSTALL_HINT)?;
        // Graphite submits the branch that is currently checked out, so make
        // sure it is the one we were asked to publish. We use plain git for the
        // checkout (it is a derrick-owned ref operation, not a stack mutation).
        super::checkout_branch(BINARY, BACKEND, &params.branch, &params.repo_root).await?;

        // `--no-interactive` keeps gt from prompting; `--no-edit` reuses commit
        // messages for the PR body so the run is fully unattended.
        let result = run(
            BINARY,
            ["submit", "--no-interactive", "--no-edit"],
            &params.repo_root,
        )
        .await?;
        if !result.success {
            return Err(StackError::Gh {
                message: format!(
                    "gt submit failed for {}: {}",
                    params.branch,
                    nonempty(&result.stderr, &result.stdout),
                ),
            });
        }

        // gt prints the PR URL on success; recover the number from it. If gt's
        // output format changes and we cannot find a URL, fall back to querying
        // `gh` for the PR associated with this branch.
        let combined = result.combined();
        let (url, number) = match super::find_pr_url_and_number(&combined) {
            Some(found) => found,
            None => super::lookup_pr_via_gh(&params.branch, &params.repo_root).await?,
        };
        let head_sha = super::git_rev_parse(&params.repo_root, &params.branch).await?;
        info!(branch = %params.branch, %url, "opened pr via gt");
        Ok(PrInfo {
            number,
            url,
            head_sha,
        })
    }

    async fn restack(&self, params: RestackParams) -> Result<RestackOutcome, StackError> {
        ensure_binary(BINARY, BACKEND, INSTALL_HINT)?;
        // Restack the branch we were asked about; gt operates on the checked-out
        // branch's upstack.
        super::checkout_branch(BINARY, BACKEND, &params.branch, &params.repo_root).await?;

        let result = run(BINARY, ["restack"], &params.repo_root).await?;
        if result.success {
            info!(branch = %params.branch, "restacked via gt");
            return Ok(RestackOutcome::Restacked);
        }

        // D19: a conflict must not be auto-resolved. Abort any in-progress
        // rebase gt may have left, then surface the manual recipe.
        let combined = result.combined();
        if looks_like_conflict(&combined) {
            let _ = run(BINARY, ["rebase", "--abort"], &params.repo_root).await;
            let recipe = format!(
                "git rebase --onto {} {} {}  # then: gt restack && gt submit",
                params.new_parent, params.old_parent, params.branch
            );
            warn!(branch = %params.branch, recipe = %recipe, "gt restack conflict; aborted");
            return Ok(RestackOutcome::Conflict { recipe });
        }

        Err(StackError::Git {
            message: format!(
                "gt restack failed for {}: {}",
                params.branch,
                nonempty(&result.stderr, &result.stdout),
            ),
        })
    }

    async fn force_push(&self, branch: &str, _repo_root: &Path) -> Result<(), StackError> {
        // Graphite force-pushes (with lease) as part of `gt submit`/`gt restack`.
        // Exposing a no-op keeps the trait uniform for mixed-mode callers.
        info!(branch, "force_push is a no-op for graphite; gt submit handles it");
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
    fn kind_is_graphite() {
        // Construct directly to avoid the PATH check in tests that only care
        // about the identifier.
        assert_eq!(GraphiteStackBackend.kind(), "graphite");
    }

    #[test]
    fn install_hint_names_the_binary() {
        assert!(INSTALL_HINT.contains("gt"));
    }
}
