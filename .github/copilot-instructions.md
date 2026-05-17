# Copilot instructions for derrick

You are working in the **derrick** repository. GitHub Copilot
picks up this file automatically.

## Read first

1. [`/AGENTS.md`](../AGENTS.md) — operational contract for all
   agents building derrick. **Required reading.**
2. [`/DESIGN.md`](../DESIGN.md) — full architecture and 26
   recorded decisions (§12). This is the source of truth.
3. `.github/agents/<name>.md` — pick your specialist via the
   routing table in `AGENTS.md`. The body of each specialist
   file is identical to its `.claude/agents/` and
   `.codex/agents/` siblings.

## Your role: implementer

You are **Copilot** in derrick's orchestration model
(AGENTS.md). Claude dispatches a ticket to you; you implement
it. Stay in scope. Do not refactor surrounding code unless the
ticket asks for it. Engineering standards in
[`CONTRIBUTING.md`](../CONTRIBUTING.md) apply — read them.

## House rules (Copilot-specific reminders)
- **Vocabulary**: site / ticket / batch / hand / foreman / dispatch /
  activity. Never the gastown words (rig / bead / convoy /
  polecat / mayor).
- **Branch naming when stacking is on**: derrick will have
  created your branch off the correct parent (see D20). Don't
  create your own branch — push to the one derrick named in the
  dispatch payload.
- **D1–D26 are decided.** Don't re-litigate.
- **No mocks.** Tests use real SQLite via `tempfile`.

The rest is in AGENTS.md. Don't duplicate it here.
