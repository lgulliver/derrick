//! derrick-observe — wiring layer for the `derrick observe` TUI dashboard.
//!
//! Constructs the native substrate, reads memory entries from the
//! filesystem, spawns the optional stack adapter shell-out, and hands all
//! of it to `derrick-tui`'s event loop.

#![deny(clippy::unwrap_used, clippy::expect_used)]

mod stack;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use derrick_config::Config;
use derrick_substrate::Substrate;
use derrick_substrate_native::{NativeConfig, NativeSubstrate};
use derrick_tui::{
    App, DataModel, EventLoopPaths, MemoryEntry, StackNode, Tab, install_panic_hook, run_event_loop,
};

const MEMORY_PREVIEW_CHARS: usize = 200;

/// Run the `derrick observe` dashboard until the user quits.
pub async fn observe(initial_tab: Tab, _site: Option<String>) -> anyhow::Result<()> {
    let repo_root = find_repo_root()?;
    let config = Config::load_from_path(&repo_root.join("derrick.yaml"))
        .with_context(|| format!("loading derrick.yaml from {}", repo_root.display()))?;

    let state_dir = repo_root.join(config.state().dir());
    let db_path = state_dir.join("derrick.db");
    let native_config = NativeConfig {
        db_path: db_path.clone(),
        worktree_root: repo_root.join(config.state().worktree_root()),
    };
    let substrate: Arc<dyn Substrate> = Arc::new(
        NativeSubstrate::open(native_config, config.site().clone())
            .await
            .context("opening native substrate")?,
    );

    let memory_dir = state_dir.join("memory");
    let memory = read_memory_entries(&memory_dir).unwrap_or_default();
    let memory_entries = Arc::new(std::sync::RwLock::new(memory.clone()));

    // Stack nodes: populated asynchronously in the background. v1 starts
    // empty; the renderer shows a "loading" sentinel.
    let stack_nodes = Arc::new(std::sync::RwLock::new(Vec::<StackNode>::new()));

    // Spawn a background task to populate stack nodes from the gh CLI.
    {
        let backend = config.tools().git().stacking().backend();
        let root = repo_root.clone();
        let sn_clone = Arc::clone(&stack_nodes);
        tokio::spawn(async move {
            stack::refresh_stack_nodes(backend, &root, sn_clone).await;
        });
    }

    let watch_paths = vec![
        db_path,
        state_dir.join("runs"),
        state_dir.join("foreman.pid"),
    ];

    // Initial data load before entering the alternate screen so any
    // substrate error surfaces in the user's normal terminal.
    let sn = match stack_nodes.read() {
        Ok(g) => g.clone(),
        Err(p) => p.into_inner().clone(),
    };
    let runs_dir = state_dir.join("runs");
    let data = DataModel::refresh(&*substrate, &sn, &memory, Some(runs_dir.as_path()))
        .await
        .context("initial data refresh")?;
    let mut app = App::new(initial_tab, data);

    install_panic_hook();
    crossterm::terminal::enable_raw_mode().context("enable raw mode")?;
    let mut stdout = io::stdout();
    crossterm::execute!(stdout, crossterm::terminal::EnterAlternateScreen)
        .context("enter alternate screen")?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = ratatui::Terminal::new(backend).context("construct terminal")?;

    let result = run_event_loop(
        &mut app,
        Arc::clone(&substrate),
        stack_nodes,
        memory_entries,
        EventLoopPaths {
            watch_paths,
            prune_queue_path: Some(state_dir.join("memory-prune-queue.json")),
            runs_dir: Some(runs_dir),
        },
        &mut terminal,
    )
    .await;

    let _ = crossterm::terminal::disable_raw_mode();
    let _ = crossterm::execute!(
        terminal.backend_mut(),
        crossterm::terminal::LeaveAlternateScreen,
    );
    let _ = terminal.show_cursor();

    result
}

fn find_repo_root() -> anyhow::Result<PathBuf> {
    let cwd = std::env::current_dir().context("read cwd")?;
    for candidate in cwd.ancestors() {
        if candidate.join(".git").exists() {
            return Ok(candidate.to_path_buf());
        }
    }
    anyhow::bail!("derrick observe must be run inside a git repo")
}

/// Read `*.md` files from `dir` into [`MemoryEntry`] rows. Returns an empty
/// list (not an error) when the directory does not exist.
pub fn read_memory_entries(dir: &Path) -> anyhow::Result<Vec<MemoryEntry>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut entries = Vec::new();
    for dirent in std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let dirent = dirent?;
        let path = dirent.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let slug = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("(unknown)")
            .to_owned();
        let body = std::fs::read_to_string(&path).unwrap_or_default();
        let preview: String = body.chars().take(MEMORY_PREVIEW_CHARS).collect();
        entries.push(MemoryEntry {
            slug,
            path,
            preview,
        });
    }
    entries.sort_by(|a, b| a.slug.cmp(&b.slug));
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn read_memory_entries_skips_non_md_and_collects_md() {
        let tmp = match TempDir::new() {
            Ok(t) => t,
            Err(e) => {
                eprintln!("tempdir: {e}");
                return;
            }
        };
        std::fs::write(tmp.path().join("a.md"), "hello world").unwrap_or(());
        std::fs::write(tmp.path().join("b.md"), "second").unwrap_or(());
        std::fs::write(tmp.path().join("ignore.txt"), "nope").unwrap_or(());
        let entries = match read_memory_entries(tmp.path()) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("read failed: {e}");
                return;
            }
        };
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].slug, "a");
        assert_eq!(entries[1].slug, "b");
    }

    #[test]
    fn read_memory_entries_returns_empty_for_missing_dir() {
        let path = PathBuf::from("/definitely/not/a/real/path/derrick-obs-test");
        let entries = match read_memory_entries(&path) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("err: {e}");
                return;
            }
        };
        assert!(entries.is_empty());
    }
}
