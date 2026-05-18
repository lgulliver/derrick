# Tasks: T013 — derrick-copilot

## Task 1: Scaffold `derrick-copilot` crate

**Crate**: `crates/derrick-copilot` (new)
**Depends on**: —
**What**: Create the crate skeleton — `Cargo.toml` with workspace deps
(`derrick-substrate`, `derrick-tools`, `derrick-config`, `async-trait`,
`tokio`, `tracing`, `thiserror`, `serde_json`, `chrono`, `tokio-util`),
`lib.rs` with module stubs, add to workspace `Cargo.toml`.
**Done when**: `cargo check -p derrick-copilot` passes; crate appears in
`cargo metadata`.

## Task 2: `BranchCreator` helper

**Crate**: `derrick-copilot`
**Depends on**: Task 1
**What**: A helper that shells to `git checkout -b derrick/<batch>/<ticket-id>
<base-branch>` (no-op if branch already exists) and `git push -u origin <branch>`
to publish before Copilot dispatch. Uses `derrick-tools` `HostAdapter` pattern
for subprocess invocation.
**Done when**: Unit tests with a fake `git` script confirm idempotent branch
creation and push; `cargo test -p derrick-copilot` passes.

## Task 3: `CopilotDispatchClient` trait + `GhCopilotClient` impl

**Crate**: `derrick-copilot`
**Depends on**: Task 1
**What**: `CopilotDispatchClient` trait with `create_task(branch, title, body)
-> Result<TaskId>` (shells to `gh issue create --title … --body … --label copilot`
then `gh issue assign <number> --assignee @copilot`) and `poll_pr(branch) ->
Result<Option<PrInfo>>` (shells to `gh pr list --head <branch> --json
number,url,headRefOid`). Includes `FakeGhClient` inline impl for tests.
**Done when**: `cargo test -p derrick-copilot` passes; fake client exercises both
`create_task` success and `poll_pr` returning `None` then `Some(PrInfo)`.

## Task 4: `CopilotHandDispatcher` + background poll task

**Crate**: `derrick-copilot`
**Depends on**: Tasks 2, 3
**What**: Full `HandDispatcher` impl. `dispatch()`: create branch → push →
create issue → assign Copilot → `substrate.assign_to_hand` → spawn
`tokio::task` that polls with exponential back-off (initial 30s, cap 5min,
configurable timeout 10min from `config.tools.copilot`) calling `hand_heartbeat`
each iteration and `transition_to_in_review` when PR found. Returns
`DispatchResult { completed_synchronously: false }`.
**Done when**: Integration tests using `tempfile` SQLite + `FakeGhClient` +
`tokio::time::pause()` confirm: (a) ticket transitions Ready→InFlight on
dispatch; (b) ticket transitions InFlight→InReview when poll finds PR;
(c) poll respects timeout; (d) heartbeat is called each poll iteration.
Coverage ≥ 80%. Clippy clean. No `unwrap`/`expect`/`panic` in non-test code.

## Task 5: Deprecate `CopilotStubDispatcher`

**Crate**: `derrick-substrate-native`
**Depends on**: Task 4
**What**: Mark `CopilotStubDispatcher` in `derrick-substrate-native` with
`#[deprecated(since = "0.1.0", note = "Use derrick_copilot::CopilotHandDispatcher")]`.
Add a compile test that the deprecated path still compiles (for one release).
**Done when**: `cargo check -p derrick-substrate-native` passes with deprecation
warning; no existing tests broken.

## Task 6: Wire into `derrick-cli` foreman

**Crate**: `derrick-cli`
**Depends on**: Task 4
**What**: In `build_foreman` (or wherever the dispatcher registry is assembled),
check `config.tools.copilot.enabled`; if true, register `CopilotHandDispatcher`.
Add `derrick-copilot` as a dep. Extend `derrick doctor` to check Copilot coding
agent is enabled on the repo when `copilot.enabled: true`. End-to-end integration
test: a `mode: crew` site with `kind: copilot` ticket dispatches through to
`InFlight` and the hand row is present in the substrate.
**Done when**: `cargo test -p derrick-cli` passes including the new integration
test; `derrick doctor` reports Copilot status; `cargo clippy --workspace -- -D
warnings` clean.
