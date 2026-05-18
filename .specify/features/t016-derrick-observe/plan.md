# Plan: T016 — derrick observe (ratatui TUI dashboard)

## Approach

Build `derrick observe` as two new crates layered on the existing `Substrate` trait:

1. **`crates/derrick-tui`** — pure library; owns `App`, `DataModel`, rendering, key handling, and the event loop. No I/O except the file watcher and the tokio runtime it gets handed.
2. **`crates/derrick-observe`** — thin wiring layer; imports `derrick-tui` + `derrick-substrate-native`, constructs the substrate, spawns the TUI, and is the entry-point called by `derrick-cli`.

The CLI gains an `Observe` variant in `Command` that delegates immediately to `derrick-observe`.

All six tabs read exclusively from `Substrate` trait methods or direct filesystem reads (memory dir); no writes except the memory-prune queue JSON file. That constraint keeps the TUI safe to run concurrently with the foreman.

**Open question resolutions (plan-time decisions):**

- **Tab names**: follow DESIGN.md §5.7 (Overview, Tickets, Stack, Activity, Tokens, Memory). The prompt's alternative names are discarded.
- **Stack data source**: shell out to the configured stack backend (`gh pr list --json`/graphite/git-spice) via the same adapter pattern used in `derrick-stack`. A new `list_stack_nodes` helper in `derrick-observe` wraps this; no new `Substrate` trait method in v1.
- **Tokens tab**: `derrick-tui` imports `derrick-substrate` only; token data is pulled from the substrate `tail_events` feed (events carry `tokens_in`/`tokens_out` in their metadata). No direct `derrick-scrub` import in the TUI crate.
- **Memory entries**: direct filesystem read of `~/.claude/projects/<repo>/memory/*.md` in `derrick-observe`; the directory path is derived from the site config. No new `Substrate` trait method.
- **`notify` async**: use `notify` with a `std::sync::mpsc` channel bridged into a tokio task. Avoids the unstable `notify-async` API surface.

## Steps

### 1. Add dependencies to both crates (`derrick-tui/Cargo.toml`, `derrick-observe/Cargo.toml`)
- `derrick-tui`: `ratatui`, `crossterm`, `tokio` (rt-multi-thread), `notify`, `chrono`, `unicode-width`
- `derrick-observe`: `derrick-tui`, `derrick-substrate`, `derrick-substrate-native`, `derrick-config`, `tokio`, `anyhow`
- Update workspace `Cargo.toml` if any new workspace-level deps are needed

### 2. `derrick-tui`: define `DataModel` and `Tab` enum (`crates/derrick-tui/src/data.rs`)
- `DataModel` snapshot struct with fields for each tab's data (batch, foreman, tickets, events, stack nodes, token summary, memory entries)
- `Tab` enum: `Overview | Tickets | Stack | Activity | Tokens | Memory`; `TryFrom<&str>` for `--tab` flag parsing

### 3. `derrick-tui`: implement `App` state (`crates/derrick-tui/src/app.rs`)
- `App { active_tab, scroll_offset, selected_row, filter, detail_open, data, last_refresh }`
- Key dispatch table: `q`, `r`, `↑`/`↓`, `⏎`, `Esc`, `/`, `?`, `1`–`6`, `d`
- Search filter state machine (active/inactive)

### 4. `derrick-tui`: implement tab renderers (`crates/derrick-tui/src/tabs/`)
- `overview.rs` — batch progress bar, foreman status, stack summary, assay result, token today, in-flight table, ready-next table
- `tickets.rs` — sortable/filterable table + detail pane
- `stack.rs` — ASCII PR tree; `⏎` → `open`/`xdg-open`
- `activity.rs` — auto-scrolling event tail, filter bar
- `tokens.rs` — per-step cost table + model-tier `BarChart` + savings attribution table
- `memory.rs` — memory entry table, detail pane, `d` prune-queue append
- `header_footer.rs` — site/mode/backend/time header and key-hint footer

### 5. `derrick-tui`: implement event loop (`crates/derrick-tui/src/event_loop.rs`)
- `tokio::select!` over: crossterm event stream, 1s tick interval, `mpsc::Receiver` from file watcher
- File watcher: `notify::recommended_watcher` watching `.derrick/derrick.db`, `.derrick/runs/`, `.derrick/foreman.pid`; sends on any `Create`/`Modify` event
- On tick or watcher event: call async `DataModel::refresh(&substrate).await`; only redraw active tab pane
- Panic hook: install `crossterm::execute!(LeaveAlternateScreen, DisableRawMode)` in `std::panic::set_hook`

### 6. `derrick-tui`: public API (`crates/derrick-tui/src/lib.rs`)
- `run(substrate: Arc<dyn Substrate>, initial_tab: Tab) -> anyhow::Result<()>`
- This is the only public symbol `derrick-observe` needs

### 7. `derrick-observe`: wire substrate + memory reads (`crates/derrick-observe/src/lib.rs`)
- `pub async fn observe(site: Option<String>, initial_tab: Tab) -> anyhow::Result<()>`
- Constructs `NativeSubstrate` from config; resolves memory dir from site config
- Calls `derrick_tui::run(substrate, initial_tab)`

### 8. `derrick-observe`: stack data helper (`crates/derrick-observe/src/stack.rs`)
- `list_stack_nodes(backend: StackBackend) -> anyhow::Result<Vec<StackNode>>` shells out to configured adapter (same approach as `derrick-stack` adapter dispatch)
- `StackNode { id, title, state, pr_url, is_conflict }`; consumed by `DataModel`

### 9. `derrick-cli`: add `Observe` subcommand (`crates/derrick-cli/src/commands/observe.rs` + `mod.rs`)
- `ObserveArgs { tab: Option<String>, site: Option<String>, read_only: bool }`
- Add `Command::Observe(ObserveArgs)` to the `Command` enum
- Handler parses `--tab` to `Tab`; calls `derrick_observe::observe(site, tab).await`

### 10. Tests

**Unit tests (in `derrick-tui`):**
- Fake clock/event source: verify tick at ~1s triggers `DataModel::refresh` call (use mock substrate)
- Filter logic: `ready` filter on a mixed ticket set returns only `Ready` tickets; `done` filter returns only `Done`
- Key dispatch: `q` sets quit flag; `1`–`6` switch active tab; `/` enters filter mode
- Memory prune queue: `d` keypress on a row appends the correct slug to the prune queue path without touching substrate

**Integration tests (in `derrick-observe`):**
- File watcher: write to a `tempfile`-backed substrate DB path and assert `DataModel::refresh` is triggered within 2s
- Overview counts: seed a substrate (via `NativeSubstrate`) with 2 in-flight + 3 ready tickets; assert `DataModel.overview.in_flight_count == 2` and `ready_count == 3`
- Activity auto-scroll: insert a new event; assert `DataModel.activity.scroll_to_bottom` is set after refresh
- Stack `⏎` integration: mock `open`/`xdg-open` via env override; assert PR URL is passed

**CLI smoke test:**
- `derrick observe --tab tickets` parses without error (clap round-trip test)
- `--read-only` flag accepted without error

## Risks

| Risk | Mitigation |
|---|---|
| **Terminal restoration on panic** | Install panic hook wrapping `LeaveAlternateScreen` + `DisableRawMode` in step 5; covered by acceptance criterion |
| **`notify` missing events on macOS FSEvents** | Use `RecommendedWatcher` (backed by kqueue/FSEvents); 1s tick is the fallback safety net |
| **Stack backend shell-out latency blocking TUI** | Run stack refresh in a spawned `tokio::task`; stale data shows until refresh completes; do not block main event loop |
| **Tokens tab: no pre-computed gain in substrate** | Derive from `tail_events` totals in `DataModel`; full savings attribution deferred until substrate stores gain snapshots |
| **Memory dir path varies by OS/site** | Derive from `derrick-config` site resolution; test with a `TempDir`-based fixture |
| **Open questions 1–5 left unresolved** | All five resolved above under "plan-time decisions" |

## Dependencies

| Dependency | Why |
|---|---|
| `derrick-substrate` (trait + models) | `DataModel::refresh` calls `list_tickets`, `tail_events`, `foreman_status`, `list_hands`, `list_batches` |
| `derrick-substrate-native` | `derrick-observe` constructs the concrete substrate |
| `derrick-config` | site name resolution, memory dir path derivation |
| `derrick-stack` (adapter dispatch pattern, not imported directly) | Stack tab shell-out follows the same backend dispatch established in T014 |
| `ratatui` + `crossterm` | mandated by D18 |
| `notify` | file watcher |
| `tokio` | async event loop |
| T014 (derrick-stack) | stack adapter dispatch pattern is the reference; T014 must be merged before stack tab integration tests run against real data |
