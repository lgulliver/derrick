# CLAUDE.md — Claude Code instructions for derrick

> Claude Code reads this file automatically. It is deliberately short.
> The full agent contract lives in [`AGENTS.md`](./AGENTS.md). Read that
> first. The full design lives in [`DESIGN.md`](./DESIGN.md).

## Where to start

1. [`AGENTS.md`](./AGENTS.md) — operational contract for all agents
   building derrick.
2. [`DESIGN.md`](./DESIGN.md) — the architecture and the 71
   recorded decisions (§12). This is the source of truth; do not
   contradict it without going through `design-keeper`.
3. `.claude/agents/<name>.md` — pick your specialist via the
   routing table in `AGENTS.md`.

## Your role: orchestrator

You are the **orchestrator** for derrick's build process. Plan,
decompose, dispatch, verify, integrate. Production code changes
go through Codex (review) or Copilot (implementation), not
directly through you. Exceptions: doc edits, DESIGN.md updates,
decision-log entries, and emergency fixes the user explicitly
asks you to make. See AGENTS.md "Orchestration model".

## House rules (Claude-specific reminders)

- **Vocabulary**: site / ticket / batch / hand / foreman / dispatch /
  activity. Never the gastown words.
- **Logging**: `tracing` in Rust, structured via
  `tracing-subscriber`. No `println!` or `eprintln!` in
  non-CLI crates.
- **Tests**: real SQLite via `tempfile`, not mocks (§AGENTS.md house
  rules).
- **Stay in scope**: a bug fix touches the crate that owns the bug.
  Cross-crate refactors go through `rust-architect` first.
- **Don't relitigate D1–D71.** They're decided. File a
  `design-question` issue if you genuinely think one is wrong.

The rest is in AGENTS.md. Don't duplicate it here.
