//! Type / alias / tool registry with strict name-uniqueness enforcement.

pub mod loader;

use std::collections::{HashMap, HashSet};
use std::fmt;

use agnes_ast::{Program, TopLevel, TypeExprAst};
use agnes_types::{ToolSignature, TypeExpr, TypeName, Validator};

#[derive(Debug, Clone, PartialEq)]
pub enum EntryKind {
    Type,
    Alias,
    Tool,
}

impl fmt::Display for EntryKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EntryKind::Type  => write!(f, "type"),
            EntryKind::Alias => write!(f, "type alias"),
            EntryKind::Tool  => write!(f, "tool"),
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
}

impl Default for Registry {
    fn default() -> Self { Self::new() }
}

impl Registry {
    pub fn new() -> Self {
        Self {
            types: HashMap::new(),
            aliases: HashMap::new(),
            tools: HashMap::new(),
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
        if let EntryKind::Tool = attempted {
            if self.tools.contains_key(name) {
                return Err(RegistryError::NameConflict {
                    name: name.into(),
                    existing_kind: EntryKind::Tool,
                    attempted_kind: attempted,
                });
            }
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

    pub fn validator_of(&self, ty: &TypeName) -> Option<Validator> {
        self.types.get(&ty.0).and_then(|v| *v)
    }

    /// Resolve a syntactic TypeExprAst into a canonical TypeExpr.
    /// - Named that refers to an alias -> the alias's TypeExpr
    /// - Named that refers to a type -> Named
    /// - Named that is unknown -> UnknownName error
    /// - Union -> flat union of resolved members
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
            TypeExprAst::Union(members) => {
                let mut set: HashSet<TypeName> = HashSet::new();
                for m in members {
                    let resolved = self.resolve(m)?;
                    for t in resolved.as_set() { set.insert(t); }
                }
                if set.len() == 1 {
                    Ok(TypeExpr::Named(set.into_iter().next().unwrap()))
                } else {
                    Ok(TypeExpr::Union(set))
                }
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
    program.toplevels.iter().filter(|t| matches!(t, TopLevel::Define { .. })).collect()
}
