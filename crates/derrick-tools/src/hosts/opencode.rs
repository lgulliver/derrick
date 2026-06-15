use std::ffi::OsString;
use std::path::PathBuf;

use async_trait::async_trait;

use crate::catalogue;
use crate::process::{CommandSpec, is_available, run_host};
use crate::{HostAdapter, HostError, HostRequest, HostResponse};

const NAME: &str = "opencode";

/// Host adapter for the OpenCode CLI (`opencode run`).
///
/// Invocation pattern:
/// ```text
/// opencode run "<prompt>" --dir <cwd> [--model provider/model] [--dangerously-skip-permissions]
/// ```
///
/// The `--dir` flag sets the project directory without changing the process
/// working directory, which keeps the host's path resolution aligned with the
/// derrick worktree. `--dangerously-skip-permissions` suppresses interactive
/// tool-permission prompts in headless pipeline runs. `--model` is forwarded
/// from [`HostRequest::model`] when set, after catalogue normalisation; opencode
/// expects a `provider/model` id (e.g. `anthropic/claude-sonnet-4-6`), which the
/// catalogue passes through verbatim.
#[derive(Clone, Debug)]
pub struct OpencodeHost {
    binary: PathBuf,
}

impl OpencodeHost {
    /// Creates an adapter that resolves `opencode` on `PATH`.
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

impl Default for OpencodeHost {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl HostAdapter for OpencodeHost {
    fn name(&self) -> &str {
        NAME
    }

    fn is_available(&self) -> bool {
        is_available(&self.binary)
    }

    async fn run(&self, request: HostRequest) -> Result<HostResponse, HostError> {
        let mut args = vec![
            OsString::from("run"),
            OsString::from(&request.prompt),
            OsString::from("--dir"),
            request.cwd.as_os_str().to_owned(),
        ];
        if let Some(ref model) = request.model {
            args.push(OsString::from("--model"));
            args.push(OsString::from(catalogue::normalize(self.name(), model)));
        }
        if request.headless {
            args.push(OsString::from("--dangerously-skip-permissions"));
        }

        let spec = CommandSpec {
            binary: self.binary.clone(),
            args,
        };
        run_host(NAME, spec, request).await
    }
}
