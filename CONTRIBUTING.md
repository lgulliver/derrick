# Contributing to derrick

Derrick is MIT-licensed and welcomes contributions. This file is the
practical guide. The architectural contract is in
[`AGENTS.md`](./AGENTS.md); the design is in [`DESIGN.md`](./DESIGN.md).

## Orchestration model — how derrick gets built

Derrick is built using its own pattern. Three roles, three hosts:

| Role | Host | What it does |
|---|---|---|
| **Orchestrator** | Claude (Claude Code) | Reads designs, picks the specialist, decomposes work into tickets, dispatches to implementers, verifies results, runs tests, updates `DESIGN.md`. **Does not write production code itself.** |
| **Reviewer** | Codex (`codex` CLI) | Adversarial pass on plans and PRs before merge. Different-family scrutiny per the assay pattern. May also implement assigned tickets when explicitly handed one. |
| **Implementer** | GitHub Copilot (`copilot` CLI) | Writes code for individual tickets. Lives at the leaf of the dispatch tree. |

In practice: a contributor opens Claude Code, asks for a feature; Claude
plans (against the relevant specialist's contract under
`.claude/agents/`); the plan goes through assay (Codex review); approved
tickets are dispatched to Copilot; Claude verifies merges and updates
DESIGN.md if a decision was taken.

Humans are always welcome to play any of these roles directly. The
contract is the same.

## Engineering standards

### Code style

- **Rust 2021**, MSRV `1.75`.
- `cargo fmt` is enforced; CI fails on diff. Config in
  [`rustfmt.toml`](./rustfmt.toml).
- `cargo clippy -- -D warnings` is enforced; CI fails on any warning.
  Workspace lints in [`Cargo.toml`](./Cargo.toml).
- No `unwrap()` or `expect()` in non-test code. Use
  `anyhow::Context` at boundaries, typed `thiserror::Error` inside
  crates.
- No `println!` / `eprintln!` outside `derrick-cli`. Use `tracing`.
- No `unsafe` without an inline comment justifying it and a test
  that would catch UB if removed.

### SOLID applied to derrick

- **Single responsibility** — one crate per concern (see the
  `crates/` layout). One module per public type. Resist
  "utils.rs" — name what it does.
- **Open/closed** — extension through traits (`Substrate`,
  `Model`, `StackBackend`, `Hand`), not by editing existing
  implementations. Adding a new model provider means a new file
  under `crates/derrick-models/src/providers/`, not a `match`
  arm edit.
- **Liskov** — every implementor of a trait passes the trait's
  conformance test suite unchanged. Conformance tests live in
  the trait's defining crate.
- **Interface segregation** — narrow traits with one or two
  methods, not god-traits. Prefer composition.
- **Dependency inversion** — depend on traits, not concrete types.
  Crates depend on `derrick-substrate` (the trait crate), not on
  `derrick-substrate-native`.

### DRY

- If you find yourself writing the same 6+ lines twice, factor it.
- *But*: AGENTS.md rule 6 — stay in scope. A bug fix is not a
  refactor. Cross-crate DRYing goes through `rust-architect`.

### Testing

- **Real SQLite** in tests via `tempfile::tempdir()`. No mocks for
  the substrate (AGENTS.md house rule 5).
- **No network in unit tests.** Network-touching tests go in
  `tests/integration/` and are gated by env var.
- **Host CLIs mocked at the process boundary** — a tiny shell
  script the test PATH picks up. Saves CI from real model APIs.
- Every new public function gets at least one test before merge.
- Every bug fix gets a regression test (fails before, passes
  after).
- Test naming: `<module>::<scenario>_<expected>`. Read like
  sentences.

### Coverage

- **Minimum 80% line coverage** across the workspace. CI fails
  below this.
- Measured via `cargo llvm-cov --workspace --lcov`.
- Local: `cargo install cargo-llvm-cov && cargo llvm-cov
  --workspace --html`.
- Coverage is a floor, not a ceiling. Critical paths (substrate,
  foreman, stack restack) should be closer to 100%.

## Decision log process

Architectural decisions are recorded in DESIGN.md §12 as
`D1`, `D2`, … entries. They are **immutable** once committed.

- Resolving an open question → add a new `D` entry, remove the
  question from §12.
- Superseding a prior decision → add a new `D` entry whose body
  begins *"Supersedes D##."* and explains the shift.
- Use the `derrick-decision-log` skill (`.claude/skills/`) to add
  entries without breaking the table format.

Open questions: file as a GitHub issue with the
`design-question` label. Don't add them to DESIGN.md until
they have a leaning.

## Conventional Commits

All commits follow [Conventional Commits 1.0.0](https://www.conventionalcommits.org/).
The CI commit-message hook (and the local pre-commit hook) reject commits
that don't.

Format:

```
<type>(<scope>): <description>

[optional body]

[optional footer(s)]
```

Allowed types:

| Type | Use |
|---|---|
| `feat` | A new user-visible feature. |
| `fix` | A bug fix. |
| `docs` | Documentation only (including `DESIGN.md` updates). |
| `test` | Adding or fixing tests; no production-code change. |
| `refactor` | Code change that is not a feature or fix. |
| `perf` | Performance improvement. |
| `build` | Build system, Cargo, or workspace changes. |
| `ci` | CI configuration changes. |
| `chore` | Housekeeping (deps bumps, file moves, gitignore). |
| `style` | Formatting only; no semantic change. |
| `revert` | Reverts a prior commit (body must include the reverted SHA). |

Scopes match crate names where applicable: `config`, `flow`,
`substrate`, `assay`, `tui`, `stack`, `models`, `scrub`,
`caveman`, `memory`, `cli`, `copilot`, `adopt`, `observe`,
`tools`, `agents` (when changing `.claude/agents/`), `design`
(when changing `DESIGN.md`), or omit when the change is
workspace-wide.

A footer of `BREAKING CHANGE: <description>` (or `!` after the
type) marks a change that breaks downstream consumers — e.g.
`feat(config)!: drop site.role field` would have been the
right message for D27 had we written it under the convention.

Examples:

- `feat(config): add layered yaml load with merge semantics`
- `fix(stack): bail on restack conflict per D19`
- `docs(design): record D27 dropping site.role`
- `test(scrub): cover rtk-equivalent rules for gt and gh`
- `ci: add coverage gate at 80%`
- `chore: bump tokio to 1.42`

Ticket IDs go in the footer, not the subject:

```
feat(substrate): implement Hand trait

Implements §8.2 with claude / copilot / human variants.

Refs: T012
```

## Pull requests

- One concern per PR. The smaller the PR, the faster the review.
- PR title and body reference the ticket id and DESIGN.md section
  it touches.
- CI must be green. Don't ask for review on a red PR.
- A passing assay (Codex review) is recorded by the orchestrator;
  human reviewers see the assay verdict alongside the diff.
- Stacked PRs are normal — see DESIGN.md §8.5. Use the native
  backend, Graphite, or git-spice depending on your tooling.

## Reporting bugs

Open a GitHub issue. Include:

- `derrick --version` output.
- `derrick doctor` output (in a fenced code block).
- Steps to reproduce.
- Expected vs actual.

For security issues, see [`SECURITY.md`](./SECURITY.md) (TBD —
use GitHub's private vulnerability reporting at
`https://github.com/lgulliver/derrick/security/advisories/new`).
