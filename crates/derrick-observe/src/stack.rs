//! Background stack refresh: shells out to `gh pr list --json` and populates
//! the shared `StackNode` vec. Runs in a spawned tokio task so latency does
//! not block the main event loop.

use std::path::Path;
use std::sync::{Arc, RwLock};

use derrick_config::StackBackendKind;
use derrick_tui::{StackLoadResult, StackNode};

/// Write `result` into the shared load-state arc.
fn set_load_result(arc: &Arc<RwLock<StackLoadResult>>, result: StackLoadResult) {
    match arc.write() {
        Ok(mut g) => *g = result,
        Err(p) => *p.into_inner() = result,
    }
}

/// Refresh the shared `nodes` vec by shelling out to `gh pr list`.
///
/// Called once on startup in a background `tokio::task`.
///
/// On success the shared `nodes` vec is replaced and `load_result` is set to
/// [`StackLoadResult::Loaded`].  On any failure `load_result` is set to
/// [`StackLoadResult::Error`] with a short human-readable message so the
/// Stack tab can render an explicit error rather than an eternal spinner. The
/// error is also emitted via `tracing::warn!`.
pub async fn refresh_stack_nodes(
    backend: StackBackendKind,
    repo_root: &Path,
    nodes: Arc<RwLock<Vec<StackNode>>>,
    load_result: Arc<RwLock<StackLoadResult>>,
) {
    if backend == StackBackendKind::None {
        // Stacking disabled — mark as loaded with an empty list so the Stack
        // tab shows "no open PRs found" rather than "loading…".
        match nodes.write() {
            Ok(mut g) => g.clear(),
            Err(p) => p.into_inner().clear(),
        }
        set_load_result(&load_result, StackLoadResult::Loaded);
        return;
    }

    let output = match tokio::process::Command::new("gh")
        .args([
            "pr",
            "list",
            "--json",
            "number,headRefName,baseRefName,url,state,title",
            "--limit",
            "50",
        ])
        .current_dir(repo_root)
        .output()
        .await
    {
        Ok(o) => o,
        Err(e) => {
            let reason = format!("gh not found or could not be executed: {e}");
            tracing::warn!("stack refresh: {reason}");
            set_load_result(&load_result, StackLoadResult::Error(reason));
            return;
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let reason = format!(
            "gh pr list exited non-zero: {}",
            stderr.lines().next().unwrap_or("(no stderr)")
        );
        tracing::warn!("stack refresh: {reason}");
        set_load_result(&load_result, StackLoadResult::Error(reason));
        return;
    }

    let values: Vec<serde_json::Value> = match serde_json::from_slice(&output.stdout) {
        Ok(v) => v,
        Err(e) => {
            let reason = format!("failed to parse gh pr list output: {e}");
            tracing::warn!("stack refresh: {reason}");
            set_load_result(&load_result, StackLoadResult::Error(reason));
            return;
        }
    };

    let fresh: Vec<StackNode> = values
        .iter()
        .map(|v| {
            let branch = v["headRefName"].as_str().unwrap_or("").to_owned();
            StackNode {
                ticket_id: branch.clone(),
                branch,
                pr_url: v["url"].as_str().map(String::from),
                pr_number: v["number"].as_u64(),
                state: v["state"]
                    .as_str()
                    .map(str::to_ascii_lowercase)
                    .unwrap_or_else(|| "open".to_owned()),
                parent_branch: v["baseRefName"].as_str().map(String::from),
            }
        })
        .collect();

    tracing::debug!("stack refresh: {} nodes loaded", fresh.len());
    match nodes.write() {
        Ok(mut g) => *g = fresh,
        Err(p) => *p.into_inner() = fresh,
    }
    set_load_result(&load_result, StackLoadResult::Loaded);
}
