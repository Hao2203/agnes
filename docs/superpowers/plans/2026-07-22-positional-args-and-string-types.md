# 位置参数与 String 类型简化 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make string literals usable directly with text tools, and switch tool calls from `:kw value` to positional Lisp-style args, so the LLM can generate correct workflows for basic tasks like writing a file.

**Architecture:** Three independent refactors stacked on `main` (`86a7040`): (1) de-special-case `llm` by removing the redundant `Expr::Llm`/`NodeKind::Llm` (it is already callable as `(tool llm …)`); (2) drop keyword args from tool calls, leaving `Expr::Tool` with positional args only and deleting `KwArgs`; (3) replace the unreachable semantic text/doc types (`PlainText`/`Markdown`/`HTML`/`Summary`/`PDF`/`Image`) with `String` and delete the `ocr` tool. Tool impls are untouched (they read args by name from a `HashMap`).

**Tech Stack:** Rust 2024, Cargo workspace, `lexpr` for parsing, `tokio` async runtime, `serde_json`. VCS is **jj** (colocated git).

## Global Constraints

- Spec: `docs/superpowers/specs/2026-07-22-positional-args-and-string-types-design.md` (already committed in change `xwmvlyuu` on top of `main`).
- Build/test gate after every task: `cargo build --workspace && cargo test --workspace` must pass.
- **jj commit workflow:** The working copy currently holds the spec (change `xwmvlyuu`). Each task starts with `jj new` (fresh change), does its work, then commits with `jj describe -m "<msg>"`. The next task begins with another `jj new`. Do not use raw `git commit` (jj manages the colocated git repo).
- Keep `define`/`declare tool` `:params`/`:provides`, `retry` `:times`, `catch` `:fallback` keyword syntax — these are special forms, not tool calls. Do **not** touch them.
- Keep `Path`/`JSON` validators (`path_validator`/`json_validator`) — their safety value is unchanged.
- Param **names** are unchanged (`lines`/`content`/`input`/`path`/`prompt`/`lang`/`command`). Tool impls read args by these names; only the call syntax and declared types change.

---

## File Structure

Production code touched (per responsibility):

- `crates/agnes-ast/src/lib.rs` — `Expr` enum (`Tool`, `Llm`), `KwArgs` type alias.
- `crates/agnes-parser/src/expr.rs` — `parse_tool`, `parse_expr` `llm` arm, `parse_positional_and_kwargs`.
- `crates/agnes-checker/src/lib.rs` — `check_expr` `Tool`/`Llm` arms, `check_tool_call`.
- `crates/agnes-compiler/src/lower.rs` — `lower_expr` `Tool`/`Llm` arms, `lower_tool`.
- `crates/agnes-compiler/src/dag.rs` — `NodeKind` enum (`Llm` variant).
- `crates/agnes-runtime/src/scheduler.rs` — `eval_expr` `Tool`/`Llm` arms, `eval_node` `Llm` arm, `bind_tool_args`.
- `crates/agnes-builtins/src/lib.rs` — type/alias/tool registration.
- `crates/agnes-builtins/src/{aliases,types,shows,tools}.rs` — aliases, validators, show impls, `ocr` impl.
- `crates/agnes-llm/src/planner.rs` — tool catalog order, system prompt.
- `examples/*.agnes`, `README.md`, `examples/chat-demo.md` — syntax/type migration.

Tests touched: `agnes-parser/tests/parse.rs`, `agnes-checker/tests/check.rs`, `agnes-compiler/tests/compile.rs`, `agnes-runtime/tests/execute.rs`, `agnes-builtins/tests/{shows,register,dispatch_routing,finish_observe}.rs`, `agnes-llm/tests/planner.rs`, `agnes-cli/tests/*`, `agnes-session/tests/*`.

---

### Task 1: De-special-case `llm`

`llm` is already callable as `(tool llm …)` (examples use this form; no test/example uses bare `(llm …)`). `Expr::Llm`/`NodeKind::Llm` duplicate the `Expr::Tool`/`NodeKind::Tool { name: "llm" }` path. Remove the duplication. After this task, `llm` is a regular tool invoked via `(tool llm :prompt "x" :input "")` (kwargs still allowed until Task 2).

**Files:**
- Modify: `crates/agnes-ast/src/lib.rs` (remove `Expr::Llm` variant)
- Modify: `crates/agnes-parser/src/expr.rs` (remove `"llm"` arm in `parse_expr`)
- Modify: `crates/agnes-checker/src/lib.rs` (remove `Expr::Llm` arm in `check_expr`)
- Modify: `crates/agnes-compiler/src/lower.rs` (remove `Expr::Llm` arm in `lower_expr`)
- Modify: `crates/agnes-compiler/src/dag.rs` (remove `Llm` variant from `NodeKind`)
- Modify: `crates/agnes-runtime/src/scheduler.rs` (remove `Expr::Llm` arm in `eval_expr`, `NodeKind::Llm` arm in `eval_node`)
- Test: `crates/agnes-builtins/tests/dispatch_routing.rs` (confirm `(tool llm …)` path covers llm)

**Interfaces:**
- Consumes: `Expr::Tool { name, positional, args }` (unchanged this task), `NodeKind::Tool { name }`.
- Produces: `Expr` no longer has `Llm`; `NodeKind` no longer has `Llm`. `llm` resolves through `tool_signature("llm")` (already registered) and `dispatch["llm"]` (already implemented in `tools.rs`).

- [ ] **Step 1: Start a fresh jj change**

Run: `jj new`
Expected: working copy moves to a new empty change on top of `xwmvlyuu`.

- [ ] **Step 2: Write the failing preservation test**

Add to `crates/agnes-builtins/tests/dispatch_routing.rs` (append a new test; the file already has `DUMMY` resolver and `args()` helper — reuse them):

```rust
#[tokio::test]
async fn llm_is_callable_via_tool_form() {
    // After de-special-casing, `llm` is an ordinary tool reached through
    // `(tool llm :prompt "p" :input "")`. The mock provider is wired by the
    // existing `dispatch()` in this file; exercise it the same way read-file etc. are.
    let d = dispatch();
    let llm = d.get("llm").expect("llm tool registered");
    let out = llm.call(args(&[("prompt", "hi"), ("input", "")]), &DUMMY).await.unwrap();
    assert_eq!(out.declared_type.to_string(), "PlainText"); // still PlainText until Task 3
}
```

If `dispatch()` in this file does not already wire a mock provider that returns a string for `llm`, reuse the exact mock setup used by the existing `read-file`/`summarize` tests in the same file (copy the `MockProvider::new(...)` call pattern). The assertion target type is `PlainText` because Task 3 has not run yet.

- [ ] **Step 3: Run test to confirm current behavior**

Run: `cargo test -p agnes-builtins --test dispatch_routing llm_is_callable_via_tool_form`
Expected: PASS (the `(tool llm …)` path already works). This test locks the behavior we must preserve across the removal.

- [ ] **Step 4: Remove `Expr::Llm` from the AST**

In `crates/agnes-ast/src/lib.rs`, delete the entire `Llm` variant:

```rust
    /// `(llm arg1 arg2 ... :key value ...)` - a builtin form for the LLM tool.
    Llm {
        span: Span,
        positional: Vec<Expr>,
        args: KwArgs,
    },
```

Also delete the doc-comment line on `Expr::Tool` that mentions `[:retry N] [:on-error <expr>]` if present is unrelated — leave `Expr::Tool` as-is this task (its `args` field stays until Task 2).

- [ ] **Step 5: Remove the parser `"llm"` arm**

In `crates/agnes-parser/src/expr.rs`, delete this arm from the `match head` in `parse_expr`:

```rust
        "llm" => {
            let (positional, args) = parse_positional_and_kwargs(rest, span)?;
            Ok(Expr::Llm {
                span,
                positional,
                args,
            })
        }
```

- [ ] **Step 6: Remove the checker `Expr::Llm` arm**

In `crates/agnes-checker/src/lib.rs`, delete this arm from `check_expr`:

```rust
        Expr::Llm {
            positional, args, ..
        } => {
            for pv in positional {
                let _ = check_expr(pv, reg, env, None, None)?;
            }
            for (_, v) in args {
                let _ = check_expr(v, reg, env, None, None)?;
            }
            Ok(TypeExpr::Named(TypeName("PlainText".into())))
        }
```

- [ ] **Step 7: Remove the compiler `Expr::Llm` arm**

In `crates/agnes-compiler/src/lower.rs`, delete the entire `Expr::Llm { positional, args, .. } => { … }` arm in `lower_expr` (the one that forbids positional args, builds `Input::Kw` from `args`, fills `:input` from upstream, and `self.add(NodeKind::Llm, inputs, TypeExpr::Named(TypeName("PlainText".into())))`).

- [ ] **Step 8: Remove `NodeKind::Llm`**

In `crates/agnes-compiler/src/dag.rs`, delete the `Llm,` variant from `pub enum NodeKind`.

- [ ] **Step 9: Remove the runtime `Expr::Llm` and `NodeKind::Llm` arms**

In `crates/agnes-runtime/src/scheduler.rs`:

Delete the `Expr::Llm { … }` arm in `eval_expr` (the one containing `let mut kwargs: HashMap<String, Value> = HashMap::new();` … `kwargs.insert("input".into(), up);` … `let provides = TypeExpr::Named(TypeName("PlainText".into()));` … `call_native("llm", kwargs, dispatch, resolver, reg, &provides).await`). `llm` now routes through the existing `Expr::Tool` arm.

Delete the `NodeKind::Llm => { … }` arm in `eval_node` (the one calling `call_native_traced(id, &node.kind, "llm", …)`). `llm` now routes through `NodeKind::Tool { name }` (which calls `call_native_traced(id, &node.kind, name, …)` — identical for `name == "llm"`).

- [ ] **Step 10: Build and test**

Run: `cargo build --workspace && cargo test --workspace`
Expected: PASS. If any test references `Expr::Llm` or `NodeKind::Llm` directly, update it to use `Expr::Tool { name: "llm", .. }` / `NodeKind::Tool { name: "llm".into() }`. Search: `grep -rn "Expr::Llm\|NodeKind::Llm" crates`.

- [ ] **Step 11: Commit**

Run:
```bash
jj describe -m "refactor(lang): de-special-case llm

Remove redundant Expr::Llm and NodeKind::Llm; llm is now called only
via (tool llm ...). It already worked through the tool path; this drops
the duplicate special form."
jj new
```

---

### Task 2: Positional tool-call arguments

Drop `:kw value` from tool calls. `Expr::Tool` keeps only `positional`; `KwArgs` is deleted (after Task 1 its only user was `Expr::Tool.args`). The existing positional + "single unfilled param binds upstream" logic in checker/compiler/runtime already handles positional args — only the kwarg branches are removed.

**Files:**
- Modify: `crates/agnes-ast/src/lib.rs` (`Expr::Tool` drop `args`; delete `KwArgs`)
- Modify: `crates/agnes-parser/src/expr.rs` (`parse_tool` positional-only; remove `parse_positional_and_kwargs`)
- Modify: `crates/agnes-checker/src/lib.rs` (`check_expr` `Tool` arm; `check_tool_call` drop kwarg loop)
- Modify: `crates/agnes-compiler/src/lower.rs` (`lower_expr` `Tool` arm; `lower_tool` drop kwarg loop)
- Modify: `crates/agnes-runtime/src/scheduler.rs` (`bind_tool_args` drop kwarg loop)
- Migrate: all tests/examples/planner that use `:k v` in tool calls

**Interfaces:**
- Produces: `Expr::Tool { span, name, positional: Vec<Expr> }` (no `args`). `KwArgs` type deleted. `parse_tool(rest, span) -> Expr::Tool`. `check_tool_call(name, positional, reg, env, flowed_in)`. `lower_tool(name, positional, upstream)`. `bind_tool_args(tool_name, positional, flowed_in, reg, dispatch, resolver, env)`.

- [ ] **Step 1: Start fresh change**

Run: `jj new`

- [ ] **Step 2: Write the failing syntax tests**

Add to `crates/agnes-parser/tests/parse.rs`:

```rust
#[test]
fn positional_tool_call_parses() {
    let prog = agnes_parser::parse_program("(tool join-lines [\"a\" \"b\"])").unwrap();
    let main = prog.main.unwrap();
    match main {
        agnes_ast::Expr::Tool { name, positional, .. } => {
            assert_eq!(name, "join-lines");
            assert_eq!(positional.len(), 1); // one list arg
        }
        other => panic!("expected Tool, got {other:?}"),
    }
}

#[test]
fn keyword_args_are_rejected() {
    // After the refactor :kw value is no longer valid syntax in a tool call.
    // `:path` is a keyword with no preceding positional meaning here, so the
    // parser must error.
    let err = agnes_parser::parse_program("(tool read-file :path \"x\")");
    assert!(err.is_err(), "keyword args should be rejected after refactor");
}
```

If `agnes_parser::parse_program` is not the public entry name, use the exact name the existing tests in this file use (copy the call pattern from a passing test above).

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p agnes-parser --test parse positional_tool_call_parses keyword_args_are_rejected`
Expected: `positional_tool_call_parses` may already PASS (positional parses today); `keyword_args_are_rejected` FAILS (kwargs still accepted). The latter is the real driver.

- [ ] **Step 4: Drop `args` from `Expr::Tool` and delete `KwArgs`**

In `crates/agnes-ast/src/lib.rs`:

Replace the `Tool` variant:

```rust
    /// `(tool <name> arg1 arg2 ...)` - positional tool call. The single
    /// unfilled required parameter (if any) binds the piped upstream.
    Tool {
        span: Span,
        name: String,
        positional: Vec<Expr>,
    },
```

Delete the `KwArgs` type alias entirely:

```rust
/// Keyword arguments: (:key value ...)
pub type KwArgs = Vec<(String, Expr)>;
```

- [ ] **Step 5: Parser — positional-only `parse_tool`**

In `crates/agnes-parser/src/expr.rs`, replace `parse_tool`:

```rust
fn parse_tool(rest: &[lexpr::Value], span: Span) -> Result<Expr, ParseError> {
    let name = rest
        .first()
        .and_then(|v| v.as_symbol())
        .ok_or_else(|| ParseError {
            span,
            message: "tool name expected".into(),
        })?
        .to_string();
    let positional = parse_exprs(&rest[1..], span)?;
    Ok(Expr::Tool {
        span,
        name,
        positional,
    })
}
```

Delete `parse_positional_and_kwargs` entirely (after this task its only caller was `parse_tool`). Also remove the `use crate::toplevel::parse_kwargs;` import at the top of the file if it becomes unused (check with `cargo build -p agnes-parser`).

Note: with positional-only parsing, `(tool read-file :path "x")` will attempt to parse `:path` as a positional expr. `parse_expr` on a keyword `lexpr::Value::Keyword` returns no atom match and falls through to the compound path, which errors "expression must start with a symbol". That satisfies the `keyword_args_are_rejected` test.

- [ ] **Step 6: Checker — drop kwarg loop**

In `crates/agnes-checker/src/lib.rs`:

Update the `check_expr` `Tool` arm:

```rust
        Expr::Tool {
            name,
            positional,
            ..
        } => check_tool_call(name, positional, reg, env, flowed_in),
```

Change `check_tool_call` signature to drop `args` and remove the kwarg loop:

```rust
fn check_tool_call(
    tool_name: &str,
    positional: &[Expr],
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
                    expected: Box::new(expected.clone()),
                    actual: Box::new(up),
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

- [ ] **Step 7: Compiler — drop kwarg loop**

In `crates/agnes-compiler/src/lower.rs`:

Update the `lower_expr` `Tool` arm:

```rust
            Expr::Tool {
                name,
                positional,
                ..
            } => self.lower_tool(name, positional, upstream),
```

Change `lower_tool` signature and drop the kwarg loop:

```rust
    fn lower_tool(
        &mut self,
        name: &str,
        positional: &[Expr],
        upstream: Option<NodeId>,
    ) -> Result<NodeId, crate::CompileError> {
        let sig = self.reg.tool_signature(name).cloned().ok_or_else(|| {
            crate::CompileError::UnknownDefine {
                name: name.to_string(),
            }
        })?;
        let mut inputs: Vec<Input> = Vec::new();
        let mut filled: std::collections::HashSet<String> = std::collections::HashSet::new();

        // Positional args bind sig.requires[i] by index.
        for (i, arg) in positional.iter().enumerate() {
            let (param_name, _) =
                sig.requires
                    .get(i)
                    .ok_or_else(|| crate::CompileError::UnknownDefine {
                        name: format!("{name}: extra positional argument at index {i}"),
                    })?;
            let src = self.lower_expr(arg, None)?;
            inputs.push(Input::Kw {
                key: param_name.clone(),
                source: Box::new(Input::FromNode(src)),
            });
            filled.insert(param_name.clone());
        }

        // Flowed-in upstream fills the sole remaining unfilled require.
        let unfilled: Vec<&String> = sig
            .requires
            .iter()
            .map(|(n, _)| n)
            .filter(|n| !filled.contains(*n))
            .collect();
        if unfilled.len() == 1
            && let Some(up) = upstream
        {
            inputs.push(Input::Kw {
                key: unfilled[0].clone(),
                source: Box::new(Input::FromNode(up)),
            });
        }

        let provides = sig.provides.clone();
        Ok(self.add(
            NodeKind::Tool {
                name: name.to_string(),
            },
            inputs,
            provides,
        ))
    }
```

Update the `use` at the top of `lower.rs` to drop `KwArgs`: `use agnes_ast::{Expr, Literal, Program};`.

- [ ] **Step 8: Runtime — drop kwarg loop**

In `crates/agnes-runtime/src/scheduler.rs`, change `bind_tool_args` signature (drop `args: &KwArgs`) and remove the kwarg loop:

```rust
async fn bind_tool_args(
    tool_name: &str,
    positional: &[Expr],
    flowed_in: Option<Value>,
    reg: &Registry,
    dispatch: &HashMap<String, ToolImpl>,
    resolver: &(dyn PathResolver + Send + Sync),
    env: &mut HashMap<String, Value>,
) -> Result<HashMap<String, Value>, RuntimeError> {
    let sig: ToolSignature =
        reg.tool_signature(tool_name)
            .cloned()
            .ok_or_else(|| RuntimeError::MissingImpl {
                tool: tool_name.to_string(),
            })?;

    let mut out: HashMap<String, Value> = HashMap::new();
    let mut filled: std::collections::HashSet<String> = std::collections::HashSet::new();

    for (i, pv) in positional.iter().enumerate() {
        let (pname, _) = sig
            .requires
            .get(i)
            .ok_or_else(|| RuntimeError::ToolFailed {
                tool: tool_name.into(),
                cause: format!("extra positional arg at index {i}"),
            })?;
        let v = eval_expr(pv, None, reg, dispatch, resolver, env).await?;
        out.insert(pname.clone(), v);
        filled.insert(pname.clone());
    }
    let unfilled: Vec<&String> = sig
        .requires
        .iter()
        .map(|(n, _)| n)
        .filter(|n| !filled.contains(*n))
        .collect();
    if unfilled.len() == 1
        && let Some(up) = flowed_in
    {
        out.insert(unfilled[0].clone(), up);
    }
    Ok(out)
}
```

Update the `Expr::Tool` arm in `eval_expr` to call `bind_tool_args(name, positional, flowed_in, reg, dispatch, resolver, env)` (drop the `args` argument). If `KwArgs` is imported in this file, drop the import.

- [ ] **Step 9: Build to find remaining kwarg references**

Run: `cargo build --workspace 2>&1 | grep -E "error|args|KwArgs" | head -40`
Expected: errors at every `Expr::Tool { … args … }` pattern and any `KwArgs` use. Fix each pattern to omit `args` (use `..`).

- [ ] **Step 10: Migrate tests and examples to positional syntax**

Transformation rule (apply to every `:kw value` inside a `(tool …)` form across tests and examples):
- `(tool read-file :path "x")` → `(tool read-file "x")`
- `(tool write-file :path "x" :content c)` → `(tool write-file "x" c)`
- `(tool translate :input t :lang "ja")` → `(tool translate t "ja")`
- `(tool summarize :input t)` → `(tool summarize t)`
- `(tool join-lines :lines [a b])` → `(tool join-lines [a b])`
- `(tool llm :prompt "p" :input "")` → `(tool llm "p" "")`
- Pipe-omitted args are already positional: `(pipe (tool read-file "x") (tool summarize))` — unchanged.

Files to migrate (search and convert every match):
`crates/agnes-parser/tests/parse.rs`, `crates/agnes-checker/tests/check.rs`, `crates/agnes-compiler/tests/compile.rs`, `crates/agnes-runtime/tests/execute.rs`, `crates/agnes-builtins/tests/*.rs`, `crates/agnes-llm/tests/planner.rs`, `crates/agnes-cli/tests/*.rs`, `crates/agnes-session/tests/*.rs`, `examples/hello.agnes`, `examples/translate.agnes`, `examples/with-define.agnes`, `examples/fan-out.agnes`, `examples/full-demo.agnes`.

Find them with: `grep -rn ":path\|:content\|:input\|:prompt\|:lang\|:lines\|:source\|:target" crates examples --include="*.rs" --include="*.agnes"`

- [ ] **Step 11: Update planner system prompt (syntax)**

In `crates/agnes-llm/src/planner.rs`:

Change the catalog param format (drop the colon) — replace:
```rust
                catalog.push_str(&format!(" :{pname} {pty}"));
```
with:
```rust
                catalog.push_str(&format!(" {pname} {pty}"));
```

Update the grammar cheatsheet line:
```
  * `(tool NAME :param1 v1 :param2 v2 ...)` - call a tool from the
    catalog below.
```
to:
```
  * `(tool NAME arg1 arg2 ...)` - call a tool from the catalog below.
    Args are positional, in the order shown in the catalog. To feed a
    piped value into a parameter, omit that parameter.
```

Update the two example blocks at the bottom of the prompt:
```
```agnes
(finish (tool summarize :input (tool read-file :path "notes.md")))
```
```
to:
```
```agnes
(finish (tool summarize (tool read-file "notes.md")))
```
```
and:
```
```agnes
(pipe (tool read-file :path "log.txt") (tool summarize) observe)
```
to:
```
```agnes
(pipe (tool read-file "log.txt") (tool summarize) observe)
```

- [ ] **Step 12: Update planner tests**

In `crates/agnes-llm/tests/planner.rs`:

- Line ~83: the asserted prompt snippet `(pipe (tool summarize :input \"x\") observe)` → `(pipe (tool summarize \"x\") observe)`.
- Any `MockProvider` DSL strings using `:k v` (e.g. `(pipe (tool summarize :input \"x\") observe)` at ~83, `(pipe (tool bogus) observe)` is fine) → convert to positional.
- The `system_prompt_lists_all_builtin_tools…` test (line ~25) still expects `"ocr"` and `"llm"` in the catalog — leave `ocr` for Task 3; this task only changes syntax, so the tool-name list is unchanged.

- [ ] **Step 13: Build and test**

Run: `cargo build --workspace && cargo test --workspace`
Expected: PASS. If a test still fails on a `:kw` parse, find it with `grep -rn ":path\|:content\|:input\|:prompt\|:lang\|:lines\|:source\|:target" crates examples` and convert it.

- [ ] **Step 14: Commit**

Run:
```bash
jj describe -m "refactor(lang): positional tool-call arguments

Drop :kw value from tool calls; Expr::Tool keeps only positional args.
KwArgs type removed. Pipe binding unchanged (single unfilled param
binds upstream). Planner prompt + examples migrated."
jj new
```

---

### Task 3: Replace unreachable semantic types with `String`; remove `ocr`

Remove `PlainText`/`Markdown`/`HTML`/`Summary`/`PDF`/`Image` types, `TextLike`/`VisualDoc` aliases, `utf8_validator`/`pdf_validator`/`image_validator`, the 6 corresponding show impls, and the `ocr` tool. Text params and `provides` become `String`. Register `parse-path` (currently missing a signature).

**Files:**
- Modify: `crates/agnes-builtins/src/lib.rs`
- Modify: `crates/agnes-builtins/src/aliases.rs`
- Modify: `crates/agnes-builtins/src/types.rs`
- Modify: `crates/agnes-builtins/src/shows.rs`
- Modify: `crates/agnes-builtins/src/tools.rs` (delete `ocr` impl)
- Migrate: tests/examples referencing removed types or `ocr`

**Interfaces:**
- Produces: registered types = `Path, JSON, Unit, Unknown, String, Int, Bool, CommandResult, Finish, Observation`. Registered tools = `read-file, write-file, summarize, translate, llm, join-lines, shell-run, parse-path` (signatures per spec §1 table). No `ocr`, no `TextLike`/`VisualDoc`.

- [ ] **Step 1: Start fresh change**

Run: `jj new`

- [ ] **Step 2: Write the failing type test**

Add to `crates/agnes-builtins/tests/register.rs`:

```rust
#[test]
fn join_lines_accepts_list_of_strings() {
    // The signature must be List String so a list of string literals
    // type-checks (the original web-server failure).
    let r = reg();
    let sig = r.tool_signature("join-lines").expect("join-lines registered");
    let lines_ty = &sig.requires[0].1;
    // lines_ty must be (List String): a String literal must satisfy it.
    let string_list = agnes_types::TypeExpr::App {
        head: agnes_types::TypeName("List".into()),
        args: vec![agnes_types::TypeExpr::named("String")],
    };
    assert!(
        agnes_types::type_expr_matches(&string_list, lines_ty),
        "join-lines :lines should accept (List String), got {lines_ty}"
    );
}

#[test]
fn removed_types_are_not_registered() {
    let r = reg();
    for gone in ["PlainText", "Markdown", "HTML", "Summary", "PDF", "Image", "TextLike", "VisualDoc"] {
        assert!(r.resolve(&agnes_types::TypeExprAst::Named(gone.into())).is_err(),
            "type {gone} should no longer be registered");
    }
}

#[test]
fn ocr_is_not_registered() {
    let r = reg();
    assert!(r.tool_signature("ocr").is_none(), "ocr must be removed");
}
```

If `reg()` is not the helper name in this file, use the existing helper. If `TypeExprAst::Named` is not the public variant name, use the exact name used elsewhere in the test file. (`resolve` returns `Result`; an unregistered name errors.)

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p agnes-builtins --test register join_lines_accepts_list_of_strings removed_types_are_not_registered ocr_is_not_registered`
Expected: FAIL (current `lines` type is `List (| PlainText Markdown)`, types still registered, `ocr` present).

- [ ] **Step 4: Update `lib.rs` registrations**

In `crates/agnes-builtins/src/lib.rs`, replace the type-registration block. Replace:

```rust
    // --- Types + validators ---
    reg.register_type("Path", Some(types::path_validator))?;
    reg.register_type("PlainText", Some(types::utf8_validator))?;
    reg.register_type("Markdown", Some(types::utf8_validator))?;
    reg.register_type("HTML", Some(types::utf8_validator))?;
    reg.register_type("JSON", Some(types::json_validator))?;
    reg.register_type("PDF", Some(types::pdf_validator))?;
    reg.register_type("Image", Some(types::image_validator))?;
    reg.register_type("Summary", Some(types::utf8_validator))?;
    reg.register_type("Unit", Some(types::unit_validator))?;
    reg.register_type("Unknown", None)?;
    // Non-workflow types used by literals.
    reg.register_type("String", None)?;
    reg.register_type("Int", None)?;
    reg.register_type("Bool", None)?;
    reg.register_type("CommandResult", None)?;
```

with:

```rust
    // --- Types + validators ---
    reg.register_type("Path", Some(types::path_validator))?;
    reg.register_type("JSON", Some(types::json_validator))?;
    reg.register_type("Unit", Some(types::unit_validator))?;
    reg.register_type("Unknown", None)?;
    // Non-workflow types used by literals.
    reg.register_type("String", None)?;
    reg.register_type("Int", None)?;
    reg.register_type("Bool", None)?;
    reg.register_type("CommandResult", None)?;
```

Remove the two alias registrations:

```rust
    // --- Aliases ---
    reg.register_alias("TextLike", aliases::text_like())?;
    reg.register_alias("VisualDoc", aliases::visual_doc())?;
```

Replace the tool-signature block. Replace the `let plaintext = …; let summary = …;` locals and all `register_tool` calls (read-file through join-lines, plus ocr) with:

```rust
    // --- Tools ---
    let path = TypeExpr::named("Path");
    let string = TypeExpr::named("String");
    let unit = TypeExpr::named("Unit");
    let command_result = TypeExpr::named("CommandResult");
    let list_string = TypeExpr::App {
        head: TypeName("List".into()),
        args: vec![string.clone()],
    };

    reg.register_tool(
        "read-file",
        ToolSignature {
            requires: vec![("path".into(), path.clone())],
            provides: string.clone(),
        },
    )?;
    reg.register_tool(
        "write-file",
        ToolSignature {
            requires: vec![
                ("path".into(), path.clone()),
                ("content".into(), string.clone()),
            ],
            provides: unit.clone(),
        },
    )?;
    reg.register_tool(
        "summarize",
        ToolSignature {
            requires: vec![("input".into(), string.clone())],
            provides: string.clone(),
        },
    )?;
    reg.register_tool(
        "translate",
        ToolSignature {
            requires: vec![
                ("input".into(), string.clone()),
                ("lang".into(), string.clone()),
            ],
            provides: string.clone(),
        },
    )?;
    reg.register_tool(
        "llm",
        ToolSignature {
            requires: vec![
                ("prompt".into(), string.clone()),
                ("input".into(), string.clone()),
            ],
            provides: string.clone(),
        },
    )?;
    reg.register_tool(
        "join-lines",
        ToolSignature {
            requires: vec![("lines".into(), list_string)],
            provides: string.clone(),
        },
    )?;
    reg.register_tool(
        "parse-path",
        ToolSignature {
            requires: vec![("path".into(), string.clone())],
            provides: path.clone(),
        },
    )?;
```

Keep the existing `shell-run` registration as-is (it uses `string_ty`/`command_result` locals — rename to the new `string`/`command_result` locals if needed so it compiles). Delete the `ocr` `register_tool` block entirely.

Drop now-unused imports: if `canonicalize_union` and `TypeName` become unused after these edits, remove them from the `use agnes_types::{…}` line (the compiler will tell you).

- [ ] **Step 5: Empty `aliases.rs`**

In `crates/agnes-builtins/src/aliases.rs`, delete both functions. The module becomes empty; replace the whole file with:

```rust
// Aliases were removed when PlainText/Markdown/HTML/PDF/Image types were
// dropped (2026-07-22). Module kept as an empty placeholder so `mod aliases;`
// in lib.rs still resolves; delete the `mod aliases;` line too if you prefer.
```

(Alternatively delete the file and remove `mod aliases;` from `lib.rs` — either is fine; pick one and make `cargo build -p agnes-builtins` pass.)

- [ ] **Step 6: Remove dead validators from `types.rs`**

In `crates/agnes-builtins/src/types.rs`, delete `utf8_validator`, `pdf_validator`, `image_validator`. Keep `path_validator`, `json_validator`, `unit_validator`.

- [ ] **Step 7: Remove dead show impls from `shows.rs`**

In `crates/agnes-builtins/src/shows.rs`, delete `plain_text`, `summary`, `markdown`, `html`, `pdf`, `image` functions, and remove their entries from `BUILTIN_SHOWS`. The list becomes:

```rust
pub const BUILTIN_SHOWS: &[(&str, ShowFn)] = &[
    ("JSON", json),
    ("Path", path),
    ("String", string),
    ("Int", int),
    ("Bool", bool_),
    ("Unit", unit),
];
```

If `as_str_or_empty` becomes unused after removing the text show fns, delete it too (the compiler will warn).

- [ ] **Step 8: Delete `ocr` tool impl**

In `crates/agnes-builtins/src/tools.rs`, delete the `ocr` block:

```rust
    // ocr (mock: fixed sentence)
    let ocr: Box<dyn for<'a> Fn(HashMap<String, Value>, &'a (dyn PathResolver + Send + Sync)) -> BoxFuture<'a, Result<Value, String>> + Send + Sync + 'static> =
        Box::new(|args, _resolver| {
            Box::pin(async move {
                let _ = arg_str(&args, "source")?;
                Ok(Value::typed(
                    JsonValue::String(
                        "Extracted text: agnes runtime dispatches LLM-planned workflows.".into(),
                    ),
                    "PlainText",
                ))
            })
        });
    m.insert("ocr".into(), Arc::new(ocr));
```

- [ ] **Step 9: Remove `ocr` from planner catalog**

In `crates/agnes-llm/src/planner.rs`, remove `"ocr",` from `BUILTIN_TOOL_ORDER` (the array with `read-file, write-file, summarize, translate, ocr, llm, join-lines`).

- [ ] **Step 10: Migrate tests and examples off removed types**

Transformation rules:
- `register_type("PlainText"|"Markdown"|"HTML"|"Summary"|"PDF"|"Image", …)` in test setup → delete the line, or replace with `register_type("String", …)` if the test needs a stand-in text type.
- `TypeExpr::named("PlainText")` (as a `provides` or param type in test fixtures) → `TypeExpr::named("String")`.
- `aliases::text_like()` / `aliases::visual_doc()` references in tests → `TypeExpr::named("String")` (text) or delete (visual doc).
- `ocr` test cases → delete the test.
- `:provides PlainText` in `define`/example → `:provides String`.

Files (search and convert): `crates/agnes-parser/tests/parse.rs` (delete `(declare type PDF)` test and `(declare tool ocr …)` test), `crates/agnes-checker/tests/check.rs` (delete local `register_type` for PlainText/Markdown/PDF/Image/Summary; delete `ocr` tool fixture and the `(pipe (tool read-file …) (tool ocr))` test; convert remaining `provides: PlainText` fixtures to `String`), `crates/agnes-compiler/tests/compile.rs` (PlainText/Summary → String), `crates/agnes-runtime/tests/execute.rs` (`:provides Summary`/`:provides PlainText` → String; update the `List (| PlainText Markdown)` comment), `crates/agnes-builtins/tests/{shows,register,dispatch_routing}.rs` (delete PDF/Image/Summary/ocr assertions; convert PlainText to String), `crates/agnes-llm/tests/planner.rs` (remove `"ocr"` from the expected-tool-list assertion at ~line 40).

Find with: `grep -rn "PlainText\|Markdown\|HTML\|Summary\|PDF\|Image\|VisualDoc\|TextLike\|ocr" crates examples --include="*.rs" --include="*.agnes"`

Note: `crates/agnes-registry/tests/register.rs` and `crates/agnes-types/src/lib.rs` tests use `"PDF"` only as an arbitrary type-name fixture in self-contained `register_type` calls — they do not depend on builtins. Leave them as-is (or rename the fixture to `"Widget"` for clarity; optional).

- [ ] **Step 11: Build and test**

Run: `cargo build --workspace && cargo test --workspace`
Expected: PASS.

- [ ] **Step 12: Commit**

Run:
```bash
jj describe -m "refactor(types): drop unreachable semantic types, use String

Remove PlainText/Markdown/HTML/Summary/PDF/Image + TextLike/VisualDoc
aliases + utf8/pdf/image validators + 6 show impls + ocr tool. Text
params and provides become String; join-lines now takes (List String).
Register parse-path signature (was missing)."
jj new
```

---

### Task 4: Examples, README, chat-demo

Examples are not exercised by `cargo test`, but must stay runnable and consistent with the new language.

**Files:**
- Modify: `examples/hello.agnes`, `examples/translate.agnes`, `examples/with-define.agnes`, `examples/fan-out.agnes`, `examples/full-demo.agnes`
- Modify: `README.md`
- Modify: `examples/chat-demo.md`

**Interfaces:** none (documentation).

- [ ] **Step 1: Start fresh change**

Run: `jj new`

- [ ] **Step 2: Rewrite examples**

`examples/hello.agnes`:
```lisp
;; The smallest agnes workflow: one tool call.
(tool llm "say hi" "")
```

`examples/translate.agnes`:
```lisp
;; Sequential pipe: read a file then translate it.
(pipe
  (tool read-file "README.md")
  (tool translate "ja"))
```

`examples/with-define.agnes`:
```lisp
;; Declare a compound tool and invoke it. The runtime dispatches the call
;; by evaluating the define's body in a fresh env with `path` and `target`
;; bound from the incoming positional args.
(define read-and-translate
  :params  [(path Path) (target String)]
  :provides String
  (pipe
    (tool read-file path)
    (tool translate target)))

(tool read-and-translate "README.md" "ja")
```

`examples/fan-out.agnes`:
```lisp
;; Parallel branches (runtime executes them sequentially, but the DAG
;; shape is genuine fan-out). Each branch is a self-contained pipe.
(par
  (pipe
    (tool read-file "README.md")
    (tool summarize))
  (pipe
    (tool read-file "README.md")
    (tool translate "ja")))
```

`examples/full-demo.agnes`:
```lisp
;; Full demo: declare a compound `read-and-translate` and dispatch it.
;; Also exercises the list literal + parameterized type flow via join-lines.

(define read-and-translate
  :params  [(path Path) (target String)]
  :provides String
  (pipe
    (tool read-file path)
    (tool translate target)))

(pipe
  (par
    (let ja (tool read-and-translate "README.md" "ja"))
    (let en (tool read-and-translate "README.md" "en")))
  (tool join-lines [ja en]))
```

- [ ] **Step 3: Update README**

In `README.md`, change the "Language at a glance" example. Replace `:provides PlainText` with `:provides String`, and convert the `(tool … :k v …)` calls to positional:

```lisp
(define read-and-translate
  :params  [(path Path) (target String)]
  :provides String
  (pipe
    (tool read-file path)
    (tool translate target)))

(pipe
  (let ja (tool read-and-translate "README.md" "ja"))
  (tool join-lines [ja ja]))
```

- [ ] **Step 4: Update chat-demo.md**

In `examples/chat-demo.md`, convert any `(tool … :k v …)` and `PlainText` references to positional + `String` (apply the same transformation rule as Task 2/3). Search: `grep -n ":path\|:content\|:input\|:prompt\|:lang\|:lines\|:target\|PlainText" examples/chat-demo.md`.

- [ ] **Step 5: Run each example**

Run: `for f in hello translate with-define fan-out full-demo; do cargo run -q -p agnes-cli -- run examples/$f.agnes >/dev/null 2>&1 && echo "$f ok" || echo "$f FAIL"; done`
Expected: each prints `ok` (exit 0). `full-demo` and `translate` need `README.md` present (it is). `hello` needs an LLM provider — if no API key is set it may error on the provider call; that is acceptable as long as it parses/checks/compiles (the error would be a provider error, not a parse/check error). If `hello` fails only on the provider call, mark it ok.

- [ ] **Step 6: Commit**

Run:
```bash
jj describe -m "docs: migrate examples and README to positional args + String"
jj new
```

---

### Task 5: End-to-end verification

Confirm the original failure is fixed and nothing regressed.

**Files:** none (verification only).

- [ ] **Step 1: Full build and test**

Run: `cargo build --workspace && cargo test --workspace`
Expected: PASS, 0 failures.

- [ ] **Step 2: Replay the web-server task (positional, no LLM needed)**

Create `/tmp/webserver.agnes`:
```lisp
(pipe
  (tool join-lines [
    "use std::io::prelude::*;"
    "use std::net::TcpListener;"
    "fn main() {"
    "    let listener = TcpListener::bind(\"127.0.0.1:7878\").unwrap();"
    "    for stream in listener.incoming() {"
    "        let mut stream = stream.unwrap();"
    "        let response = b\"Hello, World!\";"
    "        stream.write(response).unwrap();"
    "        stream.flush().unwrap();"
    "    }"
    "}"
  ])
  (tool write-file "server.rs")
  (finish "已将 Rust 服务器代码写入 server.rs。"))
```

Run: `cargo run -q -p agnes-cli -- run /tmp/webserver.agnes`
Expected: exit 0, `server.rs` written with the joined content, result `(Finish String)`.

- [ ] **Step 3: Verify the join-lines type fix directly**

Run: `cargo run -q -p agnes-cli -- run <(echo '(tool join-lines ["a" "b" "c"])')` (or write to a temp file if process substitution is unavailable).
Expected: prints `a\nb\nc`, result `String`. No type error.

- [ ] **Step 4: Commit (if any fixups were needed)**

If Steps 1-3 needed fixups, `jj describe` the current change with a message like `fix: final adjustments for positional-args migration`. Otherwise this task produces no commit (leave the empty change or `jj abandon` it).

Run (if no changes): `jj abandon`  (only if the working copy change is empty).

---

## Self-Review

**1. Spec coverage:**
- Remove PlainText/Markdown/HTML/Summary/PDF/Image → Task 3. ✓
- Remove TextLike/VisualDoc aliases → Task 3. ✓
- Remove utf8/pdf/image validators → Task 3. ✓
- Remove 6 show impls → Task 3. ✓
- Remove ocr tool → Task 3. ✓
- Tool signatures → String (table) → Task 3. ✓
- Register parse-path → Task 3. ✓
- Positional tool calls, remove `:k v` → Task 2. ✓
- Remove `Expr::Tool.args`, `KwArgs` → Task 2. ✓
- De-special llm (`Expr::Llm`, `NodeKind::Llm`) → Task 1. ✓
- Pipe omit-rule preserved → Task 2 (keeps the unfilled-param logic). ✓
- Keep `define`/`retry`/`catch` keyword syntax → Global Constraints + tasks do not touch them. ✓
- Planner prompt → Task 2 (syntax) + Task 3 (ocr). ✓
- Examples/README/chat-demo → Task 4. ✓
- Verification (web-server replay) → Task 5. ✓

**2. Placeholder scan:** No TBD/TODO. Every production-code step shows the exact replacement. Bulk test migration uses an explicit transformation rule + file list + grep command + `cargo test` gate (mechanical, unambiguous). The `reg()`/`dispatch()`/`parse_program` helper-name caveats direct the engineer to copy the exact name from a neighboring passing test rather than guess.

**3. Type consistency:** `Expr::Tool { span, name, positional }` is used identically in Tasks 1 (leaves `args`), 2 (removes `args`). `check_tool_call(name, positional, reg, env, flowed_in)`, `lower_tool(name, positional, upstream)`, `bind_tool_args(tool_name, positional, flowed_in, reg, dispatch, resolver, env)` — signatures match across checker/compiler/runtime. `NodeKind::Tool { name }` is the single dispatch path after Task 1 removes `NodeKind::Llm`. Param names (`lines`/`content`/`input`/`path`/`prompt`/`lang`) unchanged throughout.

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-07-22-positional-args-and-string-types.md`. Two execution options:

**1. Subagent-Driven (recommended)** — I dispatch a fresh subagent per task, review between tasks, fast iteration.

**2. Inline Execution** — Execute tasks in this session using executing-plans, batch execution with checkpoints.

Which approach?
