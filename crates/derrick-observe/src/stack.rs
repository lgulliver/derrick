//! Background stack refresh: shells out to `gh pr list --json` and populates
//! the shared `StackNode` vec. Runs in a spawned tokio task so latency does
//! not block the main event loop.

use std::path::Path;
use std::sync::{Arc, RwLock};

use derrick_config::StackBackendKind;
use derrick_tui::StackNode;

/// Refresh the shared `nodes` vec by shelling out to `gh pr list`.
///
/// Called once on startup in a background `tokio::task`. Silently logs
/// errors and leaves `nodes` unchanged on failure so the UI shows
/// "loading…" until data arrives or fails.
pub async fn refresh_stack_nodes(
    backend: StackBackendKind,
    repo_root: &Path,
    nodes: Arc<RwLock<Vec<StackNode>>>,
) {
    if backend == StackBackendKind::None {
        // Stacking disabled — leave the list empty; the Stack tab will show
        // "no stack data" rather than "loading…".
        match nodes.write() {
            Ok(mut g) => g.clear(),
            Err(p) => p.into_inner().clear(),
        }
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
            tracing::warn!("stack refresh: gh pr list failed: {e}");
            return;
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::warn!("stack refresh: gh pr list non-zero exit: {stderr}");
        return;
    }

    let values: Vec<serde_json::Value> = match serde_json::from_slice(&output.stdout) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("stack refresh: parse error: {e}");
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
}
