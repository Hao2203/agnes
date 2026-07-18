use agnes_registry::Registry;
use agnes_types::{TypeName, Value};

use crate::error::RuntimeError;

pub fn validate(
    reg: &Registry,
    tool: &str,
    direction: &'static str,
    ty: &TypeName,
    val: &Value,
) -> Result<(), RuntimeError> {
    if let Some(v) = reg.validator_of(ty) {
        v(&val.data).map_err(|cause| RuntimeError::RuntimeTypeError {
            tool: tool.to_string(),
            direction,
            ty: ty.clone(),
            cause,
        })?;
    }
    Ok(())
}
