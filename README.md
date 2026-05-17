# derrick

> The load-bearing tower over an oil well. The structure that lifts every
> length of pipe in and out of the hole.

`derrick` is the front door for AI-assisted feature development in a repo.
One install, one config (`derrick.yaml`), one command (`/add-feature`)
that drives the full dark-factory pipeline — spec, plan, adversarial
review, tickets, dispatch — without making the user wire each underlying
tool by hand.

It wraps [speckit](https://github.com/github/spec-kit) for the
spec-to-tasks workflow and ships its own native substrate, adversarial
plan review (assay), PR stacking, observability TUI, scrubber, and
text compressor. No external server, no daemon — one Rust binary plus
SQLite.

**Status:** early development. Architecture and 29 recorded decisions
in [DESIGN.md](./DESIGN.md). Foundation crate (`derrick-config`,
typed schema + layered loader + 14 validation rules, 93% line
coverage) has landed; the rest of the workspace is scaffolded and
under active implementation.

## What it will do

```
$ curl -fsSL https://raw.githubusercontent.com/lgulliver/derrick/main/scripts/install.sh | bash
$ cd ~/repos/my-project && derrick init
$ # then in Claude Code:
$ /add-feature build a webhook ingest endpoint with idempotent dedupe
```

That single slash command walks the pipeline:

```
spec → clarify → plan → checkpoint → assay → analyze →
tasks → bridge (tasks-to-tickets) → foreman (dispatch hands)
```

Each step is configurable in your repo's `derrick.yaml`. Steps can be
skipped per-invocation (`--no-clarify`, `--no-checkpoint`, `--no-assay`,
`--dry-run`) or removed from your pipeline entirely.

## Why

Three architectural pillars (DESIGN.md §9):

- **Memory** — derrick seeds and curates persistent agent memory so
  the assistant doesn't relearn the rig every turn.
- **Tokens** — every byte across a model boundary earns its place
  via model tiering, scrubbing, caveman compression, prompt caching,
  lazy artifact loading, and transcript-parsed telemetry. Scrub and
  caveman fire at every model boundary, including host tool calls
  (Claude Code, Codex) via hooks (D29).
- **Parallelism** — independent work runs concurrently by default
  via git worktrees per pipeline run and convoy fan-out across
  workers (hands).

Plus first-class concerns: BYOM (bring your own model — anthropic,
openai, gemini, bedrock, azure-openai, ollama, copilot-cli),
brownfield-safe `derrick init` that adopts rather than clobbers,
PR stacking native/Graphite/git-spice, and a ratatui dashboard
(`derrick observe`).

## Read next

- [DESIGN.md](./DESIGN.md) — full architecture, pipeline schema,
  install flow, and the 29 decisions (§12).
- [AGENTS.md](./AGENTS.md) — operational contract for agents
  building derrick (Claude orchestrates, Codex reviews, Copilot
  implements).
- [CONTRIBUTING.md](./CONTRIBUTING.md) — engineering standards
  (SOLID, DRY, 80% coverage, conventional commits) and the PR
  workflow.

## License

MIT. See [LICENSE](./LICENSE).
