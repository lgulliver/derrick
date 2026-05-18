//! Host CLI adapters for claude, codex, and copilot.
//!
//! Derrick uses these adapters for pipeline `host:` steps. Per DESIGN.md §6.5,
//! it passes only a working directory, a prompt, and the Copilot-specific
//! tool-permission knob; host CLIs load their own context and model defaults.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use thiserror::Error;

mod process;

pub mod hosts;

pub use hosts::{ClaudeHost, CodexHost, CopilotHost, OpencodeHost};

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

/// Input for a host CLI invocation.
#[derive(Clone, Debug)]
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
    /// Optional model override in `provider/model` format (e.g. `anthropic/claude-sonnet-4-5`).
    ///
    /// Passed as `--model <value>` to hosts that support it (currently opencode).
    /// Ignored by hosts that do not expose a `--model` flag (claude, codex, copilot).
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
        }
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
    /// Returns a registry pre-populated with claude, codex, copilot, and opencode.
    pub fn with_defaults() -> Self {
        let mut registry = Self::empty();
        registry.register("claude", Box::new(ClaudeHost::new()));
        registry.register("codex", Box::new(CodexHost::new()));
        registry.register("copilot", Box::new(CopilotHost::new()));
        registry.register("opencode", Box::new(OpencodeHost::new()));
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
