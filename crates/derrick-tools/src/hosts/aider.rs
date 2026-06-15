use std::ffi::OsString;
use std::path::PathBuf;

use async_trait::async_trait;

use crate::catalogue;
use crate::process::{CommandSpec, is_available, run_host};
use crate::{HostAdapter, HostError, HostRequest, HostResponse};

const NAME: &str = "aider";

/// Host adapter for the `aider` CLI.
///
/// Invocation pattern (headless, one-shot):
/// ```text
/// aider --model <provider/model> --message "<prompt>" \
///       --yes-always --no-auto-commits --no-dirty-commits --no-stream \
///       --no-pretty --no-show-release-notes
/// ```
///
/// The prompt is delivered as a single argv item via `--message`, which runs
/// aider once and exits. `aider` operates on the process working directory
/// (there is no `--dir` flag), so the [`HostRequest::cwd`] is applied by the
/// process layer. The headless flags suppress interactive prompts, the
/// commit/streaming/pretty behaviour, and the release-notes banner so the
/// captured stdout is clean for pipeline runs. Both `--no-auto-commits` (no
/// commit of aider's own edits) and `--no-dirty-commits` (no commit of any
/// pre-existing dirty work before processing the message) are passed because
/// derrick owns commits in its worktrees; aider must never commit on its own. `--model` is forwarded (after
/// catalogue normalisation, which passes `provider/model` through verbatim)
/// only when [`HostRequest::model`] is set; aider otherwise uses its own
/// configured default. Auth is fully delegated to aider's own configuration.
#[derive(Clone, Debug)]
pub struct AiderHost {
    binary: PathBuf,
}

impl AiderHost {
    /// Creates an adapter that resolves `aider` on `PATH`.
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

impl Default for AiderHost {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl HostAdapter for AiderHost {
    fn name(&self) -> &str {
        NAME
    }

    fn is_available(&self) -> bool {
        is_available(&self.binary)
    }

    async fn run(&self, request: HostRequest) -> Result<HostResponse, HostError> {
        let mut args = Vec::new();
        if let Some(ref model) = request.model {
            let normalized = catalogue::normalize(self.name(), model);
            args.push(OsString::from("--model"));
            args.push(OsString::from(normalized));
        }
        args.push(OsString::from("--message"));
        args.push(OsString::from(&request.prompt));
        args.push(OsString::from("--yes-always"));
        args.push(OsString::from("--no-auto-commits"));
        args.push(OsString::from("--no-dirty-commits"));
        args.push(OsString::from("--no-stream"));
        args.push(OsString::from("--no-pretty"));
        args.push(OsString::from("--no-show-release-notes"));

        let spec = CommandSpec {
            binary: self.binary.clone(),
            args,
        };
        run_host(NAME, spec, request).await
    }
}
