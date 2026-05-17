# AGENTS.md — the contract for agents building derrick

> Read this first. Then your specialist file under `.claude/agents/`,
> `.codex/agents/`, or `.github/agents/` depending on where you live.

## What derrick is

Derrick is a unified front door over **speckit**, an in-process
**assay** (adversarial plan review), and a derrick-native
**execution substrate**. One install, one config (`derrick.yaml`),
one primary command (`/add-feature`).

The full design is in [`DESIGN.md`](./DESIGN.md). It is the source
of truth. Twenty-six decisions are recorded in §12 as **D1–D26**;
do not re-litigate them without filing a `design-question` issue
and updating §12 with a new `D` entry.

## Architectural pillars (§9)

Every change is checked against these. If you can't make it
memory-aware, token-efficient, and parallel-safe, don't ship it.

- **Memory** — derrick seeds and curates persistent memory so the
  assistant doesn't relearn the rig every turn (§9.A).
- **Tokens** — every byte across a model boundary earns its place
  via tiering, scrubbing, caveman compression, prompt caching,
  lazy artifact loading, transcript-parsed telemetry (§9.B).
- **Parallelism** — independent work runs concurrently by default;
  serial work is a justified exception (§9.C).

## Workspace (§3.1)

Cargo workspace, one binary (`derrick`), many crates. Each crate
has a specialist agent (`.claude/agents/<name>.md`). The
specialist agents own their crate; cross-crate changes go
through `rust-architect`.

## Orchestration model — how derrick gets built

Derrick is built using its own pattern. Three roles, three hosts:

| Role | Host | What it does |
|---|---|---|
| **Orchestrator** | Claude (Claude Code) | Reads designs, picks the specialist, decomposes work into tickets, dispatches to implementers, verifies results, runs tests, updates `DESIGN.md`. **Does not write production code itself.** |
| **Reviewer** | Codex (`codex` CLI) | Adversarial pass on plans and PRs before merge. Different-family scrutiny per the assay pattern. May also implement assigned tickets when explicitly handed one. |
| **Implementer** | GitHub Copilot (`copilot` CLI) | Writes code for individual tickets. Lives at the leaf of the dispatch tree. |

Practical contract:

- **If you are Claude**, your job is orchestration. Plan the work
  against the relevant specialist contract, write the ticket /
  brief, hand it to Codex (for review) or Copilot (for
  implementation), then verify and integrate the result.
  Production code changes pass through Codex or Copilot, not
  through you directly. Exceptions: trivial doc tweaks, DESIGN.md
  updates, decision-log entries, and emergency fixes the user
  explicitly asks you to make.
- **If you are Codex**, your default role is adversarial review.
  You may also be handed a ticket to implement; when you are,
  the ticket scopes the work.
- **If you are Copilot**, your default role is implementation
  against a ticket Claude has dispatched. Stay in scope; the
  ticket is the contract.

Engineering standards (style, SOLID, DRY, coverage) are in
[`CONTRIBUTING.md`](./CONTRIBUTING.md).

## House rules

1. **Vocabulary**: derrick speaks site / ticket / batch / hand /
   foreman / dispatch / activity. Never re-introduce gastown's
   words (rig / bead / convoy / polecat / mayor / sling / trail).
2. **No external runtime dependencies beyond SQLite.** No daemon,
   no port, no server-side state.
3. **Hosts own their own context.** When derrick invokes claude /
   codex / copilot, those hosts load their own AGENTS.md,
   sub-agents, skills, hooks. Derrick passes the cwd and the
   prompt; it does not inject system prompts (§6.5).
4. **The substrate trait is the only contract.** Don't reach into
   SQLite from outside `derrick-substrate-native`. Don't reach into
   the `Substrate` trait from anywhere except by going through it.
5. **No mock databases.** Use a real SQLite file in tests via
   `tempfile`. Mocks have lied to us before; the substrate is too
   load-bearing to fake.
6. **Stay in scope.** A bug fix or feature stays inside the crate
   that owns it. Cross-cutting changes ("rename a type", "add a
   field everywhere") get a planning pass through
   `design-keeper` first.
7. **DESIGN.md is the rulebook, CLAUDE.md is a pointer.** All
   substantive rules live in DESIGN.md so every host (Claude,
   Codex, Copilot) reads the same content. AGENTS.md (this file)
   is the operational contract.

## Specialist routing

Pick the right specialist before starting work:

| Domain | Specialist | Owns |
|---|---|---|
| Rust idioms, workspace, traits, perf | `rust-architect` | Cross-crate Rust concerns |
| SQLite schema, foreman loop, ticket model, worktrees | `substrate-engineer` | `derrick-substrate*` |
| Pipeline state machine, step runner, run manifests | `flow-engineer` | `derrick-flow` |
| Brownfield detection, init flow, speckit detect-then-defer | `flow-engineer` | `derrick-adopt`, `derrick-config` |
| Host CLI adapters, BYOM providers, agent rule respect | `integrations-engineer` | `derrick-tools`, `derrick-models`, `derrick-copilot` |
| Scrubber, caveman, memory, telemetry, prompt caching | `token-economist` | `derrick-scrub`, `derrick-caveman`, `derrick-memory` |
| PR stacking, branches, restack, gh/graphite/git-spice | `git-stacker` | `derrick-stack` |
| ratatui dashboard, file watcher, terminal UX | `tui-engineer` | `derrick-tui` |
| Assay logic, codex prompting, multi-reviewer reconciliation | `integrations-engineer` (assay role) | `derrick-assay` |
| Tests, fixtures, integration harness | `test-engineer` | Test code anywhere |
| DESIGN.md, decision log, open questions | `design-keeper` | `DESIGN.md` only |

Cross-crate changes route through `rust-architect` (technical) and
`design-keeper` (intent recorded).

## Stop conditions

Stop and ask the human via `derrick mail --human`
(crew mode) or print an explicit message (solo/copilot mode) when:

- A change would violate a D entry in §12.
- A change would alter the substrate schema in a non-migration-
  safe way.
- A test cannot be written for a change. (Untestable change → no
  change.)
- The assay reviewer rejects the same plan twice.
- The host's own AGENTS.md / CLAUDE.md / hooks would need
  derrick-side coordination to work. We don't coordinate; the
  host's rules stand.

## How to find your file

- Claude Code: `.claude/agents/<your-name>.md`
- Codex: `.codex/agents/<your-name>.md` (also referenced from `.codex/instructions.md`)
- GitHub Copilot: `.github/agents/<your-name>.md` (also referenced from `.github/copilot-instructions.md`)

The body is identical across hosts. Only the frontmatter format
differs (Claude Code uses YAML; Codex and Copilot use plain
markdown with the same fields).
