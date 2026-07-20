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
    #[error(
        "Unknown name in type expression: `{name}`\n  Fix: (declare type {name})\n  or use one of the built-in type constructors: List, Option, |"
    )]
    UnknownName { name: String },
    #[error(
        "Type constructor `{head}` expects {expected} arg(s), got {actual}.\n  Fix: `({head} ...)` takes {expected} type argument(s)."
    )]
    ArityMismatch {
        head: String,
        expected: usize,
        actual: usize,
    },
    #[error(
        "Show implementation already registered for type `{name}`.\n  Why: `register_show` was called twice with the same type name.\n  Fix: remove the duplicate registration or pick a different type name."
    )]
    DuplicateShow { name: String },
}

pub struct Registry {
    types: HashMap<String, Option<Validator>>,
    aliases: HashMap<String, TypeExpr>,
    tools: HashMap<String, ToolSignature>,
    /// Bodies of `(define ...)` compound tools, keyed by name. The runtime
    /// dispatches to these when a tool call misses in the builtin native
    /// dispatch table.
    defines: HashMap<String, (Vec<Param>, Expr)>,
    /// Show implementations keyed by type name. Independent of the `types`
    /// map: a type can have a show without being a first-class registered
    /// type (useful for future dynamic types) and can be a registered type
    /// without having a show (fallback rendering applies).
    shows: HashMap<String, agnes_types::ShowFn>,
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
            shows: HashMap::new(),
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

    /// Register a `ShowFn` for a type name. Independent of `register_type`:
    /// a type can have a show without being registered as a first-class
    /// type. Conflicts are detected only against the `shows` map itself
    /// (does not use `ensure_free`).
    pub fn register_show(
        &mut self,
        name: &str,
        f: agnes_types::ShowFn,
    ) -> Result<(), RegistryError> {
        if self.shows.contains_key(name) {
            return Err(RegistryError::DuplicateShow {
                name: name.to_string(),
            });
        }
        self.shows.insert(name.to_string(), f);
        Ok(())
    }

    /// Look up a registered ShowFn by type name.
    pub fn show_of(&self, name: &agnes_types::TypeName) -> Option<agnes_types::ShowFn> {
        self.shows.get(&name.0).copied()
    }

    /// Serialize a `Value` for display, using registered ShowFns where
    /// available and built-in composition rules for `List`, `Option`
    /// (i.e. `(| T Unit)`), `Finish`, `Observation`, and `|` unions.
    /// Falls back to `serde_json::to_string_pretty` when no show is
    /// registered.
    pub fn show_value(&self, value: &agnes_types::Value) -> String {
        self.show_data(&value.data, &value.declared_type)
    }

    fn show_data(&self, data: &serde_json::Value, ty: &agnes_types::TypeExpr) -> String {
        use agnes_types::TypeExpr;
        match ty {
            TypeExpr::Named(name) => {
                if let Some(f) = self.show_of(name) {
                    f(data)
                } else {
                    serde_json::to_string_pretty(data)
                        .unwrap_or_else(|_| data.to_string())
                }
            }
            TypeExpr::App { head, args } => match head.0.as_str() {
                "Finish" | "Observation" if args.len() == 1 => {
                    // Transparent: render inner.
                    self.show_data(data, &args[0])
                }
                "List" if args.len() == 1 => {
                    let inner = &args[0];
                    let arr = match data.as_array() {
                        Some(a) => a,
                        None => {
                            return serde_json::to_string_pretty(data)
                                .unwrap_or_else(|_| data.to_string());
                        }
                    };
                    let parts: Vec<String> =
                        arr.iter().map(|el| self.show_data(el, inner)).collect();
                    format!("[{}]", parts.join(", "))
                }
                "|" => {
                    // Union: if any arg is "Unit" and data is null, render as empty
                    // string (Option-None case). Otherwise, pick the first non-Unit
                    // member and render with that. This is a best-effort fallback:
                    // unions with heterogeneous shapes may render imperfectly.
                    if data.is_null()
                        && args.iter().any(|a| matches!(a, TypeExpr::Named(n) if n.0 == "Unit"))
                    {
                        return String::new();
                    }
                    // Pick the first non-Unit member.
                    for a in args {
                        if let TypeExpr::Named(n) = a && n.0 != "Unit" {
                            return self.show_data(data, a);
                        }
                    }
                    // Only Unit(s): empty.
                    String::new()
                }
                _ => {
                    // Unknown App head: try outer registered show, else pretty JSON.
                    if let Some(f) = self.show_of(head) {
                        f(data)
                    } else {
                        serde_json::to_string_pretty(data)
                            .unwrap_or_else(|_| data.to_string())
                    }
                }
            },
        }
    }

    pub fn validator_of(&self, ty: &TypeName) -> Option<Validator> {
        self.types.get(&ty.0).and_then(|v| *v)
    }

    /// Resolve a syntactic TypeExprAst into a canonical TypeExpr.
    /// Recognizes `Named`, `App { head: "|" | "List" | "Option" | "Finish" | "Observation", ... }`.
    /// Any other App head fails with
    /// `UnknownName` (the message points at the built-in constructors).
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
            TypeExprAst::App { head, args } if head == "Option" => {
                if args.len() != 1 {
                    return Err(RegistryError::ArityMismatch {
                        head: "Option".into(),
                        expected: 1,
                        actual: args.len(),
                    });
                }
                let inner = self.resolve(&args[0])?;
                let unit = self.resolve(&TypeExprAst::Named("Unit".into()))?;
                Ok(canonicalize_union([inner, unit]))
            }
            TypeExprAst::App { head, args } if head == "List" => {
                if args.len() != 1 {
                    return Err(RegistryError::ArityMismatch {
                        head: "List".into(),
                        expected: 1,
                        actual: args.len(),
                    });
                }
                let inner = self.resolve(&args[0])?;
                Ok(TypeExpr::App {
                    head: TypeName("List".into()),
                    args: vec![inner],
                })
            }
            TypeExprAst::App { head, args } if head == "Finish" => {
                if args.len() != 1 {
                    return Err(RegistryError::ArityMismatch {
                        head: "Finish".into(),
                        expected: 1,
                        actual: args.len(),
                    });
                }
                let inner = self.resolve(&args[0])?;
                Ok(TypeExpr::App {
                    head: TypeName("Finish".into()),
                    args: vec![inner],
                })
            }
            TypeExprAst::App { head, args } if head == "Observation" => {
                if args.len() != 1 {
                    return Err(RegistryError::ArityMismatch {
                        head: "Observation".into(),
                        expected: 1,
                        actual: args.len(),
                    });
                }
                let inner = self.resolve(&args[0])?;
                Ok(TypeExpr::App {
                    head: TypeName("Observation".into()),
                    args: vec![inner],
                })
            }
            TypeExprAst::App { head, .. } => Err(RegistryError::UnknownName { name: head.clone() }),
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
