//! Type / alias / tool registry with strict name-uniqueness enforcement.

pub mod loader;

use std::collections::HashMap;
use std::fmt;

use agnes_ast::{Expr, Param, Program, TopLevel, TypeExprAst};
use agnes_types::{ToolSignature, TypeExpr, TypeName, Validator, canonicalize_union};

#[derive(Debug, Clone, PartialEq)]
pub enum EntryKind {
    Type,
    Alias,
    Tool,
}

impl fmt::Display for EntryKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EntryKind::Type => write!(f, "type"),
            EntryKind::Alias => write!(f, "type alias"),
            EntryKind::Tool => write!(f, "tool"),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error(
        "Name conflict: `{name}` is already registered as a {existing_kind}\n  attempted to register as: {attempted_kind}\n  suggestion: rename to `{name}V2` or choose a different name"
    )]
    NameConflict {
        name: String,
        existing_kind: EntryKind,
        attempted_kind: EntryKind,
    },
    #[error("Unknown name in type expression: `{name}`\n  Fix: (declare type {name})")]
    UnknownName { name: String },
}

pub struct Registry {
    types: HashMap<String, Option<Validator>>,
    aliases: HashMap<String, TypeExpr>,
    tools: HashMap<String, ToolSignature>,
    /// Bodies of `(define ...)` compound tools, keyed by name. The runtime
    /// dispatches to these when a tool call misses in the builtin native
    /// dispatch table.
    defines: HashMap<String, (Vec<Param>, Expr)>,
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl Registry {
    pub fn new() -> Self {
        Self {
            types: HashMap::new(),
            aliases: HashMap::new(),
            tools: HashMap::new(),
            defines: HashMap::new(),
        }
    }

    fn ensure_free(&self, name: &str, attempted: EntryKind) -> Result<(), RegistryError> {
        if self.types.contains_key(name) {
            return Err(RegistryError::NameConflict {
                name: name.into(),
                existing_kind: EntryKind::Type,
                attempted_kind: attempted,
            });
        }
        if self.aliases.contains_key(name) {
            return Err(RegistryError::NameConflict {
                name: name.into(),
                existing_kind: EntryKind::Alias,
                attempted_kind: attempted,
            });
        }
        // Tools live in a separate namespace from types/aliases (tool names
        // and type names cannot collide in practice — types are PascalCase,
        // tools are kebab-case — but we still forbid duplicate tool names).
        if let EntryKind::Tool = attempted
            && self.tools.contains_key(name)
        {
            return Err(RegistryError::NameConflict {
                name: name.into(),
                existing_kind: EntryKind::Tool,
                attempted_kind: attempted,
            });
        }
        Ok(())
    }

    pub fn register_type(&mut self, name: &str, v: Option<Validator>) -> Result<(), RegistryError> {
        self.ensure_free(name, EntryKind::Type)?;
        self.types.insert(name.to_string(), v);
        Ok(())
    }

    pub fn register_alias(&mut self, name: &str, expr: TypeExpr) -> Result<(), RegistryError> {
        self.ensure_free(name, EntryKind::Alias)?;
        self.aliases.insert(name.to_string(), expr);
        Ok(())
    }

    pub fn register_tool(&mut self, name: &str, sig: ToolSignature) -> Result<(), RegistryError> {
        self.ensure_free(name, EntryKind::Tool)?;
        self.tools.insert(name.to_string(), sig);
        Ok(())
    }

    /// Replace an existing tool signature (used when a `declare tool` overrides
    /// a previously-registered builtin or when a `define` re-registers itself).
    pub fn override_tool(&mut self, name: &str, sig: ToolSignature) {
        self.tools.insert(name.to_string(), sig);
    }

    pub fn tool_signature(&self, name: &str) -> Option<&ToolSignature> {
        self.tools.get(name)
    }

    /// Store the body of a `(define ...)` compound tool. Called by the loader
    /// alongside `override_tool` so both the checker (which needs the tool
    /// signature) and the runtime (which needs the body) can see it.
    pub fn register_define(&mut self, name: &str, params: Vec<Param>, body: Expr) {
        self.defines.insert(name.to_string(), (params, body));
    }

    /// Look up the body of a `(define ...)` compound tool for runtime dispatch.
    pub fn define_body(&self, name: &str) -> Option<&(Vec<Param>, Expr)> {
        self.defines.get(name)
    }

    pub fn validator_of(&self, ty: &TypeName) -> Option<Validator> {
        self.types.get(&ty.0).and_then(|v| *v)
    }

    /// Resolve a syntactic TypeExprAst into a canonical TypeExpr.
    /// This task's scope: only `Named` and `App { head: "|", ... }` are
    /// handled semantically. Any other App head is rejected with
    /// UnknownName — Task 6 adds List / Option / arity checks.
    pub fn resolve(&self, ast: &TypeExprAst) -> Result<TypeExpr, RegistryError> {
        match ast {
            TypeExprAst::Named(n) => {
                if let Some(alias) = self.aliases.get(n) {
                    Ok(alias.clone())
                } else if self.types.contains_key(n) {
                    Ok(TypeExpr::Named(TypeName(n.clone())))
                } else {
                    Err(RegistryError::UnknownName { name: n.clone() })
                }
            }
            TypeExprAst::App { head, args } if head == "|" => {
                let mut resolved: Vec<TypeExpr> = Vec::with_capacity(args.len());
                for m in args {
                    resolved.push(self.resolve(m)?);
                }
                Ok(canonicalize_union(resolved))
            }
            TypeExprAst::App { head, .. } => {
                // List/Option and other constructors land in Task 6.
                Err(RegistryError::UnknownName { name: head.clone() })
            }
        }
    }

    /// Apply every non-`define` top-level to the registry. `define`s are
    /// intentionally NOT applied here — the compiler handles them (and their
    /// tool signatures) after checking, so cyclic defines are caught by the
    /// compiler's cycle detector rather than being silently registered.
    pub fn load(&mut self, program: &Program) -> Result<(), RegistryError> {
        loader::load(self, program)
    }
}

/// Iterator helper for the compiler: yield only the `Define` top-levels.
pub fn defines_of(program: &Program) -> Vec<&TopLevel> {
    program
        .toplevels
        .iter()
        .filter(|t| matches!(t, TopLevel::Define { .. }))
        .collect()
}
