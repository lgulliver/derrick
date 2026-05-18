# T009 — `derrick-tools` host CLI adapters

**Specialist owner**: `integrations-engineer` (opus)
**Crate**: `crates/derrick-tools`
**Depends on**: nothing in our workspace (pure subprocess shell)
**Priority**: P0 — blocks T010 (`derrick-flow`), which is the last dogfooding-bar item.

## Why

D30 locks the split: `derrick-tools` owns host CLI
subprocess invocations (claude / codex / copilot called as
hosts via pipeline `host:` steps); `derrick-models` owns the
`Model` trait and providers (anthropic / openai / openai-cli
/ etc. — completion-shaped). Same binary may be reached via
either path through different invocation shapes. This crate
is the host-side.

When `derrick-flow` encounters a pipeline step with
`host: claude` (or codex / copilot) it needs to shell to
that host's CLI with a prompt and capture the response.
That subprocess-and-capture path is what this crate provides.

**Important contract** from DESIGN.md §6.5 / D30: derrick
passes **cwd and prompt only** (plus a small tool-permission
knob for Copilot specifically — see below). It does **not**
inject system prompts, override AGENTS.md, override the
host's model selection, or bypass the host's own rule
loading. The host CLI loads its own context (CLAUDE.md,
AGENTS.md, .claude/agents/, etc.) and uses its own
configured default model.

## Scope (v1)

### Public API

```rust
//! Host CLI adapters: claude, codex, copilot.
//! See DESIGN.md §6.5.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

/// One host the orchestrator can invoke. Implementations live
/// under `crates/derrick-tools/src/hosts/<name>.rs`.
#[async_trait::async_trait]
pub trait HostAdapter: Send + Sync {
    /// Human-readable name: "claude", "codex", "copilot".
    fn name(&self) -> &str;

    /// Returns whether the host binary is on PATH and looks
    /// invocable. Used by `derrick doctor`.
    fn is_available(&self) -> bool;

    /// Invoke the host CLI with the given request. Captures
    /// stdout/stderr; kills on timeout; surfaces nonzero
    /// exits as `HostError::NonZeroExit`.
    async fn run(&self, request: HostRequest) -> Result<HostResponse, HostError>;
}

#[derive(Clone, Debug)]
pub struct HostRequest {
    /// The prompt to send. For claude this is the literal
    /// argument to `--print` (typically a slash command
    /// like `/speckit.specify ...`). For codex it's the
    /// non-interactive prompt to `codex exec`. For copilot
    /// it's the `--prompt` value.
    pub prompt: String,
    /// Working directory the host CLI runs in. derrick-flow
    /// sets this to the worktree path (when worktrees land
    /// in T012) or the repo root.
    pub cwd: PathBuf,
    /// Wall-clock kill timeout. Hosts may take a long time;
    /// callers (typically `derrick-flow`) decide. Default
    /// surface is 10 minutes if the caller doesn't set.
    pub timeout: Duration,
    /// Extra env vars to set on top of the inherited env.
    /// Typical use: nothing; the host inherits the parent's
    /// env so its own auth (ANTHROPIC_API_KEY, etc.) is
    /// already present.
    pub env: HashMap<String, String>,
    /// Tool-permission override for Copilot specifically.
    /// Other adapters ignore this field. `Default` is the
    /// safe choice (Copilot prompts per-tool); pipelines
    /// that need autonomous execution opt in to
    /// `AllowAll`. See per-host contracts below.
    pub copilot_tools: CopilotToolPermission,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub enum CopilotToolPermission {
    /// Default: rely on Copilot's per-tool prompting.
    /// Effectively unusable from a non-interactive caller,
    /// so this is for tests + future interactive flows only.
    #[default]
    Default,
    /// Pass `--allow-all-tools` to Copilot so it can act
    /// autonomously. Required for any non-interactive
    /// pipeline use; callers (derrick-flow) opt in
    /// explicitly per step.
    AllowAll,
}

#[derive(Clone, Debug)]
pub struct HostResponse {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub elapsed: Duration,
}

#[derive(thiserror::Error, Debug)]
#[non_exhaustive]
pub enum HostError {
    #[error("host binary not found on PATH: {host}")]
    NotFound { host: String },
    #[error("host {host} exited with code {exit_code}: {stderr}")]
    NonZeroExit { host: String, exit_code: i32, stderr: String },
    #[error("host {host} timed out after {seconds}s")]
    Timeout { host: String, seconds: u64 },
    #[error("io error invoking host {host}: {source}")]
    Io { host: String, source: std::io::Error },
}

/// Registry of named host adapters.
pub struct HostRegistry { /* opaque */ }

impl HostRegistry {
    /// Returns a registry pre-populated with claude, codex,
    /// and copilot adapters using each host's CLI binary
    /// name on PATH.
    pub fn with_defaults() -> Self;

    /// Construct an empty registry (for tests that mock
    /// hosts at the process boundary).
    pub fn empty() -> Self;

    /// Add or replace an adapter for a host name.
    pub fn register(&mut self, name: &str, adapter: Box<dyn HostAdapter>);

    pub fn get(&self, name: &str) -> Option<&dyn HostAdapter>;

    /// List all known host names. Used by `derrick doctor`.
    pub fn names(&self) -> Vec<&str>;
}
```

### Per-host invocation contracts

**`claude` adapter**

- Binary: `claude` on PATH.
- Invocation: `claude --print <prompt>`. The prompt is one
  argv item (preserves spaces, newlines, slash-command
  prefix `/speckit.specify ...`). No model override —
  claude uses its own configured default.
- cwd is the request's cwd.
- env: inherits parent + any extra from the request.
- stdout is the assistant's response text.
- stderr captured for error reporting.

**`codex` adapter**

- Binary: `codex` on PATH.
- Invocation: `codex exec --skip-git-repo-check <prompt>`
  (mirrors how derrick itself invokes codex during assay
  rounds in this session). Prompt is one argv item. No
  model override — codex uses its own configured default.
- cwd / env: same as claude.

**`copilot` adapter**

- Binary: `copilot` on PATH (the standalone
  `@github/copilot` CLI, per D13).
- Invocation: `copilot -p <prompt> --add-dir <cwd>` by
  default. When `request.copilot_tools == AllowAll`,
  appends `--allow-all-tools`. Prompt is one argv item.
- This is **pipeline-step host execution only**, distinct
  from ticket dispatch via the substrate hand path which
  goes through `derrick-copilot` (T013) using
  `copilot agent run`.
- cwd / env: same as claude.

### Mocking hosts at the process boundary (test pattern)

Per AGENTS.md test-engineer working agreement: host CLIs
are mocked at the *process* boundary. Tests write a tiny
shell script to a tempdir, `chmod +x` it, prepend the
tempdir to PATH, then invoke the adapter. The adapter has
no idea it's not the real host. This is the same pattern
T006 uses for the `shell` provider tests.

For tests in this crate specifically:

```rust
fn mock_host(name: &str, body: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(name);
    std::fs::write(&path, body).unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    use std::os::unix::fs::PermissionsExt;
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    dir
}
```

### Dependencies

```toml
[dependencies]
async-trait = { workspace = true }
thiserror = { workspace = true }
tokio = { workspace = true }
tracing = { workspace = true }
which = { workspace = true }

[dev-dependencies]
tempfile = { workspace = true }
tokio = { workspace = true, features = ["macros", "rt", "rt-multi-thread"] }
```

No new top-level workspace deps. `which` is already in
workspace.dependencies.

### Tests

All tempfile-based with mock host scripts. No real claude /
codex / copilot calls.

Trait / registry tests:

- `registry_with_defaults_has_three_hosts`.
- `registry_get_unknown_returns_none`.
- `registry_register_replaces_existing`.
- `registry_names_lists_registered_hosts`.

Per-host tests (claude / codex / copilot — three sets):

- `<host>_invokes_with_correct_args` — mock host script
  echoes its argv to stderr; test asserts the expected
  flags appear (claude `--print`, codex `exec
  --skip-git-repo-check`, copilot `-p` + `--add-dir`).
- `<host>_passes_prompt_as_single_argv_item` — prompt
  contains spaces, newlines, and a leading slash; the mock
  receives it as exactly one argv element with no
  splitting or quoting damage.
- `<host>_passes_cwd` — mock host script prints `pwd`; test
  verifies it matches the request's cwd.
- `<host>_returns_stdout_as_response_text`.
- `<host>_surfaces_nonzero_exit_as_typed_error` — mock
  exits with code 7; expect `HostError::NonZeroExit {
  exit_code: 7, .. }` carrying captured stderr.
- `<host>_respects_timeout` — mock sleeps longer than
  timeout; expect `HostError::Timeout` within timeout +
  100ms.
- `<host>_is_available_returns_true_when_on_path` — mock
  script in PATH-prefixed tempdir; `is_available()` true.
- `<host>_is_available_returns_false_when_absent` — empty
  PATH; false.
- `<host>_does_not_pass_model_override` — request includes
  no model field (the field doesn't exist on `HostRequest`
  any more); the mock asserts `--model` does not appear in
  argv. This is a regression guard against re-introducing
  the knob.

Copilot-only:

- `copilot_default_omits_allow_all_tools`.
- `copilot_allow_all_appends_flag` — when
  `copilot_tools == AllowAll`, mock asserts
  `--allow-all-tools` is in argv.
- `claude_and_codex_ignore_copilot_tools_field` — set
  `copilot_tools = AllowAll` on a claude/codex request;
  mock asserts no `--allow-all-tools` flag appears.

Plus framework-level tests:

- `not_found_returns_typed_error_with_host_name`.
- `env_overrides_apply` — extra env vars from request
  visible to mock host's environment.
- `env_overrides_take_precedence_over_inherited_env` —
  parent has `FOO=parent`; request sets `FOO=request`;
  mock sees `FOO=request`.
- `env_omitted_when_request_empty` — request env map is
  empty; mock still sees inherited parent env.

**Coverage target**: 90%. Pure subprocess shell with
focused tests; high coverage is straightforward.

## Out of scope

- Streaming output. The host adapters return the full
  captured response. Streaming becomes relevant when
  `derrick-flow` invokes a host with a TUI-visible
  progress indicator; that's a follow-up if needed.
- Token usage extraction from host output. Hosts don't
  emit reliable token counts on stdout; telemetry comes
  from §9.B.7's transcript parsing (D14) and is handled
  by `derrick-observe` later.
- Model trait integration. derrick-models has its own
  trait (`Model`); derrick-tools is separate by design.
  derrick-flow uses both: derrick-models for provider
  calls (anthropic, openai, ollama), derrick-tools for
  host CLI shells (claude, codex, copilot).
- Authentication. Hosts manage their own auth (D12);
  derrick-tools never sees keys.

## Acceptance

- [ ] `cargo check -p derrick-tools` passes.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] `cargo test -p derrick-tools` passes; 3× stress green.
- [ ] `cargo llvm-cov -p derrick-tools --fail-under-lines 90`.
- [ ] Workspace `cargo llvm-cov --fail-under-lines 80` still passes.
- [ ] No `unwrap`/`expect`/`panic` in non-test code.
- [ ] All public types/methods documented.
- [ ] No gastown vocabulary.

## Reviewer notes (Codex)

Pre-implementation review. Focus on:
- Is the host/provider split clean enough vs derrick-models?
  Should there be a unified trait both implement?
- Are the per-host argv shapes correct for v1?
  (`claude --print`, `codex exec --skip-git-repo-check`,
  `copilot -p --add-dir`.)
- Is the model-override knob useful or should we drop it
  and let hosts use their own configured default?
- Are mocked tests sufficient to lock the contract?

## Implementer notes (Copilot)

Stay in `crates/derrick-tools/`. No top-level Cargo.toml
edits. Mock host scripts go in `crates/derrick-tools/tests/
fixtures/` (or inline within tests). macOS + Linux only;
Windows is a TODO with comment.
