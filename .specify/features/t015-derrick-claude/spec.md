# T015: derrick-claude — Claude Code hand dispatcher

## Why

T013 shipped Copilot dispatch. T014 shipped stacking. Neither helps users
whose repos don't have GitHub Copilot enabled. The `claude` hand type (DESIGN.md
§8.2) lets Claude Code be the executor: derrick writes a complete prompt file,
optionally spawns `claude --print` autonomously, and the Claude session calls
`derrick ticket review` to close the loop. No GitHub Copilot dependency.

## What

New crate `crates/derrick-claude` implementing `HandDispatcher` for `kind: claude`.

1. **Queue file writer**: writes `.derrick/queue/<ticket-id>.md` — a structured
   prompt that tells Claude Code to create the branch, implement the ticket, push,
   and run `derrick ticket review <id> --branch <branch> --head-sha <sha>`.

2. **Interactive mode** (default, `auto_dispatch: false`): transition ticket to
   `InFlight`, write the queue file, print a hint to stdout, return
   `completed_synchronously: false`. The user opens the file in their Claude Code
   session manually.

3. **Autonomous mode** (`auto_dispatch: true`): same, then spawn
   `claude --print < .derrick/queue/<ticket-id>.md` as a background
   `tokio::task`. The task calls `hand_heartbeat` each minute, waits for the
   ticket to reach `InReview` (polled from substrate), and releases the hand on
   timeout. No PR polling needed — Claude's final step transitions the state.

4. **Config**: `tools.claude` block in `derrick.yaml` mirrors `tools.copilot`.

5. **CLI wiring**: `build_foreman` in `derrick-cli` registers
   `ClaudeHandDispatcher` when `tools.claude.enabled: true`.

## Acceptance criteria

- `cargo test --workspace` passes
- Queue file rendered for a ticket contains branch name, parent branch, full
  body, and the exact `derrick ticket review` command with correct args
- Interactive mode: ticket transitions `Ready → InFlight`; queue file written;
  hint printed; `dispatch()` returns without spawning a subprocess
- Autonomous mode: ticket transitions `Ready → InFlight`; background task
  spawned; after a fake Claude session calls `derrick ticket review` the foreman
  verifier transitions ticket to `InReview`
- Timeout: background task calls `release_from_hand` after `poll_timeout` elapses
  and ticket hasn't reached `InReview`
- `cargo clippy --workspace -- -D warnings` clean; no `unwrap`/`expect`/`panic`
  in non-test code
