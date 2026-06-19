//! Async event loop that drives the dashboard.
//!
//! `tokio::select!`s over:
//!   1. crossterm key events (via `EventStream`)
//!   2. a 1-second tick interval
//!   3. an mpsc channel fed by a `notify` filesystem watcher
//!
//! Each event triggers a `DataModel::refresh` when appropriate, then a
//! redraw.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{Event as CtEvent, EventStream, KeyCode};
use derrick_substrate::Substrate;
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::Backend;
use ratatui::layout::{Constraint, Direction, Layout};
use tokio::sync::mpsc;

use crate::app::App;
use crate::data::{DataModel, MemoryEntry, StackLoadResult, StackNode};
use crate::tabs::{render_active_tab, render_footer, render_header, render_tabs_bar};

/// Install a panic hook that restores the terminal before letting the
/// default hook print the panic. Idempotent — safe to call multiple times.
pub fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::execute!(
            std::io::stderr(),
            crossterm::terminal::LeaveAlternateScreen,
            crossterm::cursor::Show,
        );
        let _ = crossterm::terminal::disable_raw_mode();
        default(info);
    }));
}

/// Spawn an OS-thread `notify` watcher and bridge its events into a tokio
/// channel. The watcher itself is held inside the thread to keep it alive.
fn spawn_watcher(paths: Vec<PathBuf>) -> mpsc::Receiver<()> {
    let (tx, rx) = mpsc::channel::<()>(16);
    std::thread::spawn(move || {
        let (raw_tx, raw_rx) = std::sync::mpsc::channel::<notify::Result<notify::Event>>();
        let watcher_result = notify::recommended_watcher(move |res| {
            let _ = raw_tx.send(res);
        });
        let mut watcher = match watcher_result {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!("notify: failed to create watcher: {e}");
                return;
            }
        };
        for path in &paths {
            // It's normal for some of these to not exist yet (e.g. foreman.pid
            // before the foreman starts). Log and continue.
            if let Err(e) =
                notify::Watcher::watch(&mut watcher, path, notify::RecursiveMode::Recursive)
            {
                tracing::debug!("notify: skipping {}: {e}", path.display());
            }
        }
        while let Ok(_res) = raw_rx.recv() {
            // Coalesce: a single send is enough to wake the loop.
            if tx.blocking_send(()).is_err() {
                break;
            }
        }
    });
    rx
}

/// File-system and path configuration for the event loop.
///
/// Passed as a single argument to keep `run_event_loop`'s arity below
/// the clippy `too_many_arguments` threshold.
pub struct EventLoopPaths {
    /// Paths the `notify` watcher should watch for filesystem changes.
    pub watch_paths: Vec<PathBuf>,
    /// Path to the JSON file where memory slug prune requests are queued.
    pub prune_queue_path: Option<PathBuf>,
    /// Path to `.derrick/runs/` for token aggregation from run manifests.
    /// `None` disables token tracking.
    pub runs_dir: Option<PathBuf>,
}

/// Run the event loop until `app.quit` is set.
///
/// `stack_nodes`, `stack_load_result`, and `memory_entries` are read each
/// tick to pick up updates made by background tasks (stack adapter shell-out,
/// future memory file changes).
pub async fn run_event_loop<B: Backend>(
    app: &mut App,
    substrate: Arc<dyn Substrate>,
    stack_nodes: Arc<std::sync::RwLock<Vec<StackNode>>>,
    stack_load_result: Arc<std::sync::RwLock<StackLoadResult>>,
    memory_entries: Arc<std::sync::RwLock<Vec<MemoryEntry>>>,
    paths: EventLoopPaths,
    terminal: &mut Terminal<B>,
) -> anyhow::Result<()>
where
    <B as Backend>::Error: std::error::Error + Send + Sync + 'static,
{
    let EventLoopPaths {
        watch_paths,
        prune_queue_path,
        runs_dir,
    } = paths;
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // D78: ~100 ms animation tick for the Factory tab. Drives only local
    // animation state (app.animation_frame) — the substrate is still polled at
    // the 1 Hz `tick` cadence above and on `notify` fs events. ratatui's
    // diff-based rendering keeps the 10x redraw cheap.
    let mut anim_tick = tokio::time::interval(Duration::from_millis(100));
    anim_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut watcher_rx = spawn_watcher(watch_paths);

    // Initial draw.
    redraw(terminal, app)?;

    loop {
        if app.quit {
            break;
        }

        let mut needs_refresh = false;
        let mut needs_redraw = false;

        tokio::select! {
            biased;

            maybe_key = events.next() => {
                match maybe_key {
                    Some(Ok(CtEvent::Key(k))) => {
                        // Ignore key release on terminals that send them.
                        if k.kind == crossterm::event::KeyEventKind::Release {
                            continue;
                        }
                        app.handle_key(k.code);
                        if app.refresh_requested {
                            app.refresh_requested = false;
                            needs_refresh = true;
                        }
                        if let Some(url) = app.pending_open_url.take() {
                            open_in_browser(&url);
                        }
                        if let Some(slug) = app.pending_prune_slug.take() {
                            if let Some(ref path) = prune_queue_path {
                                append_prune_slug(path, &slug);
                            }
                        }
                        // Treat ctrl-c as quit too.
                        if k.code == KeyCode::Char('c')
                            && k.modifiers.contains(crossterm::event::KeyModifiers::CONTROL)
                        {
                            app.quit = true;
                        }
                        needs_redraw = true;
                    }
                    Some(Ok(_)) => {
                        needs_redraw = true;
                    }
                    Some(Err(e)) => {
                        tracing::warn!("crossterm event error: {e}");
                    }
                    None => break,
                }
            }
            _ = tick.tick() => {
                needs_refresh = true;
                needs_redraw = true;
            }
            _ = watcher_rx.recv() => {
                needs_refresh = true;
                needs_redraw = true;
            }
            _ = anim_tick.tick() => {
                // D78: animation frame for the Factory tab. Pure local state —
                // no substrate refresh.
                app.animation_frame = app.animation_frame.wrapping_add(1);
                needs_redraw = true;
            }
        }

        if needs_refresh {
            let sn = match stack_nodes.read() {
                Ok(g) => g.clone(),
                Err(p) => p.into_inner().clone(),
            };
            let slr = match stack_load_result.read() {
                Ok(g) => g.clone(),
                Err(p) => p.into_inner().clone(),
            };
            let me = match memory_entries.read() {
                Ok(g) => g.clone(),
                Err(p) => p.into_inner().clone(),
            };
            match DataModel::refresh(&*substrate, &sn, slr, &me, runs_dir.as_deref()).await {
                Ok(data) => app.set_data(data),
                Err(e) => tracing::warn!("data refresh failed: {e}"),
            }
        }
        if needs_redraw {
            redraw(terminal, app)?;
        }
    }

    Ok(())
}

fn open_in_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(not(target_os = "macos"))]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
}

fn append_prune_slug(path: &std::path::Path, slug: &str) {
    let mut slugs: Vec<String> = if path.exists() {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    if !slugs.contains(&slug.to_owned()) {
        slugs.push(slug.to_owned());
    }
    if let Ok(json) = serde_json::to_string_pretty(&slugs) {
        let _ = std::fs::write(path, json);
    }
}

fn redraw<B: Backend>(terminal: &mut Terminal<B>, app: &App) -> anyhow::Result<()>
where
    <B as Backend>::Error: std::error::Error + Send + Sync + 'static,
{
    terminal.draw(|frame| {
        let size = frame.area();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2), // header
                Constraint::Length(3), // tabs bar
                Constraint::Min(5),    // body
                Constraint::Length(2), // footer
            ])
            .split(size);
        render_header(frame, chunks[0], app);
        render_tabs_bar(frame, chunks[1], app);
        render_active_tab(frame, chunks[2], app);
        render_footer(frame, chunks[3]);
    })?;
    Ok(())
}
