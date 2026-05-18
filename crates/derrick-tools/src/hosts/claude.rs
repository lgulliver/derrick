use std::ffi::OsString;
use std::path::PathBuf;

use async_trait::async_trait;

use crate::process::{is_available, run_host, CommandSpec};
use crate::{HostAdapter, HostError, HostRequest, HostResponse};

const NAME: &str = "claude";

/// Host adapter for the Claude Code CLI.
#[derive(Clone, Debug)]
pub struct ClaudeHost {
    binary: PathBuf,
}

impl ClaudeHost {
    /// Creates an adapter that resolves `claude` on `PATH`.
    pub fn new() -> Self {
        Self {
            binary: PathBuf::from(NAME),
        }
    }

    /// Creates an adapter using an explicit binary path.
    pub fn with_binary(binary: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
        }
    }
}

impl Default for ClaudeHost {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl HostAdapter for ClaudeHost {
    fn name(&self) -> &str {
        NAME
    }

    fn is_available(&self) -> bool {
        is_available(&self.binary)
    }

    async fn run(&self, request: HostRequest) -> Result<HostResponse, HostError> {
        let mut args = vec![OsString::from("--print")];
        if let Some(ref model) = request.model {
            args.push(OsString::from("--model"));
            args.push(OsString::from(model.as_str()));
        }
        if request.headless {
            // Suppress interactive permission prompts when running without a
            // terminal. Pipeline steps always set `HostRequest::headless = true`.
            args.push(OsString::from("--dangerously-skip-permissions"));
        }
        args.push(OsString::from(&request.prompt));
        let spec = CommandSpec {
            binary: self.binary.clone(),
            args,
        };
        run_host(NAME, spec, request).await
    }
}
