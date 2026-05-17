use std::process::Stdio;

use async_trait::async_trait;
use derrick_config::ModelDef;
use futures::stream;
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
        let timeout = request.timeout;
        let events = match time::timeout(timeout, self.invoke(request)).await {
            Ok(result) => result?,
            Err(_) => {
                return Err(ModelError::Timeout {
                    provider: PROVIDER.to_owned(),
                    seconds: timeout.as_secs(),
                });
            }
        };

        Ok(Box::pin(stream::iter(events.into_iter().map(Ok))))
    }
}

impl ShellModel {
    async fn invoke(&self, request: CompletionRequest) -> Result<Vec<CompletionEvent>, ModelError> {
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

        let output = child
            .wait_with_output()
            .await
            .map_err(|error| ModelError::Provider {
                provider: PROVIDER.to_owned(),
                message: format!("failed waiting for shell provider: {error}"),
                retryable: true,
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(ModelError::Provider {
                provider: PROVIDER.to_owned(),
                message: format!(
                    "process exited with {}; stderr: {}",
                    output.status,
                    stderr.trim()
                ),
                retryable: false,
            });
        }

        parse_stdout(&output.stdout)
    }
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

fn parse_stdout(stdout: &[u8]) -> Result<Vec<CompletionEvent>, ModelError> {
    let output = String::from_utf8_lossy(stdout);
    let mut events = Vec::new();
    let mut saw_end = false;

    for line in split_inclusive_lines(&output) {
        if let Some(text) = line.strip_prefix(CONTENT_PREFIX) {
            events.push(CompletionEvent::Content {
                text: text.to_owned(),
            });
        } else if let Some(meta) = line.strip_prefix(META_PREFIX) {
            events.push(parse_meta(meta.trim_end_matches(['\r', '\n']))?);
            saw_end = true;
            break;
        } else {
            events.push(CompletionEvent::Content {
                text: line.to_owned(),
            });
        }
    }

    if !saw_end {
        events.push(CompletionEvent::End {
            tokens_in: 0,
            tokens_out: 0,
            finish_reason: FinishReason::Error,
        });
    }

    Ok(events)
}

fn split_inclusive_lines(output: &str) -> impl Iterator<Item = &str> {
    output.split_inclusive('\n')
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
