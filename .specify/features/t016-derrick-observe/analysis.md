# Analysis: T016 — derrick observe (ratatui TUI dashboard)

## Spec coverage

The plan addresses the spec thoroughly at a high level:

- Both crates (`derrick-tui`, `derrick-observe`) are scoped correctly
- All six tabs have dedicated renderer modules (`overview.rs`, `tickets.rs`, `stack.rs`, `activity.rs`, `tokens.rs`, `memory.rs`)
- Header/footer module (`header_footer.rs`) is present
- All key bindings from the spec (`q`, `r`, `↑`/`↓`, `⏎`, `Esc`, `/`, `?`, `1`–`6`, `d`) are listed in `App` key dispatch
- All five open questions are explicitly resolved at plan-time
- All 13 acceptance criteria are traceable to a specific step or test case
- The `--tab`, `--site`, `--read-only` CLI flags are wired in step 9
- Panic hook for terminal restoration is in step 5
- File watcher using `notify` + `mpsc` channel bridge is specified

## Gaps

### 1. Stack data flow is architecturally unresolved (critical)

`list_stack_nodes` (step 8) lives in `derrick-observe`, but `DataModel::refresh` is called from within `derrick-tui`'s event loop (step 5). `derrick-tui` cannot import `derrick-observe` (circular dependency), and the plan states `derrick-tui` has "no I/O except the file watcher."

The plan does not specify how stack node data crosses from `derrick-observe` into `derrick-tui`'s `DataModel`. The options — pre-population before `run()`, a channel passed into `run()`, or an injected async callback — are not addressed. This gap will surface at implementation time and could require reworking the `DataModel` or `run()` API.

### 2. Tokens savings attribution table is explicitly deferred

The spec requires the Tokens tab to render a "savings attribution table: each of the seven RTK/scrub/caveman/caching knobs with raw, actual, saved columns." The plan acknowledges this is deferred ("full savings attribution deferred until substrate stores gain snapshots") and substitutes derivation from `tail_events` totals. This is a partial implementation of a spec-required element, not a fully addressed tab.

### 3. Memory dir path not threaded into `derrick-tui::run()`

The public API in step 6 is `run(substrate: Arc<dyn Substrate>, initial_tab: Tab)`. The memory dir path (resolved in `derrick-observe` from site config) is needed by the Memory tab renderer in `derrick-tui`. No mechanism is specified for passing it. Direct filesystem reads in `derrick-tui` would require either hardcoding a path derivation or adding the path to the `run()` signature.

## Concerns

### Stack shell-out latency and staleness UX

The risk table mentions running stack refresh in a spawned `tokio::task`. Without a clear data flow design, there is a risk that stale/empty stack data is displayed for a noticeable period on startup, with no UI indicator that a refresh is in progress. The spec does not define a loading state, but users will notice a blank Stack tab.

### `DataModel` becomes a fat struct with mixed ownership

`DataModel` spans substrate queries, filesystem reads (memory dir), and shell-out data (stack nodes). Keeping these concerns distinct — especially when refresh cadences differ (stack is slow, events are fast) — may require per-tab refresh timestamps or selective refresh. The plan does not address differential refresh rates between tabs.

### Token-economist coupling is deferred but spec expects full table

The seven-knob savings attribution table is a first-class spec element. Deferring it means the v1 Tokens tab will visibly underdeliver relative to the spec. If this is accepted, it should be recorded as a scoped-down decision (D-entry or explicit note in the plan), not just a risk row.

### `clippy` / `unwrap` hygiene has no enforcement step

The acceptance criterion "no `unwrap`/`expect`/`panic` in non-test, non-binary code" is not assigned to a step. It is easy to miss in a large new crate. Consider adding a CI check or a lint step explicitly.

## Recommendation

**Revise plan** before implementation.

The stack data flow gap (Gap 1) is the blocking issue: without a design for how `list_stack_nodes` output reaches `DataModel` inside `derrick-tui`'s event loop, the Stack tab implementation will stall or introduce an ad-hoc solution that violates the crate boundary. The fix is small — likely adding a `stack_nodes: Arc<RwLock<Vec<StackNode>>>` channel or extending `run()` to accept an optional stack-node provider — but it needs to be decided before step 4 (tab renderers) is written.

The memory dir path gap (Gap 3) is similar: extend `run()` or add a `TuiConfig` struct to carry ambient config, resolving both gaps in one API change.

The Tokens tab deferral (Gap 2) should be explicitly acknowledged as an accepted scope reduction, either as a new D-entry or a spec amendment, so reviewers know it is intentional.

Once those three points are resolved in the plan, this feature is well-specified and ready to implement.
