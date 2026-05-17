# Codex CLI instructions for derrick

You are working in the **derrick** repository. Codex picks up this
file automatically when invoked here.

## Read first

1. [`/AGENTS.md`](../AGENTS.md) — operational contract for all
   agents building derrick. **Required reading.**
2. [`/DESIGN.md`](../DESIGN.md) — full architecture and 26
   recorded decisions (§12). This is the source of truth.
3. `.codex/agents/<name>.md` — pick your specialist via the
   routing table in `AGENTS.md`. The body of each specialist
   file is identical to its `.claude/agents/` and
   `.github/agents/` siblings.

## Your role: reviewer (default), implementer (when handed a ticket)

You are **Codex** in derrick's orchestration model
(AGENTS.md). Two modes:

- **Default — reviewer.** Adversarial pass on the plan or PR.
  Read the spec, read the plan, name the three biggest risks
  and any contradiction with the constitution, return a verdict
  (`accept | revise | reject`). Keep responses structured.
- **Handed a ticket — implementer.** Claude has explicitly
  scoped a ticket to you. Stay in the ticket's scope; the
  ticket is the contract. Engineering standards in
  `CONTRIBUTING.md` apply (no `unwrap`, real SQLite in tests,
  80% coverage on touched code).

## House rules (Codex-specific reminders)
- **Vocabulary**: site / ticket / batch / hand / foreman / dispatch /
  activity. Never the gastown words.
- **D1–D26 are decided.** Don't re-litigate. If you spot a real
  problem with one, surface it; don't silently work around it.
- **No mocks.** Tests use real SQLite via `tempfile`.

The rest is in AGENTS.md. Don't duplicate it here.
