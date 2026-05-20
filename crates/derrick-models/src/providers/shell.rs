use std::process::Stdio;

use async_trait::async_trait;
use derrick_config::ModelDef;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time;

use crate::{
    AuthStore, CompletionEvent, CompletionRequest, CompletionStream, CostHint, FinishReason, Model,
    ModelError,
};

const PROVIDER: &str = "shell";
const CONTENT_PREFIX: &str = "<<DERRICK-CONTENT>> ";
const META_PREFIX: &str = "<<DERRICK-META>> ";

pub(crate) fn build(model_def: &ModelDef, _auth: &AuthStore) -> Result<Box<dyn Model>, ModelError> {
    #[cfg(windows)]
    {
        // TODO(T006 follow-up): define Windows process and script semantics.
        return Err(ModelError::Provider {
            provider: PROVIDER.to_owned(),
            message: "shell provider is supported on macOS and Linux only".to_owned(),
            retryable: false,
        });
    }

    #[cfg(not(windows))]
    {
        let argv = argv_from_model_def(model_def)?;
        Ok(Box::new(ShellModel {
            name: model_def.model().to_owned(),
            argv,
            cost_hint: None,
        }))
    }
}

#[derive(Clone, Debug)]
struct ShellModel {
    name: String,
    argv: Vec<String>,
    cost_hint: Option<CostHint>,
}

#[async_trait]
impl Model for ShellModel {
    fn name(&self) -> &str {
        &self.name
    }

    fn provider(&self) -> &str {
        PROVIDER
    }

    fn cost_hint(&self) -> Option<&CostHint> {
        self.cost_hint.as_ref()
    }

    async fn stream(&self, request: CompletionRequest) -> Result<CompletionStream, ModelError> {
        self.stream_inner(request).await
    }

    async fn complete(
        &self,
        request: CompletionRequest,
    ) -> Result<crate::CompletionResponse, ModelError> {
        let timeout = request.timeout;
        let stream = self.stream(request).await?;
        match time::timeout(timeout, consume_stream(stream)).await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(ModelError::Timeout {
                provider: PROVIDER.to_owned(),
                seconds: timeout.as_secs(),
            }),
        }
    }
}

impl ShellModel {
    async fn stream_inner(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionStream, ModelError> {
        let mut command = Command::new(&self.argv[0]);
        command
            .args(&self.argv[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = command.spawn().map_err(|error| ModelError::Provider {
            provider: PROVIDER.to_owned(),
            message: format!("failed to spawn {:?}: {error}", self.argv),
            retryable: false,
        })?;

        let mut stdin = child.stdin.take().ok_or_else(|| ModelError::Provider {
            provider: PROVIDER.to_owned(),
            message: "failed to open shell provider stdin".to_owned(),
            retryable: false,
        })?;

        let envelope = ShellEnvelope::from_request(&request);
        let input = serde_json::to_vec(&envelope).map_err(|error| ModelError::Provider {
            provider: PROVIDER.to_owned(),
            message: format!("failed to serialize shell request: {error}"),
            retryable: false,
        })?;
        stdin
            .write_all(&input)
            .await
            .map_err(|error| ModelError::Provider {
                provider: PROVIDER.to_owned(),
                message: format!("failed to write shell provider stdin: {error}"),
                retryable: true,
            })?;
        stdin
            .shutdown()
            .await
            .map_err(|error| ModelError::Provider {
                provider: PROVIDER.to_owned(),
                message: format!("failed to close shell provider stdin: {error}"),
                retryable: true,
            })?;
        drop(stdin);

        let stdout = child.stdout.take().ok_or_else(|| ModelError::Provider {
            provider: PROVIDER.to_owned(),
            message: "failed to open shell provider stdout".to_owned(),
            retryable: false,
        })?;
        let stderr = child.stderr.take().ok_or_else(|| ModelError::Provider {
            provider: PROVIDER.to_owned(),
            message: "failed to open shell provider stderr".to_owned(),
            retryable: false,
        })?;

        Ok(Box::pin(stream_output(stdout, stderr, child)))
    }
}

async fn consume_stream(
    mut stream: CompletionStream,
) -> Result<crate::CompletionResponse, ModelError> {
    let mut text = String::new();
    let mut tokens_in = 0u32;
    let mut tokens_out = 0u32;
    let mut finish_reason = FinishReason::Stop;
    while let Some(event) = stream.next().await {
        match event? {
            CompletionEvent::Content { text: chunk } => text.push_str(&chunk),
            CompletionEvent::End {
                tokens_in: ti,
                tokens_out: to,
                finish_reason: fr,
            } => {
                tokens_in = ti;
                tokens_out = to;
                finish_reason = fr;
            }
        }
    }
    Ok(crate::CompletionResponse {
        text,
        tokens_in,
        tokens_out,
        finish_reason,
    })
}

fn stream_output(
    stdout: tokio::process::ChildStdout,
    stderr: tokio::process::ChildStderr,
    mut child: tokio::process::Child,
) -> CompletionStream {
    let (tx, rx) = futures::channel::mpsc::channel::<Result<CompletionEvent, ModelError>>(64);
    tokio::task::spawn(async move {
        use tokio::io::AsyncBufReadExt;
        let mut reader = tokio::io::BufReader::new(stdout);
        let mut buf = Vec::<u8>::new();
        let mut saw_end = false;
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf).await {
                Ok(0) => break,
                Ok(_) => {
                    // read_until includes the \n delimiter, preserving any \r
                    let raw = String::from_utf8_lossy(&buf);
                    if let Some(text) = raw.strip_prefix(CONTENT_PREFIX) {
                        let _ = tx.clone().try_send(Ok(CompletionEvent::Content {
                            text: text.to_owned(),
                        }));
                    } else if let Some(meta) = raw.strip_prefix(META_PREFIX) {
                        saw_end = true;
                        let _ = child.wait().await;
                        let event = parse_meta(meta.trim_end_matches(['\r', '\n']));
                        let _ = tx.clone().try_send(event);
                        break;
                    } else {
                        let _ = tx.clone().try_send(Ok(CompletionEvent::Content {
                            text: raw.to_string(),
                        }));
                    }
                }
                Err(e) => {
                    let _ = tx.clone().try_send(Err(ModelError::Provider {
                        provider: PROVIDER.to_owned(),
                        message: format!("failed reading shell provider stdout: {e}"),
                        retryable: true,
                    }));
                    return;
                }
            }
        }
        // Wait for process exit and check exit code
        let status = child.wait().await;
        if let Ok(status) = status {
            if !status.success() {
                let stderr = tokio::io::BufReader::new(stderr);
                let stderr_text = stderr
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
                    provider: PROVIDER.to_owned(),
                    message: msg,
                    retryable: false,
                }));
                return;
            }
        }
        if !saw_end {
            let _ = tx.clone().try_send(Ok(CompletionEvent::End {
                tokens_in: 0,
                tokens_out: 0,
                finish_reason: FinishReason::Error,
            }));
        }
    });
    Box::pin(rx)
}

#[derive(Serialize)]
struct ShellEnvelope<'a> {
    system: Option<&'a str>,
    cached_prefix: Option<&'a str>,
    prompt: &'a str,
    max_tokens: Option<u32>,
    temperature: Option<f32>,
}

impl<'a> ShellEnvelope<'a> {
    fn from_request(request: &'a CompletionRequest) -> Self {
        Self {
            system: request.system.as_deref(),
            cached_prefix: request.cached_prefix.as_deref(),
            prompt: &request.prompt,
            max_tokens: request.max_tokens,
            temperature: request.temperature,
        }
    }
}

#[derive(Deserialize)]
struct ShellMeta {
    tokens_in: u32,
    tokens_out: u32,
    finish_reason: String,
}

fn argv_from_model_def(model_def: &ModelDef) -> Result<Vec<String>, ModelError> {
    let cli = model_def.cli().ok_or_else(|| ModelError::InvalidConfig {
        model: model_def.model().to_owned(),
        message: "shell provider requires cli until derrick-config exposes argv".to_owned(),
    })?;

    let argv = shell_words::split(cli).map_err(|error| ModelError::InvalidConfig {
        model: model_def.model().to_owned(),
        message: format!("invalid cli command: {error}"),
    })?;

    if argv.is_empty() {
        Err(ModelError::InvalidConfig {
            model: model_def.model().to_owned(),
            message: "shell provider cli must not be empty".to_owned(),
        })
    } else {
        Ok(argv)
    }
}

fn parse_meta(meta: &str) -> Result<CompletionEvent, ModelError> {
    let meta: ShellMeta = serde_json::from_str(meta).map_err(|error| ModelError::Provider {
        provider: PROVIDER.to_owned(),
        message: format!("invalid shell metadata: {error}"),
        retryable: false,
    })?;

    Ok(CompletionEvent::End {
        tokens_in: meta.tokens_in,
        tokens_out: meta.tokens_out,
        finish_reason: parse_finish_reason(&meta.finish_reason)?,
    })
}

fn parse_finish_reason(reason: &str) -> Result<FinishReason, ModelError> {
    match reason {
        "stop" => Ok(FinishReason::Stop),
        "length" => Ok(FinishReason::Length),
        "error" => Ok(FinishReason::Error),
        other => Err(ModelError::Provider {
            provider: PROVIDER.to_owned(),
            message: format!("invalid shell finish reason: {other}"),
            retryable: false,
        }),
    }
}
