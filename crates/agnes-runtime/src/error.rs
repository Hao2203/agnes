use agnes_types::TypeName;

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("Tool `{tool}` failed: {cause}")]
    ToolFailed { tool: String, cause: String },

    #[error(
        "Runtime type error at (tool {tool} {direction}):
  step `{tool}` declared: {direction} {ty}
  but value fails {ty} validator: {cause}

Fix suggestion:
  Either fix the tool implementation to match its declared type,
  or re-declare its signature to match reality:
    (declare tool {tool} :requires [...] :provides <TypeExpr>)"
    )]
    RuntimeTypeError {
        tool: String,
        direction: &'static str,
        ty: TypeName,
        cause: String,
    },

    #[error("No native implementation registered for tool `{tool}`")]
    MissingImpl { tool: String },
}
