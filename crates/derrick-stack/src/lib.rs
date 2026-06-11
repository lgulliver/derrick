//! derrick-stack — PR stacking. See DESIGN.md §8.5.
//!
//! Defines the [`StackBackend`] trait and its two implementations:
//!
//! - [`NoneStackBackend`]: stacking disabled. Open-PR fails with
//!   [`StackError::NotSupported`]; restack/force-push are no-ops.
//! - [`NativeStackBackend`]: derrick's own stacking engine — plain `git`
//!   (`rebase --onto`, `push --force-with-lease`) and `gh pr create`. This is
//!   the only real backend (D72); derrick owns its stacking technology and
//!   does not delegate to any third-party stacking CLI.
//!
//! The [`StackBackend`] trait stays as the §8.6 extension seam so a future
//! backend can be added without touching callers.
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
        /// Backend identifier (`"none"`, `"native"`, ...).
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
///
/// New capabilities are added as default methods that return
/// [`StackError::NotSupported`] so the trait stays the §8.6 extension seam:
/// existing backends keep compiling, and a backend opts in by overriding.
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

    /// Retarget an open PR's base branch (e.g. `gh pr edit --base`).
    ///
    /// Used by the merge cascade: when a parent PR lands, the child PR's git
    /// branch is rebased *and* its GitHub base must move to the new target,
    /// otherwise the PR keeps comparing against a merged/deleted branch.
    /// Default: [`StackError::NotSupported`].
    async fn retarget_pr(
        &self,
        _branch: &str,
        _new_base: &str,
        _repo_root: &Path,
    ) -> Result<(), StackError> {
        Err(StackError::NotSupported {
            backend: "stack-backend",
            reason: "retarget_pr not supported by this backend",
        })
    }

    /// Replace the body of the PR for `branch` (e.g. `gh pr edit --body`).
    ///
    /// Used to maintain the stack-navigation section. Default:
    /// [`StackError::NotSupported`].
    async fn set_pr_body(
        &self,
        _branch: &str,
        _body: &str,
        _repo_root: &Path,
    ) -> Result<(), StackError> {
        Err(StackError::NotSupported {
            backend: "stack-backend",
            reason: "set_pr_body not supported by this backend",
        })
    }

    /// Read the current PR body for `branch` (e.g. `gh pr view --json body`).
    ///
    /// Returns `None` when no PR exists. Default: [`StackError::NotSupported`].
    async fn pr_body(
        &self,
        _branch: &str,
        _repo_root: &Path,
    ) -> Result<Option<String>, StackError> {
        Err(StackError::NotSupported {
            backend: "stack-backend",
            reason: "pr_body not supported by this backend",
        })
    }
}

/// Start marker for the derrick stack-navigation section in a PR body.
pub const NAV_START: &str = "<!-- derrick-stack-start -->";
/// End marker for the derrick stack-navigation section in a PR body.
pub const NAV_END: &str = "<!-- derrick-stack-end -->";

/// One entry in a stack-navigation table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StackNavEntry {
    /// Ticket id the PR belongs to.
    pub ticket_id: String,
    /// Short title shown next to the position.
    pub title: String,
    /// PR URL, when the PR is open. `None` renders as a pending entry.
    pub pr_url: Option<String>,
}

/// Render the markdown for a stack-navigation section (markers included).
///
/// `entries` are in stack order (root first). `current_index` is the position
/// of the PR whose body this section is being written into; that row is marked
/// with an arrow so reviewers can see where they are in the stack.
pub fn render_nav_section(entries: &[StackNavEntry], current_index: usize) -> String {
    let mut out = String::new();
    out.push_str(NAV_START);
    out.push('\n');
    out.push_str("**Stack** (managed by derrick):\n\n");
    for (index, entry) in entries.iter().enumerate() {
        let marker = if index == current_index { "👉 " } else { "" };
        let position = index + 1;
        let link = match &entry.pr_url {
            Some(url) => format!("[{}]({})", entry.title, url),
            None => format!("{} (pending)", entry.title),
        };
        out.push_str(&format!(
            "{marker}{position}. {link} — `{}`\n",
            entry.ticket_id
        ));
    }
    out.push('\n');
    out.push_str(NAV_END);
    out
}

/// Idempotently upsert the nav section into an existing PR body.
///
/// If the body already contains the `<!-- derrick-stack-start -->` /
/// `<!-- derrick-stack-end -->` markers, the content between them (inclusive)
/// is replaced. Otherwise the section is appended. Running this twice with the
/// same inputs produces a body with exactly one section.
pub fn upsert_nav_section(body: &str, section: &str) -> String {
    match (body.find(NAV_START), body.find(NAV_END)) {
        (Some(start), Some(end_marker_start)) if end_marker_start >= start => {
            let end = end_marker_start + NAV_END.len();
            let mut out = String::with_capacity(body.len() + section.len());
            out.push_str(&body[..start]);
            out.push_str(section);
            out.push_str(&body[end..]);
            out
        }
        _ => {
            let mut out = String::with_capacity(body.len() + section.len() + 2);
            out.push_str(body.trim_end());
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(section);
            out
        }
    }
}

/// A node in the stack DAG used for topological ordering.
#[derive(Clone, Debug)]
pub struct StackNode {
    /// Ticket id.
    pub id: String,
    /// Sort ordinal (lower sorts first; `None` sorts last).
    pub ordinal: Option<u32>,
    /// Ids this node depends on (its `blocks` predecessors / parents).
    pub parents: Vec<String>,
}

/// Compute a deterministic topological order over a stack DAG.
///
/// A node appears only after all of its `parents`. Ties (nodes whose
/// dependencies are all satisfied at the same time) are broken by `ordinal`
/// then by `id`, so the order is stable across runs. Edges that point at
/// unknown ids are ignored (the caller may pass a filtered sub-DAG).
///
/// Returns `Err` listing the ids that could not be ordered when the graph
/// contains a cycle.
pub fn topological_order(nodes: &[StackNode]) -> Result<Vec<String>, Vec<String>> {
    use std::collections::{BTreeMap, BTreeSet};

    let known: BTreeSet<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
    let sort_key: BTreeMap<&str, (u32, &str)> = nodes
        .iter()
        .map(|n| {
            (
                n.id.as_str(),
                (n.ordinal.unwrap_or(u32::MAX), n.id.as_str()),
            )
        })
        .collect();
    let order_key =
        |id: &str| -> (u32, &str) { sort_key.get(id).copied().unwrap_or((u32::MAX, "")) };

    // indegree counts only edges among known nodes.
    let mut indegree: BTreeMap<&str, usize> = nodes.iter().map(|n| (n.id.as_str(), 0)).collect();
    let mut children: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for node in nodes {
        for parent in &node.parents {
            if known.contains(parent.as_str()) {
                *indegree.get_mut(node.id.as_str()).expect("known node") += 1;
                children
                    .entry(parent.as_str())
                    .or_default()
                    .push(node.id.as_str());
            }
        }
    }

    // Frontier of ready nodes, kept sorted by (ordinal, id).
    let mut frontier: Vec<&str> = indegree
        .iter()
        .filter(|&(_, &deg)| deg == 0)
        .map(|(&id, _)| id)
        .collect();
    frontier.sort_by_key(|id| order_key(id));

    let mut ordered: Vec<String> = Vec::with_capacity(nodes.len());
    while let Some(next) = frontier.first().copied() {
        frontier.remove(0);
        ordered.push(next.to_owned());
        if let Some(kids) = children.get(next) {
            let mut newly_ready = Vec::new();
            for &kid in kids {
                let deg = indegree.get_mut(kid).expect("known child");
                *deg -= 1;
                if *deg == 0 {
                    newly_ready.push(kid);
                }
            }
            for kid in newly_ready {
                let pos = frontier
                    .binary_search_by_key(&order_key(kid), |id| order_key(id))
                    .unwrap_or_else(|e| e);
                frontier.insert(pos, kid);
            }
        }
    }

    if ordered.len() == nodes.len() {
        Ok(ordered)
    } else {
        let placed: BTreeSet<&str> = ordered.iter().map(String::as_str).collect();
        Err(nodes
            .iter()
            .map(|n| n.id.as_str())
            .filter(|id| !placed.contains(id))
            .map(str::to_owned)
            .collect())
    }
}

/// Collect the transitive descendants of `root` in a DAG (excluding `root`),
/// in deterministic topological order. Used to bail a whole subtree on a
/// D19 restack conflict while leaving independent subtrees alone.
pub fn descendants_of(nodes: &[StackNode], root: &str) -> Vec<String> {
    use std::collections::{BTreeSet, VecDeque};

    // child adjacency among known nodes
    let mut children: std::collections::BTreeMap<&str, Vec<&str>> =
        std::collections::BTreeMap::new();
    for node in nodes {
        for parent in &node.parents {
            children
                .entry(parent.as_str())
                .or_default()
                .push(node.id.as_str());
        }
    }
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut queue: VecDeque<&str> = VecDeque::new();
    queue.push_back(root);
    while let Some(cur) = queue.pop_front() {
        if let Some(kids) = children.get(cur) {
            for &kid in kids {
                if seen.insert(kid) {
                    queue.push_back(kid);
                }
            }
        }
    }
    // Return in topo order restricted to the descendant set for determinism.
    match topological_order(nodes) {
        Ok(order) => order
            .into_iter()
            .filter(|id| seen.contains(id.as_str()))
            .collect(),
        Err(_) => seen.into_iter().map(str::to_owned).collect(),
    }
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
            complexity: None,
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

    fn node(id: &str, ordinal: Option<u32>, parents: &[&str]) -> StackNode {
        StackNode {
            id: id.to_owned(),
            ordinal,
            parents: parents.iter().map(|p| (*p).to_owned()).collect(),
        }
    }

    #[test]
    fn topo_order_places_parents_before_children() {
        // a -> b -> c chain (b blocks on a, c blocks on b).
        let nodes = vec![
            node("c", Some(3), &["b"]),
            node("a", Some(1), &[]),
            node("b", Some(2), &["a"]),
        ];
        let order = topological_order(&nodes).expect("acyclic");
        assert_eq!(order, vec!["a", "b", "c"]);
    }

    #[test]
    fn topo_order_breaks_ties_by_ordinal_then_id() {
        // Two roots, both ready at once: ordinal decides, then id.
        let nodes = vec![
            node("z", Some(2), &[]),
            node("a", Some(2), &[]),
            node("m", Some(1), &[]),
        ];
        let order = topological_order(&nodes).expect("acyclic");
        // ordinal 1 first (m), then ordinal 2 tie broken by id (a before z).
        assert_eq!(order, vec!["m", "a", "z"]);
    }

    #[test]
    fn topo_order_independent_subtrees_interleave_deterministically() {
        // Two independent chains a->b and x->y. Deterministic by (ordinal,id).
        let nodes = vec![
            node("b", Some(2), &["a"]),
            node("a", Some(1), &[]),
            node("y", Some(4), &["x"]),
            node("x", Some(3), &[]),
        ];
        let order = topological_order(&nodes).expect("acyclic");
        assert_eq!(order, vec!["a", "b", "x", "y"]);
    }

    #[test]
    fn topo_order_detects_cycle() {
        let nodes = vec![node("a", Some(1), &["b"]), node("b", Some(2), &["a"])];
        let err = topological_order(&nodes).expect_err("cycle");
        assert_eq!(err.len(), 2);
    }

    #[test]
    fn descendants_of_returns_transitive_subtree_in_order() {
        // a -> b -> {c, d}; e is independent.
        let nodes = vec![
            node("a", Some(1), &[]),
            node("b", Some(2), &["a"]),
            node("c", Some(3), &["b"]),
            node("d", Some(4), &["b"]),
            node("e", Some(5), &[]),
        ];
        let desc = descendants_of(&nodes, "b");
        assert_eq!(desc, vec!["c", "d"]);
        let from_a = descendants_of(&nodes, "a");
        assert_eq!(from_a, vec!["b", "c", "d"]);
        assert!(!from_a.contains(&"e".to_owned()));
    }

    #[test]
    fn nav_section_upsert_is_idempotent() {
        let entries = vec![
            StackNavEntry {
                ticket_id: "drk-1".to_owned(),
                title: "root".to_owned(),
                pr_url: Some("https://x/pull/1".to_owned()),
            },
            StackNavEntry {
                ticket_id: "drk-2".to_owned(),
                title: "child".to_owned(),
                pr_url: None,
            },
        ];
        let section = render_nav_section(&entries, 1);
        let body = "Original description.";
        let once = upsert_nav_section(body, &section);
        assert!(once.contains("Original description."));
        assert_eq!(once.matches(NAV_START).count(), 1);
        assert_eq!(once.matches(NAV_END).count(), 1);
        // Highlight is on the current (index 1) entry.
        assert!(once.contains("👉 2. child"));

        // Running again replaces, never duplicates.
        let twice = upsert_nav_section(&once, &section);
        assert_eq!(twice.matches(NAV_START).count(), 1);
        assert_eq!(twice.matches(NAV_END).count(), 1);
        assert_eq!(once, twice);
    }

    #[test]
    fn nav_section_appends_when_markers_absent() {
        let section = render_nav_section(&[], 0);
        let out = upsert_nav_section("body text", &section);
        assert!(out.starts_with("body text"));
        assert!(out.contains(NAV_START));
    }
}
