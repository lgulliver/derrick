//! Host-delegated provider (D65).
//!
//! A single `Model` implementation that routes a completion through one of the
//! five host CLIs (`claude`, `codex`, `copilot`, `opencode`, `aider`) via the
//! `derrick-tools` host adapters. It builds a one-shot [`HostRequest`] from a
//! [`CompletionRequest`], awaits the adapter's `run()`, and wraps the single
//! [`HostResponse`] into a one-shot completion stream (`Content` then `End`
//! with the host-reported token counts).
//!
//! There is no API key path: the host CLI manages its own auth. Any env vars
//! present in the [`AuthStore`] (e.g. `GH_TOKEN`, proxy vars) are forwarded to
//! the child process so the host can pick them up.

use std::collections::HashMap;
use std::env;
use std::sync::Arc;

use async_trait::async_trait;
use derrick_config::ModelDef;
use derrick_tools::{CopilotToolPermission, HostAdapter, HostError, HostRequest, HostResponse};

use crate::{
    builtin_cost_hint, AuthStore, CompletionEvent, CompletionRequest, CompletionStream, CostHint,
    FinishReason, Model, ModelError,
};

/// A model whose inference is delegated to a `derrick-tools` host adapter.
pub(crate) struct HostDelegatedModel {
    name: String,
    host: &'static str,
    adapter: Arc<dyn HostAdapter>,
    cost_hint: Option<CostHint>,
    env: HashMap<String, String>,
}

/// Builds a host-delegated model for `host` using `adapter`.
///
/// The configured `model_def.model()` becomes the model id passed RAW to the
/// adapter (the adapter normalises it per host). Auth is env-passthrough only.
pub(crate) fn build_for_host(
    host: &'static str,
    adapter: Arc<dyn HostAdapter>,
    model_def: &ModelDef,
    auth: &AuthStore,
) -> Result<Box<dyn Model>, ModelError> {
    let name = model_def.model().to_owned();
    let cost_hint = builtin_cost_hint(&name);
    Ok(Box::new(HostDelegatedModel {
        name,
        host,
        adapter,
        cost_hint,
        env: auth.env_map(),
    }))
}

impl HostDelegatedModel {
    fn build_host_request(&self, request: &CompletionRequest) -> HostRequest {
        let mut prompt = String::new();
        for part in [
            request.system.as_deref(),
            request.cached_prefix.as_deref(),
            Some(request.prompt.as_str()),
        ]
        .into_iter()
        .flatten()
        .filter(|part| !part.is_empty())
        {
            if !prompt.is_empty() {
                prompt.push_str("\n\n");
            }
            prompt.push_str(part);
        }

        let cwd = env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));

        HostRequest {
            prompt,
            cwd,
            timeout: request.timeout,
            env: self.env.clone(),
            copilot_tools: CopilotToolPermission::AllowAll,
            // Raw model id; the adapter normalises it per host.
            model: Some(self.name.clone()),
            headless: true,
            output_sink: None,
        }
    }
}

/// Classifies an [`std::io::ErrorKind`] from a host spawn/IO failure as
/// retryable or not.
///
/// Spawn/setup failures (`PermissionDenied`, `NotFound`, and the like) are
/// permanent: re-running the same host invocation will fail identically, so the
/// flow runner must bail rather than burn retries. Only genuinely transient
/// kinds — a signal interrupting the syscall, a non-blocking resource being
/// momentarily unavailable, or a timeout — are worth retrying.
fn io_kind_is_retryable(kind: std::io::ErrorKind) -> bool {
    use std::io::ErrorKind;
    matches!(
        kind,
        ErrorKind::Interrupted
            | ErrorKind::WouldBlock
            | ErrorKind::TimedOut
            | ErrorKind::BrokenPipe
            | ErrorKind::UnexpectedEof
    )
}

/// Maps a host adapter error to a typed [`ModelError`] with retry classification.
fn map_host_error(host: &str, error: HostError) -> ModelError {
    match error {
        HostError::Timeout { seconds, .. } => ModelError::Timeout {
            provider: host.to_owned(),
            seconds,
        },
        HostError::NotFound { .. } => ModelError::Provider {
            provider: host.to_owned(),
            message: format!("host binary not found on PATH: {host}"),
            retryable: false,
        },
        HostError::NonZeroExit {
            exit_code, stderr, ..
        } => ModelError::Provider {
            provider: host.to_owned(),
            message: format!("host {host} exited with code {exit_code}: {stderr}"),
            retryable: false,
        },
        HostError::Io { source, .. } => ModelError::Provider {
            provider: host.to_owned(),
            message: format!("io error invoking host {host}: {source}"),
            retryable: io_kind_is_retryable(source.kind()),
        },
        // `HostError` is `#[non_exhaustive]`; treat any future variant as a
        // non-retryable provider error rather than failing to compile.
        other => ModelError::Provider {
            provider: host.to_owned(),
            message: format!("host {host} error: {other}"),
            retryable: false,
        },
    }
}

#[async_trait]
impl Model for HostDelegatedModel {
    fn name(&self) -> &str {
        &self.name
    }

    fn provider(&self) -> &str {
        self.host
    }

    fn cost_hint(&self) -> Option<&CostHint> {
        self.cost_hint.as_ref()
    }

    fn host_delegated_auth(&self) -> bool {
        true
    }

    async fn stream(&self, request: CompletionRequest) -> Result<CompletionStream, ModelError> {
        let host_req = self.build_host_request(&request);
        // Await run() before building the stream so spawn/NotFound errors
        // surface from stream() rather than being deferred into the stream.
        let HostResponse {
            stdout,
            tokens_in,
            tokens_out,
            ..
        } = self
            .adapter
            .run(host_req)
            .await
            .map_err(|error| map_host_error(self.host, error))?;

        let events = vec![
            Ok(CompletionEvent::Content { text: stdout }),
            Ok(CompletionEvent::End {
                tokens_in,
                tokens_out,
                finish_reason: FinishReason::Stop,
            }),
        ];
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Error as IoError, ErrorKind};

    fn io_error(host: &str, kind: ErrorKind) -> ModelError {
        map_host_error(
            host,
            HostError::Io {
                host: host.to_owned(),
                source: IoError::from(kind),
            },
        )
    }

    #[test]
    fn permanent_io_kinds_are_not_retryable() {
        for kind in [
            ErrorKind::PermissionDenied,
            ErrorKind::NotFound,
            ErrorKind::AlreadyExists,
            ErrorKind::InvalidInput,
        ] {
            assert!(
                !io_error("claude", kind).is_retryable(),
                "{kind:?} must be non-retryable"
            );
        }
    }

    #[test]
    fn transient_io_kinds_are_retryable() {
        for kind in [
            ErrorKind::Interrupted,
            ErrorKind::WouldBlock,
            ErrorKind::TimedOut,
            ErrorKind::BrokenPipe,
            ErrorKind::UnexpectedEof,
        ] {
            assert!(
                io_error("codex", kind).is_retryable(),
                "{kind:?} must be retryable"
            );
        }
    }
}
