//! Shared subprocess streaming helper for CLI-backed providers.
//!
//! Unlike `shell.rs` (which speaks the `<<DERRICK-*>>` envelope protocol),
//! this helper shells to a generic CLI (`codex`, `opencode`, ...): it pipes
//! the prompt to the child's stdin as plain text, streams stdout line-by-line
//! as `CompletionEvent::Content`, and synthesises a final `End` event on
//! process exit. Token counts are reported as zero since these CLIs do not
//! emit a standard token line.

use std::process::Stdio;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tracing::warn;

use crate::{CompletionEvent, CompletionStream, FinishReason, ModelError};

pub(crate) struct SubprocessSpec {
    pub provider: &'static str,
    pub argv: Vec<String>,
    pub stdin_payload: String,
}

pub(crate) async fn stream_subprocess(
    spec: SubprocessSpec,
) -> Result<CompletionStream, ModelError> {
    let SubprocessSpec {
        provider,
        argv,
        stdin_payload,
    } = spec;

    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = command.spawn().map_err(|error| ModelError::Provider {
        provider: provider.to_owned(),
        message: format!("failed to spawn {argv:?}: {error}"),
        retryable: false,
    })?;

    let mut stdin = child.stdin.take().ok_or_else(|| ModelError::Provider {
        provider: provider.to_owned(),
        message: "failed to open subprocess stdin".to_owned(),
        retryable: false,
    })?;

    stdin
        .write_all(stdin_payload.as_bytes())
        .await
        .map_err(|error| ModelError::Provider {
            provider: provider.to_owned(),
            message: format!("failed to write subprocess stdin: {error}"),
            retryable: true,
        })?;
    stdin
        .shutdown()
        .await
        .map_err(|error| ModelError::Provider {
            provider: provider.to_owned(),
            message: format!("failed to close subprocess stdin: {error}"),
            retryable: true,
        })?;
    drop(stdin);

    let stdout = child.stdout.take().ok_or_else(|| ModelError::Provider {
        provider: provider.to_owned(),
        message: "failed to open subprocess stdout".to_owned(),
        retryable: false,
    })?;
    let stderr = child.stderr.take().ok_or_else(|| ModelError::Provider {
        provider: provider.to_owned(),
        message: "failed to open subprocess stderr".to_owned(),
        retryable: false,
    })?;

    Ok(Box::pin(stream_output(provider, stdout, stderr, child)))
}

fn stream_output(
    provider: &'static str,
    stdout: tokio::process::ChildStdout,
    stderr: tokio::process::ChildStderr,
    mut child: tokio::process::Child,
) -> CompletionStream {
    let (tx, rx) = futures::channel::mpsc::channel::<Result<CompletionEvent, ModelError>>(64);
    tokio::task::spawn(async move {
        use tokio::io::AsyncBufReadExt;
        let mut reader = tokio::io::BufReader::new(stdout);
        let mut buf = Vec::<u8>::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf).await {
                Ok(0) => break,
                Ok(_) => {
                    let text = String::from_utf8_lossy(&buf).to_string();
                    let _ = tx.clone().try_send(Ok(CompletionEvent::Content { text }));
                }
                Err(error) => {
                    let _ = tx.clone().try_send(Err(ModelError::Provider {
                        provider: provider.to_owned(),
                        message: format!("failed reading subprocess stdout: {error}"),
                        retryable: true,
                    }));
                    return;
                }
            }
        }

        let status = child.wait().await;
        match status {
            Ok(status) if status.success() => {
                let _ = tx.clone().try_send(Ok(CompletionEvent::End {
                    tokens_in: 0,
                    tokens_out: 0,
                    finish_reason: FinishReason::Stop,
                }));
            }
            Ok(status) => {
                let stderr_reader = tokio::io::BufReader::new(stderr);
                let stderr_text = stderr_reader
                    .lines()
                    .next_line()
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or_default();
                let msg = if stderr_text.is_empty() {
                    format!("process exited with {status}")
                } else {
                    format!("process exited with {status}; stderr: {stderr_text}")
                };
                let _ = tx.clone().try_send(Err(ModelError::Provider {
                    provider: provider.to_owned(),
                    message: msg,
                    retryable: false,
                }));
            }
            Err(error) => {
                warn!(target: "derrick_models::subprocess", "wait failed: {error}");
                let _ = tx.clone().try_send(Ok(CompletionEvent::End {
                    tokens_in: 0,
                    tokens_out: 0,
                    finish_reason: FinishReason::Error,
                }));
            }
        }
    });
    Box::pin(rx)
}

pub(crate) fn parse_argv(
    provider: &str,
    model: &str,
    cli: &str,
) -> Result<Vec<String>, ModelError> {
    let argv = shell_words::split(cli).map_err(|error| ModelError::InvalidConfig {
        model: model.to_owned(),
        message: format!("{provider}: invalid cli command: {error}"),
    })?;
    if argv.is_empty() {
        Err(ModelError::InvalidConfig {
            model: model.to_owned(),
            message: format!("{provider}: cli command must not be empty"),
        })
    } else {
        Ok(argv)
    }
}
