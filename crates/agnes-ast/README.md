# agnes-ast

Abstract syntax tree types for the agnes DSL.

This is the leaf crate of the workspace — it has no internal dependencies
and every other crate consumes it. The parser produces these types; the
registry, checker, compiler, and runtime all read them.

## What lives here

- `Program { toplevels, main }` — a parsed `.agnes` file.
- `TopLevel::{ DeclareType, DeclareTypeAlias, DeclareTool, Define }` — the
  four registration/declaration forms.
- `Expr::{ Tool, Pipe, Par, Let, If, Match, Foreach, Retry, Catch, Llm,
  Return, Literal, Var }` — every workflow expression form.
- `Literal::{ String, Int, Bool, Nil }` — source-position literals.
- `Param { name, ty, default }` — a formal parameter for `declare tool` /
  `define`.
- `TypeExprAst::{ Named, Union }` — the syntactic type expression, before
  the registry resolves aliases and flattens unions into `agnes_types`.
- `Span { start, end }` — byte offsets into the source, threaded through
  every node for error rendering.
- `KwArgs = Vec<(String, Expr)>` — keyword arguments (`:key value`).

`Expr::span()` returns the span of any expression regardless of variant.

## Pipeline position

```
.agnes source
     ↓  agnes-parser
   agnes-ast   ← you are here
     ↓  agnes-registry (registers types/aliases/tools)
     ↓  agnes-checker  (walks Expr with the registry)
     ↓  agnes-compiler (lowers Expr → Dag)
     ↓  agnes-runtime  (executes the Dag)
```

## Design notes

- All variants carry a `Span`; renderers rely on it — never construct
  synthesized nodes with `Span::DUMMY` in code paths that surface to users.
- `TypeExprAst` is *syntactic*: `Named("TextLike")` here is still an alias.
  Resolution to a canonical `TypeExpr` (a flat `HashSet<TypeName>`) happens
  in `agnes-registry::Registry::resolve`.
- No trait / typeclass layer in MVP. Only Type + Union + Alias.
- The crate is deliberately kept dependency-free so it compiles instantly
  and can be re-used from any tooling.
