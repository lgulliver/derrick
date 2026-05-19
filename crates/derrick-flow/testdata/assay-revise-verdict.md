model: claude
reviewer: reviewer
round: 1
verdict: revise

Now I have the full picture. Let me render the review.

---

## Speckit Plan Review — Automatic Version Bumping from Git Tag

### Highest Risks

**1. Clap won't read `DERRICK_VERSION` automatically (critical)**

The plan says Step 2 will "replace any hardcoded version reference" in `main.rs`, but the version is not in `main.rs`. It lives in `commands/mod.rs:21-24` as clap's bare `version` attribute:

```rust
#[command(name = "derrick", version, about = "Derrick orchestration CLI")]
```

The bare `version` attribute causes clap to read `CARGO_PKG_VERSION` (the compile-time string from `Cargo.toml`), not `DERRICK_VERSION`. Build.rs emitting `cargo:rustc-env=DERRICK_VERSION=...` does not affect what clap reads unless you explicitly override it with:

```rust
#[command(name = "derrick", version = env!("DERRICK_VERSION"), ...)]
```

This is a silent correctness failure: the binary would compile and `derrick --version` would still print `0.0.0`. The plan has the right idea but targets the wrong file and doesn't explain the mechanism override needed.

**2. `telemetry.rs` is the wrong file (significant)**

**3. Cargo incremental rebuild reliability (moderate)**

---

### Missing Edge Cases

**A. No `v*`-matching tag, but other tags exist**

**D. Tests for `derrick version` sub-command not planned**

The plan introduces `commands/version.rs` — new executable code with public functions. The constitution requires "every new public function gets at least one test before merge" and "minimum 80% line coverage". No test file or test strategy is mentioned for this new command. This is a constitution violation, not just a gap.

---

### Constitution Contradictions

**1. Missing test coverage plan (hard violation)**

CONTRIBUTING.md §Testing: *"Every new public function gets at least one test before merge."*
CONTRIBUTING.md §Coverage: *"No skeleton-only commits. Any commit that adds executable Rust code must also add the tests that keep the workspace above 80%."*

The plan adds `commands/version.rs` (new command handler with executable bodies) and a new `build.rs` with parsing logic, but proposes no tests for either. Acceptance criterion AC6 (`cargo test passes with no regressions`) is passive — it requires tests to *not break*, not to cover the new code. The build.rs parsing logic in particular (splitting on `-` from the right, stripping `v`, distance detection) is exactly the kind of logic that warrants unit tests and is straightforward to test with hardcoded input strings.

**2. Step 2 directs changes to `main.rs` (factual error, not strictly a constitution issue)**

---

## Verdict

revise

The core technical approach (hand-rolled `build.rs`, `cargo:rustc-env`, right-split on `-`, fallback to sentinel) is sound. The risks table is thoughtful. However, two factual errors in the step list (wrong file for version attribute; wrong `telemetry.rs` purpose) would cause a Copilot implementation to fail silently or modify the wrong files, and the absence of any test plan contradicts a hard constitution rule. A revised plan needs: corrected file targets for Steps 2 and 4, an explicit `#[command(version = env!("DERRICK_VERSION"))]` override in the implementation guidance, and a Step for tests covering the `build.rs` parser logic and the new `version` sub-command.
