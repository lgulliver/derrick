# T016: derrick observe — ratatui TUI dashboard

## Why

`derrick status` gives a one-shot snapshot of the substrate. For developers
running a multi-ticket batch in a tmux pane, that's not enough: they want a
persistent, self-updating dashboard they can leave open and glance at. D18
mandates this ships in v1 as `derrick observe`. The goal is the same data as
`derrick status --watch` but at much higher information density, with navigation
between concerns (tickets, stack, activity, tokens, memory) without spawning
additional commands.

The TUI is deliberately **read-only** in v1. No mutations means a runaway TUI
process can't corrupt substrate state.

## What

1. **`crates/derrick-tui`** — the ratatui+crossterm dashboard library.
   - `App` struct holding UI state (active tab, scroll offsets, selected rows).
   - Six tabs rendered via ratatui `Tabs` widget (see §Tabs below).
   - `DataModel` snapshot struct populated from `Substrate` trait reads.
   - Live update loop: `notify` file watcher on `.derrick/derrick.db`,
     `.derrick/runs/`, `.derrick/foreman.pid`; 1-second tick timer as fallback.
   - Incremental redraw: only the active tab pane redraws on tick; header/footer
     always redraws on state change.
   - Key bindings: `q` quit, `r` force refresh, `↑`/`↓` row navigation,
     `⏎` open detail/browser, `/` search filter, `?` help overlay, `1`–`6`
     jump to tab by number, `--tab <name>` CLI flag sets initial tab.

2. **`crates/derrick-observe`** — thin binary crate / library entry point that
   wires `derrick-tui` to the native substrate and the CLI `observe` subcommand.

3. **CLI wiring** in `derrick-cli`: `derrick observe [--tab <name>]
   [--site <name>] [--read-only]`.

### Tabs

#### [1] Overview (default)

The "09:30 standup" view. Shows:
- Active batch name + progress bar (`▰▰▱▱ N/M done · K in-flight · J ready · L blocked`)
- Foreman status (running/stopped, PID, uptime, escalation count)
- Stack summary (backend, merged/open/pending PRs, restack health)
- Last assay result (verdict, round, model, timestamp)
- Token today summary (raw → actual, savings %)
- In-flight table: ticket id, hand name, description, elapsed time
- Ready-next table: ticket id, description, blocker list

#### [2] Tickets

Sortable, filterable table of all tickets in the active batch.
- Columns: id, state (colour-coded), title, hand, age, PR link
- Filter bar (`/`): accepts `ready`, `in-flight`, `blocked`, `done`, `mine`
- `⏎` on a row opens a detail pane: full body, blocker list, hand history,
  PR link, recent events from the `events` table
- `Esc` closes detail pane

#### [3] Stack

ASCII tree of the current PR graph (populated from `derrick-stack` state via
the `Substrate` trait, or by shelling out to the configured stack backend).
- Merge state per node (open / merged / pending)
- `restack-conflict` tickets shown in red
- `⏎` on a node opens the PR URL in the user's default browser
  (`open`/`xdg-open`)

#### [4] Activity

Live tail of the `events` table from the substrate.
- Auto-scrolls to newest; `↑` pauses auto-scroll, `↓` to bottom re-enables
- Filter bar: by ticket id, hand id, run id
- Columns: timestamp, kind, scope (ticket/hand/batch), message

#### [5] Tokens

`derrick gain --pillars` data rendered live.
- Per-step cost table
- Model-tier breakdown bar chart (ratatui `BarChart`)
- Savings attribution table: each of the seven RTK/scrub/caveman/caching knobs
  with raw, actual, saved columns

#### [6] Memory

Current site's memory entries from `~/.claude/projects/<repo>/memory/`
(project, reference, feedback, lessons types).
- Table: type, name, description, age
- `d` on a row flags the entry for deletion (writes to
  `.derrick/memory-prune-queue.json`; not applied until `derrick memory prune`)
- `⏎` on a row opens full content in a scrollable detail pane
- `Esc` closes detail pane

### Header / Footer

Header: `derrick · site: <name> · mode: <crew|solo> · backend: <native|graphite|git-spice> ── HH:MM`

Footer: `q quit   r refresh   ↑↓ nav   ⏎ open   / search   ? help`

## Scope

**In scope (v1):**
- All six tabs as described above
- ratatui + crossterm only (no other TUI frameworks)
- Read-only; zero substrate writes except the memory-prune queue file
- File watcher (`notify` crate) + 1s tick
- Incremental tab redraws
- `--tab`, `--site`, `--read-only` CLI flags
- `derrick observe` subcommand wired into `derrick-cli`
- `crates/derrick-tui` as the library; `crates/derrick-observe` as the wiring layer

**Explicitly out of scope (v1):**
- Any mutation of tickets, batches, or hands from the TUI
- Mouse support
- Tokens tab live data beyond what `DataModel` already pulls from substrate
- Remote (SSH) substrate access
- Custom theme / colour configuration
- Plugin tab extension points

## Acceptance criteria

- `cargo test --workspace` passes with `derrick-tui` and `derrick-observe` included
- `cargo clippy --workspace -- -D warnings` clean; no `unwrap`/`expect`/`panic`
  in non-test, non-binary code
- `derrick observe` starts, renders the Overview tab, and exits cleanly on `q`
  with a terminal left in a usable state (no leftover raw-mode artefacts)
- `derrick observe --tab tickets` opens directly on the Tickets tab
- Tick fires at ~1s and triggers a redraw; verified by unit test with a fake
  clock/event source
- File watcher fires on a write to the substrate DB path and triggers a redraw
  (integration test using `tempfile`)
- Overview tab shows correct in-flight and ready-next counts from a seeded
  substrate (integration test)
- Tickets tab filter `ready` shows only `Ready` tickets; `done` shows only `Done`
- Activity tab auto-scrolls to newest event on new `Event` insertion
- Memory tab `d` keypress on a row appends the entry slug to
  `.derrick/memory-prune-queue.json`; the substrate is not mutated
- Stack tab `⏎` on a PR node calls the system open command with the PR URL
- `--read-only` flag is accepted without error (same behaviour as default in v1)
- Terminal is restored to cooked mode on panic (crossterm `LeaveAlternateScreen`
  in a panic hook)

## Open questions

1. **Tab naming vs prompt**: DESIGN.md §5.7 names the tabs Overview, Tickets,
   Stack, Activity, Tokens, Memory. The invocation prompt listed "Activity,
   Tickets, Stack, Batches, Hands, Constitution". This spec follows DESIGN.md
   as the canonical source. Confirm before planning if the prompt's tab names
   were intentional overrides.

2. **Stack data source**: The Stack tab needs PR graph data. Does it read from
   a `stacks` table in the substrate (the `derrick-stack` crate would need to
   expose this via the `Substrate` trait) or shell out to `gh pr list --json`?
   The `Substrate` trait currently has no stack query method — a new
   `list_stack_entries` method or a direct `gh` shell-out is needed.

3. **Tokens tab data source**: `derrick gain` computation lives in
   `derrick-scrub`/token-economist. Does `derrick-tui` import that crate
   directly, or call a lightweight substrate query storing pre-computed gain
   snapshots? Recommend the substrate-query path to keep `derrick-tui`'s
   dependency footprint small.

4. **Memory entries path**: Should `derrick-tui` read the memory directory
   (`~/.claude/projects/<repo>/memory/`) directly via filesystem, or should the
   `Substrate` trait expose a `list_memory_entries` method? Direct file reads
   are simpler but couple the TUI to an out-of-trait path.

5. **`notify` async integration**: The `notify` crate has blocking and async
   variants. Preferred approach: `notify-async` wrapper or a `std::sync::mpsc`
   channel bridged into a tokio task?
