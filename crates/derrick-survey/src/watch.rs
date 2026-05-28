//! Debounced filesystem watcher (freshness layer 1): on any change under the
//! repo root, mark the index dirty and trigger an incremental rebuild after a
//! short quiet period. The `dirty` flag lets the MCP layer surface a staleness
//! banner cheaply without re-hashing the tree on every query.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use notify::{RecursiveMode, Watcher};
use tokio::sync::mpsc;

use crate::model::BuildOptions;
use crate::{Survey, SurveyError};

const DEBOUNCE: Duration = Duration::from_millis(500);

/// Watch the repo and rebuild the index after each burst of changes. Runs
/// until the watcher channel closes. `dirty` is set when changes arrive and
/// cleared after a successful rebuild.
pub(crate) async fn watch_loop(survey: Survey, dirty: Arc<AtomicBool>) -> Result<(), SurveyError> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res {
            // Ignore events that touch only pruned trees — most importantly the
            // index's own `.derrick/index.db` writes, which would otherwise
            // re-arm the watcher after every rebuild and loop forever. An event
            // with no paths is treated as relevant (safe default).
            let relevant = event.paths.is_empty()
                || event
                    .paths
                    .iter()
                    .any(|p| !crate::walk::is_under_skipped_dir(p));
            if relevant {
                let _ = tx.send(());
            }
        }
    })?;
    watcher.watch(survey.repo_root(), RecursiveMode::Recursive)?;

    while rx.recv().await.is_some() {
        dirty.store(true, Ordering::Relaxed);
        // Drain the burst: keep resetting the timer until things go quiet.
        loop {
            tokio::select! {
                () = tokio::time::sleep(DEBOUNCE) => break,
                msg = rx.recv() => {
                    if msg.is_none() {
                        return Ok(());
                    }
                }
            }
        }
        if let Err(error) = survey.build(BuildOptions::default()).await {
            tracing::warn!(%error, "survey watch rebuild failed");
        } else {
            dirty.store(false, Ordering::Relaxed);
        }
    }
    Ok(())
}
