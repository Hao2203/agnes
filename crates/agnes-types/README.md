# agnes-types

Semantic type system for agnes: the canonical representation the checker
and runtime operate on after aliases are resolved and unions are flattened.

## What lives here

- `TypeName(pub String)` — canonical name of a type or alias. PascalCase
  by convention (`PlainText`, `Markdown`, `PDF`).
- `TypeExpr::{ Named(TypeName), Union(HashSet<TypeName>) }` — always
  canonicalized: `Union` is flat (no nested unions) and aliases have
  already been resolved.
- `type_expr_matches(actual: &TypeName, expected: &TypeExpr) -> bool` —
  the single decision procedure: set-membership. Both spec type rules call
  this and nothing else.
- `Validator = fn(&serde_json::Value) -> Result<(), String>` — structural
  runtime check attached to a `TypeName` in the registry.
- `ToolSignature { requires: Vec<(String, TypeExpr)>, provides: TypeExpr }`
  — a tool's canonical signature after alias resolution.
- `Value { data: serde_json::Value, declared_type: TypeName }` — a value
  flowing between tools. Carries the declared type of the producing tool so
  the runtime can validate at every boundary.

## The two spec rules, in one line each

1. **Parameter satisfaction:** `type_expr_matches(&arg_type, &param_expected)`
2. **Flow satisfaction:** `type_expr_matches(&upstream_provides, &downstream_expected)`

Both rules reduce to a single `HashSet::contains` after this crate has
normalized the `TypeExpr`.

## Pipeline position

```
agnes-ast::TypeExprAst  (syntactic)
        ↓  agnes-registry::resolve
agnes-types::TypeExpr   (canonical)  ← you are here
        ↓
    agnes-checker uses type_expr_matches
    agnes-runtime uses Validator at every tool boundary
```

## Design notes

- `Union` is a `HashSet` on purpose: rule checks are membership tests, and
  the canonical form has no order.
- `TypeExpr::Display` sorts union members alphabetically so error messages
  are stable.
- No inheritance, no subtyping, no variance: only set membership.
