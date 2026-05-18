use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Instant;

use tokio::process::Command;
use tokio::time;

use crate::{HostError, HostRequest, HostResponse};

pub(crate) struct CommandSpec {
    pub(crate) binary: PathBuf,
    pub(crate) args: Vec<OsString>,
}

pub(crate) async fn run_host(
    host: &str,
    spec: CommandSpec,
    request: HostRequest,
) -> Result<HostResponse, HostError> {
    // Note: no pre-check via `is_available` here. `spawn()` below already
    // classifies missing binaries as `ErrorKind::NotFound` which is mapped
    // to `HostError::NotFound`, and a pre-check would beat legitimate
    // post-spawn errors (e.g. `NotADirectory` cwd) to the punch on some
    // platforms where the binary check races the spawn check.
    let mut command = Command::new(&spec.binary);
    command
        .args(&spec.args)
        .current_dir(&request.cwd)
        .envs(&request.env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    tracing::debug!(host, cwd = %request.cwd.display(), "invoking host CLI");

    let started = Instant::now();
    let child = command.spawn().map_err(|source| {
        if source.kind() == std::io::ErrorKind::NotFound {
            HostError::NotFound {
                host: host.to_owned(),
            }
        } else {
            HostError::Io {
                host: host.to_owned(),
                source,
            }
        }
    })?;

    let timeout = request.timeout;
    let output = match time::timeout(timeout, child.wait_with_output()).await {
        Ok(output) => output.map_err(|source| HostError::Io {
            host: host.to_owned(),
            source,
        })?,
        Err(_) => {
            return Err(HostError::Timeout {
                host: host.to_owned(),
                seconds: timeout.as_secs(),
            });
        }
    };
    let elapsed = started.elapsed();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let exit_code = output.status.code().unwrap_or(-1);

    if output.status.success() {
        Ok(HostResponse {
            stdout,
            stderr,
            exit_code,
            elapsed,
        })
    } else {
        Err(HostError::NonZeroExit {
            host: host.to_owned(),
            exit_code,
            stderr,
        })
    }
}

pub(crate) fn is_available(binary: &Path) -> bool {
    if has_path_separator(binary) {
        is_invocable_file(binary)
    } else {
        which::which(binary).is_ok()
    }
}

fn has_path_separator(binary: &Path) -> bool {
    binary.components().count() > 1
}

#[cfg(unix)]
fn is_invocable_file(binary: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    binary
        .metadata()
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(windows)]
fn is_invocable_file(binary: &Path) -> bool {
    // TODO(T009 follow-up): define Windows executable probing semantics.
    binary.is_file()
}
