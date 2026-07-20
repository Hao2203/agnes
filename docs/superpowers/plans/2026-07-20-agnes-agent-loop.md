# Agnes Agent Loop Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Turn `agnes chat` from a single-shot planner into a multi-turn agent loop where the LLM outputs a DSL each iteration; results feed back automatically until it emits `finish` (or a non-`Observation` value = implicit finish); `observe` explicitly hands control back to the LLM.

**Architecture:** Runtime never gains type variables. Two new builtin tools (`finish`, `observe`) with static signature `Unknown -> Unknown` wrap the upstream `Value.declared_type` into `App{Finish|Observation, args:[<inner>]}` at `native_dispatch` time. Session reads the outermost head and either terminates (`Finish` / anything else) or feeds the serialized data back to the planner as a `<observation>` message (`Observation`). Serialization is powered by a new Show typeclass on `Registry`.

**Tech Stack:** Rust edition 2024, tokio, async-trait, serde_json, thiserror. `jj` for commits (colocated with git). No new external deps; workspace `[workspace.dependencies]` already carries everything needed.

## Global Constraints

- Rust edition 2024 throughout every crate.
- Shared external deps live in workspace root `Cargo.toml` `[workspace.dependencies]` and are pulled in with `<dep>.workspace = true`.
- **Commits use jj** (colocated with git). Workflow at the end of each task: `jj describe -m "..." && jj new`. Never `git commit`. Every commit message ends with `Co-Authored-By: Claude <noreply@anthropic.com>` on its own line.
- Language of code, comments, and error messages: English. Error messages follow the What / Why / Fix template.
- Type names use PascalCase (`Finish`, `Observation`); tool and parameter names use kebab-case (`finish`, `observe`, `:input`).
- `agnes_runtime::execute(...)` signature MUST remain source-compatible (established in prior plan; keep it).
- Existing checker rules (Rule 1 & 2) are NOT modified. This plan adds NO new checker rules.
- Existing `examples/*.agnes` MUST continue to work without edits. This plan is fully backward-compatible with unlabeled DSL.
- Real network calls MUST NOT happen in any unit or integration test. Provider tests use `MockProvider` from `agnes-llm`.
- `Provider` trait remains object-safe via `async-trait`; consumers see `Arc<dyn Provider>`.
- `SessionEvent` is opened as `#[non_exhaustive]` in Task 8 BEFORE any new variant is added. All downstream `match SessionEvent` sites acquire `_ => {}` catchall from that point on.
- `MAX_TURNS = 20` fixed default; overridable via `--max-turns <N>` CLI flag.
- Observation text truncation threshold: 8000 chars (mid-cut, keeping first 4000 and last 4000).

## File Structure (locked before task decomposition)

```
crates/
├── agnes-types/
│   └── src/lib.rs                       # MODIFIED (Task 1, Task 5): add ShowFn type alias; add Unknown-wildcard arm to type_expr_matches
├── agnes-registry/
│   └── src/lib.rs                       # MODIFIED (Task 2, Task 3): shows map, register_show, show_of, show_value; RegistryError::DuplicateShow; extend resolve() to accept Finish/Observation heads
├── agnes-builtins/
│   ├── src/lib.rs                       # MODIFIED (Task 4, Task 5): register Finish/Observation types; register show impls for all builtins; register finish/observe tools
│   ├── src/shows.rs                     # NEW (Task 4): ShowFn impls for PlainText/Summary/Markdown/HTML/PDF/Image/JSON/Path/String/Int/Bool/Unit
│   └── src/tools.rs                     # MODIFIED (Task 5): finish/observe ToolImpl closures; declared_type rewrite
├── agnes-llm/
│   ├── src/planner.rs                   # MODIFIED (Task 6, Task 7): rewrite Turn/Iteration/Observation structs, new interface (begin_user_turn/plan_next/push_observation/record_finish); new system prompt with `finish`/`observe`
│   └── src/error.rs                     # MODIFIED (Task 6): PlannerError variants unchanged but re-check What/Why/Fix
├── agnes-session/
│   ├── src/events.rs                    # MODIFIED (Task 8, Task 9): #[non_exhaustive] first; then add IterationStart, ObservationEmitted
│   ├── src/session.rs                   # MODIFIED (Task 9, Task 10, Task 11): classify_root helper; extract_inner_type helper; run_turn refactored into loop; RawDsl seeded path; cancellation
│   └── src/error.rs                     # MODIFIED (Task 10): TurnLimitExceeded variant
├── agnes-cli/
│   ├── src/cli.rs                       # MODIFIED (Task 12): --max-turns flag
│   ├── src/chat.rs                      # MODIFIED (Task 12): pass max_turns to Session; hook Ctrl-C to a cancel token
│   ├── src/sink_stderr.rs               # MODIFIED (Task 12): render IterationStart and ObservationEmitted
│   └── src/history_view.rs              # NEW (Task 13): /history with new nested Turn/Iteration structure
├── examples/
│   └── chat-demo.md                     # MODIFIED (Task 13): document new loop; add finish/observe examples
└── README.md                            # MODIFIED (Task 13): mention new agent loop capabilities
```

**Locked interfaces (referenced across tasks — every implementer sees only their own task, so signatures must match these exactly):**

- `agnes_types::ShowFn = fn(&serde_json::Value) -> String`.
- `agnes_registry::RegistryError::DuplicateShow { name: String }` (new variant).
- `agnes_registry::Registry::register_show(&mut self, name: &str, f: ShowFn) -> Result<(), RegistryError>`.
- `agnes_registry::Registry::show_of(&self, name: &TypeName) -> Option<ShowFn>`.
- `agnes_registry::Registry::show_value(&self, value: &agnes_types::Value) -> String` — recursive over App types; internal fallback for `List`, `Option` (`| T Unit`), `Finish`, `Observation`, `|` unions, and unregistered types.
- `agnes-builtins::shows::{plain_text, summary, markdown, html, pdf, image, json, path, string, int, bool, unit}` — 12 `ShowFn` functions.
- `agnes_types::TypeExpr::App { head: TypeName("Finish"), args: vec![inner] }` and same with `TypeName("Observation")` — used at runtime only; not required in DSL source.
- `agnes-builtins::register_builtins(reg: &mut Registry) -> Result<(), RegistryError>` — grows to register two new types (`Finish`, `Observation`), all show impls, and two new tools (`finish`, `observe`).
- `agnes_builtins::tools::finish_tool_impl(provider)` and `observe_tool_impl(provider)` — actually built directly inline in `native_dispatch`; no unique symbol needed but the closure must (a) take upstream `Value` from `:input` kwarg, (b) return `Value { data, declared_type: App{ head: TypeName("Finish"|"Observation"), args: vec![original_declared_type] } }`.
- `agnes_llm::planner::Turn { user_nl: String, iterations: Vec<Iteration>, outcome: TurnOutcome }` — replaces the old `{ user_nl, assistant_dsl, result_preview }`.
- `agnes_llm::planner::Iteration { assistant_dsl: String, observation: Option<Observation> }` — `observation.is_none()` means this iteration was the terminating one.
- `agnes_llm::planner::Observation { text: String, is_error: bool, type_name: Option<agnes_types::TypeName> }`.
- `agnes_llm::planner::TurnOutcome::{ Finished { result: String }, TurnLimitExceeded }`.
- `Planner::begin_user_turn(&mut self, nl: String)` — starts a new in-flight turn; must NOT clear history.
- `Planner::plan_next(&mut self) -> Result<String, PlannerError>` — one LLM round-trip; appends `assistant(dsl)` to the in-flight iterations.
- `Planner::push_observation(&mut self, dsl: String, text: String, is_error: bool, type_name: Option<TypeName>)` — appends observation to the last in-flight iteration; if the last iteration already has an observation, the caller has a bug (assert).
- `Planner::record_finish(&mut self, dsl: String, result: String)` — commits the in-flight turn: last iteration's observation stays None, outcome = Finished. Clears scratch.
- `Planner::abandon_pending_turn(&mut self)` — existing; extended semantics: also stamps `outcome = TurnLimitExceeded` if any iterations exist.
- `agnes_session::SessionEvent` — `#[non_exhaustive]` and grows two variants: `IterationStart { iter: u32 }` and `ObservationEmitted { iter: u32, text: String, is_error: bool }`.
- `agnes_session::SessionError::TurnLimitExceeded { max_turns: u32 }` (new variant).
- `agnes_session::Session::run_turn(&mut self, input: TurnInput, sink: &mut dyn EventSink) -> Result<Value, SessionError>` — signature unchanged; internal loop is entirely new.
- `agnes_session::classify_root(value: &Value) -> RootKind { Finish, Observation, Other }` — reads `Value.declared_type` outermost head. `Other` includes any non-Finish/non-Observation shape.
- `agnes_session::extract_inner_type(t: &TypeExpr) -> Option<TypeName>` — for `App{head: "Finish"|"Observation", args: [inner]}`, returns `inner`'s outermost TypeName; else `None`.

---

### Task 1: `agnes-types::ShowFn` type alias

**Files:**
- Modify: `crates/agnes-types/src/lib.rs`
- Test: `crates/agnes-types/tests/show_fn.rs`

**Interfaces:**
- Consumes: nothing new (uses existing `serde_json::Value` re-export).
- Produces: `pub type ShowFn = fn(&serde_json::Value) -> String;` at the top level of `agnes_types`.

- [ ] **Step 1: Write the failing test**

Create `crates/agnes-types/tests/show_fn.rs`:

```rust
use agnes_types::ShowFn;
use serde_json::json;

fn my_show(v: &serde_json::Value) -> String {
    v.as_str().unwrap_or("").to_string()
}

#[test]
fn show_fn_alias_accepts_a_matching_fn() {
    let f: ShowFn = my_show;
    let out = f(&json!("hello"));
    assert_eq!(out, "hello");
}

#[test]
fn show_fn_returns_owned_string_even_for_empty() {
    fn empty(_v: &serde_json::Value) -> String {
        String::new()
    }
    let f: ShowFn = empty;
    assert_eq!(f(&json!(null)), "");
}
```

- [ ] **Step 2: Run test — expect FAIL**

Run: `cargo test -p agnes-types --test show_fn`
Expected: FAIL with `unresolved import 'agnes_types::ShowFn'`.

- [ ] **Step 3: Add the alias**

Edit `crates/agnes-types/src/lib.rs`. Find the existing `pub type Validator = fn(&JsonValue) -> Result<(), String>;` line (around line 100). Immediately below it, add:

```rust
/// Function type for a `Show` implementation: takes a JSON value produced
/// by some tool call and renders it into a human/LLM-readable string.
///
/// Registered in `agnes-registry` via `Registry::register_show`. Used by
/// `Session::run_turn` at the end of each iteration to serialize the
/// returned `Value` for either the user (Finish path) or the LLM
/// (Observation path).
pub type ShowFn = fn(&JsonValue) -> String;
```

- [ ] **Step 4: Run test — expect PASS**

Run: `cargo test -p agnes-types --test show_fn`
Expected: PASS (2/2).

- [ ] **Step 5: Full workspace check**

Run: `cargo check -p agnes-types --tests`
Expected: no warnings.

- [ ] **Step 6: Commit**

```bash
jj describe -m "feat(types): ShowFn type alias for Show typeclass

Foundation for a Show typeclass mechanism used by the upcoming agent
loop to serialize Values back to the user or LLM. This task only lands
the type alias; registration and callers come in later tasks.

Co-Authored-By: Claude <noreply@anthropic.com>"
jj new
```

---

### Task 2: `agnes-registry` Show typeclass storage + API

**Files:**
- Modify: `crates/agnes-registry/src/lib.rs`
- Test: `crates/agnes-registry/tests/shows.rs`

**Interfaces:**
- Consumes: `agnes_types::{ShowFn, Value, TypeName, TypeExpr}` (existing + Task 1's ShowFn).
- Produces:
  - `RegistryError::DuplicateShow { name: String }` (new variant).
  - `Registry::register_show(&mut self, name: &str, f: ShowFn) -> Result<(), RegistryError>`.
  - `Registry::show_of(&self, name: &TypeName) -> Option<ShowFn>`.
  - `Registry::show_value(&self, value: &Value) -> String` — recursive rendering.

- [ ] **Step 1: Write the failing test**

Create `crates/agnes-registry/tests/shows.rs`:

```rust
use agnes_registry::{Registry, RegistryError};
use agnes_types::{ShowFn, TypeExpr, TypeName, Value};
use serde_json::json;

fn show_string(v: &serde_json::Value) -> String {
    v.as_str().unwrap_or("").to_string()
}

fn show_wrap(v: &serde_json::Value) -> String {
    format!("<<{}>>", v.as_str().unwrap_or(""))
}

#[test]
fn register_show_records_a_function() {
    let mut reg = Registry::new();
    reg.register_show("Widget", show_string as ShowFn).unwrap();
    let got = reg.show_of(&TypeName("Widget".into())).unwrap();
    assert_eq!(got(&json!("hi")), "hi");
}

#[test]
fn duplicate_show_rejects_second_registration() {
    let mut reg = Registry::new();
    reg.register_show("Widget", show_string as ShowFn).unwrap();
    let err = reg
        .register_show("Widget", show_wrap as ShowFn)
        .expect_err("second registration should fail");
    match err {
        RegistryError::DuplicateShow { name } => assert_eq!(name, "Widget"),
        other => panic!("expected DuplicateShow, got {other:?}"),
    }
}

#[test]
fn register_show_is_independent_of_type_registration() {
    let mut reg = Registry::new();
    // register_type is not called; register_show still succeeds.
    reg.register_show("Widget", show_string as ShowFn).unwrap();
    assert!(reg.show_of(&TypeName("Widget".into())).is_some());
}

#[test]
fn show_value_uses_registered_show_for_named_type() {
    let mut reg = Registry::new();
    reg.register_show("Widget", show_wrap as ShowFn).unwrap();
    let v = Value {
        data: json!("hello"),
        declared_type: TypeExpr::named("Widget"),
    };
    assert_eq!(reg.show_value(&v), "<<hello>>");
}

#[test]
fn show_value_falls_back_to_json_pretty_for_unregistered_type() {
    let reg = Registry::new();
    let v = Value {
        data: json!({"a": 1, "b": [2, 3]}),
        declared_type: TypeExpr::named("Unregistered"),
    };
    let out = reg.show_value(&v);
    // Pretty json contains the keys.
    assert!(out.contains("\"a\""));
    assert!(out.contains("\"b\""));
}

#[test]
fn show_value_recurses_into_list_using_element_show() {
    let mut reg = Registry::new();
    reg.register_show("Item", show_wrap as ShowFn).unwrap();
    let v = Value {
        data: json!(["a", "b", "c"]),
        declared_type: TypeExpr::App {
            head: TypeName("List".into()),
            args: vec![TypeExpr::named("Item")],
        },
    };
    assert_eq!(reg.show_value(&v), "[<<a>>, <<b>>, <<c>>]");
}

#[test]
fn show_value_unwraps_finish_wrapper() {
    let mut reg = Registry::new();
    reg.register_show("Msg", show_string as ShowFn).unwrap();
    let v = Value {
        data: json!("done"),
        declared_type: TypeExpr::App {
            head: TypeName("Finish".into()),
            args: vec![TypeExpr::named("Msg")],
        },
    };
    assert_eq!(reg.show_value(&v), "done");
}

#[test]
fn show_value_unwraps_observation_wrapper() {
    let mut reg = Registry::new();
    reg.register_show("Msg", show_string as ShowFn).unwrap();
    let v = Value {
        data: json!("thinking..."),
        declared_type: TypeExpr::App {
            head: TypeName("Observation".into()),
            args: vec![TypeExpr::named("Msg")],
        },
    };
    assert_eq!(reg.show_value(&v), "thinking...");
}

#[test]
fn show_value_option_some_returns_inner_show() {
    let mut reg = Registry::new();
    reg.register_show("Msg", show_wrap as ShowFn).unwrap();
    // Option T after canonicalize_union is (| T Unit); still: show inner if data isn't null.
    let v = Value {
        data: json!("here"),
        declared_type: agnes_types::canonicalize_union([
            TypeExpr::named("Msg"),
            TypeExpr::named("Unit"),
        ]),
    };
    assert_eq!(reg.show_value(&v), "<<here>>");
}

#[test]
fn show_value_option_none_returns_empty_string() {
    let mut reg = Registry::new();
    reg.register_show("Msg", show_wrap as ShowFn).unwrap();
    let v = Value {
        data: json!(null),
        declared_type: agnes_types::canonicalize_union([
            TypeExpr::named("Msg"),
            TypeExpr::named("Unit"),
        ]),
    };
    assert_eq!(reg.show_value(&v), "");
}
```

- [ ] **Step 2: Run test — expect FAIL**

Run: `cargo test -p agnes-registry --test shows`
Expected: FAIL with `unresolved import` and `no method named register_show`.

- [ ] **Step 3: Extend `RegistryError`**

In `crates/agnes-registry/src/lib.rs`, find `pub enum RegistryError` (around line 29). Add this variant at the end, before the closing brace:

```rust
    #[error(
        "Show implementation already registered for type `{name}`.\n  Why: `register_show` was called twice with the same type name.\n  Fix: remove the duplicate registration or pick a different type name."
    )]
    DuplicateShow { name: String },
```

- [ ] **Step 4: Add `shows` field to `Registry`**

Find the `pub struct Registry` block (around line 52). Add a new field after `defines`:

```rust
    /// Show implementations keyed by type name. Independent of the `types`
    /// map: a type can have a show without being a first-class registered
    /// type (useful for future dynamic types) and can be a registered type
    /// without having a show (fallback rendering applies).
    shows: HashMap<String, agnes_types::ShowFn>,
```

Then extend `Registry::new()` to initialize it:

```rust
    pub fn new() -> Self {
        Self {
            types: HashMap::new(),
            aliases: HashMap::new(),
            tools: HashMap::new(),
            defines: HashMap::new(),
            shows: HashMap::new(),
        }
    }
```

- [ ] **Step 5: Implement `register_show` and `show_of`**

Below the `defines_of` free function or wherever fits alphabetically in `impl Registry`, add:

```rust
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
```

- [ ] **Step 6: Implement `show_value` (recursive)**

Add this method in the same `impl Registry` block:

```rust
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
                        if let TypeExpr::Named(n) = a {
                            if n.0 == "Unit" {
                                continue;
                            }
                        }
                        return self.show_data(data, a);
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
```

- [ ] **Step 7: Run test — expect PASS**

Run: `cargo test -p agnes-registry --test shows`
Expected: PASS (9/9).

- [ ] **Step 8: Full workspace still green**

Run: `cargo test --workspace`
Expected: PASS (all pre-existing tests still pass; nothing else touches the new APIs yet).

Run: `cargo check --workspace --tests`
Expected: no warnings.

- [ ] **Step 9: Commit**

```bash
jj describe -m "feat(registry): Show typeclass storage and recursive show_value

Add shows: HashMap<String, ShowFn> to Registry with register_show /
show_of / show_value. Registration is independent of register_type
(a type may have a show without being a first-class type).

show_value walks TypeExpr recursively: List T maps over elements,
Finish T / Observation T are transparent, (| T Unit) renders as
Option (empty on null), general unions pick first non-Unit member.
Unregistered types fall back to serde_json::to_string_pretty.

RegistryError gains DuplicateShow { name }.

Co-Authored-By: Claude <noreply@anthropic.com>"
jj new
```

---

### Task 3: `agnes-registry::resolve` accepts `Finish _` and `Observation _` heads

**Files:**
- Modify: `crates/agnes-registry/src/lib.rs`
- Test: `crates/agnes-registry/tests/resolve_wrappers.rs`

**Interfaces:**
- Consumes: `agnes_ast::TypeExprAst`, `agnes_types::TypeExpr`.
- Produces: `Registry::resolve` no longer rejects `App{head: "Finish"|"Observation", ...}` as UnknownName. Arity is validated (exactly 1 arg).

**Why:** Task 5 wants tool authors to be able to write `(declare tool foo :provides (Finish PlainText))` in DSL without confusing errors. Even though the reference implementation of `finish`/`observe` uses `Unknown -> Unknown`, we want the parser/registry to accept the heads for anyone defining custom tools that returns pre-tagged values.

- [ ] **Step 1: Write the failing test**

Create `crates/agnes-registry/tests/resolve_wrappers.rs`:

```rust
use agnes_ast::TypeExprAst;
use agnes_registry::{Registry, RegistryError};
use agnes_types::{TypeExpr, TypeName};

fn ast_named(s: &str) -> TypeExprAst {
    TypeExprAst::Named(s.into())
}

#[test]
fn resolve_finish_single_arg() {
    let mut reg = Registry::new();
    reg.register_type("PlainText", None).unwrap();
    let ast = TypeExprAst::App {
        head: "Finish".into(),
        args: vec![ast_named("PlainText")],
    };
    let got = reg.resolve(&ast).unwrap();
    let expected = TypeExpr::App {
        head: TypeName("Finish".into()),
        args: vec![TypeExpr::named("PlainText")],
    };
    assert_eq!(got, expected);
}

#[test]
fn resolve_observation_single_arg() {
    let mut reg = Registry::new();
    reg.register_type("Summary", None).unwrap();
    let ast = TypeExprAst::App {
        head: "Observation".into(),
        args: vec![ast_named("Summary")],
    };
    let got = reg.resolve(&ast).unwrap();
    let expected = TypeExpr::App {
        head: TypeName("Observation".into()),
        args: vec![TypeExpr::named("Summary")],
    };
    assert_eq!(got, expected);
}

#[test]
fn resolve_finish_wrong_arity_rejects() {
    let mut reg = Registry::new();
    reg.register_type("PlainText", None).unwrap();
    let ast = TypeExprAst::App {
        head: "Finish".into(),
        args: vec![ast_named("PlainText"), ast_named("PlainText")],
    };
    let err = reg.resolve(&ast).unwrap_err();
    match err {
        RegistryError::ArityMismatch {
            head,
            expected,
            actual,
        } => {
            assert_eq!(head, "Finish");
            assert_eq!(expected, 1);
            assert_eq!(actual, 2);
        }
        other => panic!("expected ArityMismatch, got {other:?}"),
    }
}

#[test]
fn resolve_observation_zero_args_rejects() {
    let reg = Registry::new();
    let ast = TypeExprAst::App {
        head: "Observation".into(),
        args: vec![],
    };
    let err = reg.resolve(&ast).unwrap_err();
    assert!(matches!(err, RegistryError::ArityMismatch { .. }));
}

#[test]
fn resolve_nested_finish_of_list_of_plaintext() {
    let mut reg = Registry::new();
    reg.register_type("PlainText", None).unwrap();
    let ast = TypeExprAst::App {
        head: "Finish".into(),
        args: vec![TypeExprAst::App {
            head: "List".into(),
            args: vec![ast_named("PlainText")],
        }],
    };
    let got = reg.resolve(&ast).unwrap();
    let expected = TypeExpr::App {
        head: TypeName("Finish".into()),
        args: vec![TypeExpr::App {
            head: TypeName("List".into()),
            args: vec![TypeExpr::named("PlainText")],
        }],
    };
    assert_eq!(got, expected);
}
```

- [ ] **Step 2: Run test — expect FAIL**

Run: `cargo test -p agnes-registry --test resolve_wrappers`
Expected: FAIL (`UnknownName { name: "Finish" }` etc).

- [ ] **Step 3: Extend `Registry::resolve`**

In `crates/agnes-registry/src/lib.rs`, find the `pub fn resolve` method. Look for the last `TypeExprAst::App` match arm that handles `"List"`:

```rust
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
```

Immediately after this arm (still before the fallthrough `_ => Err(UnknownName)`), add two symmetric arms:

```rust
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
```

Also update the doc comment on `resolve` (three lines above). It currently says:

```rust
    /// Resolve a syntactic TypeExprAst into a canonical TypeExpr.
    /// Recognizes `Named`, `App { head: "|", ... }`, `App { head: "List", ... }`,
    /// and `App { head: "Option", ... }`. Any other App head fails with
```

Change the second line to:

```rust
    /// Recognizes `Named`, `App { head: "|" | "List" | "Option" | "Finish" | "Observation", ... }`.
    /// Any other App head fails with
```

- [ ] **Step 4: Run test — expect PASS**

Run: `cargo test -p agnes-registry --test resolve_wrappers`
Expected: PASS (5/5).

- [ ] **Step 5: Full workspace regression check**

Run: `cargo test --workspace`
Expected: PASS across all crates.

Run: `cargo check --workspace --tests`
Expected: no warnings.

- [ ] **Step 6: Commit**

```bash
jj describe -m "feat(registry): resolve accepts Finish/Observation as App heads

Symmetric with List/Option: single arg required, ArityMismatch on
mismatch. Enables tool authors to write (declare tool foo :provides
(Finish PlainText)) without UnknownName errors. runtime and native_dispatch
paths for finish/observe (Unknown -> Unknown, next task) are unaffected.

Co-Authored-By: Claude <noreply@anthropic.com>"
jj new
```

---

### Task 4: `agnes-builtins::shows` module + register all builtin show impls

**Files:**
- Create: `crates/agnes-builtins/src/shows.rs`
- Modify: `crates/agnes-builtins/src/lib.rs`
- Test: `crates/agnes-builtins/tests/shows.rs`

**Interfaces:**
- Consumes: `agnes_types::ShowFn`, `agnes_registry::Registry`.
- Produces: For each of the 12 registered built-in atomic types (`PlainText`, `Summary`, `Markdown`, `HTML`, `PDF`, `Image`, `JSON`, `Path`, `String`, `Int`, `Bool`, `Unit`), a `ShowFn` implementation is registered on the `Registry` inside `register_builtins`.

- [ ] **Step 1: Write the failing test**

Create `crates/agnes-builtins/tests/shows.rs`:

```rust
use agnes_builtins::register_builtins;
use agnes_registry::Registry;
use agnes_types::{TypeExpr, Value};
use serde_json::json;

fn reg_with_builtins() -> Registry {
    let mut r = Registry::new();
    register_builtins(&mut r).unwrap();
    r
}

fn v(data: serde_json::Value, ty: &str) -> Value {
    Value {
        data,
        declared_type: TypeExpr::named(ty),
    }
}

#[test]
fn plaintext_show_returns_raw_string() {
    let r = reg_with_builtins();
    assert_eq!(r.show_value(&v(json!("hello"), "PlainText")), "hello");
}

#[test]
fn summary_show_returns_raw_string() {
    let r = reg_with_builtins();
    assert_eq!(r.show_value(&v(json!("brief"), "Summary")), "brief");
}

#[test]
fn markdown_and_html_show_raw() {
    let r = reg_with_builtins();
    assert_eq!(r.show_value(&v(json!("# hi"), "Markdown")), "# hi");
    assert_eq!(r.show_value(&v(json!("<p>hi</p>"), "HTML")), "<p>hi</p>");
}

#[test]
fn pdf_show_returns_binary_placeholder() {
    let r = reg_with_builtins();
    // PDF data is a base64 or JSON string in practice; the show impl
    // must NOT include the raw bytes.
    let out = r.show_value(&v(json!("%PDF-1.4..."), "PDF"));
    assert!(out.starts_with("<PDF binary"));
    assert!(out.contains("bytes>"));
}

#[test]
fn image_show_returns_binary_placeholder() {
    let r = reg_with_builtins();
    let out = r.show_value(&v(json!("iVBORw0K..."), "Image"));
    assert!(out.starts_with("<Image binary"));
}

#[test]
fn json_show_pretty_prints_object() {
    let r = reg_with_builtins();
    let out = r.show_value(&v(json!({"a": 1, "b": [true, null]}), "JSON"));
    assert!(out.contains("\"a\""));
    assert!(out.contains("\"b\""));
    assert!(out.contains('\n'), "pretty print should include newlines");
}

#[test]
fn path_and_string_show_raw() {
    let r = reg_with_builtins();
    assert_eq!(r.show_value(&v(json!("/tmp/x"), "Path")), "/tmp/x");
    assert_eq!(r.show_value(&v(json!("abc"), "String")), "abc");
}

#[test]
fn int_and_bool_show_via_to_string() {
    let r = reg_with_builtins();
    assert_eq!(r.show_value(&v(json!(42), "Int")), "42");
    assert_eq!(r.show_value(&v(json!(true), "Bool")), "true");
    assert_eq!(r.show_value(&v(json!(false), "Bool")), "false");
}

#[test]
fn unit_show_is_empty_string() {
    let r = reg_with_builtins();
    assert_eq!(r.show_value(&v(json!(null), "Unit")), "");
    // Even non-null data still renders empty for Unit.
    assert_eq!(r.show_value(&v(json!("stuff"), "Unit")), "");
}

#[test]
fn list_of_plaintext_shows_bracketed_comma_joined() {
    let r = reg_with_builtins();
    let ty = TypeExpr::App {
        head: agnes_types::TypeName("List".into()),
        args: vec![TypeExpr::named("PlainText")],
    };
    let v = Value {
        data: json!(["a", "b", "c"]),
        declared_type: ty,
    };
    assert_eq!(r.show_value(&v), "[a, b, c]");
}

#[test]
fn duplicate_registration_fails() {
    // register_builtins is idempotent-unfriendly by design; calling twice
    // hits DuplicateShow (or NameConflict on types). Confirm the show side.
    let mut r = Registry::new();
    register_builtins(&mut r).unwrap();
    let err = register_builtins(&mut r).unwrap_err();
    // Could be either NameConflict (types re-registered first) or
    // DuplicateShow (if types happened to succeed). Both are acceptable
    // — we just check the second call refuses cleanly.
    let msg = format!("{err}");
    assert!(!msg.is_empty(), "error message must be non-empty");
}
```

- [ ] **Step 2: Run test — expect FAIL**

Run: `cargo test -p agnes-builtins --test shows`
Expected: FAIL — show impls for PlainText/etc. not registered, so `show_value` returns pretty JSON like `"\"hello\""` instead of `hello`.

- [ ] **Step 3: Create `crates/agnes-builtins/src/shows.rs`**

```rust
//! Show implementations for built-in types. Registered by
//! `register_builtins` after types themselves are registered.

use agnes_types::ShowFn;
use serde_json::Value as JsonValue;

/// Extract the JSON string, or the empty string when the value is null /
/// not a string. Used for text-shaped types where a stringly-typed data
/// payload is the norm.
fn as_str_or_empty(v: &JsonValue) -> &str {
    v.as_str().unwrap_or("")
}

pub fn plain_text(v: &JsonValue) -> String {
    as_str_or_empty(v).to_string()
}

pub fn summary(v: &JsonValue) -> String {
    as_str_or_empty(v).to_string()
}

pub fn markdown(v: &JsonValue) -> String {
    as_str_or_empty(v).to_string()
}

pub fn html(v: &JsonValue) -> String {
    as_str_or_empty(v).to_string()
}

pub fn path(v: &JsonValue) -> String {
    as_str_or_empty(v).to_string()
}

pub fn string(v: &JsonValue) -> String {
    as_str_or_empty(v).to_string()
}

pub fn int(v: &JsonValue) -> String {
    match v {
        JsonValue::Number(n) => n.to_string(),
        _ => v.to_string(),
    }
}

pub fn bool_(v: &JsonValue) -> String {
    match v {
        JsonValue::Bool(b) => b.to_string(),
        _ => v.to_string(),
    }
}

pub fn unit(_v: &JsonValue) -> String {
    // Unit collapses to empty. Contentful Unit payloads are also empty by
    // convention (Unit is the "no meaningful data" sentinel).
    String::new()
}

pub fn json(v: &JsonValue) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
}

pub fn pdf(v: &JsonValue) -> String {
    let byte_count = v.as_str().map(|s| s.len()).unwrap_or(0);
    format!("<PDF binary, {byte_count} bytes>")
}

pub fn image(v: &JsonValue) -> String {
    let byte_count = v.as_str().map(|s| s.len()).unwrap_or(0);
    format!("<Image binary, {byte_count} bytes>")
}

/// Type-erased list of `(name, ShowFn)` pairs to register.
pub const BUILTIN_SHOWS: &[(&str, ShowFn)] = &[
    ("PlainText", plain_text),
    ("Summary", summary),
    ("Markdown", markdown),
    ("HTML", html),
    ("PDF", pdf),
    ("Image", image),
    ("JSON", json),
    ("Path", path),
    ("String", string),
    ("Int", int),
    ("Bool", bool_),
    ("Unit", unit),
];
```

- [ ] **Step 4: Wire the module and register the shows**

Edit `crates/agnes-builtins/src/lib.rs`. At the top, alongside `mod aliases;`, add:

```rust
mod shows;
```

Then, in `register_builtins`, immediately AFTER the existing types block (right after `reg.register_type("Bool", None)?;`), add:

```rust
    // --- Show impls for built-in types ---
    for (name, f) in shows::BUILTIN_SHOWS {
        reg.register_show(name, *f)?;
    }
```

- [ ] **Step 5: Run test — expect PASS**

Run: `cargo test -p agnes-builtins --test shows`
Expected: PASS (11/11).

- [ ] **Step 6: Full regression check**

Run: `cargo test --workspace`
Expected: PASS.

Run: `cargo check --workspace --tests`
Expected: no warnings.

- [ ] **Step 7: Commit**

```bash
jj describe -m "feat(builtins): register Show impls for all built-in types

Twelve ShowFns land in crates/agnes-builtins/src/shows.rs, exposed via
a BUILTIN_SHOWS slice. register_builtins iterates it after types are
registered. Text types render raw; PDF/Image render byte-count
placeholders; JSON pretty-prints; Unit collapses to empty; numbers
and bools stringify.

Foundation for the agent loop's Show-based Value serialization.

Co-Authored-By: Claude <noreply@anthropic.com>"
jj new
```

---

### Task 5: `Unknown`-as-wildcard + register `Finish`/`Observation` types + `finish`/`observe` tools

**Files:**
- Modify: `crates/agnes-types/src/lib.rs`
- Modify: `crates/agnes-builtins/src/lib.rs`
- Modify: `crates/agnes-builtins/src/tools.rs`
- Test: `crates/agnes-types/tests/unknown_wildcard.rs`
- Test: `crates/agnes-builtins/tests/finish_observe.rs`

**Interfaces:**
- Consumes: `agnes_types::{TypeExpr, TypeName, ToolSignature, Value, type_expr_matches}`, `agnes_llm::{Provider, MockProvider}`, `agnes-builtins::native_dispatch`.
- Produces:
  - `type_expr_matches(actual, expected)` returns `true` whenever `expected` is `Named(Unknown)`, for any `actual`. Semantic change; existing tests must still pass because current call sites never had `Unknown` on the `expected` side.
  - Two new registered types (`Finish`, `Observation`) and two new registered tools (`finish`, `observe`), both with signature `:input Unknown -> Unknown`. `native_dispatch` returns closures for them that rewrite `Value.declared_type` at runtime.

**Why the wildcard step is here:** `finish` / `observe` have signature `:input Unknown -> Unknown` (agnes has no type variables). Without a wildcard match, `type_expr_matches(Summary, Unknown) = false`, and the checker rejects `(pipe (tool summarize) finish)` with a `FlowMismatch`. Making `Unknown` a wildcard on the `expected` side is the smallest semantic change that unblocks the loop-control tools; it also matches the pre-existing convention that `register_type("Unknown", None)` carries no validator (Unknown is already "any" at runtime).

**Note on the tool signature:** static `Unknown -> Unknown` because agnes has no type variables. The actual type wrapping happens at runtime.

- [ ] **Step 1: Write the failing wildcard test**

Create `crates/agnes-types/tests/unknown_wildcard.rs`:

```rust
use agnes_types::{TypeExpr, TypeName, canonicalize_union, type_expr_matches};

#[test]
fn unknown_expected_matches_any_named() {
    let expected = TypeExpr::named("Unknown");
    assert!(type_expr_matches(&TypeExpr::named("PlainText"), &expected));
    assert!(type_expr_matches(&TypeExpr::named("Summary"), &expected));
    assert!(type_expr_matches(&TypeExpr::named("Unit"), &expected));
    // Even Unknown itself.
    assert!(type_expr_matches(&TypeExpr::named("Unknown"), &expected));
}

#[test]
fn unknown_expected_matches_apps() {
    let expected = TypeExpr::named("Unknown");
    // (List PlainText)
    let list_pt = TypeExpr::App {
        head: TypeName("List".into()),
        args: vec![TypeExpr::named("PlainText")],
    };
    assert!(type_expr_matches(&list_pt, &expected));
    // (Finish Summary)
    let finish_summary = TypeExpr::App {
        head: TypeName("Finish".into()),
        args: vec![TypeExpr::named("Summary")],
    };
    assert!(type_expr_matches(&finish_summary, &expected));
    // (| PlainText Markdown)
    let union = canonicalize_union([
        TypeExpr::named("PlainText"),
        TypeExpr::named("Markdown"),
    ]);
    assert!(type_expr_matches(&union, &expected));
}

#[test]
fn unknown_actual_still_only_matches_unknown_expected() {
    // Wildcard is one-directional: `Unknown` on the ACTUAL side does NOT
    // match every expected. (This preserves the existing behavior of
    // list-literal narrowing at the runtime boundary.)
    let actual = TypeExpr::named("Unknown");
    assert!(type_expr_matches(&actual, &TypeExpr::named("Unknown")));
    assert!(!type_expr_matches(&actual, &TypeExpr::named("PlainText")));
    assert!(!type_expr_matches(&actual, &TypeExpr::named("Summary")));
}

#[test]
fn union_containing_unknown_matches_anything() {
    // (| Unknown Unit) — pathological but confirms unions distribute the
    // wildcard correctly.
    let expected = canonicalize_union([
        TypeExpr::named("Unknown"),
        TypeExpr::named("Unit"),
    ]);
    assert!(type_expr_matches(&TypeExpr::named("PlainText"), &expected));
    assert!(type_expr_matches(&TypeExpr::named("Summary"), &expected));
}
```

- [ ] **Step 2: Run wildcard test — expect FAIL**

Run: `cargo test -p agnes-types --test unknown_wildcard`
Expected: FAIL — first assertion `type_expr_matches(PlainText, Unknown)` returns `false`.

- [ ] **Step 3: Patch `type_expr_matches`**

Edit `crates/agnes-types/src/lib.rs`. Find `pub fn type_expr_matches` (around line 149). Add a new match arm at the TOP of the `match` (before the existing arms):

```rust
pub fn type_expr_matches(actual: &TypeExpr, expected: &TypeExpr) -> bool {
    match (actual, expected) {
        // `Unknown` on the expected side is a wildcard: matches any actual
        // type. Used by tool signatures like `finish :input Unknown -> Unknown`
        // to accept arbitrary upstream data. One-directional: `Unknown` on
        // the ACTUAL side still requires exact match on the expected side.
        (_, TypeExpr::Named(n)) if n.0 == "Unknown" => true,
        (TypeExpr::Named(a), TypeExpr::Named(b)) => a == b,
        // ...rest unchanged...
```

Keep everything below identical. Also update the doc comment (three lines above the fn) so it reads:

```rust
/// Recursive matching. `actual` satisfies `expected` if:
/// - `expected` is `Named(Unknown)` — wildcard (one-directional), OR
/// - both are `Named` with the same name, OR
/// - `expected` is a `|` union and any member matches `actual`, OR
/// - both are same-head `App`s of equal arity and args match position-wise.
```

- [ ] **Step 4: Run wildcard test — expect PASS**

Run: `cargo test -p agnes-types --test unknown_wildcard`
Expected: PASS (4/4).

- [ ] **Step 5: Full regression on agnes-types**

Run: `cargo test -p agnes-types`
Expected: PASS. Pre-existing tests do not put `Unknown` on the expected side (Task 5 pre-flight: `grep -rn "Unknown" crates/*/tests/` shows no test expecting `Unknown` to fail-match); the new arm is purely permissive.

Run: `cargo test --workspace`
Expected: PASS on every crate that already passed. In particular, checker tests should not break — `Unknown` on the expected side never appears in existing tool signatures until we add `finish` / `observe` below.

- [ ] **Step 6: Write the failing finish/observe test**

Create `crates/agnes-builtins/tests/finish_observe.rs`:

```rust
use agnes_builtins::{native_dispatch, register_builtins};
use agnes_llm::MockProvider;
use agnes_registry::Registry;
use agnes_types::{TypeExpr, TypeName, Value};
use serde_json::{Value as JsonValue, json};
use std::collections::HashMap;
use std::sync::Arc;

fn dispatch() -> HashMap<String, agnes_builtins::ToolImpl> {
    let mock = Arc::new(MockProvider::new(vec![]));
    native_dispatch(mock)
}

fn reg() -> Registry {
    let mut r = Registry::new();
    register_builtins(&mut r).unwrap();
    r
}

fn kwargs_with_input(v: Value) -> HashMap<String, Value> {
    let mut m = HashMap::new();
    m.insert("input".to_string(), v);
    m
}

#[tokio::test]
async fn finish_wraps_upstream_type_as_finish() {
    let d = dispatch();
    let finish = d.get("finish").expect("finish tool registered");
    let upstream = Value {
        data: json!("done"),
        declared_type: TypeExpr::named("PlainText"),
    };
    let out = finish(kwargs_with_input(upstream)).await.unwrap();
    // Data unchanged.
    assert_eq!(out.data, JsonValue::String("done".to_string()));
    // declared_type wrapped as (Finish PlainText).
    assert_eq!(
        out.declared_type,
        TypeExpr::App {
            head: TypeName("Finish".into()),
            args: vec![TypeExpr::named("PlainText")],
        }
    );
}

#[tokio::test]
async fn observe_wraps_upstream_type_as_observation() {
    let d = dispatch();
    let observe = d.get("observe").expect("observe tool registered");
    let upstream = Value {
        data: json!({"tokens": 42}),
        declared_type: TypeExpr::named("JSON"),
    };
    let out = observe(kwargs_with_input(upstream)).await.unwrap();
    assert_eq!(out.data, json!({"tokens": 42}));
    assert_eq!(
        out.declared_type,
        TypeExpr::App {
            head: TypeName("Observation".into()),
            args: vec![TypeExpr::named("JSON")],
        }
    );
}

#[tokio::test]
async fn finish_wraps_already_wrapped_type_last_one_wins() {
    // (pipe X observe finish) — spec §12 "last one wins" semantics.
    // Runtime wraps sequentially; the outermost head is what Session sees.
    let d = dispatch();
    let observe = d.get("observe").unwrap();
    let finish = d.get("finish").unwrap();

    let upstream = Value {
        data: json!("hi"),
        declared_type: TypeExpr::named("PlainText"),
    };
    let after_observe = observe(kwargs_with_input(upstream)).await.unwrap();
    let after_finish = finish(kwargs_with_input(after_observe)).await.unwrap();

    // Outer is Finish, inner is Observation of PlainText.
    assert_eq!(
        after_finish.declared_type,
        TypeExpr::App {
            head: TypeName("Finish".into()),
            args: vec![TypeExpr::App {
                head: TypeName("Observation".into()),
                args: vec![TypeExpr::named("PlainText")],
            }],
        }
    );
}

#[test]
fn finish_tool_registered_with_unknown_signature() {
    let r = reg();
    let sig = r.tool_signature("finish").expect("finish registered");
    // requires: [("input", Unknown)]
    assert_eq!(sig.requires.len(), 1);
    assert_eq!(sig.requires[0].0, "input");
    assert_eq!(sig.requires[0].1, TypeExpr::named("Unknown"));
    // provides: Unknown
    assert_eq!(sig.provides, TypeExpr::named("Unknown"));
}

#[test]
fn observe_tool_registered_with_unknown_signature() {
    let r = reg();
    let sig = r.tool_signature("observe").expect("observe registered");
    assert_eq!(sig.requires.len(), 1);
    assert_eq!(sig.requires[0].0, "input");
    assert_eq!(sig.requires[0].1, TypeExpr::named("Unknown"));
    assert_eq!(sig.provides, TypeExpr::named("Unknown"));
}

#[test]
fn finish_and_observation_types_registered() {
    let r = reg();
    // Types must be registered so (declare tool ...) syntax with (Finish _)
    // won't fail with UnknownName. Task 3 already lets resolve accept the
    // heads; register_type here makes them first-class names too.
    // The exact API check: registering again fails with NameConflict.
    let mut r2 = Registry::new();
    register_builtins(&mut r2).unwrap();
    let err = r2.register_type("Finish", None).unwrap_err();
    match err {
        agnes_registry::RegistryError::NameConflict { name, .. } => {
            assert_eq!(name, "Finish");
        }
        other => panic!("expected NameConflict, got {other:?}"),
    }
}
```

- [ ] **Step 7: Run test — expect FAIL**

Run: `cargo test -p agnes-builtins --test finish_observe`
Expected: FAIL — `finish` and `observe` not in dispatch map, not in registry.

- [ ] **Step 8: Register `Finish` and `Observation` types**

Edit `crates/agnes-builtins/src/lib.rs`. In `register_builtins`, immediately AFTER the existing type registration block (after `reg.register_type("Bool", None)?;` and BEFORE the show registration you added in Task 4), add:

```rust
    // --- Wrapper types (used at runtime by finish/observe) ---
    reg.register_type("Finish", None)?;
    reg.register_type("Observation", None)?;
```

Ordering is deliberate: types register before shows and tools so subsequent registrations can reference them.

- [ ] **Step 9: Register the `finish` and `observe` tool signatures**

Still in `register_builtins`, at the END of the tools block (after `join-lines`), add:

```rust
    // --- Loop control tools ---
    let unknown = TypeExpr::named("Unknown");
    reg.register_tool(
        "finish",
        ToolSignature {
            requires: vec![("input".into(), unknown.clone())],
            provides: unknown.clone(),
        },
    )?;
    reg.register_tool(
        "observe",
        ToolSignature {
            requires: vec![("input".into(), unknown.clone())],
            provides: unknown,
        },
    )?;
```

- [ ] **Step 10: Implement the native dispatch closures**

Edit `crates/agnes-builtins/src/tools.rs`. Find the `pub fn native_dispatch(provider: Arc<dyn Provider>) -> HashMap<String, ToolImpl>` function. Look for its last tool registration inside the returned map (should be `join-lines`). Immediately AFTER `join-lines`'s registration (before the closing `map`), add:

```rust
    // --- Loop control: finish / observe ---
    // Both are identity on data but rewrite declared_type at the outer
    // layer so Session::run_turn can classify the root shape.
    map.insert(
        "finish".to_string(),
        Arc::new(|mut kw| Box::pin(async move {
            let inner = kw
                .remove("input")
                .ok_or_else(|| "finish requires :input".to_string())?;
            Ok(Value {
                data: inner.data,
                declared_type: TypeExpr::App {
                    head: TypeName("Finish".into()),
                    args: vec![inner.declared_type],
                },
            })
        })) as ToolImpl,
    );
    map.insert(
        "observe".to_string(),
        Arc::new(|mut kw| Box::pin(async move {
            let inner = kw
                .remove("input")
                .ok_or_else(|| "observe requires :input".to_string())?;
            Ok(Value {
                data: inner.data,
                declared_type: TypeExpr::App {
                    head: TypeName("Observation".into()),
                    args: vec![inner.declared_type],
                },
            })
        })) as ToolImpl,
    );
```

**Important:** the closures use `TypeExpr` and `TypeName` — make sure `crates/agnes-builtins/src/tools.rs`'s imports include them. Check the top of that file. If missing, add:

```rust
use agnes_types::{TypeExpr, TypeName};
```

(The file already imports `agnes_types::Value`; extend to include `TypeExpr, TypeName`.)

- [ ] **Step 11: Run test — expect PASS**

Run: `cargo test -p agnes-builtins --test finish_observe`
Expected: PASS (6/6).

- [ ] **Step 12: Regression**

Run: `cargo test --workspace`
Expected: PASS. Notably, no existing runtime/CLI/checker test should break — `finish`/`observe` are additive.

Run: `cargo check --workspace --tests`
Expected: no warnings.

- [ ] **Step 13: Commit**

```bash
jj describe -m "feat(types,builtins): Unknown wildcard + finish/observe tools

type_expr_matches now treats Named(Unknown) on the expected side as a
wildcard, matching any actual type. One-directional: Unknown on the
actual side still requires exact match. Unblocks tool signatures like
finish :input Unknown -> Unknown accepting arbitrary upstream data.

Two new builtin tools with signature :input Unknown -> Unknown.
native_dispatch closures preserve data.data but wrap declared_type as
App{Finish|Observation, args:[<inner>]}. This is how the upcoming
Session::run_turn loop knows whether to terminate the turn or feed the
result back to the planner.

New wrapper types Finish and Observation are registered so that
(declare tool foo :provides (Finish PlainText)) parses cleanly (Task 3
handled resolve). Static tool signatures stay type-variable-free.

Co-Authored-By: Claude <noreply@anthropic.com>"
jj new
```

---

### Task 6: `Planner` new state + new interface (no LLM prompt changes yet)

**Files:**
- Modify: `crates/agnes-llm/src/planner.rs`
- Test: `crates/agnes-llm/tests/planner_state.rs`
- Delete: none; the old planner tests in `crates/agnes-llm/tests/planner.rs` will need updates in Task 7 when the system prompt / message layout changes; leave them untouched here.

**Interfaces:**
- Consumes: `agnes_llm::{Provider, CompletionRequest, Message, Role, LlmError, MockProvider}` (existing), `agnes_types::TypeName`, `agnes_registry::Registry`.
- Produces the new state model:
  - `pub struct Turn { pub user_nl: String, pub iterations: Vec<Iteration>, pub outcome: TurnOutcome }`.
  - `pub struct Iteration { pub assistant_dsl: String, pub observation: Option<Observation> }`.
  - `pub struct Observation { pub text: String, pub is_error: bool, pub type_name: Option<TypeName> }`.
  - `pub enum TurnOutcome { Finished { result: String }, TurnLimitExceeded }`.
- Produces new methods:
  - `pub fn begin_user_turn(&mut self, nl: String)`.
  - `pub async fn plan_next(&mut self) -> Result<String, PlannerError>` — replaces `plan(&mut self, nl)`.
  - `pub fn push_observation(&mut self, dsl: String, text: String, is_error: bool, type_name: Option<TypeName>)` — replaces `push_error_feedback`.
  - `pub fn record_finish(&mut self, dsl: String, result: String)` — replaces `record_result`.
  - `pub fn abandon_pending_turn(&mut self)` — kept; extended semantics: on any in-flight iterations, stamp `outcome = TurnLimitExceeded` and commit to history.
- Removes:
  - `pub async fn plan(&mut self, nl: &str) -> Result<String, PlannerError>` (superseded).
  - `pub fn push_error_feedback(&mut self, bad_dsl: String, err: String)` (superseded).
  - `pub fn record_result(&mut self, dsl: String, result_preview: String)` (superseded).
- Note: this task ONLY refactors state and the state-manipulation methods. The `build_system_prompt` / `build_messages` internals stay as they are; Task 7 rewrites them to match the new state and adds the new system prompt.

**Consequence for Session:** `crates/agnes-session/src/session.rs` will not compile after this task (it still calls the old methods). We deliberately let it break; Task 9/10 rewrite Session to use the new API. Between tasks the workspace is red; each task's tests scope to its own crate.

- [ ] **Step 1: Write the failing test for state**

Create `crates/agnes-llm/tests/planner_state.rs`:

```rust
use agnes_builtins::register_builtins;
use agnes_llm::{
    Iteration, MockProvider, Observation, Planner, Turn, TurnOutcome,
};
use agnes_registry::Registry;
use agnes_types::TypeName;
use std::sync::Arc;

fn reg() -> Registry {
    let mut r = Registry::new();
    register_builtins(&mut r).unwrap();
    r
}

fn planner_with(responses: Vec<String>) -> Planner {
    let r = reg();
    Planner::new(Arc::new(MockProvider::new(responses)), &r)
}

#[test]
fn begin_user_turn_seeds_but_does_not_commit() {
    let mut p = planner_with(vec![]);
    p.begin_user_turn("Translate the file".into());
    // Before anything runs, history is still empty.
    assert!(p.history().is_empty());
}

#[tokio::test]
async fn plan_next_appends_assistant_dsl_to_inflight_iterations() {
    let mut p = planner_with(vec!["```agnes\n(pipe \"hi\" finish)\n```".into()]);
    p.begin_user_turn("say hi".into());
    let dsl = p.plan_next().await.unwrap();
    assert_eq!(dsl.trim(), "(pipe \"hi\" finish)");
    // Not yet committed.
    assert!(p.history().is_empty());
}

#[tokio::test]
async fn push_observation_attaches_to_last_iteration() {
    let mut p = planner_with(vec![
        "```agnes\n(pipe (tool summarize :input \"x\") observe)\n```".into(),
        "```agnes\n(pipe \"done\" finish)\n```".into(),
    ]);
    p.begin_user_turn("...".into());

    let dsl1 = p.plan_next().await.unwrap();
    p.push_observation(
        dsl1,
        "the summary".into(),
        false,
        Some(TypeName("Summary".into())),
    );

    // Still not committed.
    assert!(p.history().is_empty());

    // Second plan_next should include the observation as a `user` message
    // in the request. We verify that indirectly by driving another iteration.
    let dsl2 = p.plan_next().await.unwrap();
    assert!(dsl2.contains("finish"));
}

#[tokio::test]
async fn record_finish_commits_the_turn_with_finished_outcome() {
    let mut p = planner_with(vec!["```agnes\n(pipe \"ok\" finish)\n```".into()]);
    p.begin_user_turn("hi".into());
    let dsl = p.plan_next().await.unwrap();
    p.record_finish(dsl.clone(), "ok".into());
    let hist = p.history();
    assert_eq!(hist.len(), 1);
    let t: &Turn = &hist[0];
    assert_eq!(t.user_nl, "hi");
    assert_eq!(t.iterations.len(), 1);
    let it: &Iteration = &t.iterations[0];
    assert_eq!(it.assistant_dsl, dsl);
    assert!(it.observation.is_none(), "final iteration has no observation");
    match &t.outcome {
        TurnOutcome::Finished { result } => assert_eq!(result, "ok"),
        other => panic!("expected Finished, got {other:?}"),
    }
}

#[tokio::test]
async fn abandon_pending_turn_stamps_turn_limit_exceeded() {
    let mut p = planner_with(vec![
        "```agnes\n(pipe \"a\" observe)\n```".into(),
        "```agnes\n(pipe \"b\" observe)\n```".into(),
    ]);
    p.begin_user_turn("won't finish".into());
    let d1 = p.plan_next().await.unwrap();
    p.push_observation(d1, "a".into(), false, None);
    let d2 = p.plan_next().await.unwrap();
    p.push_observation(d2, "b".into(), false, None);

    p.abandon_pending_turn();
    let hist = p.history();
    // abandon_pending_turn now commits the in-flight turn with a
    // TurnLimitExceeded outcome, so history grows by one.
    assert_eq!(hist.len(), 1);
    assert!(matches!(hist[0].outcome, TurnOutcome::TurnLimitExceeded));
    assert_eq!(hist[0].iterations.len(), 2);
}

#[test]
fn abandon_pending_turn_on_no_inflight_is_noop() {
    let mut p = planner_with(vec![]);
    p.abandon_pending_turn();
    assert!(p.history().is_empty());
}

#[test]
fn observation_records_type_name_when_provided() {
    let obs = Observation {
        text: "hello".into(),
        is_error: false,
        type_name: Some(TypeName("Summary".into())),
    };
    assert_eq!(obs.text, "hello");
    assert!(!obs.is_error);
    assert_eq!(obs.type_name.as_ref().unwrap().0, "Summary");
}
```

- [ ] **Step 2: Run test — expect FAIL**

Run: `cargo test -p agnes-llm --test planner_state`
Expected: FAIL with `unresolved import` for `Iteration`, `Observation`, `TurnOutcome`, and `begin_user_turn`, etc.

- [ ] **Step 3: Rewrite `agnes-llm/src/planner.rs` state model**

Open `crates/agnes-llm/src/planner.rs`. Replace the `pub struct Turn` block (currently `{ user_nl, assistant_dsl, result_preview }`) with:

```rust
/// A committed user↔agent turn: user_nl, one or more iterations of DSL
/// (with optional intermediate observations), and a final outcome.
#[derive(Debug, Clone)]
pub struct Turn {
    pub user_nl: String,
    pub iterations: Vec<Iteration>,
    pub outcome: TurnOutcome,
}

/// A single (assistant DSL, resulting observation) pair inside a turn.
/// `observation.is_none()` on the LAST iteration means that DSL was the
/// terminating one (Finish or implicit).
#[derive(Debug, Clone)]
pub struct Iteration {
    pub assistant_dsl: String,
    pub observation: Option<Observation>,
}

/// What the runtime returned during an iteration (Observation branch) or
/// what error was encountered before the next planner call.
#[derive(Debug, Clone)]
pub struct Observation {
    pub text: String,
    pub is_error: bool,
    /// Inner type name (Finish/Observation stripped one layer) for the
    /// `<observation type="...">` XML attribute. `None` on error paths.
    pub type_name: Option<agnes_types::TypeName>,
}

/// How a turn ended.
#[derive(Debug, Clone)]
pub enum TurnOutcome {
    /// Terminating iteration produced a value; `result` is the shown string.
    Finished { result: String },
    /// Session hit MAX_TURNS without a terminating iteration.
    TurnLimitExceeded,
}
```

Then delete the old `plan`, `push_error_feedback`, `record_result`, and replace `abandon_pending_turn` bodies. Find the `pub struct Planner` block and its `impl Planner`. The Planner **struct** likely holds `history: Vec<Turn>` and some in-flight scratch (`pending_nl`, `scratch: Vec<Message>` or similar). Rework them:

```rust
pub struct Planner {
    provider: Arc<dyn Provider>,
    base_system: String,
    history: Vec<Turn>,
    /// In-flight turn state. `None` when no user turn is active.
    inflight: Option<InflightTurn>,
}

struct InflightTurn {
    user_nl: String,
    iterations: Vec<Iteration>,
}
```

Replace the `impl Planner` body's public methods. Keep `new(...)` as-is at the top, then:

```rust
    /// Read-only view of committed turns.
    pub fn history(&self) -> &[Turn] {
        &self.history
    }

    /// Discard committed history. Does not touch in-flight state; call
    /// `abandon_pending_turn` first if you also want that cleared.
    pub fn reset_history(&mut self) {
        self.history.clear();
    }

    /// Start a new in-flight user turn. Aborts any existing in-flight turn
    /// (with TurnLimitExceeded outcome), so callers must have already
    /// committed or explicitly abandoned prior turns before calling this.
    pub fn begin_user_turn(&mut self, nl: String) {
        // Defensive: if a prior turn is still in-flight, abandon it. In
        // normal flow the Session calls record_finish or abandon_pending_turn
        // before begin_user_turn, so this branch is a safety net.
        if self.inflight.is_some() {
            self.abandon_pending_turn();
        }
        self.inflight = Some(InflightTurn {
            user_nl: nl,
            iterations: Vec::new(),
        });
    }

    /// Ask the LLM for the next DSL iteration. Appends `assistant(dsl)`
    /// to the in-flight iterations (with observation=None until
    /// `push_observation` or `record_finish` is called).
    pub async fn plan_next(&mut self) -> Result<String, PlannerError> {
        let messages = self.build_messages();
        let request = CompletionRequest {
            system: Some(self.effective_system()),
            messages,
            max_tokens: 2048,
        };
        let raw = self.provider.complete(request).await?;
        let dsl = crate::dsl_extract::extract_dsl(&raw);
        if dsl.trim().is_empty() {
            return Err(PlannerError::EmptyResponse);
        }
        // Append to in-flight.
        let inflight = self
            .inflight
            .as_mut()
            .expect("plan_next called with no in-flight turn (missing begin_user_turn?)");
        inflight.iterations.push(Iteration {
            assistant_dsl: dsl.clone(),
            observation: None,
        });
        Ok(dsl)
    }

    /// Attach an observation to the last in-flight iteration. If the
    /// last iteration already has an observation (double push), that is
    /// a caller bug — we panic loudly.
    pub fn push_observation(
        &mut self,
        _dsl: String,
        text: String,
        is_error: bool,
        type_name: Option<agnes_types::TypeName>,
    ) {
        let inflight = self
            .inflight
            .as_mut()
            .expect("push_observation with no in-flight turn");
        let last = inflight
            .iterations
            .last_mut()
            .expect("push_observation with no iterations (missing plan_next?)");
        assert!(
            last.observation.is_none(),
            "push_observation called twice on the same iteration"
        );
        last.observation = Some(Observation {
            text,
            is_error,
            type_name,
        });
    }

    /// Commit the in-flight turn as Finished. Consumes `inflight`.
    /// The dsl arg must equal the last iteration's assistant_dsl (sanity
    /// check); if not, we still commit but stamp a fresh iteration.
    pub fn record_finish(&mut self, dsl: String, result: String) {
        let mut inflight = self
            .inflight
            .take()
            .expect("record_finish with no in-flight turn");
        // If the last iteration's DSL doesn't match, append a synthetic
        // iteration for it. This handles the edge where RawDsl was used
        // (planner never saw plan_next for this DSL).
        let last_matches = inflight
            .iterations
            .last()
            .is_some_and(|it| it.assistant_dsl == dsl);
        if !last_matches {
            inflight.iterations.push(Iteration {
                assistant_dsl: dsl,
                observation: None,
            });
        }
        self.history.push(Turn {
            user_nl: inflight.user_nl,
            iterations: inflight.iterations,
            outcome: TurnOutcome::Finished { result },
        });
    }

    /// Commit the in-flight turn as TurnLimitExceeded. No-op if no
    /// in-flight turn exists.
    pub fn abandon_pending_turn(&mut self) {
        if let Some(inflight) = self.inflight.take() {
            self.history.push(Turn {
                user_nl: inflight.user_nl,
                iterations: inflight.iterations,
                outcome: TurnOutcome::TurnLimitExceeded,
            });
        }
    }
```

- [ ] **Step 4: Keep the private helper stubs**

`build_messages`, `effective_system`, and the tool-catalog helpers stay for now. Task 7 rewrites them for the new state and new prompt. For this task, patch them just enough to compile against the new state:

- `build_messages(&self) -> Vec<Message>`: walk `self.history` and `self.inflight` together, building an alternating user/assistant sequence:
  - For each committed `Turn`: emit `user(turn.user_nl)`, then for each iteration emit `assistant(iteration.assistant_dsl)` and, if `iteration.observation.is_some()`, emit `user(observation_text_wrapped_as_xml)`.
  - Then, if `self.inflight.is_some()`, emit `user(inflight.user_nl)` and for each iteration emit `assistant(...)` and (if observation set) `user(...)`.
- `effective_system(&self) -> String`: for now just return `self.base_system.clone()`. Task 7 will add the "prior context" summary logic.

Provide a helper for XML wrapping in the same file:

```rust
fn wrap_observation(obs: &Observation) -> String {
    if obs.is_error {
        format!(
            "<observation error=\"true\">\n{}\n</observation>",
            obs.text
        )
    } else {
        match &obs.type_name {
            Some(t) => format!(
                "<observation type=\"{}\">\n{}\n</observation>",
                t.0, obs.text
            ),
            None => format!("<observation>\n{}\n</observation>", obs.text),
        }
    }
}
```

The exact `build_messages` body:

```rust
fn build_messages(&self) -> Vec<Message> {
    let mut out = Vec::new();
    for turn in &self.history {
        out.push(Message {
            role: Role::User,
            content: turn.user_nl.clone(),
        });
        for it in &turn.iterations {
            out.push(Message {
                role: Role::Assistant,
                content: it.assistant_dsl.clone(),
            });
            if let Some(obs) = &it.observation {
                out.push(Message {
                    role: Role::User,
                    content: wrap_observation(obs),
                });
            }
        }
    }
    if let Some(inflight) = &self.inflight {
        out.push(Message {
            role: Role::User,
            content: inflight.user_nl.clone(),
        });
        for it in &inflight.iterations {
            out.push(Message {
                role: Role::Assistant,
                content: it.assistant_dsl.clone(),
            });
            if let Some(obs) = &it.observation {
                out.push(Message {
                    role: Role::User,
                    content: wrap_observation(obs),
                });
            }
        }
    }
    out
}
```

- [ ] **Step 5: Update `lib.rs` re-exports**

Edit `crates/agnes-llm/src/lib.rs`. Where `Planner`, `Turn`, `PlannerError` are already re-exported, extend to include the new public types. The `pub use planner::...;` line should read:

```rust
pub use planner::{Iteration, Observation, Planner, Turn, TurnOutcome};
```

(Do not export `InflightTurn`; it is private state.)

Keep `pub use error::PlannerError;` unchanged.

- [ ] **Step 6: Run test — expect PASS**

Run: `cargo test -p agnes-llm --test planner_state`
Expected: PASS (7/7).

- [ ] **Step 7: Old planner tests are expected to fail-to-compile**

Run: `cargo test -p agnes-llm`
Expected: `tests/planner.rs` FAILS TO COMPILE — it still calls `planner.plan(nl)`, `push_error_feedback`, `record_result`. This is expected. Task 7 rewrites those tests when the prompt/message format changes.

**Do not fix the old tests in this task.** Keep the scope tight. The workspace is red between tasks; that is the SDD contract.

- [ ] **Step 8: Confirm scoped tests pass**

Run: `cargo test -p agnes-llm --test planner_state --test provider_smoke --test resolve --test dsl_extract --test anthropic_shape --test openai_shape`
Expected: all named tests PASS (planner_state 7/7 plus prior tasks' green tests).

- [ ] **Step 9: Commit**

```bash
jj describe -m "refactor(llm): Planner state model for multi-iteration turns

Rewrite Turn into { user_nl, iterations: Vec<Iteration>, outcome }.
Iteration = { assistant_dsl, observation: Option<Observation> }.
Observation = { text, is_error, type_name: Option<TypeName> }.
TurnOutcome = Finished { result } | TurnLimitExceeded.

New API:
- begin_user_turn(nl)
- plan_next() -> extracted DSL; appends to in-flight iterations
- push_observation(dsl, text, is_error, type_name)
- record_finish(dsl, result)  -> commits Finished
- abandon_pending_turn()      -> commits TurnLimitExceeded

Removes plan/push_error_feedback/record_result. Session (Task 9+) will
be rewritten to use the new API; between tasks the workspace does not
compile end-to-end, per SDD.

System prompt and message layout are unchanged in this task; Task 7
rewrites them.

Co-Authored-By: Claude <noreply@anthropic.com>"
jj new
```

---

### Task 7: `Planner` system prompt for the agent loop + rewrite old planner tests

**Files:**
- Modify: `crates/agnes-llm/src/planner.rs`
- Modify: `crates/agnes-llm/tests/planner.rs` (the pre-existing tests, now to be rewritten against the new API)

**Interfaces:** No new public API. Same signatures as Task 6. Only the internal `build_system_prompt` / `effective_system` and the observable content of `build_messages` change.

**Goal of the new system prompt:** teach the LLM three new things:
1. Each response goes through the agnes DSL parser; write a fenced ```agnes block containing one expression.
2. The DSL you produce is executed. If the result is wrapped in `Observation _`, the observation text comes back in a `<observation>` block on your next turn. If wrapped in `Finish _` (or any other unwrapped type), the turn ends.
3. Two new builtin tools: `finish :input Unknown -> Unknown` and `observe :input Unknown -> Unknown`. Explain when to use each.

- [ ] **Step 1: Rewrite pre-existing planner tests**

The file `crates/agnes-llm/tests/planner.rs` currently exercises `plan / push_error_feedback / record_result`. Replace it wholesale with tests against the new API + new prompt.

New file content:

```rust
//! Planner tests: system prompt discipline + message construction with the
//! new agent-loop state model. Round-trips go through MockProvider; no
//! real network.

use agnes_builtins::register_builtins;
use agnes_llm::{
    CompletionRequest, Message, MockProvider, Planner, Provider, Role,
};
use agnes_registry::Registry;
use agnes_types::TypeName;
use std::sync::Arc;

fn reg() -> Registry {
    let mut r = Registry::new();
    register_builtins(&mut r).unwrap();
    r
}

fn planner_with(responses: Vec<String>) -> (Planner, Arc<MockProvider>) {
    let r = reg();
    let mock = Arc::new(MockProvider::new(responses));
    let p = Planner::new(mock.clone() as Arc<dyn Provider>, &r);
    (p, mock)
}

#[tokio::test]
async fn system_prompt_lists_all_builtin_tools_including_finish_and_observe() {
    let (mut p, mock) = planner_with(vec!["```agnes\n(pipe \"hi\" finish)\n```".into()]);
    p.begin_user_turn("hi".into());
    let _ = p.plan_next().await.unwrap();
    let seen = mock.seen();
    assert_eq!(seen.len(), 1);
    let sys = seen[0].system.as_deref().unwrap_or("");
    for name in &[
        "read-file",
        "write-file",
        "summarize",
        "translate",
        "ocr",
        "llm",
        "join-lines",
        "finish",
        "observe",
    ] {
        assert!(sys.contains(name), "system prompt missing tool `{name}`");
    }
}

#[tokio::test]
async fn system_prompt_mentions_finish_and_observation_semantics() {
    let (mut p, mock) = planner_with(vec!["```agnes\n(pipe \"hi\" finish)\n```".into()]);
    p.begin_user_turn("hi".into());
    let _ = p.plan_next().await.unwrap();
    let sys = mock.seen()[0].system.clone().unwrap_or_default();
    // The prompt must explain the loop.
    assert!(
        sys.contains("Finish") && sys.contains("Observation"),
        "system prompt must reference Finish and Observation semantics"
    );
    assert!(
        sys.contains("<observation"),
        "system prompt must show LLM the <observation> block format"
    );
}

#[tokio::test]
async fn observation_message_uses_xml_wrapper_with_type_name() {
    let (mut p, mock) = planner_with(vec![
        "```agnes\n(pipe (tool summarize :input \"x\") observe)\n```".into(),
        "```agnes\n(pipe \"done\" finish)\n```".into(),
    ]);
    p.begin_user_turn("do it".into());
    let d1 = p.plan_next().await.unwrap();
    p.push_observation(
        d1,
        "the summary".into(),
        false,
        Some(TypeName("Summary".into())),
    );
    let _d2 = p.plan_next().await.unwrap();

    // Second request's second-to-last message should be a user message
    // wrapping the observation in XML with type="Summary".
    let seen = mock.seen();
    assert_eq!(seen.len(), 2);
    let msgs2 = &seen[1].messages;
    let obs_msg = msgs2
        .iter()
        .find(|m| matches!(m.role, Role::User) && m.content.contains("<observation"))
        .expect("observation user message missing");
    assert!(
        obs_msg.content.contains("type=\"Summary\""),
        "observation message missing type=\"Summary\": {}",
        obs_msg.content
    );
    assert!(obs_msg.content.contains("the summary"));
}

#[tokio::test]
async fn error_observation_uses_error_true_attribute() {
    let (mut p, mock) = planner_with(vec![
        "```agnes\n(pipe (tool bogus) observe)\n```".into(),
        "```agnes\n(pipe \"ok\" finish)\n```".into(),
    ]);
    p.begin_user_turn("do it".into());
    let d1 = p.plan_next().await.unwrap();
    p.push_observation(d1, "parse: unknown tool 'bogus'".into(), true, None);
    let _ = p.plan_next().await.unwrap();

    let seen = mock.seen();
    let msgs2 = &seen[1].messages;
    let err_msg = msgs2
        .iter()
        .find(|m| matches!(m.role, Role::User) && m.content.contains("<observation"))
        .expect("error observation message missing");
    assert!(err_msg.content.contains("error=\"true\""));
    // Error observations MUST NOT include a type attribute.
    assert!(!err_msg.content.contains("type=\""));
    assert!(err_msg.content.contains("unknown tool 'bogus'"));
}

#[tokio::test]
async fn message_chain_alternates_roles_after_multiple_iterations() {
    // Regression guard: consecutive same-role messages break Anthropic API.
    // With observations interleaved, the chain must strictly alternate.
    let (mut p, mock) = planner_with(vec![
        "```agnes\n(pipe X observe)\n```".into(),
        "```agnes\n(pipe Y observe)\n```".into(),
        "```agnes\n(pipe \"done\" finish)\n```".into(),
    ]);
    p.begin_user_turn("try it".into());
    let d1 = p.plan_next().await.unwrap();
    p.push_observation(d1, "A".into(), false, None);
    let d2 = p.plan_next().await.unwrap();
    p.push_observation(d2, "B".into(), false, None);
    let _ = p.plan_next().await.unwrap();

    let seen = mock.seen();
    let last = &seen[seen.len() - 1].messages;
    // Roles must alternate: user, assistant, user, assistant, user, assistant, user.
    let roles: Vec<_> = last.iter().map(|m| m.role).collect();
    for pair in roles.windows(2) {
        assert_ne!(
            pair[0], pair[1],
            "consecutive same-role messages: {roles:?}"
        );
    }
    // And the last message before this LLM call must be a user (the observation).
    assert_eq!(*roles.last().unwrap(), Role::User);
}

#[tokio::test]
async fn committed_history_replays_in_subsequent_turns() {
    let (mut p, mock) = planner_with(vec![
        "```agnes\n(pipe \"first\" finish)\n```".into(),
        "```agnes\n(pipe \"second\" finish)\n```".into(),
    ]);
    p.begin_user_turn("turn 1".into());
    let d1 = p.plan_next().await.unwrap();
    p.record_finish(d1, "first".into());

    p.begin_user_turn("turn 2".into());
    let _ = p.plan_next().await.unwrap();

    let seen = mock.seen();
    let msgs2 = &seen[1].messages;
    // Should see turn 1's user_nl, assistant DSL, and turn 2's user_nl.
    let has_turn1_user = msgs2
        .iter()
        .any(|m| m.content == "turn 1" && matches!(m.role, Role::User));
    let has_turn1_assistant = msgs2
        .iter()
        .any(|m| m.content.contains("first") && matches!(m.role, Role::Assistant));
    let has_turn2_user = msgs2
        .iter()
        .any(|m| m.content == "turn 2" && matches!(m.role, Role::User));
    assert!(has_turn1_user, "history missing turn 1 user_nl");
    assert!(has_turn1_assistant, "history missing turn 1 assistant DSL");
    assert!(has_turn2_user, "history missing turn 2 user_nl");
}
```

- [ ] **Step 2: Run tests — expect FAIL**

Run: `cargo test -p agnes-llm --test planner`
Expected: FAIL — the prompt doesn't yet mention `finish`/`observe`, and observation wrapping might not exactly match the assertions.

- [ ] **Step 3: Update `build_system_prompt`**

In `crates/agnes-llm/src/planner.rs`, find `fn build_system_prompt` (or the equivalent internal helper that constructs `base_system`). Rewrite it to include the loop semantics.

Here is the replacement body. Adjust the surrounding function name and signature to what your existing code uses — the internal contract is: `fn build_system_prompt(reg: &Registry) -> String`.

```rust
fn build_system_prompt(reg: &Registry) -> String {
    // Tool catalog: iterate the fixed list of builtin tools in a
    // stable order. Registry does not expose iteration; naming
    // the tools explicitly is a deliberate choice for prompt
    // determinism.
    const BUILTIN_TOOL_ORDER: &[&str] = &[
        "read-file",
        "write-file",
        "summarize",
        "translate",
        "ocr",
        "llm",
        "join-lines",
        "finish",
        "observe",
    ];
    let mut catalog = String::new();
    for name in BUILTIN_TOOL_ORDER {
        if let Some(sig) = reg.tool_signature(name) {
            catalog.push_str(&format!("  - {} :", name));
            for (pname, pty) in &sig.requires {
                catalog.push_str(&format!(" :{pname} {pty}"));
            }
            catalog.push_str(&format!("  ->  {}\n", sig.provides));
        }
    }

    format!(r#"You are the planning brain of an agnes agent. Each turn you produce
one agnes DSL expression as an ```agnes fenced block. That expression will
be parsed, type-checked, compiled, and executed by the runtime.

Loop protocol:
  * If your expression's result is wrapped as `(Observation T)` — e.g.
    the outermost tool call is `observe` — the runtime feeds the rendered
    result back to you on the next turn as a `<observation type="T">...</observation>`
    block, and you produce another DSL expression.
  * If the result is wrapped as `(Finish T)` — outer tool is `finish` —
    the rendered result is shown to the user and the turn ENDS.
  * If the result is neither `Finish` nor `Observation` (any plain type),
    the runtime treats it as an IMPLICIT finish: shows to user, turn ends.
    So unlabeled DSL still works; you only *need* `observe` when you want
    to see the result and continue.
  * On error (parse/check/compile/execute), you receive
    `<observation error="true">...</observation>` and should produce a
    corrected DSL on the next turn.

Available builtin tools:
{catalog}
Rules:
  1. Produce EXACTLY ONE fenced ```agnes block per turn. No prose outside.
  2. Prefer `finish` at the tail to make your intent explicit; unlabeled
     is allowed but observability suffers.
  3. Use `observe` when you need to see the result to decide the next step.
  4. Do not invent tools not in the catalog above; the checker will reject.

Examples (each is a complete turn):

```agnes
(pipe (tool read-file :path "notes.md") (tool summarize) finish)
```

```agnes
(pipe (tool read-file :path "log.txt") (tool summarize) observe)
```

```agnes
(pipe "task complete" finish)
```
"#)
}
```

Then confirm the `Planner::new` constructor stores `build_system_prompt(registry)` into `base_system`. If it already does, no change needed there.

- [ ] **Step 4: Restore the >6-turn prior-context summary in `effective_system`**

Task 6 stubbed `effective_system` to just return `base_system.clone()`. Add back the "collapse anything beyond the last 6 turns into a prefix line" logic, adapted for the new `Turn { user_nl, iterations, outcome }` shape.

In `crates/agnes-llm/src/planner.rs`, add near the top:

```rust
const MAX_TURNS_VERBATIM: usize = 6;
```

Rewrite `effective_system`:

```rust
fn effective_system(&self) -> String {
    let n = self.history.len();
    if n <= MAX_TURNS_VERBATIM {
        return self.base_system.clone();
    }
    let extras: &[Turn] = &self.history[..n - MAX_TURNS_VERBATIM];
    let mut prefix = String::from("<prior context:\n");
    for t in extras {
        let iters = t.iterations.len();
        let outcome = match &t.outcome {
            TurnOutcome::Finished { result } => {
                format!("finished ({} chars)", result.chars().count())
            }
            TurnOutcome::TurnLimitExceeded => "turn-limit-exceeded".to_string(),
        };
        prefix.push_str(&format!(
            "  - user asked {:?}: {iters} iteration(s), outcome: {outcome}\n",
            t.user_nl,
        ));
    }
    prefix.push_str(">\n\n");
    prefix.push_str(&self.base_system);
    prefix
}
```

Also modify `build_messages` (the version from Task 6): the loop over history should only include the last `MAX_TURNS_VERBATIM` entries, since older turns are already summarized in `effective_system`:

```rust
fn build_messages(&self) -> Vec<Message> {
    let mut out = Vec::new();
    let n = self.history.len();
    let start = n.saturating_sub(MAX_TURNS_VERBATIM);
    for turn in &self.history[start..] {
        out.push(Message {
            role: Role::User,
            content: turn.user_nl.clone(),
        });
        for it in &turn.iterations {
            out.push(Message {
                role: Role::Assistant,
                content: it.assistant_dsl.clone(),
            });
            if let Some(obs) = &it.observation {
                out.push(Message {
                    role: Role::User,
                    content: wrap_observation(obs),
                });
            }
        }
    }
    if let Some(inflight) = &self.inflight {
        out.push(Message {
            role: Role::User,
            content: inflight.user_nl.clone(),
        });
        for it in &inflight.iterations {
            out.push(Message {
                role: Role::Assistant,
                content: it.assistant_dsl.clone(),
            });
            if let Some(obs) = &it.observation {
                out.push(Message {
                    role: Role::User,
                    content: wrap_observation(obs),
                });
            }
        }
    }
    out
}
```

Also add a test in `crates/agnes-llm/tests/planner.rs`:

```rust
#[tokio::test]
async fn old_turns_beyond_six_collapse_into_prior_context() {
    // Build 8 responses so we can commit 7 turns before the 8th; only
    // the last 6 should appear verbatim; the first 1 should be in the
    // prior-context prefix.
    let responses: Vec<String> = (0..8)
        .map(|i| format!("```agnes\n(pipe \"turn{i}\" finish)\n```"))
        .collect();
    let (mut p, mock) = planner_with(responses);
    for i in 0..7 {
        p.begin_user_turn(format!("nl {i}"));
        let d = p.plan_next().await.unwrap();
        p.record_finish(d, format!("result {i}"));
    }
    // 8th turn — the system prompt at this call should include the prior-
    // context prefix for turn 0 only.
    p.begin_user_turn("nl 7".into());
    let _ = p.plan_next().await.unwrap();
    let seen = mock.seen();
    let sys8 = seen[7].system.clone().unwrap_or_default();
    assert!(
        sys8.contains("<prior context:"),
        "system prompt missing prior-context prefix on 8th turn: {sys8}"
    );
    assert!(sys8.contains("nl 0"), "prior context should reference turn 0");
    // But turn 1 (the second one) should NOT be summarized — it should
    // still be verbatim in messages.
    let msgs8 = &seen[7].messages;
    let has_nl1_user = msgs8
        .iter()
        .any(|m| m.content == "nl 1" && matches!(m.role, Role::User));
    assert!(has_nl1_user, "turn 1 should still be in messages verbatim");
}
```

- [ ] **Step 5: Verify observation XML wrapping matches assertions**

The `wrap_observation` helper from Task 6 already produces:
- `<observation type="Summary">\n{text}\n</observation>` when `type_name = Some(TypeName("Summary"))` and `!is_error`
- `<observation error="true">\n{text}\n</observation>` when `is_error`

Those match the test assertions. If your Task 6 implementation used slightly different formatting (e.g., attribute order or spacing), reconcile now — the tests are the contract.

- [ ] **Step 6: Run tests — expect PASS**

Run: `cargo test -p agnes-llm --test planner`
Expected: PASS (7/7 — the six from Step 1 plus the prior-context test from Step 4).

- [ ] **Step 7: Full agnes-llm crate green**

Run: `cargo test -p agnes-llm`
Expected: all agnes-llm tests PASS (planner + planner_state + provider_smoke + resolve + dsl_extract + anthropic_shape + openai_shape).

- [ ] **Step 8: Regression on the rest of the workspace**

Run: `cargo test --workspace`
Expected: **agnes-session still fails to compile** because it uses `planner.plan()` / `push_error_feedback` / `record_result`. Also `agnes-cli` fails to link because it depends on agnes-session. This is expected and gets fixed in Tasks 9-11.

If any OTHER crate broke unexpectedly, stop and diagnose.

- [ ] **Step 9: Commit**

```bash
jj describe -m "feat(llm): agent-loop system prompt + updated planner tests

System prompt explains Finish/Observation semantics, the loop protocol
(runtime feeds Observation back; Finish or unlabeled ends the turn),
error observation format, and lists all 9 builtin tools including the
two new finish/observe.

Old planner tests removed; new tests assert:
  - system prompt lists all 9 tools
  - Observation messages use <observation type=\"T\"> XML
  - Error observations use error=\"true\" and drop type=
  - Multi-iteration role sequences alternate (Anthropic API guard)
  - Committed history replays into the next turn's message chain

agnes-session and agnes-cli still fail to compile against the new
Planner API — those are Tasks 9-12.

Co-Authored-By: Claude <noreply@anthropic.com>"
jj new
```

---

### Task 8: `SessionEvent` becomes `#[non_exhaustive]` (standalone commit)

**Files:**
- Modify: `crates/agnes-session/src/events.rs`
- Modify: `crates/agnes-cli/src/sink_stderr.rs`
- Test: `crates/agnes-session/tests/event_non_exhaustive.rs`

**Interfaces:** No signature changes. Only an attribute on the enum + a `_ => {}` arm in the CLI's existing match.

**Why now, standalone:** Task 9 adds two new variants. If we bundle them into one commit, downstream code that matches `SessionEvent` fails to compile. Making the enum non-exhaustive up front, in its own commit, decouples the two concerns and keeps reviews small. This also flushes out any missed match sites in one focused change.

- [ ] **Step 1: Write the failing test**

Create `crates/agnes-session/tests/event_non_exhaustive.rs`:

```rust
//! Compile-time assertion: SessionEvent is #[non_exhaustive], meaning
//! external matches without a catchall arm will not compile. We can't
//! directly test that (it would need a proc-macro), so instead we
//! verify that a match with a catchall works AND that we can construct
//! variants normally.

use agnes_session::{SessionEvent, NodeKindTag};

#[test]
fn match_with_catchall_compiles_and_runs() {
    let ev = SessionEvent::TurnFailed { error: "x".into() };
    let s = match ev {
        SessionEvent::TurnFailed { error } => error,
        _ => "other".to_string(),
    };
    assert_eq!(s, "x");
}

#[test]
fn other_variants_still_constructible() {
    let _p = SessionEvent::PlannerStart;
    let _n = SessionEvent::NodeStart {
        id: 0,
        kind: NodeKindTag::Llm,
        args: vec![],
    };
    let _r = SessionEvent::TurnResult {
        value_preview: "".into(),
        value_type: "PlainText".into(),
    };
}
```

- [ ] **Step 2: Run test — expect PASS (matches with catchall already compile)**

Run: `cargo test -p agnes-session --test event_non_exhaustive`
Expected: PASS. This test is a sentinel: after this task's changes, it still passes; after Task 9 adds new variants, this test STILL passes because we've catchall'd. If someone later removes the attribute or the catchall breaks, this test will still pass (limitation of the language), but the OTHER match sites will fail to compile and the CI will catch it.

- [ ] **Step 3: Attribute the enum**

Edit `crates/agnes-session/src/events.rs`. Find `pub enum SessionEvent {` (line 10). Immediately above it, add:

```rust
#[non_exhaustive]
```

The block becomes:

```rust
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum SessionEvent {
    PlannerStart,
    // ... unchanged ...
}
```

- [ ] **Step 4: Add catchall to the CLI's stderr sink match**

Edit `crates/agnes-cli/src/sink_stderr.rs`. Find the `impl EventSink` `async fn emit` method — it contains a `match ev { ... }` covering every existing variant. At the end of the match, immediately before the closing `}`, add:

```rust
            _ => {
                // Future SessionEvent variants render nothing by default.
                // Task 12 will add specific handlers for IterationStart and
                // ObservationEmitted; anything else stays silent.
            }
```

Note: the match arms may or may not have `.await` at the end; look at the surrounding style and mimic it.

- [ ] **Step 5: Run workspace check**

Run: `cargo check --workspace`
Expected: still fails in agnes-session/src/session.rs (still calls old Planner API from Tasks 6/7) and downstream — that is Task 9's job. But NO NEW errors should have been introduced by this task.

Specifically, `cargo check -p agnes-cli` alone will also fail (transitively through agnes-session). What we can confirm in isolation:

Run: `cargo check -p agnes-session --lib`
Expected: the `events.rs` change compiles cleanly on its own. The `session.rs` file may still have Planner-API compile errors (from Task 6/7); those are unrelated to this task.

Run: `cargo test -p agnes-session --test event_non_exhaustive --lib`
Expected: FAILS at compile time due to session.rs's Planner-API issues. This is expected.

**How to gate this task:** the change itself is one line (`#[non_exhaustive]`) and a defensive catchall. If someone later reverts the enum, the match on the CLI side will fail loudly. Manually inspect: `git diff HEAD` should show exactly two hunks (one in events.rs, one in sink_stderr.rs) plus the new test file.

- [ ] **Step 6: Commit**

```bash
jj describe -m "chore(session): mark SessionEvent non_exhaustive; catchall in CLI sink

Prepares SessionEvent for two new variants (IterationStart,
ObservationEmitted) coming in Task 9. Adding #[non_exhaustive] as its
own commit means those variants can land without simultaneously breaking
every downstream match. The CLI's stderr sink acquires a _ => {} arm
which will be specialized in Task 12.

Co-Authored-By: Claude <noreply@anthropic.com>"
jj new
```

---

### Task 9: New `SessionEvent` variants + `classify_root` helper + `SessionError::TurnLimitExceeded`

**Files:**
- Modify: `crates/agnes-session/src/events.rs`
- Modify: `crates/agnes-session/src/session.rs` (add helpers only; do NOT rewrite `run_turn` yet)
- Modify: `crates/agnes-session/src/error.rs`
- Test: `crates/agnes-session/tests/classify_root.rs`

**Interfaces:**
- Consumes: `agnes_types::{TypeExpr, TypeName, Value}`.
- Produces:
  - `SessionEvent::IterationStart { iter: u32 }`.
  - `SessionEvent::ObservationEmitted { iter: u32, text: String, is_error: bool }`.
  - `pub enum RootKind { Finish, Observation, Other }` (new module-public type on Session, but for internal use — exported so tests can see it).
  - `pub fn classify_root(v: &Value) -> RootKind`.
  - `pub fn extract_inner_type(t: &TypeExpr) -> Option<TypeName>`.
  - `SessionError::TurnLimitExceeded { max_turns: u32 }`.
- Removes: `SessionError::RetriesExhausted { last: String }` (obsolete under the new loop; error observations feed back to planner, no explicit retry counter).

**Scope guard:** this task ONLY adds the pieces. `run_turn` is not rewritten. The workspace remains red end-to-end.

- [ ] **Step 1: Write the failing tests**

Create `crates/agnes-session/tests/classify_root.rs`:

```rust
use agnes_session::{RootKind, classify_root, extract_inner_type};
use agnes_types::{TypeExpr, TypeName, Value};
use serde_json::json;

fn v(t: TypeExpr) -> Value {
    Value {
        data: json!(null),
        declared_type: t,
    }
}

#[test]
fn plain_named_type_is_other() {
    assert!(matches!(
        classify_root(&v(TypeExpr::named("PlainText"))),
        RootKind::Other
    ));
}

#[test]
fn finish_wrapper_is_finish() {
    let t = TypeExpr::App {
        head: TypeName("Finish".into()),
        args: vec![TypeExpr::named("PlainText")],
    };
    assert!(matches!(classify_root(&v(t)), RootKind::Finish));
}

#[test]
fn observation_wrapper_is_observation() {
    let t = TypeExpr::App {
        head: TypeName("Observation".into()),
        args: vec![TypeExpr::named("Summary")],
    };
    assert!(matches!(classify_root(&v(t)), RootKind::Observation));
}

#[test]
fn list_of_plaintext_is_other() {
    let t = TypeExpr::App {
        head: TypeName("List".into()),
        args: vec![TypeExpr::named("PlainText")],
    };
    assert!(matches!(classify_root(&v(t)), RootKind::Other));
}

#[test]
fn union_type_is_other() {
    // (| PlainText Markdown)
    let t = agnes_types::canonicalize_union([
        TypeExpr::named("PlainText"),
        TypeExpr::named("Markdown"),
    ]);
    assert!(matches!(classify_root(&v(t)), RootKind::Other));
}

#[test]
fn extract_inner_from_finish_returns_the_inner_name() {
    let t = TypeExpr::App {
        head: TypeName("Finish".into()),
        args: vec![TypeExpr::named("Summary")],
    };
    assert_eq!(extract_inner_type(&t), Some(TypeName("Summary".into())));
}

#[test]
fn extract_inner_from_observation_of_list_returns_list_name() {
    // extract_inner_type only unwraps the OUTER Finish/Observation; the
    // inner type is whatever comes next. For an App head like List, we
    // return the head name ("List"), because that's what the XML attribute
    // needs — a stringy label the LLM can key off of.
    let t = TypeExpr::App {
        head: TypeName("Observation".into()),
        args: vec![TypeExpr::App {
            head: TypeName("List".into()),
            args: vec![TypeExpr::named("PlainText")],
        }],
    };
    assert_eq!(extract_inner_type(&t), Some(TypeName("List".into())));
}

#[test]
fn extract_inner_from_named_finish_of_plaintext() {
    let t = TypeExpr::App {
        head: TypeName("Finish".into()),
        args: vec![TypeExpr::named("PlainText")],
    };
    assert_eq!(extract_inner_type(&t), Some(TypeName("PlainText".into())));
}

#[test]
fn extract_inner_from_non_wrapper_is_none() {
    assert!(extract_inner_type(&TypeExpr::named("PlainText")).is_none());
    let t = TypeExpr::App {
        head: TypeName("List".into()),
        args: vec![TypeExpr::named("PlainText")],
    };
    assert!(extract_inner_type(&t).is_none());
}
```

- [ ] **Step 2: Run test — expect FAIL**

Run: `cargo test -p agnes-session --test classify_root`
Expected: FAIL with `unresolved import` for `RootKind`, `classify_root`, `extract_inner_type`.

- [ ] **Step 3: Add new `SessionEvent` variants**

Edit `crates/agnes-session/src/events.rs`. Inside the existing `pub enum SessionEvent { ... }` block, at the END (after `WriteSummary`), add:

```rust
    /// Emitted at the start of each planner↔runtime iteration in a turn.
    /// `iter` is 0-indexed.
    IterationStart { iter: u32 },

    /// Emitted when the current iteration's result is fed back to the
    /// planner as an observation (i.e. runtime returned Observation _
    /// or errored). `is_error=true` means the runtime threw a
    /// parse/check/compile/execute error rather than emitting a value.
    ObservationEmitted {
        iter: u32,
        text: String,
        is_error: bool,
    },
```

- [ ] **Step 4: Extend `SessionError`**

Edit `crates/agnes-session/src/error.rs`. Replace `RetriesExhausted { last: String }` with:

```rust
    #[error(
        "Agent loop hit the iteration limit.\n  Why: `MAX_TURNS = {max_turns}` reached without a terminating iteration (finish or unlabeled result).\n  Fix: rephrase the request more narrowly, or pass `--max-turns <N>` to raise the ceiling."
    )]
    TurnLimitExceeded { max_turns: u32 },
```

- [ ] **Step 5: Add helpers `RootKind`, `classify_root`, `extract_inner_type`**

Edit `crates/agnes-session/src/session.rs`. Somewhere near the top of the file (after imports, before the `impl Session` block), add:

```rust
/// Which "root shape" a Value carries — the classification used by the
/// agent loop to decide whether to terminate (Finish/Other) or feed
/// back to the planner (Observation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootKind {
    Finish,
    Observation,
    Other,
}

/// Read `value.declared_type`'s outermost head; classify accordingly.
pub fn classify_root(value: &agnes_types::Value) -> RootKind {
    use agnes_types::TypeExpr;
    match &value.declared_type {
        TypeExpr::App { head, args } if args.len() == 1 => match head.0.as_str() {
            "Finish" => RootKind::Finish,
            "Observation" => RootKind::Observation,
            _ => RootKind::Other,
        },
        _ => RootKind::Other,
    }
}

/// For a Finish/Observation wrapper type, return the outermost name of
/// the inner type (for use as the `type="..."` attribute in observation
/// XML). Returns `None` for non-wrapper types.
pub fn extract_inner_type(t: &agnes_types::TypeExpr) -> Option<agnes_types::TypeName> {
    use agnes_types::TypeExpr;
    match t {
        TypeExpr::App { head, args } if args.len() == 1 => match head.0.as_str() {
            "Finish" | "Observation" => Some(match &args[0] {
                TypeExpr::Named(n) => n.clone(),
                TypeExpr::App { head: inner_head, .. } => inner_head.clone(),
            }),
            _ => None,
        },
        _ => None,
    }
}
```

- [ ] **Step 6: Re-export from `agnes-session/src/lib.rs`**

Edit `crates/agnes-session/src/lib.rs`. Find the existing `pub use session::...;` line and extend to include the new symbols:

```rust
pub use session::{RootKind, Session, TurnInput, classify_root, extract_inner_type};
```

Keep other re-exports (`SessionEvent`, `SessionError`, `EventSink`, etc.) unchanged.

- [ ] **Step 7: Run test — expect PASS**

Run: `cargo test -p agnes-session --test classify_root`
Expected: PASS (9/9).

**Note:** `cargo test -p agnes-session` as a whole will still fail because `session.rs::run_turn` uses the old Planner API. The `--test classify_root` scoping keeps the failing tests out of view.

- [ ] **Step 8: Old session tests are still expected to fail**

Run: `cargo test -p agnes-session`
Expected: `tests/session_end_to_end.rs` compile errors due to old Planner API in `session.rs`. Task 10 fixes it.

- [ ] **Step 9: Commit**

```bash
jj describe -m "feat(session): IterationStart, ObservationEmitted, classify_root helpers

Add two new SessionEvent variants (safe under #[non_exhaustive] from
Task 8). Add classify_root(&Value) -> RootKind{Finish,Observation,Other}
and extract_inner_type(&TypeExpr) -> Option<TypeName>, the primitives
Task 10 uses inside run_turn's new loop.

Replace SessionError::RetriesExhausted with TurnLimitExceeded{max_turns}
— the new loop feeds errors back to the planner via ObservationEmitted
so there's no separate retry counter; only MAX_TURNS bounds the loop.

Co-Authored-By: Claude <noreply@anthropic.com>"
jj new
```

---

### Task 10: `Session::run_turn` — the agent loop

**Files:**
- Modify: `crates/agnes-session/src/session.rs`
- Modify: `crates/agnes-session/tests/session_end_to_end.rs` (existing tests need updating for the new loop; and new tests get added)

**Interfaces:**
- Public API of `Session` unchanged. `run_turn(&mut self, TurnInput, &mut dyn EventSink) -> Result<Value, SessionError>` keeps its signature.
- Session gains one config field: `max_turns: u32` (default 20). Constructor stays as `new(provider: Arc<dyn Provider>) -> Result<Self, SessionError>`, but a `new_with_max_turns(provider, max_turns)` is added for the CLI.
- Constants: `pub const DEFAULT_MAX_TURNS: u32 = 20;`, `pub const OBSERVATION_TRUNCATION_THRESHOLD: usize = 8000;`.

**The loop:** for each iteration up to `max_turns`, emit `IterationStart`, produce a DSL (either from `RawDsl` at iter=0 or from `Planner::plan_next`), execute it, classify the resulting Value:
- `RootKind::Observation` → wrap and feed back via `push_observation`, emit `ObservationEmitted`, continue.
- `RootKind::Finish` or `RootKind::Other` → emit `TurnResult`, `record_finish`, return.
- Any error from parse/check/compile/execute → wrap error text as observation, emit `ObservationEmitted { is_error: true }`, `push_observation(..., is_error=true, None)`, continue.

**MAX_TURNS reached** → drain writes, emit `TurnFailed`, call `planner.abandon_pending_turn()`, return `SessionError::TurnLimitExceeded { max_turns }`.

**Removes:** `plan_with_retries` / `MAX_PLAN_RETRIES` / `dry_run` helper. Errors now feed back through the same loop as observations; there is no separate 3-retry counter.

- [ ] **Step 1: Write the failing tests**

Replace `crates/agnes-session/tests/session_end_to_end.rs` with the following. The file already exists (from the previous plan); we're keeping the tests that still make sense and updating/adding for the new loop.

```rust
//! Session integration tests: exercise the multi-iteration agent loop
//! against MockProvider. No real network.

use agnes_llm::{MockProvider, Provider};
use agnes_session::{
    EventSink, Session, SessionError, SessionEvent, TurnInput,
};
use async_trait::async_trait;
use std::sync::{Arc, Mutex};

/// Serialize integration tests that share the process-global writes()
/// recorder in agnes-builtins.
fn test_lock() -> &'static std::sync::Mutex<()> {
    static M: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    M.get_or_init(|| std::sync::Mutex::new(()))
}

#[derive(Default)]
struct RecordingSink(Arc<Mutex<Vec<SessionEvent>>>);

impl RecordingSink {
    fn events(&self) -> Vec<SessionEvent> {
        self.0.lock().unwrap().clone()
    }
    fn shared(&self) -> Arc<Mutex<Vec<SessionEvent>>> {
        Arc::clone(&self.0)
    }
}

#[async_trait]
impl EventSink for RecordingSink {
    async fn emit(&mut self, ev: SessionEvent) {
        self.0.lock().unwrap().push(ev);
    }
}

fn provider(responses: Vec<&str>) -> Arc<dyn Provider> {
    Arc::new(MockProvider::new(responses.into_iter().map(String::from).collect()))
}

#[tokio::test]
async fn single_iteration_with_explicit_finish() {
    let _g = test_lock().lock().unwrap();
    let mut s = Session::new(provider(vec!["```agnes\n(pipe \"done\" finish)\n```"])).unwrap();
    let mut sink = RecordingSink::default();
    let v = s
        .run_turn(TurnInput::NaturalLanguage("hi".into()), &mut sink)
        .await
        .unwrap();
    assert_eq!(v.data.as_str(), Some("done"));
    let evs = sink.events();
    let has_iter_0 = evs
        .iter()
        .any(|e| matches!(e, SessionEvent::IterationStart { iter: 0 }));
    let has_turn_result = evs
        .iter()
        .any(|e| matches!(e, SessionEvent::TurnResult { .. }));
    assert!(has_iter_0);
    assert!(has_turn_result);
}

#[tokio::test]
async fn unlabeled_result_is_implicit_finish() {
    let _g = test_lock().lock().unwrap();
    // No finish or observe. Result is PlainText; Session treats as implicit finish.
    let mut s = Session::new(provider(vec!["```agnes\n\"hello\"\n```"])).unwrap();
    let mut sink = RecordingSink::default();
    let v = s
        .run_turn(TurnInput::NaturalLanguage("say hi".into()), &mut sink)
        .await
        .unwrap();
    assert_eq!(v.data.as_str(), Some("hello"));
    let evs = sink.events();
    // Only one iteration.
    let iter_starts = evs
        .iter()
        .filter(|e| matches!(e, SessionEvent::IterationStart { .. }))
        .count();
    assert_eq!(iter_starts, 1);
}

#[tokio::test]
async fn observation_feeds_back_and_second_iteration_finishes() {
    let _g = test_lock().lock().unwrap();
    let mut s = Session::new(provider(vec![
        "```agnes\n(pipe \"first\" observe)\n```",
        "```agnes\n(pipe \"final\" finish)\n```",
    ]))
    .unwrap();
    let mut sink = RecordingSink::default();
    let v = s
        .run_turn(TurnInput::NaturalLanguage("go".into()), &mut sink)
        .await
        .unwrap();
    assert_eq!(v.data.as_str(), Some("final"));
    let evs = sink.events();
    // Two IterationStart events: iter=0 and iter=1.
    assert!(
        evs.iter()
            .any(|e| matches!(e, SessionEvent::IterationStart { iter: 0 }))
    );
    assert!(
        evs.iter()
            .any(|e| matches!(e, SessionEvent::IterationStart { iter: 1 }))
    );
    // ObservationEmitted with is_error=false, iter=0.
    let has_obs = evs.iter().any(|e| {
        matches!(
            e,
            SessionEvent::ObservationEmitted {
                iter: 0,
                is_error: false,
                text
            } if text == "first"
        )
    });
    assert!(has_obs, "expected ObservationEmitted iter=0 text=first, got {evs:?}");
}

#[tokio::test]
async fn parse_error_feeds_back_and_self_heals() {
    let _g = test_lock().lock().unwrap();
    let mut s = Session::new(provider(vec![
        "```agnes\n((this is not valid\n```",
        "```agnes\n(pipe \"recovered\" finish)\n```",
    ]))
    .unwrap();
    let mut sink = RecordingSink::default();
    let v = s
        .run_turn(TurnInput::NaturalLanguage("go".into()), &mut sink)
        .await
        .unwrap();
    assert_eq!(v.data.as_str(), Some("recovered"));
    let evs = sink.events();
    let has_err_obs = evs.iter().any(|e| {
        matches!(
            e,
            SessionEvent::ObservationEmitted {
                iter: 0,
                is_error: true,
                ..
            }
        )
    });
    assert!(
        has_err_obs,
        "expected an error observation for iter 0, got {evs:?}"
    );
}

#[tokio::test]
async fn raw_dsl_seeds_iteration_zero_and_continues_on_observation() {
    let _g = test_lock().lock().unwrap();
    // /run (pipe "seed" observe) → iter 0 uses the raw DSL, produces
    // Observation, feeds back; planner fills iter 1.
    let mut s = Session::new(provider(vec![
        "```agnes\n(pipe \"planned\" finish)\n```",
    ]))
    .unwrap();
    let mut sink = RecordingSink::default();
    let v = s
        .run_turn(
            TurnInput::RawDsl("(pipe \"seed\" observe)".into()),
            &mut sink,
        )
        .await
        .unwrap();
    assert_eq!(v.data.as_str(), Some("planned"));
    // Two iterations: iter 0 (raw) and iter 1 (planner-produced).
    let iter_count = sink
        .events()
        .iter()
        .filter(|e| matches!(e, SessionEvent::IterationStart { .. }))
        .count();
    assert_eq!(iter_count, 2);
}

#[tokio::test]
async fn raw_dsl_that_finishes_directly_stops_after_one_iteration() {
    let _g = test_lock().lock().unwrap();
    // /run (pipe "just this" finish) — should terminate in one iteration,
    // planner is never consulted (empty response queue is fine).
    let mut s = Session::new(provider(vec![])).unwrap();
    let mut sink = RecordingSink::default();
    let v = s
        .run_turn(
            TurnInput::RawDsl("(pipe \"just this\" finish)".into()),
            &mut sink,
        )
        .await
        .unwrap();
    assert_eq!(v.data.as_str(), Some("just this"));
    let iter_count = sink
        .events()
        .iter()
        .filter(|e| matches!(e, SessionEvent::IterationStart { .. }))
        .count();
    assert_eq!(iter_count, 1);
}

#[tokio::test]
async fn max_turns_ceiling_terminates_with_turn_limit_exceeded() {
    let _g = test_lock().lock().unwrap();
    // Planner always returns observe → never terminates on its own.
    // Set max_turns=3 and expect TurnLimitExceeded.
    let responses: Vec<String> = (0..10)
        .map(|i| format!("```agnes\n(pipe \"iter {i}\" observe)\n```"))
        .collect();
    let mut s = Session::new_with_max_turns(
        Arc::new(MockProvider::new(responses.clone())),
        3,
    )
    .unwrap();
    let mut sink = RecordingSink::default();
    let err = s
        .run_turn(TurnInput::NaturalLanguage("go".into()), &mut sink)
        .await
        .expect_err("must exceed limit");
    match err {
        SessionError::TurnLimitExceeded { max_turns } => assert_eq!(max_turns, 3),
        other => panic!("expected TurnLimitExceeded, got {other:?}"),
    }
    // Exactly 3 IterationStart events fired.
    let iter_count = sink
        .events()
        .iter()
        .filter(|e| matches!(e, SessionEvent::IterationStart { .. }))
        .count();
    assert_eq!(iter_count, 3);
    // TurnFailed was emitted before returning Err.
    let has_failed = sink
        .events()
        .iter()
        .any(|e| matches!(e, SessionEvent::TurnFailed { .. }));
    assert!(has_failed);
}

#[tokio::test]
async fn write_summary_still_emitted_before_turn_result() {
    let _g = test_lock().lock().unwrap();
    // Runs a write-file then finishes; existing WriteSummary contract holds.
    let mut s = Session::new(provider(vec![
        "```agnes\n(pipe (tool write-file :path \"/tmp/x\" :content \"hi\") finish)\n```",
    ]))
    .unwrap();
    let mut sink = RecordingSink::default();
    let _ = s
        .run_turn(TurnInput::NaturalLanguage("write it".into()), &mut sink)
        .await
        .unwrap();
    let evs = sink.events();
    let pos_write = evs
        .iter()
        .position(|e| matches!(e, SessionEvent::WriteSummary { .. }));
    let pos_result = evs
        .iter()
        .position(|e| matches!(e, SessionEvent::TurnResult { .. }));
    let (pw, pr) = (
        pos_write.expect("WriteSummary emitted"),
        pos_result.expect("TurnResult emitted"),
    );
    assert!(pw < pr, "WriteSummary must precede TurnResult");
}
```

- [ ] **Step 2: Run tests — expect FAIL**

Run: `cargo test -p agnes-session`
Expected: multiple compile errors (old `run_turn` still calls the old Planner API), and once we compile them, various logic assertions won't hold.

- [ ] **Step 3: Rewrite `Session` and `run_turn`**

Open `crates/agnes-session/src/session.rs`. Replace the entire file with:

```rust
use crate::error::SessionError;
use crate::events::{EventSink, SessionEvent};
use crate::plan_tree::build_plan_tree;
use crate::tracer_bridge::{ChannelTracer, drain};
use agnes_builtins::{ToolImpl, native_dispatch, register_builtins};
use agnes_llm::{Planner, Provider, Turn};
use agnes_registry::Registry;
use agnes_runtime::execute_with;
use agnes_types::{Value, TypeExpr};
use std::collections::HashMap;
use std::sync::Arc;

pub enum TurnInput {
    NaturalLanguage(String),
    RawDsl(String),
}

/// Default upper bound on iterations per user turn. Rationale: Claude
/// Code / LangGraph plan-and-execute defaults are 20-25. Each iteration
/// can hold a full pipe/par expression so the effective tool-call
/// budget is much higher.
pub const DEFAULT_MAX_TURNS: u32 = 20;

/// Observation text longer than this is truncated (middle-cut) before
/// being fed back to the planner. Rationale: 2000-4000 tokens depending
/// on language, matching Anthropic's tool_result guideline.
pub const OBSERVATION_TRUNCATION_THRESHOLD: usize = 8000;

/// Which "root shape" a Value carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootKind {
    Finish,
    Observation,
    Other,
}

pub fn classify_root(value: &Value) -> RootKind {
    match &value.declared_type {
        TypeExpr::App { head, args } if args.len() == 1 => match head.0.as_str() {
            "Finish" => RootKind::Finish,
            "Observation" => RootKind::Observation,
            _ => RootKind::Other,
        },
        _ => RootKind::Other,
    }
}

pub fn extract_inner_type(t: &TypeExpr) -> Option<agnes_types::TypeName> {
    match t {
        TypeExpr::App { head, args } if args.len() == 1 => match head.0.as_str() {
            "Finish" | "Observation" => Some(match &args[0] {
                TypeExpr::Named(n) => n.clone(),
                TypeExpr::App { head: inner_head, .. } => inner_head.clone(),
            }),
            _ => None,
        },
        _ => None,
    }
}

pub struct Session {
    dispatch: HashMap<String, ToolImpl>,
    planner: Planner,
    max_turns: u32,
}

impl Session {
    pub fn new(provider: Arc<dyn Provider>) -> Result<Self, SessionError> {
        Self::new_with_max_turns(provider, DEFAULT_MAX_TURNS)
    }

    pub fn new_with_max_turns(
        provider: Arc<dyn Provider>,
        max_turns: u32,
    ) -> Result<Self, SessionError> {
        let mut registry = Registry::new();
        register_builtins(&mut registry).map_err(|e| SessionError::Check(e.to_string()))?;
        let dispatch = native_dispatch(provider.clone());
        let planner = Planner::new(provider, &registry);
        Ok(Self {
            dispatch,
            planner,
            max_turns,
        })
    }

    pub fn history(&self) -> &[Turn] {
        self.planner.history()
    }

    pub fn reset_history(&mut self) {
        self.planner.reset_history();
    }

    fn drain_writes() -> Vec<(String, usize)> {
        let mut w = agnes_builtins::writes()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut *w)
    }

    /// Truncate an observation string to `OBSERVATION_TRUNCATION_THRESHOLD`
    /// characters using a middle-cut, keeping the first and last quarters.
    fn truncate_observation(text: String) -> String {
        if text.chars().count() <= OBSERVATION_TRUNCATION_THRESHOLD {
            return text;
        }
        let total_chars = text.chars().count();
        let keep = OBSERVATION_TRUNCATION_THRESHOLD / 2;
        let first: String = text.chars().take(keep).collect();
        let last: String = text.chars().rev().take(keep).collect::<String>().chars().rev().collect();
        let dropped = total_chars - 2 * keep;
        format!("{first}\n\n... [truncated {dropped} chars — full length: {total_chars}] ...\n\n{last}")
    }

    pub async fn run_turn(
        &mut self,
        input: TurnInput,
        sink: &mut dyn EventSink,
    ) -> Result<Value, SessionError> {
        match self.run_turn_inner(input, sink).await {
            Ok(v) => Ok(v),
            Err(e) => {
                let recorded = Self::drain_writes();
                if !recorded.is_empty() {
                    sink.emit(SessionEvent::WriteSummary { entries: recorded })
                        .await;
                }
                sink.emit(SessionEvent::TurnFailed {
                    error: e.to_string(),
                })
                .await;
                Err(e)
            }
        }
    }

    async fn run_turn_inner(
        &mut self,
        input: TurnInput,
        sink: &mut dyn EventSink,
    ) -> Result<Value, SessionError> {
        // Seed: NL starts an in-flight planner turn; RawDsl provides
        // iter=0's DSL directly and still opens a planner turn (so
        // history is coherent for future turns).
        let (user_nl, mut seeded_dsl) = match input {
            TurnInput::NaturalLanguage(nl) => (nl, None),
            TurnInput::RawDsl(s) => (format!("/run {s}"), Some(s)),
        };
        self.planner.begin_user_turn(user_nl);

        for iter in 0..self.max_turns {
            sink.emit(SessionEvent::IterationStart { iter }).await;

            // Get the DSL for this iteration: either the seeded RawDsl
            // (iter 0 only) or a fresh planner call.
            let dsl = match seeded_dsl.take() {
                Some(s) => {
                    // We didn't go through plan_next, but the Planner still
                    // needs to know about this assistant turn. Feed it in
                    // synthetically: append an iteration whose assistant_dsl
                    // is the raw source. push_observation / record_finish
                    // in the branches below will operate on this iteration.
                    // We use plan_next-equivalent state by appending directly.
                    // Since planner doesn't expose that, we call a helper
                    // method — see the note in step 4 for the addition.
                    self.planner.inject_assistant_dsl(s.clone());
                    s
                }
                None => {
                    sink.emit(SessionEvent::PlannerStart).await;
                    self.planner.plan_next().await?
                }
            };
            sink.emit(SessionEvent::DslProduced {
                source: dsl.clone(),
            })
            .await;

            // Try to execute this iteration.
            let result = self.try_execute(&dsl, sink).await;

            match result {
                Ok(value) => {
                    match classify_root(&value) {
                        RootKind::Observation => {
                            let inner_type = extract_inner_type(&value.declared_type);
                            // Get a template registry to serialize with — the
                            // per-turn registry created inside try_execute has
                            // been dropped; rebuild a fresh one for show only.
                            let mut show_reg = Registry::new();
                            register_builtins(&mut show_reg).ok();
                            let raw = show_reg.show_value(&value);
                            let text = Self::truncate_observation(raw);
                            sink.emit(SessionEvent::ObservationEmitted {
                                iter,
                                text: text.clone(),
                                is_error: false,
                            })
                            .await;
                            self.planner.push_observation(
                                dsl.clone(),
                                text,
                                false,
                                inner_type,
                            );
                            // Loop continues.
                        }
                        RootKind::Finish | RootKind::Other => {
                            let mut show_reg = Registry::new();
                            register_builtins(&mut show_reg).ok();
                            let s = show_reg.show_value(&value);
                            let recorded = Self::drain_writes();
                            if !recorded.is_empty() {
                                sink.emit(SessionEvent::WriteSummary { entries: recorded })
                                    .await;
                            }
                            sink.emit(SessionEvent::TurnResult {
                                value_preview: s.clone(),
                                value_type: value.declared_type.to_string(),
                            })
                            .await;
                            self.planner.record_finish(dsl, s);
                            return Ok(value);
                        }
                    }
                }
                Err(e) => {
                    let text = e.to_string();
                    sink.emit(SessionEvent::ObservationEmitted {
                        iter,
                        text: text.clone(),
                        is_error: true,
                    })
                    .await;
                    self.planner.push_observation(dsl, text, true, None);
                    // Loop continues; do NOT drain writes here — a failed
                    // iteration should not leak writes into the next.
                    let _ = Self::drain_writes();
                }
            }
        }

        // Loop fell through — MAX_TURNS reached.
        self.planner.abandon_pending_turn();
        Err(SessionError::TurnLimitExceeded {
            max_turns: self.max_turns,
        })
    }

    /// One iteration: parse/check/compile/execute a DSL. Emits DslProduced
    /// (already emitted by caller), PlanReady, NodeStart/NodeEnd via tracer.
    async fn try_execute(
        &mut self,
        dsl: &str,
        sink: &mut dyn EventSink,
    ) -> Result<Value, SessionError> {
        let program =
            agnes_parser::parse(dsl).map_err(|e| SessionError::Parse(e.to_string()))?;
        let mut turn_registry = Registry::new();
        register_builtins(&mut turn_registry).map_err(|e| SessionError::Check(e.to_string()))?;
        turn_registry
            .load(&program)
            .map_err(|e| SessionError::Check(e.to_string()))?;
        agnes_checker::check(&program, &turn_registry)
            .map_err(|e| SessionError::Check(e.to_string()))?;
        let dag = agnes_compiler::compile(&program, &turn_registry)
            .map_err(|e| SessionError::Compile(e.to_string()))?;
        sink.emit(SessionEvent::PlanReady {
            tree: build_plan_tree(&dag),
        })
        .await;
        let (tracer, mut rx) = ChannelTracer::new();
        let exec = execute_with(&dag, &turn_registry, &self.dispatch, &tracer);
        tokio::pin!(exec);
        let result = loop {
            tokio::select! {
                r = &mut exec => break r,
                _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {
                    drain(&mut rx, sink).await;
                }
            }
        };
        drain(&mut rx, sink).await;
        Ok(result?)
    }
}
```

- [ ] **Step 4: Add `Planner::inject_assistant_dsl` helper**

The Session-side loop needs to record the raw DSL as an "iteration" without going through `plan_next` (which would call the LLM). Add a new public method on `Planner` in `crates/agnes-llm/src/planner.rs`:

```rust
    /// Inject a pre-computed assistant DSL (from RawDsl input) into the
    /// in-flight turn as if `plan_next` had produced it. Does not call
    /// the provider. Behaves identically to `plan_next` from the caller's
    /// perspective: the next `push_observation` / `record_finish` will
    /// attach to this synthetic iteration.
    pub fn inject_assistant_dsl(&mut self, dsl: String) {
        let inflight = self
            .inflight
            .as_mut()
            .expect("inject_assistant_dsl with no in-flight turn");
        inflight.iterations.push(Iteration {
            assistant_dsl: dsl,
            observation: None,
        });
    }
```

Also re-export nothing new — it's a `pub` method on the already-exported `Planner`. Confirm the existing `use` in `session.rs` is `use agnes_llm::{Planner, Provider, Turn};` (no extra symbols needed).

- [ ] **Step 5: Run tests — expect PASS**

Run: `cargo test -p agnes-session`
Expected: PASS. Both existing tests (adjusted) and the new ones. If any old test from `tests/session_end_to_end.rs` had semantics that don't fit the new loop (e.g., expected `RetriesExhausted`), rewrite it in this step to match the new behavior.

- [ ] **Step 6: Full workspace**

Run: `cargo test --workspace`
Expected: agnes-cli still broken because sink_stderr and REPL glue haven't been updated for the new events — Task 12 fixes those. Everything else (types, registry, builtins, llm, session) should pass.

Specifically:
- `cargo test -p agnes-types -p agnes-registry -p agnes-builtins -p agnes-llm -p agnes-session` should PASS.
- `cargo test -p agnes-cli` will FAIL — expected.

- [ ] **Step 7: Commit**

```bash
jj describe -m "feat(session): agent loop — iterations until Finish/Other/limit

run_turn now runs a bounded iteration loop:
  IterationStart -> plan_next (or seeded RawDsl) -> DslProduced -> execute
  -> classify_root:
       Observation  -> ObservationEmitted, push_observation, continue
       Finish/Other -> TurnResult, record_finish, return
  Errors -> ObservationEmitted{is_error=true}, push_observation, continue

MAX_TURNS default 20, overridable via new_with_max_turns. Truncation
threshold 8000 chars, mid-cut. RawDsl seeds iter 0 via new
Planner::inject_assistant_dsl helper, then joins the same loop.

Removes: plan_with_retries, MAX_PLAN_RETRIES, dry_run. The old 3-retry
counter is subsumed by MAX_TURNS since errors now feed back through
observations.

agnes-cli still fails to compile against the new events; Task 12
handles it.

Co-Authored-By: Claude <noreply@anthropic.com>"
jj new
```

---

### Task 11: Ctrl-C cancellation for `Session::run_turn`

**Files:**
- Modify: `crates/agnes-session/src/session.rs`
- Modify: `crates/agnes-session/src/error.rs`
- Test: `crates/agnes-session/tests/cancel.rs`

**Interfaces:**
- Session gains one method: `pub async fn run_turn_cancellable(&mut self, input: TurnInput, sink: &mut dyn EventSink, cancel: Arc<tokio::sync::Notify>) -> Result<Value, SessionError>`.
- `run_turn(input, sink)` remains a convenience wrapper: `run_turn_cancellable(input, sink, Arc::new(Notify::new()))`.
- `SessionError::Cancelled { after_iterations: u32 }` (new variant).

**Semantics:** the cancel token is awaited at the top of each iteration. If it fires between iterations, we drain writes, `abandon_pending_turn`, emit `TurnFailed { error: "cancelled after N iterations" }`, and return `SessionError::Cancelled`.

Task 12 will wire the CLI's Ctrl-C handler to fire this notify.

- [ ] **Step 1: Write the failing test**

Create `crates/agnes-session/tests/cancel.rs`:

```rust
use agnes_llm::{MockProvider, Provider};
use agnes_session::{EventSink, Session, SessionError, SessionEvent, TurnInput};
use async_trait::async_trait;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

fn test_lock() -> &'static std::sync::Mutex<()> {
    static M: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    M.get_or_init(|| std::sync::Mutex::new(()))
}

#[derive(Default)]
struct Recording(Arc<Mutex<Vec<SessionEvent>>>);
#[async_trait]
impl EventSink for Recording {
    async fn emit(&mut self, ev: SessionEvent) {
        self.0.lock().unwrap().push(ev);
    }
}

#[tokio::test]
async fn cancel_before_first_iteration_returns_cancelled_with_zero() {
    let _g = test_lock().lock().unwrap();
    let provider: Arc<dyn Provider> = Arc::new(MockProvider::new(vec![]));
    let mut s = Session::new(provider).unwrap();
    let mut sink = Recording::default();
    let cancel = Arc::new(Notify::new());
    // Pre-notify: the loop should see it on the very first check.
    cancel.notify_one();
    let err = s
        .run_turn_cancellable(
            TurnInput::NaturalLanguage("go".into()),
            &mut sink,
            cancel,
        )
        .await
        .expect_err("expected cancelled");
    match err {
        SessionError::Cancelled { after_iterations } => assert_eq!(after_iterations, 0),
        other => panic!("expected Cancelled, got {other:?}"),
    }
}

#[tokio::test]
async fn cancel_between_iterations_stops_after_current_iteration() {
    let _g = test_lock().lock().unwrap();
    // Provider always says observe (loop wants to continue). We fire the
    // cancel after the first ObservationEmitted arrives.
    let responses: Vec<String> = (0..10)
        .map(|i| format!("```agnes\n(pipe \"iter {i}\" observe)\n```"))
        .collect();
    let provider: Arc<dyn Provider> = Arc::new(MockProvider::new(responses));
    let mut s = Session::new(provider).unwrap();

    let ev_log = Arc::new(Mutex::new(Vec::new()));
    struct Sink(Arc<Mutex<Vec<SessionEvent>>>, Arc<Notify>);
    #[async_trait]
    impl EventSink for Sink {
        async fn emit(&mut self, ev: SessionEvent) {
            let is_obs = matches!(ev, SessionEvent::ObservationEmitted { .. });
            self.0.lock().unwrap().push(ev);
            // Fire cancel immediately after the FIRST observation.
            if is_obs && self.0.lock().unwrap().iter().filter(|e| matches!(e, SessionEvent::ObservationEmitted {..})).count() == 1 {
                self.1.notify_one();
            }
        }
    }

    let cancel = Arc::new(Notify::new());
    let mut sink = Sink(ev_log.clone(), cancel.clone());
    let err = s
        .run_turn_cancellable(
            TurnInput::NaturalLanguage("go".into()),
            &mut sink,
            cancel,
        )
        .await
        .expect_err("expected cancelled");
    match err {
        SessionError::Cancelled { after_iterations } => {
            // Exactly one iteration ran (iter 0), so after_iterations = 1.
            assert_eq!(after_iterations, 1);
        }
        other => panic!("expected Cancelled, got {other:?}"),
    }
    let evs = ev_log.lock().unwrap();
    let has_failed = evs
        .iter()
        .any(|e| matches!(e, SessionEvent::TurnFailed { .. }));
    assert!(has_failed);
}
```

- [ ] **Step 2: Run test — expect FAIL**

Run: `cargo test -p agnes-session --test cancel`
Expected: FAIL — `run_turn_cancellable` and `SessionError::Cancelled` don't exist yet.

- [ ] **Step 3: Add `SessionError::Cancelled`**

Edit `crates/agnes-session/src/error.rs`. Append inside the `pub enum SessionError`:

```rust
    #[error(
        "Turn cancelled after {after_iterations} iteration(s).\n  Why: user pressed Ctrl-C while the agent was mid-loop.\n  Fix: re-issue the request, or press Ctrl-D to leave the REPL entirely."
    )]
    Cancelled { after_iterations: u32 },
```

- [ ] **Step 4: Add `run_turn_cancellable` and refactor `run_turn`**

Edit `crates/agnes-session/src/session.rs`. Change `run_turn` into a wrapper and add the real implementation:

```rust
    pub async fn run_turn(
        &mut self,
        input: TurnInput,
        sink: &mut dyn EventSink,
    ) -> Result<Value, SessionError> {
        // Uncancellable variant: a fresh Notify never fires.
        let never = Arc::new(tokio::sync::Notify::new());
        self.run_turn_cancellable(input, sink, never).await
    }

    pub async fn run_turn_cancellable(
        &mut self,
        input: TurnInput,
        sink: &mut dyn EventSink,
        cancel: Arc<tokio::sync::Notify>,
    ) -> Result<Value, SessionError> {
        match self.run_turn_inner(input, sink, cancel).await {
            Ok(v) => Ok(v),
            Err(e) => {
                let recorded = Self::drain_writes();
                if !recorded.is_empty() {
                    sink.emit(SessionEvent::WriteSummary { entries: recorded })
                        .await;
                }
                sink.emit(SessionEvent::TurnFailed {
                    error: e.to_string(),
                })
                .await;
                Err(e)
            }
        }
    }
```

Update the existing `run_turn_inner` signature to accept the cancel token:

```rust
    async fn run_turn_inner(
        &mut self,
        input: TurnInput,
        sink: &mut dyn EventSink,
        cancel: Arc<tokio::sync::Notify>,
    ) -> Result<Value, SessionError> {
        // ... seed as before ...

        for iter in 0..self.max_turns {
            // Cancellation check BEFORE emitting IterationStart, so a
            // pre-fired cancel returns with after_iterations = iter (0).
            if cancel_fired(&cancel) {
                self.planner.abandon_pending_turn();
                return Err(SessionError::Cancelled {
                    after_iterations: iter,
                });
            }
            sink.emit(SessionEvent::IterationStart { iter }).await;
            // ... rest of the iteration body unchanged ...
        }
        // ... same MAX_TURNS fallthrough ...
    }
```

And add the helper (placement: after `truncate_observation`):

```rust
/// Non-async, non-blocking check: has the Notify been signaled? We
/// implement this via a try_recv-shaped pattern using `try_notified`.
/// tokio::sync::Notify doesn't have a direct "is signaled" query, but
/// a fresh Notified future polled once returns Ready if a permit is
/// stored.
fn cancel_fired(n: &tokio::sync::Notify) -> bool {
    // If notify_one was called, one permit is stored; a fresh notified()
    // future returns Ready(()) on first poll.
    let mut fut = std::pin::pin!(n.notified());
    use std::task::{Context, Poll, Waker};
    let waker = Waker::noop();
    let mut cx = Context::from_waker(&waker);
    matches!(fut.as_mut().poll(&mut cx), Poll::Ready(()))
}
```

**Note on `Waker::noop`:** available in Rust 1.85+ (Rust 2024 edition). If MSRV is older, use `futures::task::noop_waker` instead — but with edition 2024 in the workspace, `Waker::noop()` is fine.

- [ ] **Step 5: Update the imports in session.rs**

Add to the top of `crates/agnes-session/src/session.rs`:

```rust
use tokio::sync::Notify;
```

- [ ] **Step 6: Run test — expect PASS**

Run: `cargo test -p agnes-session --test cancel`
Expected: PASS (2/2).

Run: `cargo test -p agnes-session`
Expected: all session tests PASS (session_end_to_end + classify_root + cancel + event_non_exhaustive).

- [ ] **Step 7: Commit**

```bash
jj describe -m "feat(session): cooperative Ctrl-C cancellation via tokio::sync::Notify

Add run_turn_cancellable(input, sink, cancel: Arc<Notify>). The loop
polls the notify at the top of each iteration; a fired notify aborts
with SessionError::Cancelled { after_iterations }. run_turn is now a
thin wrapper with a never-firing notify. Task 12 will wire the CLI's
Ctrl-C handler to fire this token.

Co-Authored-By: Claude <noreply@anthropic.com>"
jj new
```

---

### Task 12: CLI — render new events, `--max-turns` flag, Ctrl-C wiring

**Files:**
- Modify: `crates/agnes-cli/src/sink_stderr.rs`
- Modify: `crates/agnes-cli/src/cli.rs`
- Modify: `crates/agnes-cli/src/chat.rs`
- Modify: `crates/agnes-cli/src/main.rs`
- Test: `crates/agnes-cli/tests/max_turns_flag.rs`

**Interfaces:**
- `Args::llm.max_turns: Option<u32>` (new field on `LlmFlags`, or a peer field on `Args` — see step 3).
- `chat::run(provider: Arc<dyn Provider>, max_turns: Option<u32>)` — signature grows by one arg.
- StderrEventSink handles `IterationStart` and `ObservationEmitted`.
- Ctrl-C during a turn calls `cancel.notify_one()` instead of exiting the process.

- [ ] **Step 1: Write the test for the flag parsing**

Create `crates/agnes-cli/tests/max_turns_flag.rs`:

```rust
use agnes_cli::cli::{Args, Command};
use clap::Parser;

#[test]
fn max_turns_defaults_to_none() {
    let a = Args::try_parse_from(["agnes", "chat"]).unwrap();
    assert!(matches!(a.cmd, Some(Command::Chat)));
    assert!(a.max_turns.is_none());
}

#[test]
fn max_turns_from_flag() {
    let a = Args::try_parse_from(["agnes", "--max-turns", "42", "chat"]).unwrap();
    assert_eq!(a.max_turns, Some(42));
}

#[test]
fn max_turns_rejects_non_numeric() {
    let e = Args::try_parse_from(["agnes", "--max-turns", "abc", "chat"]);
    assert!(e.is_err());
}
```

- [ ] **Step 2: Run test — expect FAIL**

Run: `cargo test -p agnes-cli --test max_turns_flag`
Expected: FAIL — `max_turns` field doesn't exist on `Args`.

- [ ] **Step 3: Add `max_turns` to `Args`**

Edit `crates/agnes-cli/src/cli.rs`. Add a field to `pub struct Args`:

```rust
#[derive(Debug, Parser)]
#[command(name = "agnes", version, about = "agnes DSL runtime")]
pub struct Args {
    #[command(flatten)]
    pub llm: LlmFlags,

    /// Override the default per-turn iteration limit (default 20).
    #[arg(long, global = true)]
    pub max_turns: Option<u32>,

    #[command(subcommand)]
    pub cmd: Option<Command>,
}
```

The `global = true` makes the flag apply regardless of subcommand.

- [ ] **Step 4: Add event rendering in stderr sink**

Edit `crates/agnes-cli/src/sink_stderr.rs`. Find the existing `_ => { ... }` catchall (added in Task 8). REPLACE it with specific arms for the two new events, then keep a smaller `_ => {}` catchall for genuine future variants:

```rust
            SessionEvent::IterationStart { iter } => {
                let _ = writeln!(
                    e,
                    "\n─── iteration {iter} ───────────────────────────────"
                );
                self.start = Instant::now();
                self.printed_plan_header = false;
                self.printed_trace_header = false;
            }
            SessionEvent::ObservationEmitted {
                iter,
                text,
                is_error,
            } => {
                let t = self.t();
                let tag = if is_error { "✗ error" } else { "↓ observed" };
                let preview: String = text.chars().take(120).collect();
                let ellipsis = if text.chars().count() > 120 { "…" } else { "" };
                let _ = writeln!(
                    e,
                    "{t} {tag} (iter {iter}, {} chars): {preview}{ellipsis}",
                    text.chars().count()
                );
            }
            _ => {
                // Future SessionEvent variants render nothing by default.
            }
```

- [ ] **Step 5: Thread `max_turns` through the chat entry point**

Edit `crates/agnes-cli/src/chat.rs`. Change the `pub async fn run` signature and the `Session::new` call:

```rust
pub async fn run(provider: Arc<dyn Provider>, max_turns: Option<u32>) -> anyhow::Result<()> {
    banner();
    let mut session = match max_turns {
        Some(n) => Session::new_with_max_turns(provider, n)?,
        None => Session::new(provider)?,
    };
    // ... rest unchanged initially ...
}
```

Also add cancellation support inside the REPL loop. Replace the `session.run_turn(input, &mut sink).await` block with:

```rust
                let cancel = std::sync::Arc::new(tokio::sync::Notify::new());
                let cancel_for_signal = cancel.clone();
                // Set up a one-shot Ctrl-C handler for the duration of
                // this turn only. rustyline is not active while we're
                // awaiting run_turn, so a stray SIGINT would kill the
                // process; the handler swaps that behavior for a soft
                // cancel that lets the loop return SessionError::Cancelled.
                let ctrlc_task = tokio::spawn(async move {
                    let _ = tokio::signal::ctrl_c().await;
                    cancel_for_signal.notify_one();
                });
                let result = session
                    .run_turn_cancellable(input, &mut sink, cancel)
                    .await;
                ctrlc_task.abort();
                match result {
                    Ok(v) => println!("{}", v.data),
                    Err(agnes_session::SessionError::Cancelled { after_iterations }) => {
                        eprintln!("(cancelled after {after_iterations} iteration(s))");
                    }
                    Err(e) => eprintln!("error: {e}"),
                }
```

- [ ] **Step 6: Propagate `max_turns` from CLI dispatch**

Edit `crates/agnes-cli/src/main.rs`. In the branch that dispatches `Command::Chat`, pass `args.max_turns`:

```rust
        Command::Chat => {
            crate::chat::run(provider, args.max_turns).await?;
        }
```

(Adjust to your actual dispatch structure; the change is only that `run` now takes a second argument.)

- [ ] **Step 7: Run tests — expect PASS**

Run: `cargo test -p agnes-cli --test max_turns_flag`
Expected: PASS (3/3).

Run: `cargo test -p agnes-cli`
Expected: all tests PASS. Both the new flag test and the existing acceptance/input-balance/plan-view-snapshot tests.

Run: `cargo test --workspace`
Expected: PASS end-to-end.

- [ ] **Step 8: Run clippy full-workspace**

Run: `cargo clippy --workspace --all-targets --tests -- -D warnings`
Expected: PASS clean. Address any warning by editing the responsible file inline (typical: `unused_variables`, `needless_borrow`).

- [ ] **Step 9: Commit**

```bash
jj describe -m "feat(cli): render IterationStart/ObservationEmitted; --max-turns; Ctrl-C

StderrEventSink renders iteration separators (─── iteration N ───) and
observation lines (↓ observed (iter N, chars): preview…) or error
markers (✗ error). Catchall remains for future variants.

--max-turns <N> is a global flag; passed to Session::new_with_max_turns
when set (default 20).

chat::run installs a per-turn Ctrl-C handler that fires an
Arc<Notify> instead of killing the process; Session::run_turn_cancellable
returns SessionError::Cancelled which we render as
'(cancelled after N iteration(s))'.

Co-Authored-By: Claude <noreply@anthropic.com>"
jj new
```

---

### Task 13: `/history` renderer for nested turns + demo doc + full acceptance

**Files:**
- Create: `crates/agnes-cli/src/history_view.rs`
- Modify: `crates/agnes-cli/src/lib.rs` (module wiring)
- Modify: `crates/agnes-cli/src/chat.rs` (delegate to history_view)
- Modify: `examples/chat-demo.md`
- Modify: `README.md`

**Interfaces:**
- `pub fn render_history(turns: &[agnes_llm::Turn], out: &mut dyn std::io::Write) -> std::io::Result<()>`.

- [ ] **Step 1: Write the failing test**

Create `crates/agnes-cli/tests/history_view.rs`:

```rust
use agnes_cli::history_view::render_history;
use agnes_llm::{Iteration, Observation, Turn, TurnOutcome};
use agnes_types::TypeName;

fn iter(dsl: &str, obs: Option<Observation>) -> Iteration {
    Iteration {
        assistant_dsl: dsl.into(),
        observation: obs,
    }
}

fn obs(text: &str, is_error: bool, type_name: Option<&str>) -> Observation {
    Observation {
        text: text.into(),
        is_error,
        type_name: type_name.map(|s| TypeName(s.into())),
    }
}

#[test]
fn empty_history_prints_nothing() {
    let mut out = Vec::new();
    render_history(&[], &mut out).unwrap();
    assert_eq!(String::from_utf8(out).unwrap(), "");
}

#[test]
fn single_turn_single_iteration_finished_prints_expected() {
    let turns = vec![Turn {
        user_nl: "read notes".into(),
        iterations: vec![iter("(pipe \"notes.md\" (tool read-file) finish)", None)],
        outcome: TurnOutcome::Finished {
            result: "notes contents".into(),
        },
    }];
    let mut out = Vec::new();
    render_history(&turns, &mut out).unwrap();
    let s = String::from_utf8(out).unwrap();
    assert!(s.contains("--- turn 0 ---"));
    assert!(s.contains("user: read notes"));
    assert!(s.contains("iter 0: (pipe"));
    assert!(s.contains("outcome: Finished: notes contents"));
}

#[test]
fn multi_iteration_turn_shows_observations_between_dsls() {
    let turns = vec![Turn {
        user_nl: "translate this".into(),
        iterations: vec![
            iter(
                "(pipe (tool read-file :path \"x\") observe)",
                Some(obs("hello world", false, Some("PlainText"))),
            ),
            iter(
                "(pipe (tool translate :lang \"ja\") finish)",
                None,
            ),
        ],
        outcome: TurnOutcome::Finished {
            result: "こんにちは".into(),
        },
    }];
    let mut out = Vec::new();
    render_history(&turns, &mut out).unwrap();
    let s = String::from_utf8(out).unwrap();
    // Two iterations, one observation.
    assert!(s.contains("iter 0:"));
    assert!(s.contains("iter 1:"));
    assert!(s.contains("obs (PlainText): hello world"));
    assert!(s.contains("outcome: Finished: こんにちは"));
}

#[test]
fn error_observations_are_flagged() {
    let turns = vec![Turn {
        user_nl: "boom".into(),
        iterations: vec![
            iter(
                "(pipe (tool bogus) observe)",
                Some(obs("parse: unknown tool bogus", true, None)),
            ),
            iter("(pipe \"ok\" finish)", None),
        ],
        outcome: TurnOutcome::Finished {
            result: "ok".into(),
        },
    }];
    let mut out = Vec::new();
    render_history(&turns, &mut out).unwrap();
    let s = String::from_utf8(out).unwrap();
    assert!(s.contains("obs (error): parse: unknown tool bogus"));
}

#[test]
fn turn_limit_exceeded_outcome_is_labelled() {
    let turns = vec![Turn {
        user_nl: "spinny".into(),
        iterations: vec![iter("(pipe x observe)", Some(obs("x", false, None)))],
        outcome: TurnOutcome::TurnLimitExceeded,
    }];
    let mut out = Vec::new();
    render_history(&turns, &mut out).unwrap();
    let s = String::from_utf8(out).unwrap();
    assert!(s.contains("outcome: TurnLimitExceeded"));
}
```

- [ ] **Step 2: Run test — expect FAIL**

Run: `cargo test -p agnes-cli --test history_view`
Expected: FAIL (`history_view` module doesn't exist).

- [ ] **Step 3: Create `history_view.rs`**

Create `crates/agnes-cli/src/history_view.rs`:

```rust
//! Render agnes-llm::Turn history for the `/history` slash command.

use agnes_llm::{Turn, TurnOutcome};
use std::io::Write;

pub fn render_history(turns: &[Turn], out: &mut dyn Write) -> std::io::Result<()> {
    for (i, t) in turns.iter().enumerate() {
        writeln!(out, "--- turn {i} ---")?;
        writeln!(out, "user: {}", t.user_nl)?;
        for (j, it) in t.iterations.iter().enumerate() {
            writeln!(out, "iter {j}: {}", it.assistant_dsl)?;
            if let Some(obs) = &it.observation {
                let label = if obs.is_error {
                    "error".to_string()
                } else {
                    obs.type_name
                        .as_ref()
                        .map(|n| n.0.clone())
                        .unwrap_or_else(|| "?".to_string())
                };
                writeln!(out, "  obs ({label}): {}", obs.text)?;
            }
        }
        match &t.outcome {
            TurnOutcome::Finished { result } => {
                writeln!(out, "outcome: Finished: {result}")?;
            }
            TurnOutcome::TurnLimitExceeded => {
                writeln!(out, "outcome: TurnLimitExceeded")?;
            }
        }
    }
    Ok(())
}
```

- [ ] **Step 4: Wire the module**

Edit `crates/agnes-cli/src/lib.rs`. Add:

```rust
pub mod history_view;
```

next to the other `pub mod` declarations.

- [ ] **Step 5: Delegate `/history` command**

Edit `crates/agnes-cli/src/chat.rs`. Find the `if cmd == "history"` block. Replace with:

```rust
    if cmd == "history" {
        let mut stdout = std::io::stdout().lock();
        crate::history_view::render_history(session.history(), &mut stdout).ok();
        return Ok(true);
    }
```

- [ ] **Step 6: Update `examples/chat-demo.md`**

Replace the file content with:

```markdown
# Interactive `agnes chat` demo

`agnes chat` runs a multi-turn agent loop. Each user turn drives an LLM
that emits agnes DSL; the runtime executes the DSL; if the result is
wrapped in `Observation _`, the observation is fed back and the LLM
continues; if the result is `Finish _` or any other unwrapped type, the
turn ends.

## Quick start (missing key)

```
$ env -u ANTHROPIC_API_KEY -u OPENAI_API_KEY -u AGNES_LLM_PROVIDER \
  cargo run -p agnes-cli -- chat
```

Expected stderr (anyhow prepends `Error: ` to the first line):

```
Error: Missing provider selection.
  Why: neither the CLI flag `--llm-provider` nor the env var `AGNES_LLM_PROVIDER` is set.
  Fix: pass --llm-provider, set AGNES_LLM_PROVIDER, or add it to .env.
```

Exit status: non-zero.

## Quick start (real key)

```
$ ANTHROPIC_API_KEY=... cargo run -p agnes-cli -- \
    --llm-provider anthropic --llm-model claude-haiku-4-5 chat
```

Then in the REPL:

```
agnes chat — type your goal, or /run <dsl>, /history, /reset, /quit

> read the README and summarize it in one sentence

─── iteration 0 ─────────────────────────────
━━━ Planning ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
━━━ Generated DSL ━━━━━━━━━━━━━━━━━━━━━━━━
(pipe (tool read-file :path "README.md") (tool summarize) finish)
━━━ Plan ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
├── read-file → PlainText
├── summarize → Summary
└── finish   → Unknown
━━━ Trace ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
[+0.043s] ▶ read-file :path=README.md
[+0.081s] ✔ read-file (38ms) → PlainText: <content>…
[+0.083s] ▶ summarize :input=<from read-file>
[+1.410s] ✔ summarize (1327ms) → Summary: agnes is a…
[+1.412s] ▶ finish :input=<from summarize>
[+1.413s] ✔ finish (1ms) → (Finish Summary): agnes is a…
agnes is a Rust runtime for a small typed workflow DSL.
```

## `observe` example (agent decides to look before speaking)

```
> summarize the README, but only if it's less than 4000 chars

─── iteration 0 ─────────────────────────────
━━━ Generated DSL ━━━━━━━━━━━━━━━━━━━━━━━━
(pipe (tool read-file :path "README.md") observe)
[+0.081s] ↓ observed (iter 0, 3200 chars): # agnes …

─── iteration 1 ─────────────────────────────
━━━ Generated DSL ━━━━━━━━━━━━━━━━━━━━━━━━
(pipe (tool summarize :input "…") finish)
[+1.410s] ✔ summarize (1327ms) → Summary: agnes is a…
agnes is a Rust runtime for a small typed workflow DSL.
```

## `--max-turns`

```
$ cargo run -p agnes-cli -- chat --max-turns 5
```

Cap the loop at 5 iterations per turn. On exhaustion:

```
━━━ Turn Failed ━━━━━━━━━━━━━━━━━━━━━━━━━━━
Agent loop hit the iteration limit.
  Why: `MAX_TURNS = 5` reached without a terminating iteration (finish or unlabeled result).
  Fix: rephrase the request more narrowly, or pass `--max-turns <N>` to raise the ceiling.
```

## Mocked built-in tools

Note: this build ships in-memory mocks for the I/O-adjacent tools. See
[`crates/agnes-builtins/src/tools.rs`](../crates/agnes-builtins/src/tools.rs)
for `MOCK_README`, `MOCK_NOTES`, `MOCK_DRAFT` — the strings `read-file`
returns for well-known paths. `write-file` records to a process-global
`writes()` log, drained per turn as `WriteSummary`. `ocr` returns fixed
placeholder text. `llm`, `summarize`, `translate` use the real Provider.

## Manual verification checklist (pending user verification)

- [ ] Missing-key path prints the What/Why/Fix block above and exits non-zero.
- [ ] Real-key path executes translate/summarize with visible plan tree and per-node trace.
- [ ] Two-iteration `observe → finish` path shows both iterations on stderr with the observation line in between.
- [ ] Error-observation path (LLM emits a broken DSL) recovers in a subsequent iteration.
- [ ] `--max-turns 2` for a "loop forever with observe" prompt correctly hits `TurnLimitExceeded`.
- [ ] Ctrl-C during a long turn prints `(cancelled after N iteration(s))` and returns to the prompt.
- [ ] `/history` shows nested iterations with the observation `type` labels.
```

- [ ] **Step 7: Update `README.md`**

Find the "Try it" section (Task 12 of the prior plan added it) and REPLACE the "Interactive chat" subsection with:

```markdown
## Interactive chat (agent loop)

Set an API key and:

    ANTHROPIC_API_KEY=... cargo run -p agnes-cli -- chat --llm-provider anthropic

Each natural-language turn drives a multi-iteration agent loop:

1. LLM emits a DSL program.
2. Runtime executes it. If the result is wrapped as `(Observation _)` (via
   the new `observe` tool), the rendered result feeds back to the LLM and
   the loop continues.
3. If the result is `(Finish _)` (via the `finish` tool) or any plain
   type, it's shown to the user and the turn ends.
4. Loop is bounded by `--max-turns <N>` (default 20).

Ctrl-C during a turn cancels the current loop and returns to the prompt.
`/run <dsl>` injects a hand-written DSL as iteration 0; `/history` shows
past turns and their iterations; `/reset` clears history. See
[examples/chat-demo.md](examples/chat-demo.md).
```

- [ ] **Step 8: Full test run + lint**

Run: `cargo test --workspace`
Expected: PASS end-to-end.

Run: `cargo clippy --workspace --all-targets --tests -- -D warnings`
Expected: clean.

Run: `cargo fmt --all`
Expected: no diff, or all changes applied.

- [ ] **Step 9: Manual verification of missing-key error**

Run:

```bash
env -u ANTHROPIC_API_KEY -u OPENAI_API_KEY -u AGNES_LLM_PROVIDER \
  cargo run -p agnes-cli -- chat 2>&1 | head -5
echo "EXIT=$?"
```

Expected stderr starts with `Error: Missing provider selection.` and `EXIT=1`.

Real-key REPL run (Steps 4/5 from the demo-doc checklist) is deferred to user manual verification — capture the output and paste into the demo file only if you can run it here.

- [ ] **Step 10: Commit**

```bash
jj describe -m "feat(cli): nested-turn /history renderer + demo doc for agent loop

New history_view module renders Turn { user_nl, iterations, outcome } as
a nested outline. Each iteration prints its DSL and, if present, its
observation labelled by type or as an error. Outcome prints as Finished
or TurnLimitExceeded.

README section updated for the agent-loop model. examples/chat-demo.md
covers both single-iteration finish and multi-iteration observe→finish,
the new --max-turns flag, and Ctrl-C cancellation. The real-key
transcript remains labelled as pending user verification.

Co-Authored-By: Claude <noreply@anthropic.com>"
jj new
```
