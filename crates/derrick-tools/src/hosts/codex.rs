use std::ffi::OsString;
use std::path::PathBuf;

use async_trait::async_trait;

use crate::process::{is_available, run_host, CommandSpec};
use crate::{HostAdapter, HostError, HostRequest, HostResponse};

const NAME: &str = "codex";

/// Host adapter for the Codex CLI.
#[derive(Clone, Debug)]
pub struct CodexHost {
    binary: PathBuf,
}

impl CodexHost {
    /// Creates an adapter that resolves `codex` on `PATH`.
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

impl Default for CodexHost {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl HostAdapter for CodexHost {
    fn name(&self) -> &str {
        NAME
    }

    fn is_available(&self) -> bool {
        is_available(&self.binary)
    }

    async fn run(&self, request: HostRequest) -> Result<HostResponse, HostError> {
        let mut args = vec![
            OsString::from("exec"),
            OsString::from("--skip-git-repo-check"),
        ];
        if let Some(ref model) = request.model {
            args.push(OsString::from("--model"));
            args.push(OsString::from(model.as_str()));
        }
        args.push(OsString::from(&request.prompt));
        let spec = CommandSpec {
            binary: self.binary.clone(),
            args,
        };
        run_host(NAME, spec, request).await
    }
}
