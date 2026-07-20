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

impl From<&str> for TypeName {
    fn from(s: &str) -> Self {
        TypeName(s.to_string())
    }
}

impl From<String> for TypeName {
    fn from(s: String) -> Self {
        TypeName(s)
    }
}

/// Canonicalized type expression. There are exactly two shapes:
/// - `Named(TypeName)`  — an atomic type name
/// - `App { head, args }` — a constructor application. `head == "|"` for
///   unions (args are all `Named`, deduplicated, alphabetical); other heads
///   (`"List"`, and future container heads) hold `TypeExpr` args recursively.
///
/// `(Option T)` never appears in canonical form — the registry expands it
/// to `App { head: "|", args: [T, Unit] }` at resolve time.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TypeExpr {
    Named(TypeName),
    App { head: TypeName, args: Vec<TypeExpr> },
}

impl TypeExpr {
    /// Convenience: build a Named from anything Into<TypeName>.
    pub fn named(name: impl Into<TypeName>) -> Self {
        TypeExpr::Named(name.into())
    }
}

impl fmt::Display for TypeExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TypeExpr::Named(n) => write!(f, "{n}"),
            TypeExpr::App { head, args } => {
                write!(f, "({head}")?;
                for a in args {
                    write!(f, " {a}")?;
                }
                write!(f, ")")
            }
        }
    }
}

/// Canonicalize a set of union members: flatten nested `(| ...)`, deduplicate,
/// sort by Display for stable canonical order, and collapse a singleton to `Named`.
/// Nested `(| ...)` members are flattened into the enclosing union; non-`|`
/// members (Named or non-union `App`, e.g. `(List String)`) are preserved as-is.
///
/// The result is either `Named(n)` (if 1 unique element) or
/// `App { head: TypeName("|"), args: sorted_unique }`.
pub fn canonicalize_union(members: impl IntoIterator<Item = TypeExpr>) -> TypeExpr {
    let mut set: HashSet<TypeExpr> = HashSet::new();
    for m in members {
        match m {
            TypeExpr::App { ref head, ref args } if head.0 == "|" => {
                for inner in args.iter().cloned() {
                    set.insert(inner);
                }
            }
            other => {
                set.insert(other);
            }
        }
    }
    let mut vec: Vec<TypeExpr> = set.into_iter().collect();
    // Sort by Display for a stable canonical order.
    vec.sort_by_key(|a| a.to_string());
    if vec.len() == 1 {
        vec.into_iter().next().unwrap()
    } else {
        TypeExpr::App {
            head: TypeName("|".into()),
            args: vec,
        }
    }
}

/// Runtime type validator. Structural check only, no semantic guessing.
pub type Validator = fn(&JsonValue) -> Result<(), String>;

/// Function type for a `Show` implementation: takes a JSON value produced
/// by some tool call and renders it into a human/LLM-readable string.
///
/// Registered in `agnes-registry` via `Registry::register_show`. Used by
/// `Session::run_turn` at the end of each iteration to serialize the
/// returned `Value` for either the user (Finish path) or the LLM
/// (Observation path).
pub type ShowFn = fn(&JsonValue) -> String;

/// Tool signature after registry resolution.
#[derive(Debug, Clone)]
pub struct ToolSignature {
    pub requires: Vec<(String, TypeExpr)>,
    pub provides: TypeExpr,
}

/// A value flowing between tools at runtime. Carries the type declared by
/// the producing tool for boundary validation.
///
/// For list literals whose static provides is `(List Unknown)` — e.g. lists
/// of variables whose types aren't propagated through the compiler — the
/// runtime scheduler narrows `declared_type` to the actual observed element
/// types. This lets boundary validation match against the concrete inner
/// union of the expected type.
#[derive(Debug, Clone)]
pub struct Value {
    pub data: JsonValue,
    pub declared_type: TypeExpr,
}

impl Value {
    /// Convenience constructor for values with an atomic named type.
    pub fn typed(data: JsonValue, ty: impl Into<TypeName>) -> Self {
        Self {
            data,
            declared_type: TypeExpr::Named(ty.into()),
        }
    }

    /// Constructor for values with a parameterized type (e.g. `(List String)`).
    pub fn typed_expr(data: JsonValue, ty: TypeExpr) -> Self {
        Self {
            data,
            declared_type: ty,
        }
    }
}

/// Recursive matching. `actual` satisfies `expected` if:
/// - both are `Named` with the same name, OR
/// - `expected` is a `|` union and any member matches `actual`, OR
/// - both are same-head `App`s of equal arity and args match position-wise.
///
/// The recursion enters union expansion at every level of `expected` — so
/// `(List String)` matches `(List (| String Int))` because `String` matches
/// `(| String Int)`.
pub fn type_expr_matches(actual: &TypeExpr, expected: &TypeExpr) -> bool {
    match (actual, expected) {
        (TypeExpr::Named(a), TypeExpr::Named(b)) => a == b,
        (_, TypeExpr::App { head, args }) if head.0 == "|" => {
            args.iter().any(|u| type_expr_matches(actual, u))
        }
        (TypeExpr::App { head: h1, args: a1 }, TypeExpr::App { head: h2, args: a2 })
            if h1 == h2 && a1.len() == a2.len() =>
        {
            a1.iter()
                .zip(a2.iter())
                .all(|(x, y)| type_expr_matches(x, y))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_matches_named() {
        let expected = TypeExpr::named("PlainText");
        assert!(type_expr_matches(&TypeExpr::named("PlainText"), &expected));
        assert!(!type_expr_matches(&TypeExpr::named("PDF"), &expected));
    }

    #[test]
    fn union_contains_member() {
        let expected =
            canonicalize_union([TypeExpr::named("PlainText"), TypeExpr::named("Markdown")]);
        assert!(type_expr_matches(&TypeExpr::named("Markdown"), &expected));
        assert!(!type_expr_matches(&TypeExpr::named("PDF"), &expected));
    }

    #[test]
    fn canonicalize_flattens_nested_unions() {
        // (| (| A B) C) → (| A B C), single element collapses if applicable.
        let inner = canonicalize_union([TypeExpr::named("A"), TypeExpr::named("B")]);
        let outer = canonicalize_union([inner, TypeExpr::named("C")]);
        match outer {
            TypeExpr::App { head, args } => {
                assert_eq!(head.0, "|");
                assert_eq!(args.len(), 3);
                // Alphabetical ordering
                assert_eq!(args[0], TypeExpr::named("A"));
                assert_eq!(args[1], TypeExpr::named("B"));
                assert_eq!(args[2], TypeExpr::named("C"));
            }
            other => panic!("expected App with head `|`, got {other:?}"),
        }
    }

    #[test]
    fn canonicalize_collapses_singleton() {
        let out = canonicalize_union([TypeExpr::named("A")]);
        assert_eq!(out, TypeExpr::named("A"));
    }

    #[test]
    fn list_of_string_matches_list_of_union() {
        // (List String) as actual, (List (| String Int)) as expected.
        let list_string = TypeExpr::App {
            head: TypeName("List".into()),
            args: vec![TypeExpr::named("String")],
        };
        let list_union = TypeExpr::App {
            head: TypeName("List".into()),
            args: vec![canonicalize_union([
                TypeExpr::named("String"),
                TypeExpr::named("Int"),
            ])],
        };
        assert!(type_expr_matches(&list_string, &list_union));
    }

    #[test]
    fn value_typed_helper_wraps_named() {
        let v = Value::typed(serde_json::json!("x"), "PlainText");
        assert_eq!(v.declared_type, TypeExpr::named("PlainText"));
    }
}
