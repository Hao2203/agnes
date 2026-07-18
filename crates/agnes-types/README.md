# agnes-types

Semantic type system for agnes: the canonical representation the checker
and runtime operate on after aliases are resolved and unions are flattened.

## What lives here

- `TypeName(pub String)` — canonical name of a type, alias, or type
  constructor. PascalCase by convention for atoms (`PlainText`,
  `Markdown`, `PDF`), lowercase for the union head (`|`), initial-cap
  for container heads (`List`).
- `TypeExpr` — the canonical shape. Exactly two variants:
  - `Named(TypeName)` — an atomic type name.
  - `App { head: TypeName, args: Vec<TypeExpr> }` — a constructor
    application. `head == "|"` for unions (args are the flattened,
    deduplicated, alphabetically-sorted members); `head == "List"` for
    `(List T)`; more container heads can be added the same way.
  - `(Option T)` never appears in canonical form — the registry expands
    it to `App { head: "|", args: [T, Unit] }` at resolve time.
- `canonicalize_union(members)` — flattens nested `(| ...)`, dedupes,
  sorts, and collapses a singleton back to `Named`.
- `type_expr_matches(actual, expected) -> bool` — the recursive
  decision procedure. Both spec type rules call this and nothing else;
  the recursion enters union expansion at every level of `expected`,
  so `(List String)` matches `(List (| String Int))`.
- `Validator = fn(&serde_json::Value) -> Result<(), String>` — structural
  runtime check attached to a `TypeName` in the registry.
- `ToolSignature { requires: Vec<(String, TypeExpr)>, provides: TypeExpr }`
  — a tool's canonical signature after alias resolution.
- `Value { data: serde_json::Value, declared_type: TypeExpr }` — a value
  flowing between tools. Carries the declared type of the producing tool
  (which may be a parameterized `TypeExpr`, e.g. `(List String)`) so the
  runtime can validate at every boundary.

## The two spec rules, in one line each

1. **Parameter satisfaction:** `type_expr_matches(&arg_type, &param_expected)`
2. **Flow satisfaction:** `type_expr_matches(&upstream_provides, &downstream_expected)`

Both rules reduce to structural matching after this crate has
normalized the `TypeExpr`.

## Pipeline position

```
agnes-ast::TypeExprAst  (syntactic, includes App-shape for (List T) etc.)
        ↓  agnes-registry::resolve
agnes-types::TypeExpr   (canonical)  ← you are here
        ↓
    agnes-checker uses type_expr_matches
    agnes-runtime uses Validator (and recurses through App args) at every tool boundary
```

## Design notes

- Union args are kept as a sorted `Vec` on purpose: canonical form has
  no order dependence, and `Display` prints them alphabetically so
  error messages are stable.
- No inheritance, no subtyping, no variance: only structural matching
  plus set membership at `|` nodes.
