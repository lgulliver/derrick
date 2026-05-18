# T013: derrick-copilot — Copilot hand implementation for crew mode dispatch

## Why

T012 shipped the foreman loop with `CopilotStubDispatcher` — a placeholder that
returns `DispatchError::NotImplemented { kind: "copilot" }`. Until T013 lands,
`mode: crew` cannot dispatch real tickets; the foreman can run its loop but will
never move a ticket from `Ready` to `InFlight`. T013 replaces the stub with a
real `CopilotHandDispatcher` that invokes the GitHub Copilot coding agent via the
`gh` CLI, polls for PR creation, and feeds the resulting branch + PR metadata back
into the substrate so the foreman's verifier can confirm the merge per D32/D33.

## What

A new crate `derrick-copilot` (or a module inside `derrick-substrate-native`)
implementing `HandDispatcher` for the Copilot coding agent:

1. **Dispatch**: invoke `gh copilot task create` (or the equivalent Copilot coding
   agent CLI surface) with the ticket's title, body, and target branch. Register a
   fresh `Hand` row in the substrate. Call `substrate.assign_to_hand(ticket, hand)`.

2. **Poll for PR**: after dispatch, poll `gh pr list --head <branch>` until a PR
   appears (up to a configurable timeout, default 10 minutes, with exponential
   back-off). When found, call `substrate.transition_to_in_review(ticket,
   InReviewMetadata { branch, pr_url, pr_number, head_sha })`.

3. **Wire crew mode end-to-end**: with `CopilotHandDispatcher` replacing
   `CopilotStubDispatcher` in the foreman's dispatcher registry, `mode: crew` in
   `derrick.yaml` dispatches tickets to Copilot and the foreman loop verifies
   merges per D32/D33 without further manual steps.

4. **Branch naming**: `derrick/<batch>/<ticket-id>` per DESIGN.md §8.3 / D19.
   The dispatcher creates this branch before dispatching if it doesn't exist.

5. **Hand lifecycle**: the dispatcher registers the hand, dispatches, starts a
   background polling task (or writes a hand record for the foreman's next tick
   to pick up), and returns. The hand's `last_seen` is updated by the polling loop.

## Scope

**In scope:**
- `CopilotHandDispatcher` implementing `HandDispatcher` trait from T012
- Branch creation (`git checkout -b derrick/<batch>/<ticket>`) before dispatch
- Copilot task creation via `gh` CLI subprocess (using `derrick-tools` HostAdapter pattern)
- PR poll loop with timeout + exponential back-off
- `transition_to_in_review` call with real `InReviewMetadata`
- Config wiring: `tools.copilot.enabled: true` gates dispatch; `tools.copilot.poll_timeout` (default 10m), `tools.copilot.poll_interval` (default 30s)
- Registration in foreman's dispatcher registry so `kind: copilot` tickets are dispatched

**Out of scope:**
- Claude hand dispatcher (T014 or later)
- Multi-site Copilot agent federation
- Re-stacking on merge (T014 `derrick-stack`)
- Copilot agent authentication — assumes `gh auth` is already configured
- TUI / observe dashboard (T015)

## Acceptance criteria

- `cargo test -p derrick-copilot` (or the owning crate) passes with ≥80% coverage
- A `mode: crew` site with `kind: copilot` tickets dispatches successfully in an
  integration test using a mock `gh` subprocess
- `CopilotStubDispatcher` in `derrick-substrate-native` is replaced or superseded
  by `CopilotHandDispatcher` from this crate
- `derrick foreman tick` on a `Ready` + `kind: copilot` ticket transitions it to
  `InFlight` and writes a hand row
- `derrick foreman tick` on a hand that has a PR open transitions the ticket to
  `InReview` with the correct `InReviewMetadata`
- Clippy `-D warnings` clean; no `unwrap`/`expect`/`panic` in non-test code

## Open questions

1. Does `gh copilot` have a `task create` subcommand stable enough to script against,
   or should dispatch go through the GitHub Issues API (`gh issue create` + assign
   Copilot) as a more stable surface? The MCP dispatch attempts in this session used
   `create_pull_request_with_copilot` and hit 401 repeatedly — worth understanding
   why before picking the CLI surface.

2. Should polling run synchronously inside `dispatch()` (blocking until PR appears)
   or asynchronously (dispatch returns immediately, foreman picks up the poll on next
   tick via hand heartbeat)? Async is cleaner for the foreman model but requires the
   foreman to know when to poll vs when to wait.

3. The `HandDispatcher::dispatch` signature returns `DispatchResult` with a
   `completed_synchronously` flag — if polling is async, this is `false` and the
   foreman loop handles `transition_to_in_review`. Is that the right cut?
