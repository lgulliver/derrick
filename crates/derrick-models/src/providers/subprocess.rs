//! Shared subprocess streaming helper for CLI-backed providers.
//!
//! Unlike `shell.rs` (which speaks the `<<DERRICK-*>>` envelope protocol),
//! this helper shells to a generic CLI (`codex`, `opencode`, ...): it pipes
//! the prompt to the child's stdin as plain text, streams stdout line-by-line
//! as `CompletionEvent::Content`, and synthesises a final `End` event on
//! process exit. Token counts are reported as zero since these CLIs do not
//! emit a standard token line.

use std::process::Stdio;

use futures::SinkExt;
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
        const STDERR_LIMIT: usize = 8 * 1024;

        let mut tx = tx;
        let mut stdout_reader = tokio::io::BufReader::new(stdout);
        let mut stderr_reader = tokio::io::BufReader::new(stderr);
        let mut stdout_buf = Vec::<u8>::new();
        let mut stderr_buf = Vec::<u8>::new();
        let mut stdout_open = true;
        let mut stderr_open = true;
        let mut stderr_text = String::new();
        let mut stderr_truncated = false;

        while stdout_open || stderr_open {
            tokio::select! {
                result = stdout_reader.read_until(b'\n', &mut stdout_buf), if stdout_open => {
                    match result {
                        Ok(0) => stdout_open = false,
                        Ok(_) => {
                            let text = String::from_utf8_lossy(&stdout_buf).to_string();
                            stdout_buf.clear();
                            if tx.send(Ok(CompletionEvent::Content { text })).await.is_err() {
                                let _ = child.kill().await;
                                let _ = child.wait().await;
                                return;
                            }
                        }
                        Err(error) => {
                            let _ = tx.send(Err(ModelError::Provider {
                                provider: provider.to_owned(),
                                message: format!("failed reading subprocess stdout: {error}"),
                                retryable: true,
                            })).await;
                            let _ = child.kill().await;
                            let _ = child.wait().await;
                            return;
                        }
                    }
                }
                result = stderr_reader.read_until(b'\n', &mut stderr_buf), if stderr_open => {
                    match result {
                        Ok(0) => stderr_open = false,
                        Ok(_) => {
                            if stderr_text.len() < STDERR_LIMIT {
                                let chunk = String::from_utf8_lossy(&stderr_buf);
                                let remaining = STDERR_LIMIT - stderr_text.len();
                                if chunk.len() <= remaining {
                                    stderr_text.push_str(&chunk);
                                } else {
                                    let cut = chunk
                                        .char_indices()
                                        .map(|(idx, _)| idx)
                                        .take_while(|idx| *idx <= remaining)
                                        .last()
                                        .unwrap_or(0);
                                    stderr_text.push_str(&chunk[..cut]);
                                    stderr_truncated = true;
                                }
                            } else {
                                stderr_truncated = true;
                            }
                            stderr_buf.clear();
                        }
                        Err(error) => {
                            let _ = tx.send(Err(ModelError::Provider {
                                provider: provider.to_owned(),
                                message: format!("failed reading subprocess stderr: {error}"),
                                retryable: true,
                            })).await;
                            let _ = child.kill().await;
                            let _ = child.wait().await;
                            return;
                        }
                    }
                }
            }
        }

        let status = child.wait().await;
        match status {
            Ok(status) if status.success() => {
                let _ = tx
                    .send(Ok(CompletionEvent::End {
                        tokens_in: 0,
                        tokens_out: 0,
                        finish_reason: FinishReason::Stop,
                    }))
                    .await;
            }
            Ok(status) => {
                let mut stderr_summary = stderr_text.trim().to_owned();
                if stderr_truncated {
                    if !stderr_summary.is_empty() {
                        stderr_summary.push(' ');
                    }
                    stderr_summary.push_str("(stderr truncated)");
                }
                let msg = if stderr_summary.is_empty() {
                    format!("process exited with {status}")
                } else {
                    format!("process exited with {status}; stderr: {stderr_summary}")
                };
                let _ = tx
                    .send(Err(ModelError::Provider {
                        provider: provider.to_owned(),
                        message: msg,
                        retryable: false,
                    }))
                    .await;
            }
            Err(error) => {
                warn!(target: "derrick_models::subprocess", "wait failed: {error}");
                let _ = tx
                    .send(Ok(CompletionEvent::End {
                        tokens_in: 0,
                        tokens_out: 0,
                        finish_reason: FinishReason::Error,
                    }))
                    .await;
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
