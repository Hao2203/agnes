use agnes_types::TypeExpr;

#[derive(Debug, thiserror::Error)]
pub enum CheckError {
    #[error(
        "Type error at (tool {tool} :{param} <arg>):
  parameter `{param}` requires one of: {expected}
  but got type: {actual}

Fix suggestion (one of):
  A) Change the argument's source to produce one of the accepted types
  B) Extend {tool} to accept {actual}:
     (declare tool {tool} :requires [({param} (| {expected} {actual})) ...] ...)"
    )]
    ParamMismatch {
        tool: String,
        param: String,
        expected: Box<TypeExpr>,
        actual: Box<TypeExpr>,
    },

    #[error(
        "Type error at (pipe ... (tool {downstream_tool}) ...):
  step `{downstream_tool}` requires one of: {expected}
  but upstream step `{upstream}` provides: {actual}

Fix suggestion (one of):
  A) Insert a converting tool between them
  B) Extend {downstream_tool} to accept {actual}"
    )]
    FlowMismatch {
        upstream: String,
        downstream_tool: String,
        expected: Box<TypeExpr>,
        actual: Box<TypeExpr>,
    },

    #[error(
        "Unknown tool `{name}` at call site.

Fix suggestion (paste at top of file):
  (declare tool {name} :requires [...] :provides <TypeExpr>)"
    )]
    UnknownTool { name: String },

    #[error(
        "Unknown variable `{name}` in expression.
  Was it introduced with (let {name} ...) earlier in scope?"
    )]
    UnknownVar { name: String },

    #[error(
        "Define `{name}` body provides type {body_type} which does not satisfy declared :provides {declared}"
    )]
    DefineSignatureMismatch {
        name: String,
        declared: Box<TypeExpr>,
        body_type: Box<TypeExpr>,
    },

    #[error(transparent)]
    Registry(#[from] agnes_registry::RegistryError),
}
