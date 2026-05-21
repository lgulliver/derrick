//! `opencode` provider.
//!
//! Shells to the `opencode` CLI (default `opencode run`). `opencode`
//! does not currently expose a stable HTTP surface, so this provider
//! always uses the CLI path. The `OPENCODE_API_KEY` credential is
//! looked up via D12 conventions but is treated as host-managed —
//! we do not embed it in the spawned command (the host CLI reads it
//! from its own environment).

use async_trait::async_trait;
use derrick_config::ModelDef;

use crate::providers::subprocess::{parse_argv, stream_subprocess, SubprocessSpec};
use crate::{
    builtin_cost_hint, AuthStore, CompletionRequest, CompletionStream, CostHint, Model, ModelError,
};

const PROVIDER: &str = "opencode";
const DEFAULT_CLI: &str = "opencode run";

pub(crate) fn build(model_def: &ModelDef, _auth: &AuthStore) -> Result<Box<dyn Model>, ModelError> {
    let cli = model_def
        .cli()
        .map(str::to_owned)
        .unwrap_or_else(|| DEFAULT_CLI.to_owned());

    let model_name = model_def.model().to_owned();
    let cost_hint = builtin_cost_hint(&model_name);

    Ok(Box::new(OpencodeModel {
        name: model_name,
        cli,
        cost_hint,
    }))
}

struct OpencodeModel {
    name: String,
    cli: String,
    cost_hint: Option<CostHint>,
}

#[async_trait]
impl Model for OpencodeModel {
    fn name(&self) -> &str {
        &self.name
    }

    fn provider(&self) -> &str {
        PROVIDER
    }

    fn cost_hint(&self) -> Option<&CostHint> {
        self.cost_hint.as_ref()
    }

    fn host_delegated_auth(&self) -> bool {
        true
    }

    async fn stream(&self, request: CompletionRequest) -> Result<CompletionStream, ModelError> {
        let argv = parse_argv(PROVIDER, &self.name, &self.cli)?;
        let mut payload = String::new();
        if let Some(system) = &request.system {
            payload.push_str(system);
            payload.push_str("\n\n");
        }
        if let Some(prefix) = &request.cached_prefix {
            payload.push_str(prefix);
            payload.push_str("\n\n");
        }
        payload.push_str(&request.prompt);

        stream_subprocess(SubprocessSpec {
            provider: PROVIDER,
            argv,
            stdin_payload: payload,
        })
        .await
    }
}
