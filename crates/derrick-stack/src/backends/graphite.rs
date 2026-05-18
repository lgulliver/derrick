//! Graphite stacking backend (v1 stub).
//!
//! All operations return [`StackError::NotSupported`] with a remediation
//! note pointing at `gt restack`. A future ticket will swap this for a real
//! `gt` invocation.

use std::path::Path;

use async_trait::async_trait;

use crate::{OpenPrParams, PrInfo, RestackOutcome, RestackParams, StackBackend, StackError};

/// Graphite stub backend.
#[derive(Clone, Copy, Debug, Default)]
pub struct GraphiteStackBackend;

const NOT_IMPL: &str = "not implemented in v1; run 'gt restack' manually";

#[async_trait]
impl StackBackend for GraphiteStackBackend {
    fn kind(&self) -> &'static str {
        "graphite"
    }

    async fn open_pr(&self, _params: OpenPrParams) -> Result<PrInfo, StackError> {
        Err(StackError::NotSupported {
            backend: "graphite",
            reason: NOT_IMPL,
        })
    }

    async fn restack(&self, _params: RestackParams) -> Result<RestackOutcome, StackError> {
        Err(StackError::NotSupported {
            backend: "graphite",
            reason: NOT_IMPL,
        })
    }

    async fn force_push(&self, _branch: &str, _repo_root: &Path) -> Result<(), StackError> {
        Err(StackError::NotSupported {
            backend: "graphite",
            reason: NOT_IMPL,
        })
    }
}
