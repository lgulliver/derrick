use std::ffi::OsString;
use std::path::PathBuf;

use async_trait::async_trait;

use crate::catalogue;
use crate::process::{CommandSpec, is_available, run_host};
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
        let args = build_args(&request);
        let spec = CommandSpec {
            binary: self.binary.clone(),
            args,
        };
        run_host(NAME, spec, request).await
    }
}

/// Builds the Copilot CLI argv from a request.
///
/// `--allow-all-tools` is emitted when either the caller sets
/// [`CopilotToolPermission::AllowAll`] explicitly, OR the request is
/// [`headless`](HostRequest::headless). Copilot has no separate
/// `--no-interactive` flag: `--allow-all-tools` is exactly what suppresses its
/// per-tool approval prompts, so headless (no-terminal pipeline) runs would
/// otherwise hang. Honouring `headless` here makes the adapter self-sufficient
/// so callers no longer need to special-case copilot by forcing `copilot_tools`
/// — though doing so remains valid and produces the same single flag.
fn build_args(request: &HostRequest) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("-p"),
        OsString::from(&request.prompt),
        OsString::from("--add-dir"),
        request.cwd.as_os_str().to_owned(),
    ];
    if let Some(ref model) = request.model {
        args.push(OsString::from("--model"));
        args.push(OsString::from(catalogue::normalize(NAME, model)));
    }
    if request.headless || request.copilot_tools == CopilotToolPermission::AllowAll {
        args.push(OsString::from("--allow-all-tools"));
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> HostRequest {
        HostRequest::new("do the thing", "/repo")
    }

    fn has(args: &[OsString], needle: &str) -> bool {
        args.iter().any(|arg| arg == needle)
    }

    fn count(args: &[OsString], needle: &str) -> usize {
        args.iter().filter(|arg| *arg == needle).count()
    }

    #[test]
    fn interactive_default_has_no_allow_all_tools() {
        let args = build_args(&request());
        assert!(!has(&args, "--allow-all-tools"));
    }

    #[test]
    fn headless_produces_non_interactive_invocation() {
        let mut req = request();
        req.headless = true;
        let args = build_args(&req);
        // Headless alone must emit the flag so the CLI never blocks on an
        // approval prompt, even when the caller left copilot_tools defaulted.
        assert_eq!(req.copilot_tools, CopilotToolPermission::Default);
        assert!(has(&args, "--allow-all-tools"));
    }

    #[test]
    fn explicit_allow_all_still_works() {
        let mut req = request();
        req.copilot_tools = CopilotToolPermission::AllowAll;
        let args = build_args(&req);
        assert!(has(&args, "--allow-all-tools"));
    }

    #[test]
    fn headless_and_explicit_allow_all_emit_single_flag() {
        let mut req = request();
        req.headless = true;
        req.copilot_tools = CopilotToolPermission::AllowAll;
        let args = build_args(&req);
        assert_eq!(count(&args, "--allow-all-tools"), 1);
    }
}
