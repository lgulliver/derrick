# T013 — Fix `derrick-adopt` detect→apply TOCTOU (OQ6)

**Specialist owner**: `flow-engineer` (per `AGENTS.md` routing table)
**Crate**: `crates/derrick-adopt`
**Depends on**: nothing
**Priority**: P2 — correctness edge case, low likelihood, non-corrupting

## Why

`derrick-adopt` snapshots the detected on-disk state (hooks,
`.mcp.json`, `.claude/settings.json`) at **detect** time, then
merges against that snapshot at **apply** time without re-reading
disk before promotion. Any external edit to those files between
detect and apply is silently lost or merged stale.

This is a time-of-check-to-time-of-use (TOCTOU) gap. Recorded as
**OQ6** in `DESIGN.md §5.6`. The greenfield twin re-reads at write
time; the brownfield adopt path does not.

## Scope

Add a detect→merge revalidation pass in the adopt flow:

- Before promotion, re-read the files that were snapshotted at
  detect time (or hash them at detect time and compare at apply
  time).
- If a snapshotted file changed since detect, either re-run the
  merge against the current content or abort with a clear
  "files changed since detection, re-run `derrick init`" message.
- Also clean up stale `.derrick/.adopt-stage-*` dirs on partial
  failure (existing TODO in `crates/derrick-adopt/src/lib.rs`).

Out of scope: any behaviour change to the greenfield path.

## Acceptance

- A test that edits a snapshotted file between detect and apply
  and asserts the change is not silently lost (either picked up
  or the apply aborts with the clear message).
- Stale adopt-stage dir cleanup covered by a test.
- Real filesystem via `tempfile`, no mocks.

## Notes

Surfaced by the specialist review sweep. See `DESIGN.md §5.6`
"Known gap (OQ6)".
