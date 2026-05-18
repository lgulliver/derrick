# T014: derrick-stack — PR stacking for crew-mode batches

## Why

T012 shipped the foreman loop and T013 shipped real Copilot dispatch. Both
produce PRs that are all rooted on `main`, which creates merge conflicts when
tickets share files. T014 wires up the stacking architecture from DESIGN.md §8.5
so that batches produce proper stacked PRs: each ticket's branch is based off
its dependency's branch, and when a parent merges the foreman rebases and
force-pushes all dependents.

## What

Implement `StackBackend` trait in `crates/derrick-stack` (currently a skeleton),
integrate it into the foreman loop in `derrick-substrate-native`, thread a
`parent_branch` field through dispatch so hands create branches off the right
parent, and add a `derrick stack` CLI subcommand.

### Scope

**In scope:**
1. `StackBackend` trait + `NativeStackBackend` + `NoneStackBackend` + `GraphiteStackBackend` stub
2. `parent_branch_for_ticket` helper: resolves the parent branch from the dependency graph
3. `DispatchContext` struct: replaces the two-argument `HandDispatcher::dispatch` signature
4. Foreman restack step: after verifying a merge, restack all `InFlight`/`InReview` dependents
5. On restack conflict: block the dependent ticket with `BlockReason::RestackConflict`
6. `blocks_dependents` substrate method (reverse of the existing `blocks_predecessors`)
7. `auto_pr` support in `NativeStackBackend` + `CopilotHandDispatcher`: when `stacking.auto_pr: true`,
   open the PR immediately after pushing the branch
8. `derrick stack` subcommand: show, restack, submit
9. `derrick doctor` squash-merge warning (D21) when `stacking.backend != none`

**Out of scope:**
- Claude hand dispatcher (T015)
- TUI observe dashboard (T015)
- git-spice backend (stubs only)

## Acceptance criteria

- `cargo test --workspace` passes (≥ 80% coverage on new code)
- A batch with two tickets A→B (A blocks B) dispatches B off A's branch, not main
- Foreman tick, after A merges, rebases B's branch onto main and force-pushes
- Restack conflict blocks B with `BlockReason::RestackConflict` and records the
  `git rebase --onto` recipe in the activity log
- `derrick stack` shows the batch stack with PR status and restack health
- `cargo clippy --workspace -- -D warnings` clean; no `unwrap`/`expect`/`panic` in non-test code
