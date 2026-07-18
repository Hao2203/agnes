# agnes Type System Upgrade — Parameterized Types

**Date:** 2026-07-18
**Status:** Design (superseding parts of `2026-07-18-agnes-dsl-mvp-design.md` §II, §III, §VI)
**Predecessor:** `2026-07-18-agnes-dsl-mvp-design.md` (MVP spec — parts preserved by reference)

## 0. Motivation & Scope

The MVP type system has two variants — `Named` and `Union` — and one rule: set-membership on `HashSet<TypeName>`. This is deliberately spare, and it works. But it does not represent **structure inside a type**, so `List<String>` cannot be spelled. This blocks any tool whose parameter is "a sequence of typed values" — the immediate motivator is a `run` local-command tool that needs `:args (List String)`.

This upgrade generalizes the type system so it can express parameterized types (`List`, `Option`), rewrites the Union syntax onto standard S-expression form, and cleans up a few concrete Lisp-idiomatic wrinkles that made the language read as "sexpr with exceptions". The result:

- One core type constructor form: `(head arg₁ arg₂ …)`.
- One decision procedure: recursive positional match, with `|` (union) as the only head that participates in set-membership.
- One rule for how to read every parenthesized form in the language: **head first, arguments after**.

This spec **does not** introduce the `run` tool. That is a follow-up (sub-project B) that will land after this upgrade is complete.

## 1. Non-Goals

- **No type variables**: `(List T)` where `T` is a lowercase generic is rejected. Users write `(List String)`, `(List (| String Int))`, etc. — always concrete.
- **No variance**: types are invariant. `(List String)` and `(List Markdown)` are unrelated. To accept both, spell it `(List (| String Markdown))`.
- **No global unification**: no Hindley-Milner-style inference. The one local piece of directional inference is described in §5.3 (rule 3).
- **No `Map`, `Tuple`, `Result`, or other parameterized types this round.** The mechanism (`App` head + args) is general, so future additions are localized changes; but only `List` and `Option` are added now, plus `|` on the new mechanism.
- **No change to language semantics for `pipe`, `par`, `let`, `if`, `match`, `foreach`, `retry`, `catch`, `return`, `llm`, or tool dispatch.** Only the type system, syntax for types, and one new expression form (list literal) change.

## 2. Grammar Changes

### 2.1 Types

```
TypeExpr  ::= Symbol                              # atomic type name
            | "(" Head TypeExpr* ")"              # parameterized / union form
Head      ::= "|" | "List" | "Option" | Symbol    # any symbol; only three are built-in constructors
```

**Removed:** the infix union form `(A | B | C)`. `|` is now a head, not an infix separator.

**Examples:**

```lisp
PlainText                          ;; atomic
(List String)                      ;; parameterized
(| PlainText Markdown HTML)        ;; union — NEW SYNTAX (was: (PlainText | Markdown | HTML))
(Option String)                    ;; sugar for (| String Unit)
(List (| PlainText Markdown))      ;; nested
(List Unknown)                     ;; the type of an untyped empty list — see §5.3
```

### 2.2 Parameter Declarations

Parameter form in `:params [...]` and `:requires [...]` changes from `(name: Type ...)` to `(name Type ...)` — position-based rather than name-with-suffix-colon.

**Before:**
```lisp
(declare tool ocr :requires [(source: (PDF | Image))] :provides PlainText)
(define greet :params [(who: PlainText) (times: Int :default 1)] :provides PlainText body)
```

**After:**
```lisp
(declare tool ocr :requires [(source (| PDF Image))] :provides PlainText)
(define greet :params [(who PlainText) (times Int :default 1)] :provides PlainText body)
```

Rules for a param element:

- Element is a list. First position is the param name (symbol). Second position is the type. Following positions are optional `:kwarg value` pairs (currently only `:default lit`).
- Names remain kebab-case for tool params (unchanged convention).

### 2.3 List Literals

New expression form at the value layer:

```
Expr ::= … existing forms …
       | "(" "list" Expr* ")"
       | "[" Expr* "]"                            # reader-macro form
```

Both forms produce a `Expr::List { items }`. The bracket form is a reader macro — the parser rewrites `[e₁ e₂ …]` to `(list e₁ e₂ …)` before the rest of the pipeline sees it. **Commas are forbidden inside `[...]`**; the parser emits `ParseError` on encountering `,` with a message suggesting whitespace-separated elements.

Examples:

```lisp
["a" "b" "c"]                      ;; a (List String) — three elements
(list "a" "b" "c")                 ;; same
[]                                 ;; a (List Unknown) — empty
[(tool read-file :path "x") "y"]   ;; elements can be any Expr
```

## 3. AST Changes

```rust
// agnes-ast

pub enum TypeExprAst {
    Named(String),
    App { head: String, args: Vec<TypeExprAst> },
    // Union variant REMOVED. (A | B) is App { head: "|", args: [A, B] }.
}

pub enum Expr {
    // … existing variants …
    List { span: Span, items: Vec<Expr> },
}
```

`Literal`, `Param`, `TopLevel`, `Program`, `Span`, `KwArgs` unchanged.

`Param { name, ty, default }` unchanged in shape; only the parser producing it differs.

## 4. Canonical Type Representation

```rust
// agnes-types

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TypeExpr {
    Named(TypeName),
    App { head: TypeName, args: Vec<TypeExpr> },
}
```

`Hash + Eq` are derived so `TypeExpr` can be a `HashSet` key (used in `canonicalize_union` and the checker).

### 4.1 Canonical-Form Invariants

Enforced by `agnes_registry::Registry::resolve`:

1. All aliases resolved: no `Named(name)` where `name` is an alias.
2. `(Option T)` is expanded to `(| T Unit)`.
3. Union args are all `Named` (nested `|` is flattened).
4. Union args are deduplicated and stored in a stable order (alphabetical) so `Hash` / `Eq` are order-invariant.
5. Single-element unions collapse to `Named`.
6. Only the three built-in heads (`List`, `Option`, `|`) plus registered type constructor names appear as `App.head`. Unknown head → `UnknownName` error at resolve time.

`(Option ...)` never appears in canonical form — always expanded. `List` remains as `App` — its arg is itself a `TypeExpr` and is not flattened.

### 4.2 Matching Rule

```
type_expr_matches(actual, expected):
    match (actual, expected):
        (Named(a), Named(b))                         → a == b
        (_, App { head: "|", args })                 → any(type_expr_matches(actual, u) for u in args)
        (App { head: h1, args: a1 }, App { head: h2, args: a2 }) if h1 == h2 →
            a1.len() == a2.len() && zip(a1, a2).all(type_expr_matches)
        _                                            → false
```

Notes:
- Only the `expected` side's `|` participates in set-membership. `actual` on the left is always a concrete type in normal use (tool provides / literal type / list literal reconstructed type). The recursion enters `expected`'s union branch whenever the current expected node is a `|`, at any depth.
- All other App heads (`List` and future container constructors) match **structurally**: same head, same arity, arg-by-arg recursive match. Because the recursion enters union expansion at each level independently, `(List String)` **does** match `(List (| String Int))`: outer heads match, then recurse `String` vs `(| String Int)`, and `String` is a union member. This is the intended "widening through union" semantics — widening happens wherever `expected` has a `|`, not just at the top level.
- No sub-type / variance rules beyond union widening. `(List String)` does **not** match `(List Markdown)`: outer heads match but `String ≠ Markdown` and neither side is a union.

## 5. Type Checking Rules

The MVP spec's §II.4 rules 1 and 2 are preserved verbatim, but their bodies now bottom out at the recursive `type_expr_matches`. A new **rule 3** is added.

### 5.1 Rule 1: Parameter Satisfaction

Unchanged wording. For every keyword arg `(:k v)` in a tool call, `type_expr_matches(type_of(v), expected_type_of(:k))` must hold.

### 5.2 Rule 2: Flow Satisfaction

Unchanged wording. In a `pipe`, if the downstream tool has exactly one unfilled required parameter, `type_expr_matches(upstream_provides, that_param_type)` must hold.

### 5.3 Rule 3: Literal Adaptation (new)

Two forms of literal are **contextually retypable** when the checker has a target type in mind:

- The **empty list** `[]` / `(list)` — default type is `(List Unknown)`. In any position expecting `(List T)` for some `T`, the empty list adopts `(List T)` and the check passes without recursion.
- The literal **`nil`** — default type is `Unit`. In any position expecting a type containing `Unit` in a union (e.g., `(Option T)`), `nil` adopts `Unit` and matches through the union rule (rule 2 covers this already; rule 3 is only about the empty list).

The mechanism: `check_expr` takes an optional `hint: Option<&TypeExpr>` parameter. Callers pass the expected type where one is available (kwarg positions, flow positions with a single unfilled required param). `Expr::List` with no items consults the hint: if present and structurally compatible (`App { head: "List", args: _ }`), use it; otherwise default to `(List Unknown)`.

Positions **without** a hint (e.g., `(let xs [])` where `xs` has no downstream user visible to the checker):

- The empty list has type `(List Unknown)`.
- `(List Unknown)` does **not** automatically match `(List T)` for other `T`. It must appear in a position that also accepts `(List Unknown)` — or, more commonly, the empty list should not be bound via `let` and then used downstream; write the empty list directly at the tool call site.

### 5.4 Type of Non-Empty List Literals

For `[e₁ e₂ … eₙ]`, the checker recursively types each `eᵢ` to get `Tᵢ`, then:

- If all `Tᵢ` are the same, list type is `(List T)`.
- If `Tᵢ` differ, list type is `(List U)` where `U = canonicalize_union([T₁, …, Tₙ])`.

Rule 3 does **not** widen individual `eᵢ` based on the surrounding hint (only the empty case does). Non-empty lists have a fully determined type from their elements.

### 5.5 Env

The type environment stores `TypeExpr` values (not just `TypeName`), so a variable can be bound to a parameterized type via `let`.

## 6. Registry Changes

`Registry::resolve(&TypeExprAst) -> Result<TypeExpr, RegistryError>` implements the canonicalization rules of §4.1. Concretely:

- `Named(name)` — look up as alias first, then as type; error `UnknownName` otherwise. Alias body already canonical.
- `App { head: "|", args }` — resolve each arg, flatten any inner `|`, deduplicate into a stable `Vec`, collapse if 1 element.
- `App { head: "Option", args }` — arity check (exactly 1), resolve arg, expand into `(| T Unit)` via the `|` path.
- `App { head: "List", args }` — arity check (exactly 1), resolve arg, return `App { head: "List", args: [resolved] }`.
- `App { head: other, args }` — currently rejected with `UnknownName { name: other }`. Message suggests `List`, `Option`, `|`.

New error variant:

```rust
#[error("Type constructor `{head}` expects {expected} arg(s), got {actual}.\n  Fix: `({head} ...)` takes {expected} type argument{plural}.")]
ArityMismatch { head: String, expected: usize, actual: usize, plural: &'static str }
```

Alias body may contain parameterized types (`(declare type-alias Argv (List String))` — legal). Alias name may **not** appear as an App head (`(declare type-alias L List)` followed by `(L Int)` — `L` looked up as constructor, not found, reported as `UnknownName`).

`agnes_types::canonicalize_union(members: impl IntoIterator<Item = TypeExpr>) -> TypeExpr` is added as a shared helper. Used by registry (for `|` resolution) and by the checker (for typing non-empty mixed-element list literals).

## 7. Value Representation

```rust
// agnes-types

pub struct Value {
    pub data: serde_json::Value,
    pub declared_type: TypeExpr,   // was TypeName in MVP
}

impl Value {
    pub fn typed(data: JsonValue, ty: impl Into<TypeName>) -> Self {
        Self { data, declared_type: TypeExpr::Named(ty.into()) }
    }
    pub fn typed_expr(data: JsonValue, ty: TypeExpr) -> Self {
        Self { data, declared_type: ty }
    }
}
```

Every construction site in `agnes-builtins/src/tools.rs`, `agnes-runtime`, and any downstream tool code migrates to these constructors or to explicit struct construction with the new field type.

## 8. Runtime Boundary Validation

`agnes_runtime::boundary::validate` becomes recursive on the expected `TypeExpr`:

```rust
pub fn validate(
    reg: &Registry,
    tool: &str,
    direction: &'static str,
    expected: &TypeExpr,
    val: &Value,
) -> Result<(), RuntimeError>;
```

Recursion rules:

- **`Named(n)`** — look up `n`'s validator via `reg.validator_of`. If present, call it on `val.data`. If absent (`String`, `Int`, `Bool`, `Unknown`), skip.
- **`App { head: "|", args }`** — the value's `declared_type` must recursively match one of the union members via `type_expr_matches`. Once a match is found, recurse into `validate` with that member as `expected`. If no member matches, error.
- **`App { head: "List", args: [inner] }`** — `val.data` must be `JsonValue::Array`. For each element, recursively `validate` against `inner`. The element's `Value` is reconstructed on the fly: `data` is the element's `JsonValue`, `declared_type` comes from the surrounding `List`'s known element type (the `inner` in scope).
- **`App { head: other, .. }`** — canonical form violation; internal error.

`Validator` remains `fn(&JsonValue) -> Result<(), String>` — no boxed closures needed. Composition happens in `validate` via recursion on the type, not by composing validators.

The MVP `RuntimeError::RuntimeTypeError` variant is unchanged. When a nested element fails validation, the cause message includes the element's array index for locatability.

## 9. Compiler Changes

`agnes-compiler` gains one new node kind:

```rust
NodeKind::List
```

Inputs: one per element, in order, each an `Input::FromNode(NodeId)` produced by lowering the element expression. Provides: `TypeExpr::App { head: "List", args: [element_type] }` where element_type is the checker-determined list type's inner (available via the AST → node lowering because the checker has already run).

For the empty list, `provides` is `(List Unknown)` unless the surrounding position was already hinted — in which case the compiler uses the hinted type as `provides`. (The compiler does not re-do inference; it lowers with the types the checker already assigned.)

Other node kinds' `provides` fields already have type `TypeExpr` from the MVP; no signature change needed.

## 10. Runtime Scheduler Changes

`agnes_runtime::scheduler::eval_node` gets one new arm:

```rust
NodeKind::List => {
    let mut elems = Vec::with_capacity(node.inputs.len());
    for input in &node.inputs {
        elems.push(eval_input(dag, input, reg, dispatch, cache, env).await?);
    }
    let elem_ty = derive_element_type_from(&elems); // union of elem declared_types
    Value {
        data: JsonValue::Array(elems.iter().map(|v| v.data.clone()).collect()),
        declared_type: TypeExpr::App {
            head: TypeName("List".into()),
            args: vec![elem_ty],
        },
    }
}
```

`derive_element_type_from` mirrors §5.4: single type → `Named(T)`; mixed → `canonicalize_union(elem_types)`. For empty at runtime (only reachable if the compiler passed through an already-typed empty), `declared_type` uses the node's `provides` directly.

## 11. Migration Path

This is a **breaking change** to the syntax. All existing `.agnes` files and internal MVP artifacts must migrate:

- **Documentation:** the MVP spec `2026-07-18-agnes-dsl-mvp-design.md` is preserved as historical reference; add a short note at its head pointing to this upgrade for the current type-system spec. Example section IX rewrites tool signatures using the new `(name Type)` param form and `(| A B)` union form.
- **Examples:** `examples/hello.agnes`, `translate.agnes`, `fan-out.agnes`, `with-define.agnes`, `full-demo.agnes` — the ones using `define` (`with-define.agnes`, `full-demo.agnes`) currently spell params as `(path: Path)` and must be rewritten to `(path Path)`. `fan-out.agnes` uses `par` and existing tools with no param declarations, unaffected. The other two are single tool calls, unaffected.
- **Built-ins:** `agnes-builtins/src/aliases.rs` (`TextLike`, `VisualDoc`) and `register_builtins` tool signatures — mechanical replacement of `TypeExpr::Union(HashSet::from([...]))` with `canonicalize_union([...])`; every `Value` construction switches to `Value::typed(...)`.
- **Tests:** `agnes-parser/tests/parse.rs`, `agnes-checker/tests/check.rs` snapshots, `agnes-registry/tests/register.rs`, `agnes-compiler/tests/compile.rs`, `agnes-runtime/tests/execute.rs`, `agnes-cli/tests/acceptance.rs` — all touch either union syntax, param syntax, or Value construction; re-record insta snapshots.
- **Per-crate READMEs** — update the code snippets that show union types or param syntax.

There is **no runtime-compat shim** for the old syntax; the parser rejects `(A | B)` outright with a clear "use `(| A B)` instead" hint.

## 12. Acceptance Criteria

A single acceptance workflow that exercises the whole upgrade end-to-end:

```lisp
(declare type-alias Argv (List String))

;; A dormant declare — proves parser + registry + checker + compiler
;; can carry an alias-to-parameterized type through the whole pipeline.
;; The `run` implementation lands in sub-project B.
(declare tool run
  :requires [(cmd String) (args Argv)]
  :provides PlainText)

;; Exercises (Option T) sugar expanding to (| T Unit) and a param that
;; can be nil at runtime.
(define maybe-greet
  :params [(name (Option String))]
  :provides PlainText
  (tool llm :prompt "greet" :input "hi"))

;; Exercises list literals as tool kwargs and the (| ...) union head.
(declare tool join-lines
  :requires [(lines (List (| PlainText Markdown)))]
  :provides PlainText)

;; Actually runs join-lines with a list literal of two PlainText values.
;; Both bracket and (list ...) forms should parse to the same shape.
(pipe
  (tool join-lines :lines [(tool read-file :path "README.md")
                           (tool read-file :path "README.md")]))
```

The compiler + checker must accept the above with no errors. The runtime must execute the workflow — `join-lines` requires a mock implementation in `agnes-builtins` for the acceptance run (it can be as trivial as concatenating the array elements with newlines).

**Negative cases** must produce What/Why/Fix errors:

- `["a" 1]` in a position expecting `(List String)` → `ParamMismatch` naming `(List (| Int String))` vs `(List String)`.
- `(List)` (zero args) → `ArityMismatch` for `List` expects 1.
- `(Option A B)` → `ArityMismatch` for `Option` expects 1.
- `(Foo Bar)` where `Foo` is not a registered type constructor → `UnknownName` on `Foo`; message suggests `List`, `Option`, `|`.
- Old syntax `(A | B)` → `ParseError` with hint "union types now use prefix form `(| A B)`".
- `["a", "b"]` (commas inside brackets) → `ParseError` with hint "list literals use whitespace separation; remove the comma".
- `(let xs [])` followed by `(tool run :cmd "x" :args xs)` → `ParamMismatch` (`xs`'s type is `(List Unknown)` because the `let` binding has no hint; `(List Unknown)` does not match `(List String)`).

## 13. Deferred / Future Work

- **`Map<K,V>`** — same App mechanism, but adds a JSON-object literal syntax question. Deferred to a follow-up.
- **`Tuple`** — requires positional access / destructuring syntax (`nth`, pattern match on elements), which is a separate feature. Deferred.
- **Type variables and parametric `define`** — e.g., `(define map :params [(f (-> T U)) (xs (List T))] :provides (List U))`. Requires higher-kinded plumbing and unification. Deferred; the current design does not preclude it.
- **`nil` widening beyond `(| … Unit)`** — currently `nil` only fits union positions containing `Unit`. If future tools want an untyped null, they can accept `Unknown`.
- **Rich error messages for arg-position mismatch** — e.g., "container heads match; arg 0 differs: expected X, got Y". Deferred cosmetic improvement.

## 14. Cross-References

- MVP spec: `docs/superpowers/specs/2026-07-18-agnes-dsl-mvp-design.md` §II (superseded), §III.5 (partially affected — param syntax), §VI (aliases rewritten with new syntax), §VII (acceptance workflows migrate).
- Follow-up: sub-project B (`run` local-command tool) will consume `(List String)` from this upgrade.
