# Overnight orchestration log — 2026-05-17 → 18

User left around 23:30 with the instruction:
> Yeah, let's go. I'm happy for you to keep orchestrating across
> feature, and follow our loop to ensure we're not breaking CI
> and so on. We can review in the morning.

## Tickets shipped (in order)

| # | Title | Codex rounds | Copilot run | Crate coverage | Workspace coverage |
|---|---|---:|---:|---:|---:|
| T002 | `derrick-substrate` trait surface | 2 | 4m 44s | 83.11% | — |
| T003 | `derrick-scrub` subprocess output filter | 4 | 8m 8s | 96.38% | — |
| T004 | `derrick-caveman` prose compressor | 2 | 8m 49s | 90.71% | — |
| T005 | `derrick-memory` two-domain store + D9 gate | 3 | 16m 47s | 90.00% | — |
| T006 | `derrick-models` BYOM trait + shell provider | 2 | 7m 12s | 90.55% | — |
| T007 | `derrick-substrate-native` SQLite-backed CRUD | 3 | 10m 48s | 93.08% | 92.29% |
| T008 | `derrick-cli` minimal (init/status/doctor/run-stub) | 4 | 10m 59s | 80.56% | 90%+ |
| T009 | `derrick-tools` host CLI adapters | 2 | 12m 50s | 92.86% | — |
| T010 | `derrick-flow` pipeline orchestrator (final dogfooding-bar item) | 4 | 12m 57s | 82.26% | — |

All shipped under conventional commits, all CI runs green
(matrix: ubuntu-latest + macos-latest, plus rustfmt, clippy
-D warnings, coverage ≥80%, pre-commit).

## Design log

| # | Decision |
|---|---|
| D27 | (pre-overnight) Drop `site.role` and `pipeline[].role` for `runner: derrick` steps |
| D28 | (pre-overnight) Supersedes D1/D24 — GitHub-only distribution |
| D29 | (pre-overnight) Scrub and caveman fire at every model boundary, not just derrick's pipeline seams |

No new D entries this session — the assay loop caught design
drift in tickets before it reached implementation, so no
supersedes were needed.

## Two interventions worth flagging

1. **Pre-commit `end-of-file-fixer` fought T004's corpus.** The
   caveman ticket explicitly requires byte-exact fixtures for
   no-trailing-newline cases. The auto-fix added newlines and
   broke the contract. I excluded `crates/*/tests/corpus/` from
   both `trailing-whitespace` and `end-of-file-fixer` in
   `.pre-commit-config.yaml`. Documented in the T004 commit body.

2. **Two trivial clippy fixes after T007 landed.** Copilot's
   first pass used `&PathBuf` instead of `&Path` in a test
   helper and left a now-orphaned `PathBuf` import; the lib-test
   clippy gate caught it. Two-line fix, folded into the T007
   commit before push.

## Orchestration loop performance

- **Codex rounds per ticket**: 2–4. Median 2. T003 (4 rounds)
  and T001 (5 rounds, pre-overnight) suggest the rule shape
  for new spec patterns is the rounds driver — once a pattern
  is established (e.g. shell-provider style for T006, trait+
  CRUD style for T007 after T002), assays converge faster.
- **Worst single-codex-finding**: T007 round 1's "ForemanMode
  write hole" — codex noticed the T002 trait had no
  write API for `Attached` vs `Detached` despite the type
  existing. Resolved by deferring mode column to T008 (foreman
  loop ticket, not yet drafted).
- **Best single-codex-catch**: T002 round 1's `Batch ordering`
  finding — T002 spec called for "ordered" batches but didn't
  carry an ordinal anywhere. Fix landed before any code was
  written. T003 round 1's catch of `Action::Replace(String)`
  being too weak for capture-group replacement was similar.
- **Copilot quality**: clean across all 6 runs. Two minor
  post-hoc fixes (T007 clippy, T002 vocabulary fixture
  `polecat-1` → `hand-1`) but no test failures, no scope
  leaks, no design drift.

## Dogfooding bar progress

Per AGENTS.md, the bar is `T001` + `T002` + `T007` + a minimal
`derrick-cli` + `derrick-flow`.

| Bar item | Status |
|---|---|
| `derrick-config` (T001) | ✅ Shipped pre-overnight |
| `derrick-substrate` trait (T002) | ✅ |
| `derrick-substrate-native` (T007) | ✅ |
| `derrick-cli` minimal (T008) | ✅ |
| `derrick-flow` minimal (T009) | ⏳ NOT drafted yet |

**All four bar items shipped.** T009 (derrick-tools) and
T010 (derrick-flow) landed on the second overnight stretch
after the user picked option A (split derrick-flow + derrick-
tools) and option A (dogfood on first real artifacts) in the
morning review.

End-to-end smoke verified against the built binary with
mocked claude/codex hosts: `derrick run add-feature` walks
the full spec→clarify→plan→checkpoint→assay→analyze→tasks
pipeline, writes a complete run manifest with config-hash
and skip flags, lands real spec.md / plan.md / tasks.md on
disk. The pipeline orchestrator is live.

```
$ derrick init                                  # exits 1, T011 pointer (correct)
$ derrick init --greenfield --site smoke ...    # writes yaml + opens substrate
$ derrick status                                # reads substrate, prints state
$ derrick run add-feature --prompt "..."        # exits 1, T009 pointer (correct)
```

The `derrick` binary works, it's wired through the substrate,
config and memory layers are real, scrub + caveman are real
and tested, and the only missing piece is "actually execute
the pipeline" — which is T009.

## What's NOT shipped (deliberately deferred)

- T010 foreman loop (extends T007's foreman table). Needed
  for `crew` mode.
- T011 `derrick-adopt` (brownfield init + host hooks per D29).
- T012 `derrick-stack` (PR stacking).
- T013 `derrick-copilot` (Copilot hand impl).
- T014 `derrick-tui` (observe dashboard).
- T015 `derrick-observe` (read aggregator).
- T016 `derrick-tools` (host CLI shims for assay).
- T006a anthropic provider impl (currently a stub).

None of these block dogfooding. They block crew-mode +
brownfield + Copilot dispatch + TUI + full BYOM.

## Recommended morning agenda

1. Skim the commit log (`git log --oneline` — ~18 commits
   added overnight).
2. Check `gh run list` — last 8 runs all green.
3. Try the smoke test against the built binary
   (`cargo build -p derrick-cli`; then `derrick --version`,
   `derrick init` (refuses with T011 pointer),
   `derrick init --greenfield --site <name>` in a temp dir).
4. Read `DESIGN.md §12` decision table — no new D entries
   tonight, but you can see the existing 29.
5. Decide T009 scope before drafting:
   - Option A: T009 derrick-flow + lightweight inline host
     shells (just enough to call `claude`/`codex` from the
     pipeline) — single ticket, gets to dogfooding fastest.
   - Option B: T009 derrick-flow minimal (pipeline state
     machine only, no real host calls — writes placeholder
     artifacts), then T010 derrick-tools (host adapters),
     then T011 wire T009→T010. Three tickets, cleaner
     separation but slower.

   My lean: A. The host shells are small (~50 LOC each for
   claude/codex), and bundling them into T009 keeps the
   dogfooding cycle short. We can always extract to
   derrick-tools later when it grows.

6. After T009 lands and works against the substrate end-to-
   end (run `derrick run add-feature` and watch it produce
   real `spec.md` / `plan.md` / `assay/verdict.md` /
   `tasks.md`), the dogfooding bar is met. I write up the
   switch proposal and we run the first
   `/add-feature` against derrick itself.

## Tickets queued for codex review

None pending. T008 is the last item codex reviewed; it
accepted on round 4. Drafting T009 is the explicit morning
decision (per agenda item 5).
