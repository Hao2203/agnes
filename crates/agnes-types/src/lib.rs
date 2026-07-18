//! Semantic type system for agnes.

use serde_json::Value as JsonValue;
use std::collections::HashSet;
use std::fmt;

/// Canonical name of a type or type alias. PascalCase by convention.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TypeName(pub String);

impl fmt::Display for TypeName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Canonicalized type expression. `Union` is always non-empty and flat
/// (no nested unions, aliases already resolved).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeExpr {
    Named(TypeName),
    Union(HashSet<TypeName>),
}

impl TypeExpr {
    /// Flatten to a set of concrete type names.
    pub fn as_set(&self) -> HashSet<TypeName> {
        match self {
            TypeExpr::Named(n) => {
                let mut s = HashSet::new();
                s.insert(n.clone());
                s
            }
            TypeExpr::Union(s) => s.clone(),
        }
    }
}

impl fmt::Display for TypeExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TypeExpr::Named(n) => write!(f, "{n}"),
            TypeExpr::Union(members) => {
                let mut names: Vec<&str> = members.iter().map(|t| t.0.as_str()).collect();
                names.sort();
                write!(f, "({})", names.join(" | "))
            }
        }
    }
}

/// Runtime type validator. Structural check only, no semantic guessing.
/// Returns `Ok(())` on pass, `Err(reason)` on fail.
pub type Validator = fn(&JsonValue) -> Result<(), String>;

/// Tool signature after registry resolution. Both `requires` items and
/// `provides` are canonicalized.
#[derive(Debug, Clone)]
pub struct ToolSignature {
    pub requires: Vec<(String, TypeExpr)>,
    pub provides: TypeExpr,
}

/// A value flowing between tools at runtime. Carries the type declared
/// by the producing tool for boundary validation on the consuming end.
#[derive(Debug, Clone)]
pub struct Value {
    pub data: JsonValue,
    pub declared_type: TypeName,
}

/// Rule primitive: does `actual` satisfy `expected`?
/// This is a set-membership test — the checker's only decision procedure.
pub fn type_expr_matches(actual: &TypeName, expected: &TypeExpr) -> bool {
    match expected {
        TypeExpr::Named(n) => n == actual,
        TypeExpr::Union(members) => members.contains(actual),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_matches_named() {
        let expected = TypeExpr::Named(TypeName("PlainText".into()));
        assert!(type_expr_matches(&TypeName("PlainText".into()), &expected));
        assert!(!type_expr_matches(&TypeName("PDF".into()), &expected));
    }

    #[test]
    fn union_contains_member() {
        let mut set = std::collections::HashSet::new();
        set.insert(TypeName("PlainText".into()));
        set.insert(TypeName("Markdown".into()));
        let expected = TypeExpr::Union(set);
        assert!(type_expr_matches(&TypeName("Markdown".into()), &expected));
        assert!(!type_expr_matches(&TypeName("PDF".into()), &expected));
    }

    #[test]
    fn utf8_validator_accepts_valid_string() {
        let v = |json: &serde_json::Value| -> Result<(), String> {
            match json.as_str() {
                Some(s) if !s.as_bytes().is_empty() && std::str::from_utf8(s.as_bytes()).is_ok() => Ok(()),
                Some(_) => Err("empty".into()),
                None => Err("not a string".into()),
            }
        };
        assert!(v(&serde_json::json!("hello")).is_ok());
        assert!(v(&serde_json::json!(42)).is_err());
    }
}
