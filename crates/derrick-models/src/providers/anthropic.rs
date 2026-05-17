use derrick_config::ModelDef;

use crate::{AuthStore, Model, ModelError};

pub(crate) fn build(
    _model_def: &ModelDef,
    _auth: &AuthStore,
) -> Result<Box<dyn Model>, ModelError> {
    Err(ModelError::Provider {
        provider: "anthropic".to_owned(),
        message: "not implemented in T006; see T006a".to_owned(),
        retryable: false,
    })
}
