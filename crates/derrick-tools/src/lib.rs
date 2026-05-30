//! Host CLI adapters for claude, codex, copilot, opencode, and aider.
//!
//! Derrick uses these adapters for pipeline `host:` steps and, since D65, as
//! the inference path for `derrick-models` host-delegated providers. Per
//! DESIGN.md §6.5, it passes a working directory, a prompt, the
//! Copilot-specific tool-permission knob, and an optional model override; host
//! CLIs load their own context and manage their own auth. All five adapters
//! forward `--model` when [`HostRequest::model`] is set, normalising the id per
//! host via [`catalogue`] just before it is passed on the command line.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use thiserror::Error;

mod process;

pub mod catalogue;
pub mod hosts;

pub use hosts::{AiderHost, ClaudeHost, CodexHost, CopilotHost, OpencodeHost};

/// One host CLI that derrick can invoke.
#[async_trait]
pub trait HostAdapter: Send + Sync {
    /// Human-readable host name.
    fn name(&self) -> &str;

    /// Returns whether the host binary is available and looks invocable.
    fn is_available(&self) -> bool;

    /// Invokes the host CLI with the given request.
    async fn run(&self, request: HostRequest) -> Result<HostResponse, HostError>;
}

/// Which standard stream a streamed output line came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StreamSource {
    /// The process's standard output.
    Stdout,
    /// The process's standard error.
    Stderr,
}

/// The boxed per-line callback behind an [`OutputSink`].
type OutputSinkFn = Arc<dyn Fn(StreamSource, &str) + Send + Sync>;

/// A callback invoked once per complete line of host output as it arrives.
///
/// This is how derrick surfaces live agent activity (run-feedback Layer 2)
/// without the host layer depending on any UI: the caller supplies a closure
/// that forwards each line to a progress front-end. The full output is still
/// captured and returned in [`HostResponse`] regardless.
#[derive(Clone)]
pub struct OutputSink(OutputSinkFn);

impl OutputSink {
    /// Wraps a per-line callback.
    pub fn new(sink: impl Fn(StreamSource, &str) + Send + Sync + 'static) -> Self {
        Self(Arc::new(sink))
    }

    /// Delivers one line to the callback.
    pub(crate) fn emit(&self, source: StreamSource, line: &str) {
        (self.0)(source, line);
    }
}

impl std::fmt::Debug for OutputSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OutputSink(..)")
    }
}

/// Input for a host CLI invocation.
///
/// `Debug` is hand-implemented (not derived) because [`env`](Self::env) carries
/// the forwarded process environment, which can contain secrets (API tokens,
/// `GH_TOKEN`, proxy credentials). The manual impl prints only the count of env
/// vars — never keys or values — and redacts the prompt, which may embed
/// sensitive task content. This keeps `tracing` of a `HostRequest` safe even at
/// TRACE level.
#[derive(Clone)]
pub struct HostRequest {
    /// Prompt to pass as one argv item.
    pub prompt: String,
    /// Working directory for the host process.
    pub cwd: PathBuf,
    /// Wall-clock timeout for the host process.
    pub timeout: Duration,
    /// Additional environment variables to set for the host process.
    pub env: HashMap<String, String>,
    /// Copilot-specific tool permission override.
    pub copilot_tools: CopilotToolPermission,
    /// Optional model override (e.g. `claude-opus-4-8` or `anthropic/claude-sonnet-4-6`).
    ///
    /// Passed as `--model <value>` to all five host adapters, after per-host
    /// normalisation via [`catalogue::normalize`] (claude/codex/copilot strip a
    /// leading `provider/`; opencode/aider keep `provider/model` verbatim).
    /// `None` means the host uses its own default or configured model.
    pub model: Option<String>,
    /// Run the host in headless mode — suppress interactive permission prompts.
    ///
    /// Pipeline steps always set this to `true`: derrick's pipeline runs without
    /// a terminal and cannot answer interactive prompts. Host adapters map this
    /// to their CLI's equivalent flag (`--dangerously-skip-permissions` for
    /// `claude`; `--yes` / `--no-interactive` for other hosts). Defaults to
    /// `false` so interactive invocations (e.g. `derrick run` in a developer
    /// terminal) retain the host's normal confirmation behaviour.
    pub headless: bool,
    /// Optional per-line sink for streaming host output as it is produced.
    ///
    /// When set, [`run`](HostAdapter::run) invokes the sink once per complete
    /// line of stdout/stderr while the process runs, in addition to capturing
    /// the full output in [`HostResponse`]. `None` (the default) preserves the
    /// previous capture-only behaviour.
    pub output_sink: Option<OutputSink>,
}

impl HostRequest {
    /// Builds a request with the default ten-minute timeout and no extra env.
    pub fn new(prompt: impl Into<String>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            prompt: prompt.into(),
            cwd: cwd.into(),
            timeout: Duration::from_secs(600),
            env: HashMap::new(),
            copilot_tools: CopilotToolPermission::Default,
            model: None,
            headless: false,
            output_sink: None,
        }
    }
}

impl std::fmt::Debug for HostRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostRequest")
            // Redacted: may embed sensitive task content.
            .field("prompt", &"<redacted>")
            .field("cwd", &self.cwd)
            .field("timeout", &self.timeout)
            // Redacted: never print env keys or values (secrets).
            .field("env", &format_args!("<{} vars redacted>", self.env.len()))
            .field("copilot_tools", &self.copilot_tools)
            .field("model", &self.model)
            .field("headless", &self.headless)
            .field("output_sink", &self.output_sink)
            .finish()
    }
}

/// Tool permission override for Copilot host invocations.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub enum CopilotToolPermission {
    /// Rely on Copilot's default per-tool prompting.
    #[default]
    Default,
    /// Pass `--allow-all-tools` so Copilot can run non-interactively.
    AllowAll,
}

/// Captured result of a successful host CLI invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostResponse {
    /// Captured stdout decoded as UTF-8 lossily.
    pub stdout: String,
    /// Captured stderr decoded as UTF-8 lossily.
    pub stderr: String,
    /// Process exit code.
    pub exit_code: i32,
    /// Wall-clock elapsed time.
    pub elapsed: Duration,
    /// Input tokens consumed by the host (0 when not reported).
    pub tokens_in: u32,
    /// Output tokens produced by the host (0 when not reported).
    pub tokens_out: u32,
}

/// Errors returned by host CLI adapters.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum HostError {
    /// Host binary was not found.
    #[error("host binary not found on PATH: {host}")]
    NotFound {
        /// Host name.
        host: String,
    },
    /// Host process exited unsuccessfully.
    #[error("host {host} exited with code {exit_code}: {stderr}")]
    NonZeroExit {
        /// Host name.
        host: String,
        /// Process exit code, or -1 when no code was reported.
        exit_code: i32,
        /// Captured stderr.
        stderr: String,
    },
    /// Host process exceeded its timeout.
    #[error("host {host} timed out after {seconds}s")]
    Timeout {
        /// Host name.
        host: String,
        /// Timeout duration in seconds.
        seconds: u64,
    },
    /// I/O error while invoking the host.
    #[error("io error invoking host {host}: {source}")]
    Io {
        /// Host name.
        host: String,
        /// Source I/O error.
        source: std::io::Error,
    },
}

/// Registry of named host adapters.
#[derive(Default)]
pub struct HostRegistry {
    adapters: HashMap<String, Box<dyn HostAdapter>>,
}

impl HostRegistry {
    /// Returns a registry pre-populated with the five host CLIs: claude,
    /// codex, copilot, opencode, and aider.
    pub fn with_defaults() -> Self {
        let mut registry = Self::empty();
        registry.register("claude", Box::new(ClaudeHost::new()));
        registry.register("codex", Box::new(CodexHost::new()));
        registry.register("copilot", Box::new(CopilotHost::new()));
        registry.register("opencode", Box::new(OpencodeHost::new()));
        registry.register("aider", Box::new(AiderHost::new()));
        registry
    }

    /// Constructs an empty registry.
    pub fn empty() -> Self {
        Self {
            adapters: HashMap::new(),
        }
    }

    /// Adds or replaces an adapter for a host name.
    pub fn register(&mut self, name: &str, adapter: Box<dyn HostAdapter>) {
        self.adapters.insert(name.to_owned(), adapter);
    }

    /// Returns the adapter registered for a host name.
    pub fn get(&self, name: &str) -> Option<&dyn HostAdapter> {
        self.adapters.get(name).map(std::convert::AsRef::as_ref)
    }

    /// Lists all registered host names in stable sorted order.
    pub fn names(&self) -> Vec<&str> {
        let mut names = self
            .adapters
            .keys()
            .map(std::string::String::as_str)
            .collect::<Vec<_>>();
        names.sort_unstable();
        names
    }
}
