use std::ffi::OsString;
use std::path::PathBuf;

use async_trait::async_trait;

use crate::process::{is_available, run_host, CommandSpec};
use crate::{CopilotToolPermission, HostAdapter, HostError, HostRequest, HostResponse};

const NAME: &str = "copilot";

/// Host adapter for the GitHub Copilot CLI.
#[derive(Clone, Debug)]
pub struct CopilotHost {
    binary: PathBuf,
}

impl CopilotHost {
    /// Creates an adapter that resolves `copilot` on `PATH`.
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

impl Default for CopilotHost {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl HostAdapter for CopilotHost {
    fn name(&self) -> &str {
        NAME
    }

    fn is_available(&self) -> bool {
        is_available(&self.binary)
    }

    async fn run(&self, request: HostRequest) -> Result<HostResponse, HostError> {
        let mut args = vec![
            OsString::from("-p"),
            OsString::from(&request.prompt),
            OsString::from("--add-dir"),
            request.cwd.as_os_str().to_owned(),
        ];
        if let Some(ref model) = request.model {
            args.push(OsString::from("--model"));
            args.push(OsString::from(model.as_str()));
        }
        if request.copilot_tools == CopilotToolPermission::AllowAll {
            args.push(OsString::from("--allow-all-tools"));
        }

        let spec = CommandSpec {
            binary: self.binary.clone(),
            args,
        };
        run_host(NAME, spec, request).await
    }
}
