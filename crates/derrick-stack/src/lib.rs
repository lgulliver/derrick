//! derrick-stack — PR stacking. See DESIGN.md §8.5.
//!
//! Defines the [`StackBackend`] trait and three implementations:
//!
//! - [`NoneStackBackend`]: stacking disabled. Open-PR fails with
//!   [`StackError::NotSupported`]; restack/force-push are no-ops.
//! - [`NativeStackBackend`]: shells to `git rebase --onto` and `gh pr create`.
//! - [`GraphiteStackBackend`]: not implemented in v1; documents the manual
//!   `gt restack` recipe via [`StackError::NotSupported`].
//!
//! The crate is transport-agnostic: it knows about git, gh, and config but
//! has no substrate or foreman dependency. Callers (foreman, CLI) compute
//! parent branches via [`parent_branch_for`] and feed them in.

#![deny(missing_docs)]

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use derrick_substrate::{Ticket, TicketId};
use thiserror::Error;

pub mod backends;

pub use backends::graphite::GraphiteStackBackend;
pub use backends::native::NativeStackBackend;
pub use backends::none::NoneStackBackend;

/// Errors returned by [`StackBackend`] operations.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum StackError {
    /// I/O error invoking a child process or reading its output.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// `git` returned a non-zero exit code or produced unexpected output.
    #[error("git error: {message}")]
    Git {
        /// Human-readable diagnostic.
        message: String,
    },
    /// `gh` returned a non-zero exit code or produced unexpected output.
    #[error("gh error: {message}")]
    Gh {
        /// Human-readable diagnostic.
        message: String,
    },
    /// Operation is not supported by the selected backend.
    #[error("not supported by backend {backend}: {reason}")]
    NotSupported {
        /// Backend identifier (`"none"`, `"graphite"`, ...).
        backend: &'static str,
        /// Human-readable reason or remediation.
        reason: &'static str,
    },
}

/// Result of a successful [`StackBackend::open_pr`] call.
#[derive(Clone, Debug)]
pub struct PrInfo {
    /// PR number assigned by the host (GitHub).
    pub number: u64,
    /// PR URL as printed by `gh`.
    pub url: String,
    /// Head SHA of the PR at the moment it was opened.
    pub head_sha: String,
}

/// Inputs for [`StackBackend::open_pr`].
#[derive(Clone, Debug)]
pub struct OpenPrParams {
    /// Branch whose tip should become the PR head.
    pub branch: String,
    /// Base branch the PR targets.
    pub parent_branch: String,
    /// PR title.
    pub title: String,
    /// PR body (markdown).
    pub body: String,
    /// Open as draft PR.
    pub draft: bool,
    /// Repository root (where `gh` is invoked).
    pub repo_root: PathBuf,
}

/// Inputs for [`StackBackend::restack`].
#[derive(Clone, Debug)]
pub struct RestackParams {
    /// Branch to be restacked.
    pub branch: String,
    /// Current parent branch (`upstream` of the rebase).
    pub old_parent: String,
    /// New parent branch (`--onto` of the rebase).
    pub new_parent: String,
    /// Repository root.
    pub repo_root: PathBuf,
}

/// Outcome of a [`StackBackend::restack`] attempt.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum RestackOutcome {
    /// Restack succeeded. Branch is now based off `new_parent`.
    Restacked,
    /// Conflict during rebase. `recipe` is the exact `git rebase --onto`
    /// command the human can run to resolve. The backend MUST have aborted
    /// the rebase before returning this variant.
    Conflict {
        /// Recipe command the human can run after resolving conflicts.
        recipe: String,
    },
}

/// Trait implemented by stacking backends.
#[async_trait]
pub trait StackBackend: Send + Sync {
    /// Backend identifier used in events and errors.
    fn kind(&self) -> &'static str;
    /// Open a PR for `params.branch` targeting `params.parent_branch`.
    async fn open_pr(&self, params: OpenPrParams) -> Result<PrInfo, StackError>;
    /// Restack `params.branch` from `params.old_parent` onto `params.new_parent`.
    async fn restack(&self, params: RestackParams) -> Result<RestackOutcome, StackError>;
    /// Force-push `branch` with `--force-with-lease` semantics.
    async fn force_push(&self, branch: &str, repo_root: &Path) -> Result<(), StackError>;
}

/// Compute the branch name for a ticket given the configured `pattern`.
///
/// Replaces `{{batch}}` and `{{ticket_id}}` substrings.
pub fn compute_branch_name(pattern: &str, batch: &str, ticket_id: &str) -> String {
    pattern
        .replace("{{batch}}", batch)
        .replace("{{ticket_id}}", ticket_id)
}

/// Compute the parent branch for a ticket dispatch.
///
/// `predecessors` is the list of dependency ticket IDs (e.g. from
/// `blocks_predecessors`). `predecessor_tickets` is the corresponding set of
/// fully-loaded tickets that the caller has fetched from the substrate.
/// Returns the computed branch name for the highest-`ordinal` predecessor, or
/// `target_branch` if there are no predecessors.
pub fn parent_branch_for(
    predecessors: &[TicketId],
    predecessor_tickets: &[Ticket],
    target_branch: &str,
    branch_pattern: &str,
) -> String {
    let _ = predecessors;
    if predecessor_tickets.is_empty() {
        return target_branch.to_owned();
    }
    // Pick the predecessor with the highest ordinal (tickets with no ordinal
    // sort lowest so an explicit ordinal always wins).
    let pick = predecessor_tickets
        .iter()
        .max_by_key(|ticket| ticket.ordinal.unwrap_or(0));
    let Some(pick) = pick else {
        return target_branch.to_owned();
    };
    let batch = pick
        .batch
        .as_ref()
        .map(|b| b.as_str().to_owned())
        .unwrap_or_default();
    compute_branch_name(branch_pattern, &batch, pick.id.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use derrick_substrate::{BatchName, TicketState};

    fn fake_ticket(id: &str, batch: Option<&str>, ordinal: Option<u32>) -> Ticket {
        Ticket {
            id: TicketId::new(id).expect("ticket id"),
            batch: batch.map(|b| BatchName::new(b).expect("batch name")),
            ordinal,
            title: "t".to_owned(),
            body: "b".to_owned(),
            state: TicketState::Ready,
            labels: Vec::new(),
            owner: None,
            merge_sha: None,
            block_reason: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn parent_branch_for_no_predecessors_returns_target() {
        let result = parent_branch_for(&[], &[], "main", "derrick/{{batch}}/{{ticket_id}}");
        assert_eq!(result, "main");
    }

    #[test]
    fn parent_branch_for_single_predecessor_returns_computed_branch() {
        let pred = fake_ticket("drk-1", Some("alpha"), Some(1));
        let predecessors = vec![pred.id.clone()];
        let result = parent_branch_for(
            &predecessors,
            &[pred],
            "main",
            "derrick/{{batch}}/{{ticket_id}}",
        );
        assert_eq!(result, "derrick/alpha/drk-1");
    }

    #[test]
    fn parent_branch_for_picks_highest_ordinal() {
        let lower = fake_ticket("drk-1", Some("alpha"), Some(1));
        let higher = fake_ticket("drk-2", Some("alpha"), Some(5));
        let result = parent_branch_for(
            &[lower.id.clone(), higher.id.clone()],
            &[lower, higher],
            "main",
            "derrick/{{batch}}/{{ticket_id}}",
        );
        assert_eq!(result, "derrick/alpha/drk-2");
    }

    #[test]
    fn compute_branch_name_replaces_pattern() {
        let name = compute_branch_name("derrick/{{batch}}/{{ticket_id}}", "alpha", "drk-1");
        assert_eq!(name, "derrick/alpha/drk-1");
    }
}
