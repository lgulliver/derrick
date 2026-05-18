# Plan: T013 — derrick-copilot

## Approach

Create `crates/derrick-copilot` as a new crate implementing `HandDispatcher` for
the Copilot coding agent. The dispatcher uses `gh` CLI subprocesses (via the
`derrick-tools` `HostAdapter` pattern) to create a branch, dispatch a Copilot task,
and poll for the resulting PR. When the PR is found, it calls
`substrate.transition_to_in_review` with real `InReviewMetadata`. The crate replaces
`CopilotStubDispatcher` in `derrick-substrate-native` and wires into `derrick-cli`'s
foreman construction path.

The async dispatch model: `dispatch()` returns `completed_synchronously: false` after
kicking off the Copilot task. The foreman's next tick polls hand heartbeat and, when
the hand's background task has called `transition_to_in_review`, the verifier step
picks it up. The polling loop runs as a `tokio::task::spawn` inside the dispatcher
and updates `hand_heartbeat` on each poll iteration so the foreman's cleanup pass
doesn't time it out.

## Steps

1. **New crate scaffold** (`crates/derrick-copilot/`)
   - `Cargo.toml` with deps: `derrick-substrate`, `derrick-tools`, `derrick-config`,
     `async-trait`, `tokio`, `tracing`, `thiserror`, `serde_json`, `chrono`
   - Add to workspace `Cargo.toml`

2. **`BranchCreator` helper**
   - Shells to `git checkout -b derrick/<batch>/<ticket-id> <base-branch>` (no-op if
     branch exists)
   - `git push -u origin <branch>` to publish before Copilot dispatch

3. **`CopilotDispatchClient` trait + `GhCopilotClient` impl**
   - `create_task(branch, title, body) -> Result<TaskId>` — shells to
     `gh issue create --title ... --body ... --label copilot` then
     `gh issue assign <number> --assignee @copilot` (the stable surface that avoids
     the 401 Copilot API issue seen in this session)
   - `poll_pr(branch) -> Result<Option<PrInfo>>` — shells to
     `gh pr list --head <branch> --json number,url,headRefOid` and returns `None`
     if empty

4. **`CopilotHandDispatcher` struct + `HandDispatcher` impl**
   - `dispatch()`: create branch → push → create issue → assign Copilot → register
     hand → `assign_to_hand` → spawn poll task → return `DispatchResult`
   - Poll task: loop with exponential back-off (initial 30s, max 5min) calling
     `poll_pr`, `hand_heartbeat`, and `transition_to_in_review` when PR found

5. **Background poll task with exponential back-off**
   - Configurable `poll_interval` (default 30s) and `poll_timeout` (default 10min)
     from `config.tools.copilot`
   - Uses `tokio::time::sleep`; respects cancellation via `CancellationToken`

6. **Integration tests** using `tempfile` SQLite + mock `GhCopilotClient`
   - `dispatch_creates_branch_and_registers_hand`
   - `poll_task_transitions_to_in_review_when_pr_found`
   - `poll_task_respects_timeout`
   - `dispatch_is_idempotent_on_existing_branch`

7. **Deprecate `CopilotStubDispatcher`** in `derrick-substrate-native` — add
   `#[deprecated]` pointing to `derrick-copilot::CopilotHandDispatcher`

8. **Wire into `derrick-cli`**
   - `build_foreman` reads `config.tools.copilot.enabled`; if true, registers
     `CopilotHandDispatcher` in the dispatcher registry

## Risks

- **Copilot API surface**: the `create_pull_request_with_copilot` MCP endpoint 401'd
  repeatedly. Using `gh issue create` + assign-to-Copilot is more stable but depends
  on Copilot being set up in the repo. Document the setup requirement in `derrick doctor`.
- **Race condition on poll**: Copilot may push multiple commits before opening a PR.
  The poll must wait for a PR, not just any push to the branch.
- **tokio::task::spawn in dispatch()**: if the foreman process exits before the poll
  task completes, the task is silently dropped. Mitigation: the hand row's `last_seen`
  stops being updated, and the foreman's next run picks it up as abandoned and re-queues.

## Dependencies

- T012 `derrick-substrate-native` (trait surface, `CopilotStubDispatcher` to supersede)
- `derrick-tools` (HostAdapter subprocess pattern)
- `derrick-config` (`tools.copilot` block already partially defined)
