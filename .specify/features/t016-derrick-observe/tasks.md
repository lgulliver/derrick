# Tasks: T016 — derrick observe (ratatui TUI dashboard)

## Task 1: Crate scaffolding and workspace wiring

**Crate**: workspace root + `crates/derrick-tui` + `crates/derrick-observe`

**Depends on**: —

**What**: Create both new crates (`derrick-tui` as a library, `derrick-observe` as a thin library/binary entry-point), add them to the workspace `Cargo.toml`, and populate their `Cargo.toml` files with the required dependencies (`ratatui`, `crossterm`, `tokio`, `notify`, `chrono`, `unicode-width` for `derrick-tui`; `derrick-tui`, `derrick-substrate`, `derrick-substrate-native`, `derrick-config`, `tokio`, `anyhow` for `derrick-observe`). Stub out `lib.rs` for each crate so the workspace builds clean.

**Done when**: `cargo build --workspace` succeeds with both new crates included; `cargo clippy --workspace -- -D warnings` is clean on the stubs.

---

## Task 2: `derrick-tui` — DataModel, App state, and event loop

**Crate**: `crates/derrick-tui`

**Depends on**: Task 1

**What**: Implement `data.rs` (`DataModel` snapshot struct covering all six tabs; `Tab` enum with `TryFrom<&str>`), `app.rs` (`App` holding active tab, scroll offset, selected row, filter state, detail pane open/closed, quit flag, and the key dispatch table for `q`, `r`, `↑`/`↓`, `⏎`, `Esc`, `/`, `?`, `1`–`6`, `d`), and `event_loop.rs` (tokio `select!` over crossterm event stream, 1s tick, and `mpsc::Receiver` from a `notify` file watcher on `.derrick/derrick.db`, `.derrick/runs/`, `.derrick/foreman.pid`). Address analysis Gaps 1 and 3 by introducing a `TuiConfig { memory_dir: PathBuf, stack_nodes: Arc<RwLock<Vec<StackNode>>> }` that is passed into `run()`, so the event loop can populate `DataModel` from substrate queries plus these injected values without importing `derrick-observe`. Install a `std::panic::set_hook` that calls `crossterm::execute!(LeaveAlternateScreen, DisableRawMode)` before unwinding.

**Done when**: `run(substrate, config, initial_tab)` compiles; unit tests confirm: tick fires → `DataModel::refresh` is called (mock substrate); `q` keypress sets quit flag; `1`–`6` switches `active_tab`; `/` enters filter mode; `d` on a memory row appends the slug to the prune-queue path without touching the substrate.

---

## Task 3: `derrick-tui` — tab renderers and header/footer

**Crate**: `crates/derrick-tui`

**Depends on**: Task 2

**What**: Implement all renderer modules under `crates/derrick-tui/src/tabs/`: `overview.rs` (batch progress bar, foreman status, stack summary, assay result, token-today summary, in-flight table, ready-next table), `tickets.rs` (sortable/filterable table + scrollable detail pane), `stack.rs` (ASCII PR tree from injected `StackNode` slice; `⏎` shells out to `open`/`xdg-open` with the PR URL), `activity.rs` (auto-scrolling event tail with filter bar; auto-scroll pauses on `↑`, resumes on `↓`-to-bottom), `tokens.rs` (per-step cost table + model-tier `BarChart` + savings summary derived from `tail_events` totals — full seven-knob savings attribution is explicitly deferred to a follow-on ticket), `memory.rs` (entry table, scrollable detail pane, `d` prune-queue append), and `header_footer.rs` (site/mode/backend/time header; key-hint footer). Each renderer receives `&DataModel` and `&App` and returns a `ratatui::Frame`.

**Done when**: Unit tests pass for: `ready` filter returns only `Ready` tickets; `done` filter returns only `Done` tickets; Activity auto-scroll flag is set after a new event is appended to `DataModel`; Stack `⏎` calls the system open command with the correct PR URL (env-override mock).

---

## Task 4: `derrick-observe` — substrate wiring, memory reads, stack shell-out

**Crate**: `crates/derrick-observe`

**Depends on**: Task 2

**What**: Implement `lib.rs` (`pub async fn observe(site: Option<String>, initial_tab: Tab) -> anyhow::Result<()>` that constructs `NativeSubstrate` from config, resolves the memory directory from site config, reads `*.md` files from that directory to populate `MemoryEntry` values, and constructs `TuiConfig` before calling `derrick_tui::run()`). Implement `stack.rs` (`list_stack_nodes(backend: StackBackend) -> anyhow::Result<Vec<StackNode>>` that shells out to the configured stack adapter — same dispatch pattern as `derrick-stack` — and writes results into the `Arc<RwLock<Vec<StackNode>>>` inside `TuiConfig`; run this in a spawned `tokio::task` so latency does not block the main event loop; show a "loading…" sentinel in the Stack tab until first result arrives).

**Done when**: Integration tests pass: seeding a `NativeSubstrate` (via `tempfile`) with 2 in-flight + 3 ready tickets produces `DataModel.overview.in_flight_count == 2` and `ready_count == 3`; writing to the substrate DB path triggers `DataModel::refresh` within 2s (file watcher test); memory dir fixture with two `*.md` files populates `DataModel.memory` with two entries.

---

## Task 5: `derrick-cli` — `observe` subcommand

**Crate**: `crates/derrick-cli`

**Depends on**: Task 4

**What**: Add `crates/derrick-cli/src/commands/observe.rs` with `ObserveArgs { tab: Option<String>, site: Option<String>, read_only: bool }`, add `Command::Observe(ObserveArgs)` to the `Command` enum in `main.rs`/`mod.rs`, and implement the handler: parse `--tab` to `Tab` (error on unknown name), then call `derrick_observe::observe(site, tab).await`. `--read-only` is accepted without error and behaves identically to the default in v1.

**Done when**: `derrick observe --tab tickets` parses and round-trips via a clap unit test; `derrick observe --read-only` accepts without error; `cargo clippy --workspace -- -D warnings` remains clean.

---

## Task 6: Full test pass and CI hygiene

**Crate**: `crates/derrick-tui` + `crates/derrick-observe`

**Depends on**: Tasks 3, 4, 5

**What**: Run `cargo test --workspace` and fix any remaining failures. Verify all 13 acceptance criteria from the spec are covered by an existing test or add a targeted test for any gap. Confirm `cargo clippy --workspace -- -D warnings` is clean with no `unwrap`/`expect`/`panic` in non-test, non-binary code (add a `#![deny(clippy::unwrap_used, clippy::expect_used)]` attribute to `derrick-tui/src/lib.rs` to enforce this mechanically). Verify `derrick observe` starts, renders the Overview tab, and exits cleanly on `q` with the terminal left in cooked mode.

**Done when**: `cargo test --workspace` green; `cargo clippy --workspace -- -D warnings` clean; manual smoke-test of `derrick observe` exits without raw-mode artefacts; `derrick observe --tab tickets` opens on the Tickets tab.
