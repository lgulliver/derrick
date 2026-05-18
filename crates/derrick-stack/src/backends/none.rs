//! No-op stacking backend.

use std::path::Path;

use async_trait::async_trait;

use crate::{OpenPrParams, PrInfo, RestackOutcome, RestackParams, StackBackend, StackError};

/// Stacking disabled. `open_pr` returns [`StackError::NotSupported`];
/// restack and force-push are no-ops so callers in mixed-mode can blindly
/// invoke them.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoneStackBackend;

#[async_trait]
impl StackBackend for NoneStackBackend {
    fn kind(&self) -> &'static str {
        "none"
    }

    async fn open_pr(&self, _params: OpenPrParams) -> Result<PrInfo, StackError> {
        Err(StackError::NotSupported {
            backend: "none",
            reason: "stacking disabled",
        })
    }

    async fn restack(&self, _params: RestackParams) -> Result<RestackOutcome, StackError> {
        Ok(RestackOutcome::Restacked)
    }

    async fn force_push(&self, _branch: &str, _repo_root: &Path) -> Result<(), StackError> {
        Ok(())
    }
}
