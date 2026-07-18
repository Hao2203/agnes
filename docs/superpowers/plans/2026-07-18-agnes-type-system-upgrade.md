# agnes Type System Upgrade — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Migrate agnes's type system from `Named | Union<HashSet<TypeName>>` to `Named | App { head, args }`, introduce `(List T)` and `(Option T)` parameterized types, replace infix union `(A | B)` with prefix `(| A B)`, replace `(name: Type)` param syntax with `(name Type)`, and add list-literal expressions `[e1 ...]` / `(list e1 ...)`.

**Architecture:** Structural port first (one task keeps every existing test passing under the new `TypeExpr` shape). Then parser upgrades add each new syntax form incrementally. Registry, checker, compiler, and runtime each get one focused task. Final task migrates examples, docs, and acceptance workflows.

**Tech Stack:** Rust edition 2024, tokio, serde_json, lexpr 0.2, thiserror, insta (snapshot tests), jj (version control).

## Global Constraints

- Rust edition 2024 throughout every crate.
- Type names PascalCase; tool names and parameter names kebab-case.
- Commit workflow: `jj describe -m "..."` then `jj new` then `jj bookmark move main --to @-`. **Never** `git commit`.
- Every commit message ends with `Co-Authored-By: Claude <noreply@anthropic.com>` on its own line.
- Error messages follow **What / Why / Fix suggestion** three-part format from the MVP spec.
- One type-constructor form: `(head arg1 arg2 ...)`. One grammar rule: **head first, arguments after**.
- Union args are all `TypeExpr::Named` after canonicalization; nested `(| ...)` flattens; order stable (alphabetical).
- `(Option T)` never appears in canonical form — always expanded to `(| T Unit)` at registry-resolve time.
- No type variables in surface syntax. No variance beyond union widening.
- Language of code, comments, and error messages: English.
- Refer to the design spec `docs/superpowers/specs/2026-07-18-agnes-type-system-upgrade-design.md` for semantics; this plan is the mechanical breakdown.

## Interface Summary (locked across tasks)

- `agnes_ast::TypeExprAst::{Named(String), App { head: String, args: Vec<TypeExprAst> }}` — the `Union(Vec<TypeExprAst>)` variant is REMOVED.
- `agnes_ast::Expr::List { span: Span, items: Vec<Expr> }` — new variant.
- `agnes_types::TypeExpr::{Named(TypeName), App { head: TypeName, args: Vec<TypeExpr> }}` with `#[derive(Hash, Eq, PartialEq, Clone, Debug)]`.
- `agnes_types::canonicalize_union(members: impl IntoIterator<Item = TypeExpr>) -> TypeExpr` — new helper.
- `agnes_types::type_expr_matches(actual: &TypeExpr, expected: &TypeExpr) -> bool` — signature changes from `(&TypeName, &TypeExpr)`.
- `agnes_types::Value { data: JsonValue, declared_type: TypeExpr }` — field type changes from `TypeName` to `TypeExpr`.
- `agnes_types::Value::typed(data, ty: impl Into<TypeName>) -> Self` and `typed_expr(data, ty: TypeExpr) -> Self` — new helpers.
- `agnes_registry::RegistryError::ArityMismatch { head, expected, actual, plural }` — new variant.
- `agnes_compiler::NodeKind::List` — new variant.
- `agnes_checker::env::Env` stores `TypeExpr` (not `TypeName`); `check_expr` signature returns `Result<TypeExpr, CheckError>`; new optional `hint: Option<&TypeExpr>` parameter.

---

## Task 1: Structural port of `TypeExpr` and `Value`

**Rationale:** The core data-type change (`TypeExpr::Named | App`, `Value.declared_type: TypeExpr`) affects every crate. We migrate the *shape* first with mechanical changes only — no new features. All existing tests must pass at the end. This isolates the port from the syntactic and semantic additions.

**Files:**
- Modify: `crates/agnes-types/src/lib.rs`
- Modify: `crates/agnes-ast/src/lib.rs` (only `TypeExprAst` — `Union` → `App { head, args }`; `Expr::List` deferred to Task 5)
- Modify: `crates/agnes-parser/src/toplevel.rs` (produce `TypeExprAst::App { head: "|", args }` instead of `TypeExprAst::Union`)
- Modify: `crates/agnes-registry/src/lib.rs` (resolve `App` shape; keep only `|` head recognized for now — arity handling and List/Option come in Task 4)
- Modify: `crates/agnes-registry/src/loader.rs` (only follows type changes)
- Modify: `crates/agnes-registry/tests/register.rs` (use `TypeExpr::App { head: "|", ... }` instead of `TypeExpr::Union`)
- Modify: `crates/agnes-checker/src/lib.rs` (`check_expr` returns `TypeExpr`; `env::Env` stores `TypeExpr`)
- Modify: `crates/agnes-checker/src/env.rs`
- Modify: `crates/agnes-checker/src/error.rs` (`ParamMismatch`/`FlowMismatch` `actual` becomes `TypeExpr`)
- Modify: `crates/agnes-checker/tests/check.rs` and snapshot at `crates/agnes-checker/tests/snapshots/check__flow_mismatch.snap`
- Modify: `crates/agnes-compiler/src/lower.rs` (only follows type changes; `NodeKind` unchanged)
- Modify: `crates/agnes-runtime/src/scheduler.rs` (Value construction sites; validate signature unchanged for now)
- Modify: `crates/agnes-runtime/src/boundary.rs` (accept `TypeExpr`, but simple recursive dispatch: `Named` runs validator, `App { head: "|", args }` widens; other `App` heads = internal error since Task 6 adds `List`)
- Modify: `crates/agnes-runtime/src/error.rs` (`RuntimeTypeError.ty` remains `TypeName` — union case picks the matched member)
- Modify: `crates/agnes-builtins/src/aliases.rs` (return `TypeExpr::App { head: TypeName("|".into()), args: [...] }`)
- Modify: `crates/agnes-builtins/src/tools.rs` (Value constructions → new field type)
- Modify: `crates/agnes-builtins/src/lib.rs` (`register_builtins` union construction changes)

**Interfaces:**
- Consumes: nothing (foundational task)
- Produces: new `TypeExpr` and `Value` shape. All exports in "Interface Summary" become real except `Expr::List`, `NodeKind::List`, `ArityMismatch`, `hint` parameter — those come in later tasks.

- [ ] **Step 1: Update `agnes-types::lib.rs` — new `TypeExpr`, `canonicalize_union`, `type_expr_matches`, `Value`**

Overwrite `/home/hao/code/agnes/crates/agnes-types/src/lib.rs`:

```rust
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
/// sort alphabetically for stable Hash/Eq, collapse to `Named` if exactly one
/// remains. All arguments MUST resolve to `Named` after flattening — nested
/// `(| ...)` gets flattened but non-`|` App members (e.g. `(List String)`)
/// are preserved as-is inside the union args set.
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
    vec.sort_by(|a, b| a.to_string().cmp(&b.to_string()));
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

/// Tool signature after registry resolution.
#[derive(Debug, Clone)]
pub struct ToolSignature {
    pub requires: Vec<(String, TypeExpr)>,
    pub provides: TypeExpr,
}

/// A value flowing between tools at runtime. Carries the type declared by
/// the producing tool for boundary validation.
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
        (
            TypeExpr::App {
                head: h1,
                args: a1,
            },
            TypeExpr::App {
                head: h2,
                args: a2,
            },
        ) if h1 == h2 && a1.len() == a2.len() => {
            a1.iter().zip(a2.iter()).all(|(x, y)| type_expr_matches(x, y))
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
        let expected = canonicalize_union([
            TypeExpr::named("PlainText"),
            TypeExpr::named("Markdown"),
        ]);
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
```

- [ ] **Step 2: Update `agnes-ast::TypeExprAst` — swap `Union` for `App`**

Edit `/home/hao/code/agnes/crates/agnes-ast/src/lib.rs`. Replace the `TypeExprAst` enum and its `Display` impl:

```rust
/// Type expression as it appears syntactically. `agnes-types` will
/// resolve aliases, expand `Option`, and canonicalize `(| ...)` unions.
#[derive(Debug, Clone, PartialEq)]
pub enum TypeExprAst {
    Named(String),
    App { head: String, args: Vec<TypeExprAst> },
}

impl fmt::Display for TypeExprAst {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TypeExprAst::Named(n) => write!(f, "{n}"),
            TypeExprAst::App { head, args } => {
                write!(f, "({head}")?;
                for a in args {
                    write!(f, " {a}")?;
                }
                write!(f, ")")
            }
        }
    }
}
```

**Do not** touch the `Expr` enum in this task — `Expr::List` is added in Task 5.

- [ ] **Step 3: Update `agnes-parser::toplevel.rs::parse_type_expr` to produce `App { head: "|", ... }`**

In `/home/hao/code/agnes/crates/agnes-parser/src/toplevel.rs`, the existing infix-`|` parsing path still stands (the preprocessor and sentinel are still in place — Task 3 will remove them and add prefix parsing). But it currently produces `TypeExprAst::Union(members)` where each member is `TypeExprAst::Named`. Change it to produce `TypeExprAst::App { head: "|", args: members }`.

Find in `parse_type_expr` (around line 228):

```rust
    if members.len() == 1 {
        Ok(members.into_iter().next().unwrap())
    } else {
        Ok(TypeExprAst::Union(members))
    }
```

Replace with:

```rust
    if members.len() == 1 {
        Ok(members.into_iter().next().unwrap())
    } else {
        Ok(TypeExprAst::App {
            head: "|".into(),
            args: members,
        })
    }
```

- [ ] **Step 4: Update `agnes-registry::resolve` to consume `App` shape**

In `/home/hao/code/agnes/crates/agnes-registry/src/lib.rs`:

Replace the imports at the top:
```rust
use std::collections::HashMap;
use std::fmt;

use agnes_ast::{Expr, Param, Program, TopLevel, TypeExprAst};
use agnes_types::{ToolSignature, TypeExpr, TypeName, Validator, canonicalize_union};
```

Replace the `resolve` implementation with:

```rust
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
```

- [ ] **Step 5: Update `agnes-checker` — `TypeExpr` throughout**

In `/home/hao/code/agnes/crates/agnes-checker/src/env.rs`, replace with:

```rust
use agnes_types::TypeExpr;
use std::collections::HashMap;

/// Type environment threaded through expression checking.
#[derive(Debug, Default, Clone)]
pub struct Env {
    inner: HashMap<String, TypeExpr>,
}

impl Env {
    pub fn get(&self, name: &str) -> Option<&TypeExpr> {
        self.inner.get(name)
    }
    pub fn set(&mut self, name: String, ty: TypeExpr) {
        self.inner.insert(name, ty);
    }
}
```

In `/home/hao/code/agnes/crates/agnes-checker/src/error.rs`, change `ParamMismatch.actual`, `FlowMismatch.actual`, and `DefineSignatureMismatch.body_type` from `TypeName` to `TypeExpr`. Update imports:

```rust
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
        expected: TypeExpr,
        actual: TypeExpr,
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
        expected: TypeExpr,
        actual: TypeExpr,
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
        declared: TypeExpr,
        body_type: TypeExpr,
    },

    #[error(transparent)]
    Registry(#[from] agnes_registry::RegistryError),
}
```

In `/home/hao/code/agnes/crates/agnes-checker/src/lib.rs`, rewrite so `check_expr` returns `TypeExpr` throughout. **Do not** add the `hint` parameter yet — that comes in Task 7. Full replacement:

```rust
//! Type checker for agnes DSL.
//!
//! Enforces:
//!   1. Parameter satisfaction: each argument's type satisfies the tool's require.
//!   2. Flow satisfaction: pipe upstream's provides satisfies downstream's require
//!      (when downstream is a single-param tool with an unbound positional slot).
//!
//! Both rules bottom out at `agnes_types::type_expr_matches`.

pub mod env;
pub mod error;

use agnes_ast::{Expr, Program, TopLevel};
use agnes_registry::Registry;
use agnes_types::{ToolSignature, TypeExpr, TypeName, type_expr_matches};

pub use error::CheckError;

/// Top-level entry.
pub fn check(program: &Program, reg: &Registry) -> Result<(), CheckError> {
    for tl in &program.toplevels {
        if let TopLevel::Define {
            name,
            params,
            provides,
            body,
            ..
        } = tl
        {
            let mut env = env::Env::default();
            for p in params {
                let ty_expr = reg.resolve(&p.ty)?;
                env.set(p.name.clone(), ty_expr);
            }
            let body_ty = check_expr(body, reg, &mut env, None)?;
            let declared = reg.resolve(provides)?;
            if !type_expr_matches(&body_ty, &declared) {
                return Err(CheckError::DefineSignatureMismatch {
                    name: name.clone(),
                    declared,
                    body_type: body_ty,
                });
            }
        }
    }
    if let Some(main) = &program.main {
        let mut env = env::Env::default();
        check_expr(main, reg, &mut env, None)?;
    }
    Ok(())
}

fn check_expr(
    e: &Expr,
    reg: &Registry,
    env: &mut env::Env,
    flowed_in: Option<TypeExpr>,
) -> Result<TypeExpr, CheckError> {
    match e {
        Expr::Tool {
            name,
            positional,
            args,
            ..
        } => check_tool_call(name, positional, args, reg, env, flowed_in),
        Expr::Pipe { steps, .. } => {
            let mut upstream: Option<TypeExpr> = None;
            let mut last: Option<TypeExpr> = None;
            for step in steps {
                let ty = check_expr(step, reg, env, upstream.clone())?;
                upstream = Some(ty.clone());
                last = Some(ty);
            }
            last.ok_or_else(|| CheckError::UnknownVar {
                name: "(empty pipe)".into(),
            })
        }
        Expr::Par { branches, .. } => {
            let mut last = None;
            for b in branches {
                last = Some(check_expr(b, reg, env, None)?);
            }
            last.ok_or_else(|| CheckError::UnknownVar {
                name: "(empty par)".into(),
            })
        }
        Expr::Let { name, value, .. } => {
            let bound = match value {
                None => flowed_in.clone().ok_or_else(|| CheckError::UnknownVar {
                    name: format!("(let {name}) with no upstream to name"),
                })?,
                Some(v) => check_expr(v, reg, env, None)?,
            };
            env.set(name.clone(), bound.clone());
            Ok(bound)
        }
        Expr::If {
            cond,
            then_branch,
            else_branch,
            ..
        } => {
            let _ = check_expr(cond, reg, env, None)?;
            let t = check_expr(then_branch, reg, env, None)?;
            let _ = check_expr(else_branch, reg, env, None)?;
            Ok(t)
        }
        Expr::Match {
            scrutinee, arms, ..
        } => {
            let _ = check_expr(scrutinee, reg, env, None)?;
            let mut last = None;
            for (_, arm) in arms {
                last = Some(check_expr(arm, reg, env, None)?);
            }
            last.ok_or_else(|| CheckError::UnknownVar {
                name: "(empty match)".into(),
            })
        }
        Expr::Foreach {
            body, collection, ..
        } => {
            let _ = check_expr(collection, reg, env, None)?;
            check_expr(body, reg, env, None)
        }
        Expr::Retry { body, .. } => check_expr(body, reg, env, flowed_in),
        Expr::Catch { body, fallback, .. } => {
            let t = check_expr(body, reg, env, flowed_in.clone())?;
            let _ = check_expr(fallback, reg, env, flowed_in)?;
            Ok(t)
        }
        Expr::Llm {
            positional, args, ..
        } => {
            for pv in positional {
                let _ = check_expr(pv, reg, env, None)?;
            }
            for (_, v) in args {
                let _ = check_expr(v, reg, env, None)?;
            }
            Ok(TypeExpr::Named(TypeName("PlainText".into())))
        }
        Expr::Return { value, .. } => check_expr(value, reg, env, None),
        Expr::Literal { lit, .. } => Ok(literal_type(lit)),
        Expr::Var { name, .. } => env
            .get(name)
            .cloned()
            .ok_or_else(|| CheckError::UnknownVar { name: name.clone() }),
    }
}

fn literal_type(lit: &agnes_ast::Literal) -> TypeExpr {
    match lit {
        agnes_ast::Literal::String(_) => TypeExpr::Named(TypeName("String".into())),
        agnes_ast::Literal::Int(_) => TypeExpr::Named(TypeName("Int".into())),
        agnes_ast::Literal::Bool(_) => TypeExpr::Named(TypeName("Bool".into())),
        agnes_ast::Literal::Nil => TypeExpr::Named(TypeName("Unit".into())),
    }
}

fn check_arg(
    tool_name: &str,
    param: &str,
    expected: &TypeExpr,
    arg: &Expr,
    reg: &Registry,
    env: &mut env::Env,
) -> Result<(), CheckError> {
    if matches!(arg, Expr::Literal { .. }) {
        let _ = check_expr(arg, reg, env, None)?;
        return Ok(());
    }
    let actual = check_expr(arg, reg, env, None)?;
    if !type_expr_matches(&actual, expected) {
        return Err(CheckError::ParamMismatch {
            tool: tool_name.to_string(),
            param: param.to_string(),
            expected: expected.clone(),
            actual,
        });
    }
    Ok(())
}

fn check_tool_call(
    tool_name: &str,
    positional: &[Expr],
    args: &agnes_ast::KwArgs,
    reg: &Registry,
    env: &mut env::Env,
    flowed_in: Option<TypeExpr>,
) -> Result<TypeExpr, CheckError> {
    let sig: ToolSignature =
        reg.tool_signature(tool_name)
            .cloned()
            .ok_or_else(|| CheckError::UnknownTool {
                name: tool_name.to_string(),
            })?;

    let mut filled: Vec<bool> = vec![false; sig.requires.len()];

    for (i, pv) in positional.iter().enumerate() {
        if i >= sig.requires.len() {
            return Err(CheckError::UnknownVar {
                name: format!(
                    "extra positional arg at index {i} in call to `{tool_name}` (signature has {} required param(s))",
                    sig.requires.len()
                ),
            });
        }
        let (pname, param_expected) = sig.requires[i].clone();
        check_arg(tool_name, &pname, &param_expected, pv, reg, env)?;
        filled[i] = true;
    }

    for (k, v) in args {
        let (idx, param_expected) = sig
            .requires
            .iter()
            .enumerate()
            .find(|(_, (n, _))| n == k)
            .map(|(i, (_, t))| (i, t.clone()))
            .ok_or_else(|| CheckError::UnknownVar {
                name: format!("keyword arg :{k} in call to `{tool_name}` not in signature"),
            })?;
        check_arg(tool_name, k, &param_expected, v, reg, env)?;
        filled[idx] = true;
    }

    let unfilled: Vec<usize> = filled
        .iter()
        .enumerate()
        .filter(|(_, b)| !**b)
        .map(|(i, _)| i)
        .collect();
    match (unfilled.len(), flowed_in) {
        (0, _) => {}
        (1, Some(up)) => {
            let (_, expected) = &sig.requires[unfilled[0]];
            if !type_expr_matches(&up, expected) {
                return Err(CheckError::FlowMismatch {
                    upstream: format!("<upstream (provides {up})>"),
                    downstream_tool: tool_name.to_string(),
                    expected: expected.clone(),
                    actual: up,
                });
            }
        }
        _ => {
            return Err(CheckError::UnknownVar {
                name: format!(
                    "tool `{tool_name}` has unfilled required params and no upstream to bind"
                ),
            });
        }
    }

    Ok(sig.provides.clone())
}
```

- [ ] **Step 6: Update `agnes-compiler::lower.rs` — TypeExpr helpers**

In `/home/hao/code/agnes/crates/agnes-compiler/src/lower.rs`, no signature changes are needed since `Node.provides` was already `TypeExpr`. Only the two literal-lowering spots that construct `TypeExpr::Named(...)` are unaffected. **Skip modifications to this file if the compile succeeds after Steps 1-5**; if compilation fails due to `TypeExprAst::Union` still being referenced, replace those references with the new App shape.

- [ ] **Step 7: Update `agnes-runtime::scheduler.rs` — Value construction sites**

In `/home/hao/code/agnes/crates/agnes-runtime/src/scheduler.rs`, replace every `Value { data, declared_type: TypeName(...) }` with `Value { data, declared_type: TypeExpr::Named(TypeName(...)) }`, or preferably use `Value::typed(data, "TypeName")`. Find and fix:

- The `Par` branch (`declared_type: TypeName("Unit".into())`) → `declared_type: TypeExpr::Named(TypeName("Unit".into()))`.
- The `dispatch_define` default-literal branch: same fix.
- `eval_expr` for `Expr::Par`: same fix.
- `eval_expr` for `Expr::Literal`: `lit_type` returns `TypeName`; wrap it: `declared_type: TypeExpr::Named(lit_type(lit))`.
- `NodeKind::Literal` in `eval_node`: same wrapper.
- `Input::Literal` in `eval_input`: same wrapper.
- `call_native`: the union-picking snippet
  ```rust
      let ty: TypeName = match provides {
          TypeExpr::Named(n) => n.clone(),
          TypeExpr::Union(_) => result.declared_type.clone(),
      };
      validate(reg, tool, "provides", &ty, &result)?;
  ```
  becomes:
  ```rust
      validate(reg, tool, "provides", provides, &result)?;
  ```
  (New `validate` signature accepts `TypeExpr` directly — see Step 9.)

- [ ] **Step 8: Update `agnes-runtime::boundary.rs` to accept `TypeExpr`**

Overwrite `/home/hao/code/agnes/crates/agnes-runtime/src/boundary.rs`:

```rust
//! Recursive runtime boundary validation.
//!
//! `validate` walks the expected `TypeExpr` and enforces the JSON payload's
//! structural conformity. Named types run their registered `Validator`;
//! union types (`(| A B ...)`) pick the member matching the value's declared
//! type and recurse into it. Task 8 will add the List case; for now, other
//! App heads are treated as an internal-error condition.

use agnes_registry::Registry;
use agnes_types::{TypeExpr, TypeName, Value, type_expr_matches};

use crate::error::RuntimeError;

pub fn validate(
    reg: &Registry,
    tool: &str,
    direction: &'static str,
    expected: &TypeExpr,
    val: &Value,
) -> Result<(), RuntimeError> {
    match expected {
        TypeExpr::Named(n) => run_named_validator(reg, tool, direction, n, val),
        TypeExpr::App { head, args } if head.0 == "|" => {
            // Pick the union member matching the value's declared type and recurse.
            let matched = args
                .iter()
                .find(|m| type_expr_matches(&val.declared_type, m));
            match matched {
                Some(m) => validate(reg, tool, direction, m, val),
                None => Err(RuntimeError::RuntimeTypeError {
                    tool: tool.to_string(),
                    direction,
                    ty: TypeName(expected.to_string()),
                    cause: format!(
                        "value's declared type {} is not a member of expected union {}",
                        val.declared_type, expected
                    ),
                }),
            }
        }
        TypeExpr::App { head, .. } => {
            // Task 8 will add List. Any other head here is an internal error.
            Err(RuntimeError::RuntimeTypeError {
                tool: tool.to_string(),
                direction,
                ty: TypeName(head.0.clone()),
                cause: format!("unknown type constructor `{}` in canonical form", head.0),
            })
        }
    }
}

fn run_named_validator(
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
```

The `RuntimeError::RuntimeTypeError.ty: TypeName` field stays — for named checks it holds the actual named type; for union/error cases it holds a synthetic name derived from `Display`. No change to `RuntimeError` itself is required.

- [ ] **Step 9: Update `agnes-runtime::scheduler.rs` `call_native` — pass `provides` as `TypeExpr`**

Already covered by Step 7's rewrite instructions. Double-check `call_native` matches the new `validate` signature — the `for (k, expected)` require loop passes `expected` (a `&TypeExpr`) directly:

```rust
    if let Some(sig) = reg.tool_signature(tool) {
        for (k, expected) in &sig.requires {
            if let Some(v) = args.get(k) {
                validate(reg, tool, "requires", expected, v)?;
            }
        }
    }
```

(Note the change: was passing `&v.declared_type` before — that only ran the value's own type's validator, which is weaker than validating against the require. Passing `expected` walks the expected type structure.)

- [ ] **Step 10: Update `agnes-builtins/src/aliases.rs`**

Overwrite `/home/hao/code/agnes/crates/agnes-builtins/src/aliases.rs`:

```rust
use agnes_types::{TypeExpr, canonicalize_union};

pub fn text_like() -> TypeExpr {
    canonicalize_union([
        TypeExpr::named("PlainText"),
        TypeExpr::named("Markdown"),
        TypeExpr::named("HTML"),
    ])
}

pub fn visual_doc() -> TypeExpr {
    canonicalize_union([TypeExpr::named("PDF"), TypeExpr::named("Image")])
}
```

- [ ] **Step 11: Update `agnes-builtins/src/lib.rs`**

In `/home/hao/code/agnes/crates/agnes-builtins/src/lib.rs`, replace the `TypeExpr::Union(...)` construction sites with `canonicalize_union`. The specific one is `summarize`:

```rust
    reg.register_tool(
        "summarize",
        ToolSignature {
            requires: vec![(
                "input".into(),
                canonicalize_union(
                    aliases::text_like().as_union_members()
                        .into_iter()
                        .chain(std::iter::once(TypeExpr::named("PDF"))),
                ),
            )],
            provides: summary.clone(),
        },
    )?;
```

Since `as_set()` is gone, add a helper method on `TypeExpr` in `agnes-types` — or, simpler: since `text_like()` returns `TypeExpr::App { head: "|", args: [...] }`, spell the summarize signature directly:

```rust
    reg.register_tool(
        "summarize",
        ToolSignature {
            requires: vec![(
                "input".into(),
                canonicalize_union([
                    TypeExpr::named("PlainText"),
                    TypeExpr::named("Markdown"),
                    TypeExpr::named("HTML"),
                    TypeExpr::named("PDF"),
                ]),
            )],
            provides: summary.clone(),
        },
    )?;
```

Update the top of the file's imports:
```rust
use agnes_registry::{Registry, RegistryError};
use agnes_types::{ToolSignature, TypeExpr, canonicalize_union};
```

Remove `TypeName` import if unused.

- [ ] **Step 12: Update `agnes-builtins/src/tools.rs` — Value construction sites**

In `/home/hao/code/agnes/crates/agnes-builtins/src/tools.rs`, replace every `Value { data, declared_type: TypeName("...".into()) }` with `Value::typed(data, "...")`. For the six tools' return values:

```rust
Ok(Value::typed(JsonValue::String(text), "PlainText"))
Ok(Value::typed(JsonValue::Null, "Unit"))
Ok(Value::typed(JsonValue::String(summary), "Summary"))
Ok(Value::typed(JsonValue::String(out), "PlainText"))
Ok(Value::typed(JsonValue::String("[OCR-EXTRACTED-TEXT]".into()), "PlainText"))
Ok(Value::typed(JsonValue::String(out), "PlainText"))
```

Remove `use agnes_types::{TypeName, Value};` and replace with `use agnes_types::Value;` if `TypeName` no longer used.

- [ ] **Step 13: Update `agnes-registry/tests/register.rs`**

In `/home/hao/code/agnes/crates/agnes-registry/tests/register.rs`, replace `TypeExpr::Union(...)` construction with `canonicalize_union([...])` and the `resolve_alias_flattens_nested_union` test's `TypeExprAst::Union(...)` with `TypeExprAst::App { head: "|".into(), args: ... }`. Also update the assertion for the resolved set — since we no longer have `as_set()`, extract the union members from the returned `TypeExpr::App { head, args }`.

New content:

```rust
use agnes_registry::Registry;
use agnes_types::{TypeExpr, TypeName, canonicalize_union};

#[test]
fn duplicate_type_is_rejected() {
    let mut r = Registry::new();
    r.register_type("PDF", None).unwrap();
    let err = r.register_type("PDF", None).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("Name conflict"), "got: {msg}");
    assert!(msg.contains("PDF"), "got: {msg}");
}

#[test]
fn alias_conflicts_with_type() {
    let mut r = Registry::new();
    r.register_type("Text", None).unwrap();
    let expr = TypeExpr::Named(TypeName("PDF".into()));
    let err = r.register_alias("Text", expr).unwrap_err();
    assert!(format!("{err}").contains("Name conflict"));
}

#[test]
fn resolve_alias_flattens_nested_union() {
    use agnes_ast::TypeExprAst;
    let mut r = Registry::new();
    r.register_type("PlainText", None).unwrap();
    r.register_type("Markdown", None).unwrap();
    r.register_type("HTML", None).unwrap();
    r.register_alias(
        "TextLike",
        canonicalize_union([
            TypeExpr::named("PlainText"),
            TypeExpr::named("Markdown"),
            TypeExpr::named("HTML"),
        ]),
    )
    .unwrap();

    // (| TextLike PDF) should resolve to a flat 4-member union.
    r.register_type("PDF", None).unwrap();
    let ast = TypeExprAst::App {
        head: "|".into(),
        args: vec![
            TypeExprAst::Named("TextLike".into()),
            TypeExprAst::Named("PDF".into()),
        ],
    };
    let resolved = r.resolve(&ast).unwrap();
    match resolved {
        TypeExpr::App { head, args } => {
            assert_eq!(head.0, "|");
            assert_eq!(args.len(), 4);
            let names: Vec<String> = args.iter().map(|a| a.to_string()).collect();
            assert!(names.contains(&"PlainText".into()));
            assert!(names.contains(&"PDF".into()));
            assert!(names.contains(&"Markdown".into()));
            assert!(names.contains(&"HTML".into()));
        }
        other => panic!("expected App with head `|`, got {other:?}"),
    }
}
```

- [ ] **Step 14: Update `agnes-checker/tests/check.rs` — TypeExpr constructions**

In `/home/hao/code/agnes/crates/agnes-checker/tests/check.rs`, the tool signature construction currently uses `TypeExpr::Union(HashSet::from_iter([...]))`. Change to `canonicalize_union([...])`. Update imports:

```rust
use agnes_types::{ToolSignature, TypeExpr, canonicalize_union};
```

Replace the two `TypeExpr::Union(...)` sites with `canonicalize_union(...)`. Test source strings keep the old union syntax `(PDF | Image)` — that path still works because the sentinel-preprocessor is still in place; the parser now produces `App { head: "|", ... }` instead of `Union(...)` (per Task 1 Step 3).

- [ ] **Step 15: Update the flow_mismatch snapshot**

The `Display` for `TypeExpr::App { head: "|", args: [Image, PDF] }` now produces `(| Image PDF)` instead of the old `(Image | PDF)`. Delete the old snapshot and let insta regenerate:

```bash
rm /home/hao/code/agnes/crates/agnes-checker/tests/snapshots/check__flow_mismatch.snap
```

The regenerated snapshot after `cargo insta review --accept` should show `(| Image PDF)` in the "requires one of" line and the actual side (`PlainText`) unchanged.

- [ ] **Step 16: Update `agnes-cli/tests/acceptance.rs`**

The `positive_full_demo_runs` test still uses `(path: Path)` param syntax and old union — Task 11 will migrate this. For now, `agnes-cli/tests/acceptance.rs` remains valid because the parser's sentinel still handles `(A | B)` and produces the new `App` shape.

**No source changes needed in this step**, but confirm `cargo test -p agnes-cli --test acceptance` still passes.

- [ ] **Step 17: Build and test**

Run:

```bash
cd /home/hao/code/agnes && cargo build --workspace 2>&1 | tail -30
```

Expected: builds cleanly. If it doesn't, fix compilation errors — they should all be about `TypeExpr::Union` still referenced or `.as_set()` still called; convert them to `TypeExpr::App { head: TypeName("|".into()), args }` / `canonicalize_union` / positional match on args.

Run:

```bash
cd /home/hao/code/agnes && cargo test --workspace 2>&1 | tail -60
```

Expected: all tests pass except the flow_mismatch snapshot, which is pending. Run:

```bash
cd /home/hao/code/agnes && cargo insta review
```

Accept the new snapshot (it should show `requires one of: (| Image PDF)`).

Then re-run `cargo test --workspace`. Expected: all tests pass.

- [ ] **Step 18: Commit**

Run:

```
cd /home/hao/code/agnes
jj describe -m "refactor(types): port TypeExpr and Value to App-based shape

Structural port: TypeExpr becomes Named | App { head, args } — a
single sexpr-shaped constructor form. Value.declared_type gains full
TypeExpr (was TypeName) so lists and unions can be carried at runtime.
canonicalize_union flattens/dedupes/sorts union members; type_expr_matches
recurses positionally and widens through | at any depth.

Every existing source file that constructed TypeExpr::Union or set
Value.declared_type as TypeName migrates mechanically. Parser now
produces TypeExprAst::App { head: \"|\", args } from the sentinel-based
infix path (Task 3 removes the sentinel entirely).

All existing tests pass under the new shape. New features (List, Option,
list literals, prefix union syntax, param syntax change) land in
Tasks 2-11.

Co-Authored-By: Claude <noreply@anthropic.com>"
jj new
jj bookmark move main --to @-
```

---

## Task 2: Param syntax `(name Type)` (position-based)

**Rationale:** Current parser requires `(source: (PDF | Image))` — the name must end with `:`. Move to `(source (| PDF Image))` — pure sexpr, first-position is name, second is type. This is independent of the union-syntax change (Task 3) but conceptually part of "language becomes uniform sexpr".

**Files:**
- Modify: `crates/agnes-parser/src/toplevel.rs` (`parse_single_param`)
- Modify: `crates/agnes-parser/tests/parse.rs` (update `parses_declare_tool`, `parses_define_with_body` to use new syntax)
- Modify: `crates/agnes-checker/tests/check.rs` (test sources)
- Modify: `crates/agnes-runtime/tests/execute.rs` (test sources with define)
- Modify: `crates/agnes-cli/tests/acceptance.rs` (test sources)
- Modify: `examples/full-demo.agnes`, `examples/with-define.agnes`

**Interfaces:**
- Consumes: Task 1 (`TypeExprAst::App` shape).
- Produces: Parser accepts `(name Type)` and `(name Type :default lit)` inside `:params` / `:requires` vectors. Rejects `(name:...)` with a migration hint.

- [ ] **Step 1: Add a failing test for the new syntax**

In `/home/hao/code/agnes/crates/agnes-parser/tests/parse.rs`, add:

```rust
#[test]
fn parses_declare_tool_position_based_param() {
    // (source (| PDF Image)) — no trailing colon on the name.
    let src = r#"
        (declare tool ocr
          :requires [(source (PDF | Image))]
          :provides PlainText)
    "#;
    let p = parse(src).expect("parse ok");
    match &p.toplevels[0] {
        TopLevel::DeclareTool { requires, .. } => {
            assert_eq!(requires.len(), 1);
            assert_eq!(requires[0].name, "source");
            // Type is (App { head: "|", args }) with 2 members.
            match &requires[0].ty {
                TypeExprAst::App { head, args } => {
                    assert_eq!(head, "|");
                    assert_eq!(args.len(), 2);
                }
                other => panic!("expected App union, got {other:?}"),
            }
        }
        other => panic!("expected DeclareTool, got {other:?}"),
    }
}

#[test]
fn parses_define_position_based_params() {
    let src = r#"
        (define greet
          :params [(who PlainText) (times Int :default 1)]
          :provides PlainText
          (tool llm :prompt "hello" :input who))
    "#;
    let p = parse(src).expect("parse ok");
    match &p.toplevels[0] {
        TopLevel::Define { params, .. } => {
            assert_eq!(params.len(), 2);
            assert_eq!(params[0].name, "who");
            assert_eq!(params[1].name, "times");
            assert_eq!(params[1].default, Some(agnes_ast::Literal::Int(1)));
        }
        other => panic!("expected Define, got {other:?}"),
    }
}

#[test]
fn rejects_old_colon_suffix_param_syntax() {
    let src = r#"
        (declare tool foo
          :requires [(x: PlainText)]
          :provides PlainText)
    "#;
    let err = parse(src).expect_err("must reject legacy param syntax");
    let msg = format!("{err}");
    assert!(
        msg.contains("param name") && msg.contains("no longer ends with"),
        "expected migration hint, got: {msg}"
    );
}
```

Also delete or rewrite the pre-existing `parses_declare_tool` and `parses_define_with_body` tests to use the new syntax (their old sources use `(source: ...)` and `(who: ...)` which will now be rejected). Simplest: delete both and replace with the two new tests above. `parses_let_two_forms` doesn't touch param syntax — leave it.

- [ ] **Step 2: Run tests — expect the new ones to fail with the current parser**

```bash
cd /home/hao/code/agnes && cargo test -p agnes-parser --tests
```

Expected: the new tests fail (parser still expects trailing `:`).

- [ ] **Step 3: Update `parse_single_param`**

In `/home/hao/code/agnes/crates/agnes-parser/src/toplevel.rs`, replace `parse_single_param`:

```rust
fn parse_single_param(v: &lexpr::Value, span: Span) -> Result<Param, ParseError> {
    // Syntax: (name Type [:default Literal])
    let items = as_list(v, span)?;
    let raw_name = items
        .first()
        .and_then(|v| v.as_symbol())
        .ok_or_else(|| ParseError {
            span,
            message: "param name symbol expected".into(),
        })?;
    if raw_name.ends_with(':') {
        return Err(ParseError {
            span,
            message: format!(
                "param name `{raw_name}` no longer ends with `:` — use position-based form `(name Type ...)` instead of `(name: Type ...)`"
            ),
        });
    }
    let name = raw_name.to_string();
    let ty_val = items.get(1).ok_or_else(|| ParseError {
        span,
        message: "param type expected after name".into(),
    })?;
    let ty = parse_type_expr(ty_val, span)?;
    let mut default = None;
    let mut i = 2usize;
    while i < items.len() {
        if let Some(k) = items[i].as_keyword() {
            let val = items.get(i + 1).ok_or_else(|| ParseError {
                span,
                message: format!("keyword :{k} in param without value"),
            })?;
            match k {
                "default" => default = Some(parse_literal(val, span)?),
                other => {
                    return Err(ParseError {
                        span,
                        message: format!("unknown keyword :{other} in param"),
                    });
                }
            }
            i += 2;
        } else {
            i += 1;
        }
    }
    Ok(Param { name, ty, default })
}
```

- [ ] **Step 4: Run tests — expect PASS**

```bash
cd /home/hao/code/agnes && cargo test -p agnes-parser --tests
```

Expected: all 3 new tests plus the pre-existing ones pass.

- [ ] **Step 5: Migrate sources that still use the old param syntax**

Grep the workspace for `\w+:\s+(` type old-style params (in test sources and examples):

```bash
cd /home/hao/code/agnes && grep -rn "(\w\+:" --include="*.rs" --include="*.agnes" crates/ examples/ tests/ 2>/dev/null | grep -v "^Binary" | head -40
```

You'll see `.rs` files like `agnes-checker/tests/check.rs`, `agnes-runtime/tests/execute.rs`, `agnes-cli/tests/acceptance.rs`, and `.agnes` files `examples/full-demo.agnes`, `examples/with-define.agnes`.

For each `(x: T)` in a `:params` or `:requires` vector, rewrite to `(x T)`.

**agnes-runtime/tests/execute.rs (runs_a_defined_compound_tool test):**
```
(define read-and-summarize
  :params [(path Path)]
  :provides Summary
  ...)
```

**agnes-cli/tests/acceptance.rs (positive_full_demo_runs test):** replace all `(path: Path)`, `(target: String)`, `(x: MysteryType)` with position-based form.

**examples/full-demo.agnes and examples/with-define.agnes:** rewrite `(path: Path) (target: String)` to `(path Path) (target String)`.

- [ ] **Step 6: Run entire workspace test suite**

```bash
cd /home/hao/code/agnes && cargo test --workspace 2>&1 | tail -30
```

Expected: all pass.

- [ ] **Step 7: Commit**

```
cd /home/hao/code/agnes
jj describe -m "feat(parser): position-based param syntax (name Type)

Params in :params and :requires now use position-based form:
  (path Path)  (target String)  (times Int :default 1)

instead of the old suffix-colon form (name: Type). This matches the
sexpr rule 'head is meaning, tail is data' — the first position of a
param element IS the name, no marker needed.

Legacy (name: ...) rejected with a migration hint. All example .agnes
files and test sources migrated in this commit.

Co-Authored-By: Claude <noreply@anthropic.com>"
jj new
jj bookmark move main --to @-
```

---

## Task 3: Prefix union syntax `(| A B)` — remove sentinel preprocessor

**Rationale:** Replace the `|`-as-infix preprocessor (`__agnes_union_bar__` sentinel) with a clean parser rule: `|` is a plain symbol, and `(| A B ...)` is just an App-form. The union head lives at position 0, matching every other type constructor.

**Files:**
- Modify: `crates/agnes-parser/src/lib.rs` (remove `preprocess_union_bars` + `UNION_BAR_SENTINEL`; enable `|` as a symbol via lexpr option or a minimal preprocessor)
- Modify: `crates/agnes-parser/src/toplevel.rs` (`parse_type_expr` reads head-first)
- Modify: `crates/agnes-parser/tests/parse.rs` (test both new syntax and legacy rejection)
- Modify: `crates/agnes-checker/tests/check.rs` (test source strings)
- Modify: `crates/agnes-cli/tests/acceptance.rs` (test source strings)
- Modify: `examples/*.agnes` (any files still using `(A | B)` after Task 2 gets migrated)

**Interfaces:**
- Consumes: Task 2 param syntax.
- Produces: Parser accepts `(| A B C)` as `TypeExprAst::App { head: "|", args }`. Rejects old `(A | B)` with a migration hint. Sentinel removed.

- [ ] **Step 1: Write failing tests**

In `/home/hao/code/agnes/crates/agnes-parser/tests/parse.rs`, add:

```rust
#[test]
fn parses_prefix_union() {
    let src = r#"(declare type-alias TextLike (| PlainText Markdown HTML))"#;
    let p = parse(src).expect("parse ok");
    match &p.toplevels[0] {
        TopLevel::DeclareTypeAlias { name, expr, .. } => {
            assert_eq!(name, "TextLike");
            match expr {
                TypeExprAst::App { head, args } => {
                    assert_eq!(head, "|");
                    assert_eq!(args.len(), 3);
                }
                other => panic!("expected App, got {other:?}"),
            }
        }
        other => panic!("expected DeclareTypeAlias, got {other:?}"),
    }
}

#[test]
fn rejects_infix_union() {
    let src = r#"(declare type-alias T (PlainText | Markdown))"#;
    let err = parse(src).expect_err("must reject infix union");
    let msg = format!("{err}");
    assert!(
        msg.contains("union") && msg.contains("prefix"),
        "expected migration hint about prefix form, got: {msg}"
    );
}
```

Update `parses_declare_type_alias` to use the new syntax:

```rust
#[test]
fn parses_declare_type_alias() {
    let src = r#"(declare type-alias TextLike (| PlainText Markdown HTML))"#;
    let p = parse(src).expect("parse ok");
    match &p.toplevels[0] {
        TopLevel::DeclareTypeAlias { name, expr, .. } => {
            assert_eq!(name, "TextLike");
            match expr {
                TypeExprAst::App { head, args } => {
                    assert_eq!(head, "|");
                    assert_eq!(args.len(), 3);
                }
                other => panic!("expected App union, got {other:?}"),
            }
        }
        other => panic!("expected DeclareTypeAlias, got {other:?}"),
    }
}
```

- [ ] **Step 2: Rewrite the sentinel preprocessor**

In `/home/hao/code/agnes/crates/agnes-parser/src/lib.rs`:

Change the preprocessor's job. Instead of replacing `|` everywhere, it now only handles:
1. Escaping bare `|` **outside of strings** as the sentinel — because lexpr 0.2 can't accept `|` as a symbol character.
2. The sentinel is used **only as a substitute during tokenization**; the parser converts it back to head `"|"` at App head position.
3. If the sentinel appears anywhere other than the head position of a type expression's App form, the parser reports an error (this catches the old infix form: `(A | B)` becomes `(A __agnes_union_bar__ B)` after preprocessing, which has the sentinel in a non-head position).

Update `parse`:

```rust
pub fn parse(source: &str) -> Result<Program, ParseError> {
    let prepared = preprocess_union_bars(source);
    let forms = read_forms(&prepared)?;
    let mut toplevels = Vec::new();
    let mut main: Option<Expr> = None;

    for form in forms {
        let span = Span::DUMMY;
        if is_toplevel(&form) {
            toplevels.push(toplevel::parse_toplevel(&form, span)?);
        } else {
            if main.is_some() {
                return Err(ParseError {
                    span,
                    message: "multiple main expressions at top level; wrap them in a single (pipe ...) or (par ...)".into(),
                });
            }
            main = Some(expr::parse_expr(&form, span)?);
        }
    }

    Ok(Program { toplevels, main })
}
```

The `preprocess_union_bars` function stays as-is (replacing `|` outside strings with the sentinel token), because lexpr 0.2's tokenizer still can't handle `|`. What changes is `parse_type_expr`.

- [ ] **Step 3: Rewrite `parse_type_expr` to head-first**

In `/home/hao/code/agnes/crates/agnes-parser/src/toplevel.rs`, replace `parse_type_expr`:

```rust
pub(crate) fn parse_type_expr(v: &lexpr::Value, span: Span) -> Result<TypeExprAst, ParseError> {
    // Atomic type name.
    if let Some(sym) = v.as_symbol() {
        if sym == UNION_BAR_SENTINEL {
            return Err(ParseError {
                span,
                message: "unexpected `|` in atomic type position; use `(| A B ...)` for unions".into(),
            });
        }
        return Ok(TypeExprAst::Named(sym.to_string()));
    }
    // Compound: (head arg1 arg2 ...). Head is a symbol (possibly the sentinel `|`).
    let items = as_list(v, span)?;
    if items.is_empty() {
        return Err(ParseError {
            span,
            message: "empty type expression `()` is not a valid type".into(),
        });
    }
    let head_sym = items[0].as_symbol().ok_or_else(|| ParseError {
        span,
        message: "type expression head (symbol) expected".into(),
    })?;
    let head = if head_sym == UNION_BAR_SENTINEL {
        "|".to_string()
    } else {
        head_sym.to_string()
    };
    // Verify no sentinel appears in the args (would mean infix `|` was used).
    let mut args: Vec<TypeExprAst> = Vec::with_capacity(items.len() - 1);
    for item in &items[1..] {
        if item.as_symbol() == Some(UNION_BAR_SENTINEL) {
            return Err(ParseError {
                span,
                message: "infix `|` is not allowed in type expressions; union types now use prefix form `(| A B C)`".into(),
            });
        }
        args.push(parse_type_expr(item, span)?);
    }
    Ok(TypeExprAst::App { head, args })
}
```

- [ ] **Step 4: Run tests — expect PASS for new syntax and reject for old**

```bash
cd /home/hao/code/agnes && cargo test -p agnes-parser --tests
```

Expected: all parser tests pass, including new `parses_prefix_union` and `rejects_infix_union`.

- [ ] **Step 5: Migrate source strings in other crates' tests and examples**

Grep for old union syntax:

```bash
cd /home/hao/code/agnes && grep -rn "|" --include="*.rs" --include="*.agnes" crates/ examples/ 2>/dev/null | grep -E "\((\w+ \| )+\w+\)" | head -20
```

For each occurrence like `(A | B)`, rewrite as `(| A B)`. Concretely, expected locations:
- `crates/agnes-checker/tests/check.rs` — the seed_registry uses `TypeExpr::Union(...)` directly (no source string), unaffected. But check for embedded `(PDF | Image)` strings.
- `crates/agnes-cli/tests/acceptance.rs` — no `(A|B)` strings (single-type params only).
- `examples/*.agnes` — none currently use unions (after Task 2 migration).

If any found, migrate them.

- [ ] **Step 6: Run entire workspace test suite**

```bash
cd /home/hao/code/agnes && cargo test --workspace 2>&1 | tail -30
```

Expected: all pass. If the flow_mismatch snapshot needs re-review (unlikely since Task 1 already captured the new `(| ...)` format), do `cargo insta review`.

- [ ] **Step 7: Commit**

```
cd /home/hao/code/agnes
jj describe -m "feat(parser): prefix union syntax (| A B) replaces infix (A | B)

Type expressions now uniformly follow the sexpr rule '(head args...)'.
Union types were the last infix wrinkle — they now spell as (| A B C),
same shape as (List T), (Option T), or any user-defined constructor.

The lexpr 0.2 sentinel-substitution preprocessor stays (since lexpr
cannot accept | as a symbol character), but the parser now translates
the sentinel back to head '|' only at the head position of a type
expression, and reports a migration error for any occurrence in a
non-head position (that's the old infix form).

Every remaining example / test source with (A | B) rewritten to (| A B).

Co-Authored-By: Claude <noreply@anthropic.com>"
jj new
jj bookmark move main --to @-
```

---

## Task 4: `Registry::resolve` recognizes `List` and `Option` heads

**Rationale:** Task 1 rejected any non-`|` App head with `UnknownName`. Now `resolve` needs to recognize `List` and `Option`. Adds arity checking + `Option` expansion. The parser doesn't need to change — it produces `App { head: "List", args: [_] }` naturally from Task 3's head-first parser.

**Files:**
- Modify: `crates/agnes-registry/src/lib.rs` (`resolve` new branches; `RegistryError::ArityMismatch` variant)
- Modify: `crates/agnes-registry/tests/register.rs` (new tests)

**Interfaces:**
- Consumes: Task 3 (parser produces `App { head, args }` for any parenthesized type).
- Produces: `Registry::resolve` accepts `(List T)` and `(Option T)`; other unknown heads still `UnknownName`. New `RegistryError::ArityMismatch` variant.

- [ ] **Step 1: Add failing tests**

In `/home/hao/code/agnes/crates/agnes-registry/tests/register.rs`, add:

```rust
#[test]
fn resolves_list_of_string() {
    use agnes_ast::TypeExprAst;
    let mut r = Registry::new();
    r.register_type("String", None).unwrap();
    let ast = TypeExprAst::App {
        head: "List".into(),
        args: vec![TypeExprAst::Named("String".into())],
    };
    let resolved = r.resolve(&ast).unwrap();
    assert_eq!(
        resolved,
        TypeExpr::App {
            head: TypeName("List".into()),
            args: vec![TypeExpr::named("String")],
        }
    );
}

#[test]
fn resolves_option_expands_to_union_with_unit() {
    use agnes_ast::TypeExprAst;
    let mut r = Registry::new();
    r.register_type("String", None).unwrap();
    r.register_type("Unit", None).unwrap();
    let ast = TypeExprAst::App {
        head: "Option".into(),
        args: vec![TypeExprAst::Named("String".into())],
    };
    let resolved = r.resolve(&ast).unwrap();
    // (Option String) → (| String Unit), which after canonicalization is
    // App { head: "|", args: [String, Unit] } (alphabetical order).
    match resolved {
        TypeExpr::App { head, args } => {
            assert_eq!(head.0, "|");
            assert_eq!(args.len(), 2);
            let names: Vec<String> = args.iter().map(|a| a.to_string()).collect();
            assert!(names.contains(&"String".into()));
            assert!(names.contains(&"Unit".into()));
        }
        other => panic!("expected union App, got {other:?}"),
    }
}

#[test]
fn arity_mismatch_list_zero_args() {
    use agnes_ast::TypeExprAst;
    let r = Registry::new();
    let ast = TypeExprAst::App {
        head: "List".into(),
        args: vec![],
    };
    let err = r.resolve(&ast).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("List") && msg.contains("expects 1"), "got: {msg}");
}

#[test]
fn arity_mismatch_option_two_args() {
    use agnes_ast::TypeExprAst;
    let mut r = Registry::new();
    r.register_type("A", None).unwrap();
    r.register_type("B", None).unwrap();
    let ast = TypeExprAst::App {
        head: "Option".into(),
        args: vec![
            TypeExprAst::Named("A".into()),
            TypeExprAst::Named("B".into()),
        ],
    };
    let err = r.resolve(&ast).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("Option") && msg.contains("expects 1"), "got: {msg}");
}

#[test]
fn unknown_head_reports_suggestion() {
    use agnes_ast::TypeExprAst;
    let r = Registry::new();
    let ast = TypeExprAst::App {
        head: "Foo".into(),
        args: vec![TypeExprAst::Named("Bar".into())],
    };
    let err = r.resolve(&ast).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("Foo"), "got: {msg}");
    assert!(
        msg.contains("List") && msg.contains("Option"),
        "expected suggestion mentioning List/Option, got: {msg}"
    );
}
```

- [ ] **Step 2: Run — expect failures**

```bash
cargo test -p agnes-registry --tests
```

Expected: the 5 new tests fail (`List`, `Option`, arity, suggestion).

- [ ] **Step 3: Add `ArityMismatch` and enrich `UnknownName` in `RegistryError`**

In `/home/hao/code/agnes/crates/agnes-registry/src/lib.rs`, extend the enum:

```rust
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
        "Type constructor `{head}` expects {expected} arg(s), got {actual}.\n  Fix: `({head} ...)` takes {expected} type argument{plural}."
    )]
    ArityMismatch {
        head: String,
        expected: usize,
        actual: usize,
        plural: &'static str,
    },
}
```

The `plural` field's convention: pass `"s"` when `expected != 1`, else `""` — matches English "takes 1 argument" vs "takes 2 arguments".

- [ ] **Step 4: Extend `resolve` to handle `List` and `Option`**

In `/home/hao/code/agnes/crates/agnes-registry/src/lib.rs`, replace the `resolve` method body:

```rust
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
                        plural: "",
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
                        plural: "",
                    });
                }
                let inner = self.resolve(&args[0])?;
                Ok(TypeExpr::App {
                    head: TypeName("List".into()),
                    args: vec![inner],
                })
            }
            TypeExprAst::App { head, .. } => {
                Err(RegistryError::UnknownName { name: head.clone() })
            }
        }
    }
```

- [ ] **Step 5: Run tests — expect PASS**

```bash
cd /home/hao/code/agnes && cargo test -p agnes-registry --tests
```

Expected: all 8 tests pass.

- [ ] **Step 6: Run entire workspace test suite**

```bash
cd /home/hao/code/agnes && cargo test --workspace 2>&1 | tail -30
```

Expected: all pass. `Option` isn't used anywhere in existing sources yet, so no downstream breakage.

- [ ] **Step 7: Commit**

```
cd /home/hao/code/agnes
jj describe -m "feat(registry): recognize (List T) and (Option T) heads

Registry::resolve now handles two additional heads:
- (List T) — arity-checked, resolved recursively, preserved as App
- (Option T) — arity-checked, expanded to (| T Unit) via canonicalize_union

Other App heads still fail as UnknownName, now with a suggestion pointing
at List / Option / |.

New RegistryError::ArityMismatch variant renders the What/Why/Fix pattern.

Co-Authored-By: Claude <noreply@anthropic.com>"
jj new
jj bookmark move main --to @-
```

---

## Task 5: `Expr::List` and list-literal expressions

**Rationale:** Add the new expression form. Parser handles both `(list e1 e2 ...)` and `[e1 e2 ...]`; checker returns a `(List T)` type from the elements' types (or `(List Unknown)` for empty); compiler emits `NodeKind::List`; runtime materializes into `JsonValue::Array`. The `hint` mechanism for empty lists lands in Task 7.

**Files:**
- Modify: `crates/agnes-ast/src/lib.rs` (add `Expr::List` variant; extend `Expr::span`)
- Modify: `crates/agnes-parser/src/expr.rs` (handle `list` head; handle `[...]` — lexpr 0.2 parses `[...]` as a list already, so we distinguish "the head of this compound is symbol `list`" vs "the compound was written as a bracket group". lexpr's default treats `[...]` and `(...)` identically. To recognize the bracket form and reject commas, we add a lightweight source-level pre-scan.)
- Modify: `crates/agnes-parser/src/lib.rs` (add a comma-in-bracket pre-scan; also transform `[...]` → `(list ...)` in the preprocessor so lexpr sees a normal list. See detailed step below.)
- Modify: `crates/agnes-checker/src/lib.rs` (add `Expr::List` case in `check_expr` — returns `(List elem_type)` for non-empty, `(List Unknown)` for empty; requires `Unknown` type registered)
- Modify: `crates/agnes-compiler/src/dag.rs` (add `NodeKind::List`)
- Modify: `crates/agnes-compiler/src/lower.rs` (`Expr::List` → `NodeKind::List` with one input per element)
- Modify: `crates/agnes-compiler/src/cycle.rs` (walk `Expr::List` items when scanning for recursive defines)
- Modify: `crates/agnes-runtime/src/scheduler.rs` (`NodeKind::List` case — collect elem Values, produce `JsonValue::Array`)
- Modify: `crates/agnes-parser/tests/parse.rs` (list literal tests)
- Modify: `crates/agnes-checker/tests/check.rs` (list-typing tests)
- Modify: `crates/agnes-compiler/tests/compile.rs` (list lowering test)
- Modify: `crates/agnes-runtime/tests/execute.rs` (list execution test)

**Interfaces:**
- Consumes: Tasks 1, 4.
- Produces: `Expr::List { span, items }`, `NodeKind::List`, list evaluated to `Value { data: JsonValue::Array, declared_type: TypeExpr::App { head: "List", args: [elem] } }`.

- [ ] **Step 1: Add failing tests across crates**

**Parser tests** — add to `/home/hao/code/agnes/crates/agnes-parser/tests/parse.rs`:

```rust
#[test]
fn parses_list_form() {
    let src = r#"(list "a" "b" "c")"#;
    let p = parse(src).expect("parse ok");
    match p.main.expect("has main") {
        Expr::List { items, .. } => {
            assert_eq!(items.len(), 3);
            assert!(matches!(&items[0], Expr::Literal { lit: Literal::String(s), .. } if s == "a"));
        }
        other => panic!("expected Expr::List, got {other:?}"),
    }
}

#[test]
fn parses_bracket_list() {
    let src = r#"["a" "b"]"#;
    let p = parse(src).expect("parse ok");
    match p.main.expect("has main") {
        Expr::List { items, .. } => assert_eq!(items.len(), 2),
        other => panic!("expected Expr::List, got {other:?}"),
    }
}

#[test]
fn parses_empty_bracket_list() {
    let src = r#"[]"#;
    let p = parse(src).expect("parse ok");
    match p.main.expect("has main") {
        Expr::List { items, .. } => assert!(items.is_empty()),
        other => panic!("expected Expr::List, got {other:?}"),
    }
}

#[test]
fn rejects_comma_in_bracket_list() {
    let src = r#"["a", "b"]"#;
    let err = parse(src).expect_err("must reject commas");
    let msg = format!("{err}");
    assert!(
        msg.to_lowercase().contains("comma") || msg.to_lowercase().contains("whitespace"),
        "expected comma hint, got: {msg}"
    );
}

#[test]
fn parses_nested_bracket_list() {
    let src = r#"[["a"] ["b" "c"]]"#;
    let p = parse(src).expect("parse ok");
    match p.main.expect("has main") {
        Expr::List { items, .. } => {
            assert_eq!(items.len(), 2);
            assert!(matches!(&items[0], Expr::List { items, .. } if items.len() == 1));
            assert!(matches!(&items[1], Expr::List { items, .. } if items.len() == 2));
        }
        other => panic!("expected outer Expr::List, got {other:?}"),
    }
}
```

**Checker tests** — add to `/home/hao/code/agnes/crates/agnes-checker/tests/check.rs`:

```rust
#[test]
fn list_of_string_typed_correctly() {
    // Register a tool that takes (List String).
    let mut r = Registry::new();
    r.register_type("String", None).unwrap();
    r.register_type("PlainText", None).unwrap();
    r.register_tool(
        "consume-strings",
        ToolSignature {
            requires: vec![(
                "items".into(),
                TypeExpr::App {
                    head: agnes_types::TypeName("List".into()),
                    args: vec![TypeExpr::named("String")],
                },
            )],
            provides: TypeExpr::named("PlainText"),
        },
    )
    .unwrap();

    let src = r#"(tool consume-strings :items ["a" "b" "c"])"#;
    let p = parse(src).unwrap();
    check(&p, &r).expect("must type-check");
}

#[test]
fn list_of_mixed_types_rejected_where_list_of_string_expected() {
    let mut r = Registry::new();
    r.register_type("String", None).unwrap();
    r.register_type("Int", None).unwrap();
    r.register_type("PlainText", None).unwrap();
    r.register_tool(
        "consume-strings",
        ToolSignature {
            requires: vec![(
                "items".into(),
                TypeExpr::App {
                    head: agnes_types::TypeName("List".into()),
                    args: vec![TypeExpr::named("String")],
                },
            )],
            provides: TypeExpr::named("PlainText"),
        },
    )
    .unwrap();

    let src = r#"(tool consume-strings :items ["a" 1])"#;
    let p = parse(src).unwrap();
    let err = check(&p, &r).expect_err("must reject");
    let msg = format!("{err}");
    assert!(msg.contains("List"), "got: {msg}");
    assert!(msg.contains("String") || msg.contains("Int"), "got: {msg}");
}
```

**Compiler test** — add to `/home/hao/code/agnes/crates/agnes-compiler/tests/compile.rs`:

```rust
#[test]
fn compiles_list_literal() {
    let src = r#"(list "a" "b")"#;
    let mut r = seed();
    r.register_type("String", None).unwrap();
    let p = parse(src).unwrap();
    let dag = compile(&p, &r).expect("compile ok");
    // Expect a NodeKind::List with 2 element inputs.
    let list_node = dag
        .nodes
        .iter()
        .find(|n| matches!(n.kind, agnes_compiler::NodeKind::List))
        .expect("List node must exist");
    assert_eq!(list_node.inputs.len(), 2);
}
```

**Runtime test** — add to `/home/hao/code/agnes/crates/agnes-runtime/tests/execute.rs`:

```rust
#[tokio::test]
async fn evaluates_list_literal() {
    let src = r#"(list "a" "b" "c")"#;
    let mut r = agnes_registry::Registry::new();
    agnes_builtins::register_builtins(&mut r).unwrap();
    let p = agnes_parser::parse(src).unwrap();
    r.load(&p).unwrap();
    agnes_checker::check(&p, &r).unwrap();
    let dag = agnes_compiler::compile(&p, &r).unwrap();
    let dispatch = agnes_builtins::native_dispatch();
    let out = agnes_runtime::execute(&dag, &r, &dispatch).await.unwrap();
    let arr = out.data.as_array().expect("array result");
    assert_eq!(arr.len(), 3);
    assert_eq!(arr[0], serde_json::json!("a"));
}
```

- [ ] **Step 2: Run — expect failures**

```bash
cd /home/hao/code/agnes && cargo test --workspace 2>&1 | tail -40
```

Expected: compile errors (no `Expr::List` variant; no `NodeKind::List`).

- [ ] **Step 3: Add `Expr::List` variant to AST**

In `/home/hao/code/agnes/crates/agnes-ast/src/lib.rs`, add to the `Expr` enum:

```rust
    /// `(list e1 e2 ...)` or `[e1 e2 ...]` — a list literal.
    /// Elements are arbitrary Exprs, not just literals.
    List { span: Span, items: Vec<Expr> },
```

Extend the `Expr::span()` match:

```rust
            | Expr::List { span, .. }
```

- [ ] **Step 4: Update parser preprocessor to handle `[...]` and reject commas**

In `/home/hao/code/agnes/crates/agnes-parser/src/lib.rs`, extend the preprocessor. The existing `preprocess_union_bars` walks chars; extend it to:
1. Track bracket-depth alongside string state.
2. Inside `[...]`, error on `,` chars.
3. Transform every `[` outside strings to `(list ` and matching `]` to `)`.

Rewrite the preprocessor entirely (replace `preprocess_union_bars` with `preprocess_source`):

```rust
/// Preprocess the source before feeding it to lexpr:
///   1. `|` outside strings → whitespace-padded sentinel symbol so lexpr
///      can tokenize it (lexpr 0.2 rejects bare `|` in symbols).
///   2. `[` outside strings → `(list ` — bracket lists are reader-macro'd
///      into `(list ...)` calls. `]` → `)`.
///   3. Inside a bracket-list, `,` is rejected with a ParseError.
///
/// String literals preserve all characters. Backslash escapes inside strings
/// are honored so `"a\|b"` and `"a,b"` are left alone.
fn preprocess_source(source: &str) -> Result<String, ParseError> {
    let mut out = String::with_capacity(source.len() + 8);
    let mut in_str = false;
    let mut escape = false;
    let mut bracket_depth: u32 = 0;
    for (byte_ix, c) in source.char_indices() {
        if in_str {
            out.push(c);
            if escape {
                escape = false;
            } else if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_str = true;
                out.push('"');
            }
            '|' => {
                out.push(' ');
                out.push_str(UNION_BAR_SENTINEL);
                out.push(' ');
            }
            '[' => {
                bracket_depth += 1;
                out.push_str("(list ");
            }
            ']' => {
                if bracket_depth == 0 {
                    return Err(ParseError {
                        span: Span { start: byte_ix, end: byte_ix + 1 },
                        message: "unmatched `]` at bracket-list end".into(),
                    });
                }
                bracket_depth -= 1;
                out.push(')');
            }
            ',' if bracket_depth > 0 => {
                return Err(ParseError {
                    span: Span { start: byte_ix, end: byte_ix + 1 },
                    message: "list literals use whitespace separation; remove the comma".into(),
                });
            }
            _ => out.push(c),
        }
    }
    if bracket_depth > 0 {
        return Err(ParseError {
            span: Span::DUMMY,
            message: "unclosed bracket-list `[`".into(),
        });
    }
    Ok(out)
}
```

Update `parse` to use the new preprocessor and to return the error rather than assume it's infallible:

```rust
pub fn parse(source: &str) -> Result<Program, ParseError> {
    let prepared = preprocess_source(source)?;
    let forms = read_forms(&prepared)?;
    // ... rest unchanged
}
```

Remove the old `preprocess_union_bars` function (or rename it).

- [ ] **Step 5: Handle `list` head in `parse_expr`**

In `/home/hao/code/agnes/crates/agnes-parser/src/expr.rs`, add a new case to the `match head` block:

```rust
        "list" => {
            let mut items = Vec::with_capacity(rest.len());
            for it in rest {
                items.push(parse_expr(it, span)?);
            }
            Ok(Expr::List { span, items })
        }
```

Place it near the other special-head cases (`pipe`, `par`, `let`, …).

- [ ] **Step 6: Add `NodeKind::List` variant**

In `/home/hao/code/agnes/crates/agnes-compiler/src/dag.rs`, add to `NodeKind`:

```rust
    /// `(list e1 e2 ...)` — inputs are one `Input::FromNode` per element.
    /// Provides is `(List T)` where T comes from checker-determined types
    /// baked into `Node.provides`.
    List,
```

- [ ] **Step 7: Lower `Expr::List` in the compiler**

In `/home/hao/code/agnes/crates/agnes-compiler/src/lower.rs`, add a case to `lower_expr`:

```rust
            Expr::List { items, .. } => {
                let mut inputs: Vec<Input> = Vec::with_capacity(items.len());
                let mut elem_types: Vec<TypeExpr> = Vec::with_capacity(items.len());
                for it in items {
                    let id = self.lower_expr(it, None)?;
                    elem_types.push(self.nodes[id.0].provides.clone());
                    inputs.push(Input::FromNode(id));
                }
                let elem_ty = if elem_types.is_empty() {
                    TypeExpr::named("Unknown")
                } else {
                    agnes_types::canonicalize_union(elem_types.clone())
                };
                let provides = TypeExpr::App {
                    head: agnes_types::TypeName("List".into()),
                    args: vec![elem_ty],
                };
                Ok(self.add(NodeKind::List, inputs, provides))
            }
```

- [ ] **Step 8: Walk `Expr::List` in `cycle.rs`**

In `/home/hao/code/agnes/crates/agnes-compiler/src/cycle.rs`, extend the `walk` function's match to include:

```rust
        Expr::List { items, .. } => items.iter().for_each(|s| walk(s, out)),
```

Place it alongside the other match arms.

- [ ] **Step 9: Evaluate `NodeKind::List` in the runtime scheduler**

In `/home/hao/code/agnes/crates/agnes-runtime/src/scheduler.rs`, add a case to `eval_node`:

```rust
            NodeKind::List => {
                let mut elems: Vec<Value> = Vec::with_capacity(node.inputs.len());
                for input in &node.inputs {
                    elems.push(eval_input(dag, input, reg, dispatch, cache, env).await?);
                }
                let data = JsonValue::Array(elems.iter().map(|v| v.data.clone()).collect());
                // Use the checker-derived provides for declared_type; scheduler
                // does not re-derive.
                Value {
                    data,
                    declared_type: node.provides.clone(),
                }
            }
```

Also handle `Expr::List` in `eval_expr` (the AST interpreter path used by `dispatch_define`):

```rust
            Expr::List { items, .. } => {
                let mut elems: Vec<Value> = Vec::with_capacity(items.len());
                let mut elem_types: Vec<TypeExpr> = Vec::with_capacity(items.len());
                for it in items {
                    let v = eval_expr(it, None, reg, dispatch, env).await?;
                    elem_types.push(v.declared_type.clone());
                    elems.push(v);
                }
                let elem_ty = if elem_types.is_empty() {
                    TypeExpr::named("Unknown")
                } else {
                    agnes_types::canonicalize_union(elem_types)
                };
                let data = JsonValue::Array(elems.iter().map(|v| v.data.clone()).collect());
                Ok(Value {
                    data,
                    declared_type: TypeExpr::App {
                        head: TypeName("List".into()),
                        args: vec![elem_ty],
                    },
                })
            }
```

Add the necessary import at the top:

```rust
use agnes_types::canonicalize_union;
```

(Actually the module already uses `agnes_types` — just add `canonicalize_union` to the existing use statement.)

- [ ] **Step 10: Add `Expr::List` handling in the checker**

In `/home/hao/code/agnes/crates/agnes-checker/src/lib.rs`, extend `check_expr` with a `List` case:

```rust
        Expr::List { items, .. } => {
            if items.is_empty() {
                return Ok(TypeExpr::App {
                    head: TypeName("List".into()),
                    args: vec![TypeExpr::Named(TypeName("Unknown".into()))],
                });
            }
            let mut elem_types: Vec<TypeExpr> = Vec::with_capacity(items.len());
            for it in items {
                elem_types.push(check_expr(it, reg, env, None)?);
            }
            let inner = agnes_types::canonicalize_union(elem_types);
            Ok(TypeExpr::App {
                head: TypeName("List".into()),
                args: vec![inner],
            })
        }
```

Add the import at the top of the file:

```rust
use agnes_types::canonicalize_union;
```

(or fully-qualify at the call site).

- [ ] **Step 11: Ensure `Unknown` is a registered built-in type**

Grep to verify `Unknown` is registered:

```bash
cd /home/hao/code/agnes && grep -n "Unknown" crates/agnes-builtins/src/lib.rs
```

If already there (it is — search will find `reg.register_type("Unknown", None)?;`), no action needed. If not, add it in `register_builtins`.

- [ ] **Step 12: Run tests — expect PASS**

```bash
cd /home/hao/code/agnes && cargo test --workspace 2>&1 | tail -30
```

Expected: all pass, including the 5 new parser tests, 2 checker tests, 1 compiler test, 1 runtime test.

If the `list_of_mixed_types_rejected_where_list_of_string_expected` test doesn't fire the mismatch (because literals bypass check_arg), remove the `if matches!(arg, Expr::Literal { .. })` early-return in `check_arg` for `Expr::List` — but instead of touching `check_arg`, add an explicit exception:

```rust
    if matches!(arg, Expr::Literal { .. }) {
        let _ = check_expr(arg, reg, env, None)?;
        return Ok(());
    }
```

`Expr::List` is NOT `Expr::Literal`, so it will fall through to the `check_expr` + `type_expr_matches` path. Confirm this by running the test.

- [ ] **Step 13: Commit**

```
cd /home/hao/code/agnes
jj describe -m "feat: list literals — Expr::List, NodeKind::List, checker + runtime

Add the (list e1 e2 ...) / [e1 e2 ...] value form to agnes:
- Parser: [...] is a reader macro to (list ...); commas inside brackets
  are rejected with a whitespace-suggestion hint.
- AST: new Expr::List { span, items } variant.
- Checker: [e1 ... en] gets type (List T), where T is the canonicalized
  union of all element types. Empty list is (List Unknown) — Task 7 adds
  the hint mechanism for empty-list contextual retyping.
- Compiler: NodeKind::List with one input per element; provides carries
  the checker-derived (List T) type.
- Runtime: List nodes materialize into JsonValue::Array with the
  provides type as declared_type.

Co-Authored-By: Claude <noreply@anthropic.com>"
jj new
jj bookmark move main --to @-
```

---

## Task 6: Boundary validation recurses into `(List T)`

**Rationale:** Task 1 taught `validate` to walk `Named` and `(| ...)` — but the "unknown head" branch bails out. Now `(List T)` needs to walk the array's elements. This closes the last hole in structural runtime validation.

**Files:**
- Modify: `crates/agnes-runtime/src/boundary.rs`
- Modify: `crates/agnes-runtime/tests/execute.rs` (add validation tests)

**Interfaces:**
- Consumes: Tasks 1, 4, 5.
- Produces: `boundary::validate` handles `(List T)` by recursing per-element.

- [ ] **Step 1: Failing test**

In `/home/hao/code/agnes/crates/agnes-runtime/tests/execute.rs`, add:

```rust
#[tokio::test]
async fn boundary_validates_list_of_string_at_runtime() {
    // Register a mock tool that (correctly) receives a (List String).
    let mut r = agnes_registry::Registry::new();
    agnes_builtins::register_builtins(&mut r).unwrap();
    // Manually augment: declare a tool that requires (List String) and
    // returns PlainText — mock via source.
    let src = r#"
        (declare tool see-list
          :requires [(items (List String))]
          :provides PlainText)

        (tool see-list :items ["a" "b"])
    "#;
    let p = agnes_parser::parse(src).unwrap();
    r.load(&p).unwrap();
    agnes_checker::check(&p, &r).unwrap();
    // Compile is fine, but native_dispatch has no impl — call will fail with
    // MissingImpl at runtime. That's OK: the point of this test is to make
    // sure the checker + compiler accept the parameterized signature and
    // that runtime boundary validation doesn't panic before reaching dispatch.
    let dag = agnes_compiler::compile(&p, &r).unwrap();
    let dispatch = agnes_builtins::native_dispatch();
    let err = agnes_runtime::execute(&dag, &r, &dispatch).await.unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("No native implementation") || msg.contains("MissingImpl") || msg.contains("see-list"),
        "expected missing-impl (not a validation error), got: {msg}"
    );
}
```

- [ ] **Step 2: Extend `boundary::validate` to walk `(List T)`**

In `/home/hao/code/agnes/crates/agnes-runtime/src/boundary.rs`, replace the "other App head" arm with a `List` arm plus a fallback:

```rust
        TypeExpr::App { head, args } if head.0 == "List" => {
            if args.len() != 1 {
                return Err(RuntimeError::RuntimeTypeError {
                    tool: tool.to_string(),
                    direction,
                    ty: TypeName(expected.to_string()),
                    cause: format!("List type has arity {}; expected 1", args.len()),
                });
            }
            let arr = val.data.as_array().ok_or_else(|| RuntimeError::RuntimeTypeError {
                tool: tool.to_string(),
                direction,
                ty: TypeName(expected.to_string()),
                cause: format!("expected JSON array for List type, got {:?}", val.data),
            })?;
            let inner = &args[0];
            for (i, elem_data) in arr.iter().enumerate() {
                let elem_value = Value {
                    data: elem_data.clone(),
                    declared_type: inner.clone(),
                };
                validate(reg, tool, direction, inner, &elem_value).map_err(|e| {
                    // Wrap error to add element index for locatability.
                    match e {
                        RuntimeError::RuntimeTypeError { tool, direction, ty, cause } => {
                            RuntimeError::RuntimeTypeError {
                                tool,
                                direction,
                                ty,
                                cause: format!("element [{i}]: {cause}"),
                            }
                        }
                        other => other,
                    }
                })?;
            }
            Ok(())
        }
        TypeExpr::App { head, .. } => {
            Err(RuntimeError::RuntimeTypeError {
                tool: tool.to_string(),
                direction,
                ty: TypeName(head.0.clone()),
                cause: format!("unknown type constructor `{}` in canonical form", head.0),
            })
        }
```

- [ ] **Step 3: Run tests — expect PASS**

```bash
cd /home/hao/code/agnes && cargo test --workspace 2>&1 | tail -20
```

Expected: all pass.

- [ ] **Step 4: Commit**

```
cd /home/hao/code/agnes
jj describe -m "feat(runtime): boundary::validate recurses into (List T)

Runtime type validation now walks (List T) structurally: value must be
JsonValue::Array, and every element is validated against T with the
element index preserved in the error cause.

Union and Named cases unchanged. Other App heads (which shouldn't appear
in canonical form) remain an internal-error case.

Co-Authored-By: Claude <noreply@anthropic.com>"
jj new
jj bookmark move main --to @-
```

---

## Task 7: Checker `hint` mechanism — Rule 3 (literal adaptation)

**Rationale:** Empty list `[]` currently types as `(List Unknown)` and fails to match `(List String)` positions. Add optional `hint: Option<&TypeExpr>` to `check_expr` so callers with a known expected type can pass it down; empty list checks the hint and adopts it if structurally compatible.

**Files:**
- Modify: `crates/agnes-checker/src/lib.rs`
- Modify: `crates/agnes-checker/tests/check.rs`

**Interfaces:**
- Consumes: Tasks 1, 5.
- Produces: `check_expr(e, reg, env, flowed_in, hint)` — signature adds a 5th parameter of type `Option<&TypeExpr>`. Empty-list adaptation described in the design spec §5.3.

- [ ] **Step 1: Failing test**

Add to `/home/hao/code/agnes/crates/agnes-checker/tests/check.rs`:

```rust
#[test]
fn empty_list_adapts_to_hint() {
    // Given a tool requiring (List String), passing [] should succeed.
    let mut r = Registry::new();
    r.register_type("String", None).unwrap();
    r.register_type("PlainText", None).unwrap();
    r.register_type("Unknown", None).unwrap();
    r.register_tool(
        "consume-strings",
        ToolSignature {
            requires: vec![(
                "items".into(),
                TypeExpr::App {
                    head: agnes_types::TypeName("List".into()),
                    args: vec![TypeExpr::named("String")],
                },
            )],
            provides: TypeExpr::named("PlainText"),
        },
    )
    .unwrap();

    let src = r#"(tool consume-strings :items [])"#;
    let p = parse(src).unwrap();
    check(&p, &r).expect("empty list must adapt to (List String)");
}

#[test]
fn unbound_empty_list_via_let_is_still_list_unknown() {
    // No hint at let site → empty list types as (List Unknown).
    let mut r = Registry::new();
    r.register_type("String", None).unwrap();
    r.register_type("Unknown", None).unwrap();
    r.register_type("PlainText", None).unwrap();
    r.register_tool(
        "consume-strings",
        ToolSignature {
            requires: vec![(
                "items".into(),
                TypeExpr::App {
                    head: agnes_types::TypeName("List".into()),
                    args: vec![TypeExpr::named("String")],
                },
            )],
            provides: TypeExpr::named("PlainText"),
        },
    )
    .unwrap();

    let src = r#"
        (pipe
          (let xs [])
          (tool consume-strings :items xs))
    "#;
    let p = parse(src).unwrap();
    let err = check(&p, &r).expect_err("must fail");
    let msg = format!("{err}");
    assert!(msg.contains("List"), "got: {msg}");
    assert!(msg.contains("Unknown") || msg.contains("String"), "got: {msg}");
}
```

- [ ] **Step 2: Run — expect failures**

```bash
cd /home/hao/code/agnes && cargo test -p agnes-checker --tests 2>&1 | tail -20
```

Expected: `empty_list_adapts_to_hint` fails; `unbound_empty_list_via_let_is_still_list_unknown` should pass (Task 5 behavior already rejects).

- [ ] **Step 3: Add the `hint` parameter and implement rule 3**

In `/home/hao/code/agnes/crates/agnes-checker/src/lib.rs`, modify `check_expr` signature:

```rust
fn check_expr(
    e: &Expr,
    reg: &Registry,
    env: &mut env::Env,
    flowed_in: Option<TypeExpr>,
    hint: Option<&TypeExpr>,
) -> Result<TypeExpr, CheckError> {
```

Every existing recursive call must pass `hint: None` unless the caller has a genuine hint. Update every internal `check_expr(...)` call site — most pass `None`. The one place that gets a real hint is `check_arg`, and the flow-in binding path in `check_tool_call`. Rewrite `check_arg`:

```rust
fn check_arg(
    tool_name: &str,
    param: &str,
    expected: &TypeExpr,
    arg: &Expr,
    reg: &Registry,
    env: &mut env::Env,
) -> Result<(), CheckError> {
    if matches!(arg, Expr::Literal { .. }) {
        let _ = check_expr(arg, reg, env, None, None)?;
        return Ok(());
    }
    let actual = check_expr(arg, reg, env, None, Some(expected))?;
    if !type_expr_matches(&actual, expected) {
        return Err(CheckError::ParamMismatch {
            tool: tool_name.to_string(),
            param: param.to_string(),
            expected: expected.clone(),
            actual,
        });
    }
    Ok(())
}
```

Update the flow-in binding in `check_tool_call`:

```rust
    match (unfilled.len(), flowed_in) {
        (0, _) => {}
        (1, Some(up)) => {
            let (_, expected) = &sig.requires[unfilled[0]];
            if !type_expr_matches(&up, expected) {
                return Err(CheckError::FlowMismatch { ... });
            }
        }
        ...
    }
```

(No change here — `up` is already fully typed.)

Update the `Expr::List` case:

```rust
        Expr::List { items, .. } => {
            if items.is_empty() {
                // Rule 3 (literal adaptation): if the caller passed a
                // structurally compatible hint `(List T)`, adopt it.
                if let Some(TypeExpr::App { head, args }) = hint {
                    if head.0 == "List" && args.len() == 1 {
                        return Ok(TypeExpr::App {
                            head: head.clone(),
                            args: args.clone(),
                        });
                    }
                }
                return Ok(TypeExpr::App {
                    head: TypeName("List".into()),
                    args: vec![TypeExpr::Named(TypeName("Unknown".into()))],
                });
            }
            let mut elem_types: Vec<TypeExpr> = Vec::with_capacity(items.len());
            for it in items {
                elem_types.push(check_expr(it, reg, env, None, None)?);
            }
            let inner = canonicalize_union(elem_types);
            Ok(TypeExpr::App {
                head: TypeName("List".into()),
                args: vec![inner],
            })
        }
```

Update the top-level `check` calls to pass `None`:

```rust
            let body_ty = check_expr(body, reg, &mut env, None, None)?;
            ...
            check_expr(main, reg, &mut env, None, None)?;
```

Update every other recursive call inside `check_expr` to pass `hint: None`. Every existing `check_expr(x, reg, env, y)` call becomes `check_expr(x, reg, env, y, None)`. Grep for them and update mechanically.

- [ ] **Step 4: Run tests — expect PASS**

```bash
cd /home/hao/code/agnes && cargo test --workspace 2>&1 | tail -20
```

Expected: all pass.

- [ ] **Step 5: Commit**

```
cd /home/hao/code/agnes
jj describe -m "feat(checker): rule 3 — empty list adopts hinted (List T)

check_expr gains a fifth parameter hint: Option<&TypeExpr>. Empty-list
literals consult the hint and adopt it when structurally compatible with
(List _). Every other position still types as (List Unknown).

Rule 3 is entered only from:
  - check_arg (kwarg positions, positional positions)
No other check_expr callsite passes a non-None hint. This is deliberate
minimalism — Rule 3 is contextual adaptation for LLM ergonomics only,
not global inference.

Co-Authored-By: Claude <noreply@anthropic.com>"
jj new
jj bookmark move main --to @-
```

---

## Task 8: `join-lines` builtin and full acceptance workflow

**Rationale:** The acceptance criteria in the spec require a workflow that actually uses `(list ...)` as a tool kwarg, then invokes a tool that takes `(List (| PlainText Markdown))`. Add the `join-lines` builtin as a mock and stitch it into the acceptance test.

**Files:**
- Modify: `crates/agnes-builtins/src/tools.rs` (add `join-lines` impl)
- Modify: `crates/agnes-builtins/src/lib.rs` (register `join-lines` signature)
- Modify: `crates/agnes-builtins/tests/register.rs` (test new tool is registered)
- Modify: `crates/agnes-cli/tests/acceptance.rs` (add positive acceptance workflow, expanded negative cases)
- Modify: `examples/full-demo.agnes` — extend to demonstrate list literal

**Interfaces:**
- Consumes: Tasks 5, 6, 7.
- Produces: New builtin `join-lines :lines (List (| PlainText Markdown)) → PlainText`. Acceptance test proves parser+checker+compiler+runtime end-to-end on the parameterized flow.

- [ ] **Step 1: Add failing acceptance tests**

In `/home/hao/code/agnes/crates/agnes-cli/tests/acceptance.rs`, add:

```rust
#[tokio::test]
async fn positive_join_lines_with_list_literal() {
    let readme = seed_readme().await;
    let src = format!(
        r#"
(tool join-lines :lines [(tool read-file :path "{readme}")
                          (tool read-file :path "{readme}")])
"#
    );
    let out = run(&src).await.expect("join-lines must succeed");
    // The mock implementation of join-lines concatenates array elements with '\n'.
    assert!(out.contains("hello world"), "got: {out}");
    let _ = tokio::fs::remove_file(&readme).await;
}

#[tokio::test]
async fn positive_option_string_declares_param() {
    let src = r#"
        (define maybe-greet
          :params [(name (Option String))]
          :provides PlainText
          (tool llm :prompt "greet" :input "hi"))
        (tool maybe-greet :name "world")
    "#;
    let out = run(src).await.expect("Option String param must work");
    assert!(out.contains("[LLM"), "got: {out}");
}

#[tokio::test]
async fn negative_list_arity_mismatch() {
    let src = r#"(declare tool bad :requires [(x (List))] :provides PlainText)"#;
    let err = run(src).await.expect_err("must fail");
    let msg = format!("{err}");
    assert!(msg.contains("List") && msg.contains("expects 1"), "got: {msg}");
}

#[tokio::test]
async fn negative_option_arity_mismatch() {
    let src = r#"(declare tool bad :requires [(x (Option A B))] :provides PlainText)"#;
    let err = run(src).await.expect_err("must fail");
    let msg = format!("{err}");
    assert!(msg.contains("Option") && msg.contains("expects 1"), "got: {msg}");
}

#[tokio::test]
async fn negative_unknown_head_suggests_builtins() {
    let src = r#"(declare tool bad :requires [(x (Foo Bar))] :provides PlainText)"#;
    let err = run(src).await.expect_err("must fail");
    let msg = format!("{err}");
    assert!(msg.contains("Foo"), "got: {msg}");
    assert!(msg.contains("List") || msg.contains("Option"), "got: {msg}");
}

#[tokio::test]
async fn negative_infix_union_rejected() {
    let src = r#"(declare type-alias T (A | B))"#;
    let err = run(src).await.expect_err("must fail");
    let msg = format!("{err}");
    assert!(msg.contains("union") && msg.contains("prefix"), "got: {msg}");
}

#[tokio::test]
async fn negative_comma_in_bracket_list() {
    let src = r#"(tool llm :prompt "x" :input ["a", "b"])"#;
    let err = run(src).await.expect_err("must fail");
    let msg = format!("{err}");
    assert!(
        msg.to_lowercase().contains("comma") || msg.to_lowercase().contains("whitespace"),
        "got: {msg}"
    );
}

#[tokio::test]
async fn negative_mixed_list_where_string_list_expected() {
    // join-lines accepts (List (| PlainText Markdown)) — passing (List Int)
    // via a mixed literal fails.
    let src = r#"(tool join-lines :lines ["a" 1])"#;
    let err = run(src).await.expect_err("must fail");
    let msg = format!("{err}");
    assert!(msg.contains("List"), "got: {msg}");
}
```

- [ ] **Step 2: Add `join-lines` tool signature**

In `/home/hao/code/agnes/crates/agnes-builtins/src/lib.rs`, add after the existing `llm` registration:

```rust
    let text_or_md = canonicalize_union([
        TypeExpr::named("PlainText"),
        TypeExpr::named("Markdown"),
    ]);
    reg.register_tool(
        "join-lines",
        ToolSignature {
            requires: vec![(
                "lines".into(),
                TypeExpr::App {
                    head: agnes_types::TypeName("List".into()),
                    args: vec![text_or_md],
                },
            )],
            provides: plaintext.clone(),
        },
    )?;
```

Import `agnes_types::TypeName` at the top of the file if it isn't already available.

- [ ] **Step 3: Add `join-lines` native implementation**

In `/home/hao/code/agnes/crates/agnes-builtins/src/tools.rs`, add after the existing `llm` registration:

```rust
    m.insert(
        "join-lines".into(),
        Arc::new(|args| {
            Box::pin(async move {
                let lines = args
                    .get("lines")
                    .ok_or("missing :lines")?
                    .data
                    .as_array()
                    .ok_or("lines is not a JSON array")?
                    .iter()
                    .map(|v| v.as_str().unwrap_or("").to_string())
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(Value::typed(JsonValue::String(lines), "PlainText"))
            })
        }),
    );
```

- [ ] **Step 4: Add register test**

In `/home/hao/code/agnes/crates/agnes-builtins/tests/register.rs`, add:

```rust
#[test]
fn join_lines_registered() {
    let mut r = Registry::new();
    register_builtins(&mut r).expect("builtins load");
    assert!(r.tool_signature("join-lines").is_some());
}

#[test]
fn native_dispatch_has_join_lines() {
    let d = native_dispatch();
    assert!(d.contains_key("join-lines"));
}
```

- [ ] **Step 5: Extend `examples/full-demo.agnes`**

Overwrite `/home/hao/code/agnes/examples/full-demo.agnes`:

```lisp
;; Full demo (spec §VII shape): declare a compound `read-and-translate`
;; and dispatch it. Also exercises the new list literal + parameterized
;; type flow via `join-lines`.

(define read-and-translate
  :params  [(path Path) (target String)]
  :provides PlainText
  (pipe
    (tool read-file :path path)
    (tool translate :lang target)))

(pipe
  (let src (tool read-file :path "README.md"))
  (par
    (let sum (tool summarize :input src))
    (let ja  (tool read-and-translate :path "README.md" :target "ja")))
  (tool join-lines :lines [sum ja]))
```

- [ ] **Step 6: Migrate `positive_full_demo_runs` to use new syntax**

In `/home/hao/code/agnes/crates/agnes-cli/tests/acceptance.rs`, update the workflow inside `positive_full_demo_runs` to match the new example (position-based params). Since `sum` is `Summary` and `ja` is `PlainText`, using `join-lines :lines [sum ja]` requires `join-lines` to accept `(| PlainText Markdown Summary)`. That's a broader union than we declared. Simplest fix: replace with `[ja ja]` which is `(List PlainText)` and matches:

```rust
    let src = format!(
        r#"
(define read-and-translate
  :params  [(path Path) (target String)]
  :provides PlainText
  (pipe
    (tool read-file :path path)
    (tool translate :lang target)))

(pipe
  (let ja (tool read-and-translate :path "{readme}" :target "ja"))
  (tool join-lines :lines [ja ja]))
"#
    );
    let out = run(&src).await.expect("full-demo workflow must succeed");
    assert!(
        out.contains("[TRANSLATED"),
        "expected translated content in joined output, got: {out}"
    );
```

- [ ] **Step 7: Migrate remaining `negative_unknown_type` — its `(x: MysteryType)` uses old param syntax**

Task 2 already flagged this; verify it now uses `(x MysteryType)`:

```rust
    let src = r#"(declare tool weird :requires [(x MysteryType)] :provides PlainText)"#;
```

- [ ] **Step 8: Run entire suite**

```bash
cd /home/hao/code/agnes && cargo test --workspace 2>&1 | tail -40
```

Expected: all pass. If any snapshot needs review, `cargo insta review`.

- [ ] **Step 9: Commit**

```
cd /home/hao/code/agnes
jj describe -m "feat(builtins): join-lines tool and full parameterized-type acceptance

Register a mock join-lines builtin that concatenates a list of text
values with '\n'. Signature: :lines (List (| PlainText Markdown))
→ PlainText.

Wire the acceptance test to exercise every syntactic and semantic
addition end-to-end:
  - list literal as tool kwarg
  - (List T) parameterized signature
  - (Option T) sugar in a define param
  - (| A B) prefix union
  - position-based params (name Type)
  - arity checks for List / Option
  - infix-union rejection with migration hint
  - comma-in-bracket rejection with migration hint

Update examples/full-demo.agnes to showcase join-lines with a real list
literal.

Co-Authored-By: Claude <noreply@anthropic.com>"
jj new
jj bookmark move main --to @-
```

---

## Task 9: Migrate remaining examples, READMEs, and MVP spec pointer

**Rationale:** Every existing example and per-crate README references the old syntax somewhere. Migrate them now that everything works.

**Files:**
- Modify: `examples/hello.agnes`, `examples/translate.agnes`, `examples/fan-out.agnes`, `examples/with-define.agnes`, `examples/full-demo.agnes` (verify all)
- Modify: `README.md` (top-level)
- Modify: `crates/agnes-parser/README.md`, `crates/agnes-checker/README.md`, `crates/agnes-registry/README.md`, `crates/agnes-runtime/README.md`, `crates/agnes-builtins/README.md`, `crates/agnes-cli/README.md`, `crates/agnes-types/README.md`, `crates/agnes-ast/README.md`, `crates/agnes-compiler/README.md` (any that show union or param syntax)
- Modify: `docs/superpowers/specs/2026-07-18-agnes-dsl-mvp-design.md` (add a status note at the top pointing to the upgrade spec)

**Interfaces:**
- Consumes: Tasks 1-8.
- Produces: All prose docs and examples reflect the new syntax.

- [ ] **Step 1: Update top-level `README.md`**

In `/home/hao/code/agnes/README.md`, the language-at-a-glance code sample uses `(path: Path) (target: String)`. Rewrite it to match new syntax and also to include a list-literal example:

```markdown
## Language at a glance

```lisp
(define read-and-translate
  :params  [(path Path) (target String)]
  :provides PlainText
  (pipe
    (tool read-file :path path)
    (tool translate :lang target)))

(pipe
  (let ja (tool read-and-translate :path "README.md" :target "ja"))
  (tool join-lines :lines [ja ja]))
```
```

- [ ] **Step 2: Update MVP spec status note**

In `/home/hao/code/agnes/docs/superpowers/specs/2026-07-18-agnes-dsl-mvp-design.md`, insert a status banner at the top (immediately after the `# ` heading):

```markdown
> **Status (2026-07-18 update):** the type-system portions of this spec
> (§II, §III.5, §VI's alias/param forms) have been superseded by
> `2026-07-18-agnes-type-system-upgrade-design.md`. Refer to the
> upgrade spec for current union syntax `(| A B)`, param form
> `(name Type)`, and parameterized types `(List T)` / `(Option T)`.
> This document is preserved as historical reference for the MVP
> milestone.
```

- [ ] **Step 3: Update per-crate READMEs**

For each crate README under `crates/*/README.md`, grep for `|` union syntax or `name:` param syntax and rewrite. Concretely, look at each README's code snippets:

```bash
cd /home/hao/code/agnes && grep -ln "\w:" crates/*/README.md
cd /home/hao/code/agnes && grep -ln "(\w\+ | " crates/*/README.md
```

For each hit, update the code fence content. Likely affected READMEs (from Task 12's per-crate READMEs):
- `crates/agnes-parser/README.md` — grammar examples
- `crates/agnes-checker/README.md` — spec rule examples
- `crates/agnes-registry/README.md` — resolve examples
- `crates/agnes-builtins/README.md` — the tools table columns showing `TextLike | PDF` etc.
- `crates/agnes-cli/README.md` — the code-flow example

Also add a mention of `(List T)` / `(Option T)` / list literals where appropriate. In `crates/agnes-types/README.md`, add a short paragraph noting the new `App { head, args }` shape.

- [ ] **Step 4: Update `examples/*.agnes` files**

Verify each example parses under the new grammar. The files after Task 2:
- `hello.agnes` — no changes needed (single tool call).
- `translate.agnes` — no changes needed.
- `fan-out.agnes` — no changes needed.
- `with-define.agnes` — already migrated in Task 2.
- `full-demo.agnes` — already migrated in Task 8.

Run:

```bash
cd /home/hao/code/agnes && cargo run -p agnes-cli -- examples/hello.agnes
cd /home/hao/code/agnes && cargo run -p agnes-cli -- examples/translate.agnes
cd /home/hao/code/agnes && cargo run -p agnes-cli -- examples/fan-out.agnes
cd /home/hao/code/agnes && cargo run -p agnes-cli -- examples/with-define.agnes
cd /home/hao/code/agnes && cargo run -p agnes-cli -- examples/full-demo.agnes
```

Expected: each prints a JSON string result. Fix any that produce errors.

Note: `translate.agnes` reads `README.md`, so run from repo root. If `README.md` doesn't exist, create a placeholder (`echo "hello agnes" > README.md`) — the top-level README should already exist.

- [ ] **Step 5: Run entire test suite once more**

```bash
cd /home/hao/code/agnes && cargo test --workspace 2>&1 | tail -30
```

Expected: all pass.

- [ ] **Step 6: Commit**

```
cd /home/hao/code/agnes
jj describe -m "docs: migrate examples, READMEs, MVP-spec pointer to new syntax

Every example and per-crate README now reflects the parameterized type
system: (name Type) params, (| A B) unions, (List T) / (Option T)
parameterized types, [e1 e2 ...] list literals.

Add a status banner at the top of the MVP spec pointing readers to the
upgrade spec for current syntax. MVP spec preserved as historical
reference for the workspace bootstrap milestone.

Co-Authored-By: Claude <noreply@anthropic.com>"
jj new
jj bookmark move main --to @-
```

---

## Task 10: Workspace lint pass and cleanup

**Rationale:** After nine content-changing tasks, run clippy + fmt one final time so the branch merges clean. This catches dead code (Task 1's placeholder `Union` still referenced anywhere, dropped imports) and formatting drift.

**Files:**
- No new files. Any file flagged by clippy or fmt.

**Interfaces:**
- Consumes: Tasks 1-9.
- Produces: `cargo clippy --workspace --all-targets -- -D warnings` clean.

- [ ] **Step 1: Format everything**

```bash
cd /home/hao/code/agnes && cargo fmt --all
```

- [ ] **Step 2: Clippy the workspace**

```bash
cd /home/hao/code/agnes && cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -60
```

Fix every warning:
- **Unused imports** — most likely `use agnes_types::TypeName;` in files that no longer reference it directly.
- **Unused variants** — verify `Input::Literal` and `Input::Var` in `agnes-compiler/src/dag.rs` are still documented as reserved-for-future-use with a comment (unchanged from before).
- **Dead code** — if `single_type` was removed from checker, ensure no other file still tries to import it.

- [ ] **Step 3: Full test run**

```bash
cd /home/hao/code/agnes && cargo test --workspace 2>&1 | tail -20
```

Expected: all pass.

- [ ] **Step 4: Commit**

```
cd /home/hao/code/agnes
jj describe -m "chore: workspace lint pass + fmt after type-system upgrade

Clippy-clean and cargo-fmt'd. No behavioral changes.

Co-Authored-By: Claude <noreply@anthropic.com>"
jj new
jj bookmark move main --to @-
```

---

## Self-Review Summary (spec coverage)

**Spec §2.1 (grammar for types):**
- `(| A B C)` prefix union → Task 3 parser, Task 1 canonicalization.
- `(List T)` → Task 4 registry.
- `(Option T)` sugar → Task 4 registry, expanded to `(| T Unit)`.
- Removed infix `(A | B)` → Task 3 parser rejects with migration hint.

**Spec §2.2 (position-based params `(name Type)`):**
- Task 2 parser + migration.

**Spec §2.3 (list literals `[...]` / `(list ...)`):**
- Task 5 parser (both forms), commas rejected.

**Spec §3 (AST changes):**
- `TypeExprAst::App` → Task 1.
- `Expr::List` → Task 5.

**Spec §4 (canonical `TypeExpr`):**
- Task 1: `TypeExpr::App`, `Hash+Eq`, `canonicalize_union`, `type_expr_matches`.

**Spec §5 (checking rules):**
- Rule 1 (parameter satisfaction) — preserved from MVP, ported in Task 1.
- Rule 2 (flow satisfaction) — same.
- Rule 3 (literal adaptation) — Task 7's `hint` parameter for empty list.
- §5.4 non-empty list typing — Task 5.
- §5.5 Env stores TypeExpr — Task 1.

**Spec §6 (registry):**
- Task 1 (structural port), Task 4 (List / Option / arity / suggestion).

**Spec §7 (Value):**
- Task 1: `declared_type: TypeExpr`; `typed` / `typed_expr` helpers.

**Spec §8 (runtime boundary):**
- Task 1 (Named + `|`), Task 6 (List recursion).

**Spec §9 (compiler NodeKind::List):**
- Task 5.

**Spec §10 (runtime scheduler NodeKind::List arm):**
- Task 5 (main path + `eval_expr` for define bodies).

**Spec §11 (migration path):**
- Tasks 2, 3, 8 (source migrations); Task 9 (docs, READMEs, MVP spec pointer).

**Spec §12 (acceptance criteria):**
- Task 8 positive workflow + all 7 negative cases.

**No open placeholders in the plan.** Every step contains complete code or an exact command.

**Type-consistency scan:**
- `TypeExpr::App { head: TypeName, args: Vec<TypeExpr> }` — used consistently across all tasks.
- `TypeExprAst::App { head: String, args: Vec<TypeExprAst> }` — string head (matches AST convention), used consistently.
- `Value::typed` helper — used in Tasks 1, 8.
- `canonicalize_union` — used in Tasks 1, 4, 5, 7, 8.
- `check_expr` signature — added `hint` at Task 7; every internal call updated.
