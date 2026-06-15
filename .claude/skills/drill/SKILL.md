---
name: drill
description: Run the full dark-factory feature pipeline for a prompt — spec, plan, clarify, adversarial assay, ticket creation, and foreman dispatch. Use when the user wants to build a feature end-to-end with derrick. Triggers include "/drill", "derrick drill", "build the X feature", or a natural-language feature description handed to derrick.
---

# Drill — the feature pipeline

`/drill <prompt>` drives the entire dark-factory pipeline for one
feature description: spec → plan → clarify → assay → tickets →
foreman dispatch. The slash command is intentionally thin — it just
resolves to `derrick run drill --prompt "..."` — and defers here for
the workflow narrative. The user never sees the underlying tools;
all output and questions are in **derrick's voice**.

See DESIGN.md §5.3 for the authoritative flow.

## When to use

- The user describes a feature and wants derrick to build it.
- The user types `/drill <prompt>` or runs `derrick drill "<prompt>"`.
- A halted run needs resuming (see Resume below).

## Invocation

```
derrick drill "build a webhook ingest endpoint with idempotent dedupe"
```

Or via Claude Code:

```
/drill build a webhook ingest endpoint with idempotent dedupe
```

The positional prompt is shorthand; `derrick run drill --prompt "..."`
is the canonical underlying form. `add` is a hidden, deprecated alias
for `drill` and the runner still accepts the legacy `"add-feature"`
`pipeline_id` so older run manifests stay resumable (D64).

## The eight stages

The pipeline runs eight stages in sequence. Each stage that surfaces
output does so in a clean, styled terminal UI. Stages that block on
user input wait indefinitely; autonomous stages show a spinner then a
summary. Every step logs to `.derrick/runs/<utc-ts>/step-<id>.log`.

1. **Specify** — runs `speckit specify` internally, then shows a clean
   summary of what derrick understood. Waits for an explicit yes/no;
   on no, takes a free-text correction and re-runs specify.
2. **Plan** — runs `speckit plan`; shows a brief human-readable plan
   summary. No confirmation here — the assay (stage 5) is the gate.
3. **Clarify** — runs `speckit clarify` interactively, streaming
   questions to the terminal and capturing answers in-session.
4. **Apply clarifications** — an in-process step that folds the
   answers into the plan and shows a diff-style summary of the change.
5. **Assay** — the adversarial review gate. One or more reviewers
   critique the plan; the pipeline refuses to proceed past an open
   blocking finding.
6. **Summary and confirmation** — derrick presents the final plan and
   asks the user to confirm before any tickets are created.
7. **Task and agent generation** — decomposes the plan into tickets
   in a batch on the substrate.
8. **Begin work (detachable)** — the foreman dispatches tickets to
   hands. Detachable: the user can leave and re-attach with
   `derrick observe` / `derrick status`.

## Failure and resume

Failure of any step halts the pipeline with a numbered error and the
exact resume command:

```
derrick run drill --resume-from <step>
```

When a prompt is re-run without `--resume-from` or `--force`, drill
normalises the prompt to a stable key and auto-resumes a matching
incomplete run rather than starting fresh (avoids ticket-ID
collisions). `--force` starts a brand-new run and records
`resume_of: <old_run_id>` for lineage.

## Skip flags

Stages can be skipped per-run: `--no-clarify`, `--no-assay`,
`--no-github-issues`, and the general `--skip <step>` / `--unskip
<step>`. `--dry-run` previews every state-mutating step without
committing it.

## What this skill does NOT do

- Replace `derrick init`. The repo must be initialised first.
- Add code itself — it orchestrates the host CLIs and the foreman.

## Reference

- DESIGN.md §5.3 — `derrick drill`, the feature pipeline.
- DESIGN.md §5.2 — `derrick init` (must run first).
- D64 — the `add` → `drill` rename and deprecated aliases.
- `.claude/commands/drill.md` — the thin command that defers here.
