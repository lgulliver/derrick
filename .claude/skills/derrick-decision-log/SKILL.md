---
name: derrick-decision-log
description: Record a new architectural decision in DESIGN.md §12. Use when locking in a choice that has been open, when superseding an existing decision, or when promoting an open question to a resolved decision. Triggers include "record a decision", "lock this in", "add D entry", "decision log", or natural-language equivalents.
---

# Derrick Decision Log

Append a new `D#` entry to DESIGN.md §12 (Decisions taken)
without breaking the format. Update or remove the corresponding
open question if there was one.

## When to use

- A choice that has been an open question is now resolved.
- A new constraint emerges mid-implementation that future PRs need
  to respect.
- An existing `D` entry is being superseded.

## Format

Each entry is one row in the existing table:

```
| D## | **Title**: one or two sentences describing the decision. | §section |
```

- `D##` is the next integer (D27, D28, …). Never reuse a number
  even if a decision is superseded.
- **Title** is bold, ≤8 words, captures the essence.
- Body is one or two sentences. Reference the chosen approach,
  not the alternatives.
- `§section` points at the load-bearing locus in DESIGN.md.
- A superseding decision body begins *"Supersedes D##."* and
  explains the shift.

## Process

1. Read DESIGN.md §12 to find the next free `D` number.
2. Locate the affected section in DESIGN.md. If it doesn't reflect
   the decision yet, update it first (the prose, not the table).
3. Add the new row to the §12 decisions table.
4. If the decision resolves an open question, remove the question
   from the "Remaining open questions" subsection.
5. Commit with a message in the form
   `design: D## — <title>` so the decision is greppable in git
   history.

## What this skill does NOT do

- Edit existing `D` entries. They are immutable except for typo
  fixes (handled by direct edit, not this skill).
- Add code. Pure documentation skill.
- Add open questions. New open questions are added directly to
  §12 by `design-keeper` without using this skill.

## Reference

- DESIGN.md §12 — Decisions taken & remaining open questions.
- AGENTS.md — house rule 7 (DESIGN.md is the rulebook).
- `.claude/agents/design-keeper.md` — the agent that uses this
  skill most often.
