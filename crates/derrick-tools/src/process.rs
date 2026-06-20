use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Instant;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::time;

use crate::{HostError, HostRequest, HostResponse, OutputSink, StreamSource};

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
    let mut child = command.spawn().map_err(|source| {
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

    // Report the child pid to the live sink (D75/D76) so callers can record
    // it for foreman liveness and emit `HandStarted` while the agent runs.
    // `child.id()` is `Some` on unix/macOS once spawned; `None` on platforms
    // without a pid concept.
    let pid = child.id();
    if let (Some(pid_value), Some(sink)) = (pid, request.pid_sink.as_ref()) {
        sink.emit(pid_value);
    }

    // Drain both pipes concurrently while the child runs. Reading on separate
    // tasks prevents the classic pipe-buffer-full deadlock and lets us forward
    // each line to the sink as it arrives (Layer 2), while still accumulating
    // the complete output byte-for-byte for the captured `HostResponse`.
    let sink = request.output_sink.clone();
    let stdout_task = tokio::spawn(pump(
        child.stdout.take(),
        sink.clone(),
        StreamSource::Stdout,
    ));
    let stderr_task = tokio::spawn(pump(child.stderr.take(), sink, StreamSource::Stderr));

    let timeout = request.timeout;
    let status = match time::timeout(timeout, child.wait()).await {
        Ok(status) => status.map_err(|source| HostError::Io {
            host: host.to_owned(),
            source,
        })?,
        Err(_) => {
            // `kill_on_drop` reaps the child; the detached pump tasks finish at EOF.
            return Err(HostError::Timeout {
                host: host.to_owned(),
                seconds: timeout.as_secs(),
            });
        }
    };
    // Pipes are closed once the child has exited, so these joins resolve promptly.
    let stdout_bytes = stdout_task.await.unwrap_or_default();
    let stderr_bytes = stderr_task.await.unwrap_or_default();
    let elapsed = started.elapsed();
    let stdout = String::from_utf8_lossy(&stdout_bytes).into_owned();
    let stderr = String::from_utf8_lossy(&stderr_bytes).into_owned();
    let exit_code = status.code().unwrap_or(-1);

    if status.success() {
        Ok(HostResponse {
            stdout,
            stderr,
            exit_code,
            elapsed,
            tokens_in: 0,
            tokens_out: 0,
            pid,
        })
    } else {
        Err(HostError::NonZeroExit {
            host: host.to_owned(),
            exit_code,
            stderr,
            stdout,
        })
    }
}

/// Reads `reader` to EOF, returning the raw bytes. When `sink` is set, each
/// complete line (newline-delimited, trailing `\r` trimmed) is forwarded as it
/// arrives, plus any final unterminated line. Returning the raw bytes keeps the
/// captured `HostResponse` byte-identical to the previous capture-only path.
async fn pump<R>(reader: Option<R>, sink: Option<OutputSink>, source: StreamSource) -> Vec<u8>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let Some(mut reader) = reader else {
        return Vec::new();
    };
    let mut raw = Vec::new();
    let mut line = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                raw.extend_from_slice(&buf[..n]);
                if let Some(sink) = &sink {
                    for &byte in &buf[..n] {
                        if byte == b'\n' {
                            emit_line(sink, source, &line);
                            line.clear();
                        } else {
                            line.push(byte);
                        }
                    }
                }
            }
        }
    }
    if let Some(sink) = &sink {
        if !line.is_empty() {
            emit_line(sink, source, &line);
        }
    }
    raw
}

/// Decode one buffered line lossily, trim a trailing `\r`, and forward it.
fn emit_line(sink: &OutputSink, source: StreamSource, line: &[u8]) {
    let text = String::from_utf8_lossy(line);
    sink.emit(source, text.trim_end_matches('\r'));
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
