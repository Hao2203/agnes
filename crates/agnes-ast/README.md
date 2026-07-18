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
  Return, List, Literal, Var }` — every workflow expression form. `List`
  carries the bracketed list literal `[e1 e2 ...]`.
- `Literal::{ String, Int, Bool, Nil }` — source-position literals.
- `Param { name, ty, default }` — a formal parameter for `declare tool` /
  `define`. The surface syntax is prefix `(name Type)` (no colon).
- `TypeExprAst::{ Named, App { head, args } }` — the syntactic type
  expression. `App` covers unions (`head == "|"`), container
  constructors (`head == "List"`, `head == "Option"`), and any future
  head; args are recursive `TypeExprAst` nodes. Alias resolution and
  canonicalization into `agnes_types::TypeExpr` happens in
  `agnes-registry`.
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
- `TypeExprAst` is *syntactic*: `Named("TextLike")` here is still an
  alias, and an `App { head: "Option", ... }` still needs desugaring
  into `(| T Unit)`. Resolution to canonical `agnes_types::TypeExpr`
  happens in `agnes-registry::Registry::resolve`.
- No trait / typeclass layer. The type system is: atomic Names, `App`
  applications (unions + parameterized containers), and aliases —
  nothing more.
- The crate is deliberately kept dependency-free so it compiles instantly
  and can be re-used from any tooling.
