# Interactive `agnes chat` — Real LLM + Mocked I/O Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship an interactive `agnes chat` REPL where a real LLM turns user NL into agnes DSL, the DSL runs through the existing parser/checker/compiler/runtime, and the user sees the generated DSL, plan tree, live per-node trace, and final result. LLM-participating tools (`llm` / `summarize` / `translate`) call a real provider; I/O-adjacent tools (`read-file` / `write-file` / `ocr`) become in-memory mocks so the demo needs only an API key.

**Architecture:** Add two new crates. `agnes-llm` holds a `Provider` trait with `Anthropic` + `OpenAiCompat` implementations plus a `Planner` (NL → DSL with error-feedback retry). `agnes-session` is a **headless** engine that emits `SessionEvent`s to an `EventSink`; the CLI is a thin frontend implementing `StderrEventSink`. A future GUI would be another frontend on the same API. Runtime gets an additive `Tracer` trait + `execute_with(...)`; existing `execute()` stays intact.

**Tech Stack:** Rust edition 2024, tokio, reqwest (`rustls-tls`, no openssl), serde/serde_json, async-trait, dotenvy, clap 4, rustyline 14, thiserror, insta (snapshots).

## Global Constraints

- Rust edition 2024 throughout every crate.
- All new crates named `agnes-<component>` under `crates/<name>/`; shared external deps live in workspace root `Cargo.toml` `[workspace.dependencies]` and are pulled in with `<dep>.workspace = true`.
- **Commits use jj** (colocated with git). Workflow at the end of each task: `jj describe -m "..." && jj new`. Never `git commit`. Every commit message ends with `Co-Authored-By: Claude <noreply@anthropic.com>` on its own line.
- Language of code, comments, and error messages: English. Error messages follow the What / Why / Fix template from the MVP spec §2.5.
- Type names use PascalCase (`PlainText`); tool names and parameter names use kebab-case (`read-file`, `write-file`).
- No new type/alias/tool registrations — this plan only changes tool **implementations**, not signatures. `register_builtins` stays unchanged.
- The existing `agnes_runtime::execute(...)` signature MUST remain source-compatible; new capability is added as `execute_with(...)`. Every existing runtime/checker/cli test keeps passing.
- API keys come from env vars only (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`); never from CLI flags — this avoids leaking keys into shell history. Provider selection, model, and `base_url` may come from either CLI flags OR env vars OR a `.env` file loaded at CLI startup.
- Provider trait is object-safe via `async-trait` and returned as `Arc<dyn Provider>` — every consumer accepts the trait object, not a concrete type.
- Real network calls MUST NOT happen in any unit or integration test. Provider tests use `MockProvider` (defined in `agnes-llm`).
- Trace visualization is **on by default** in `agnes chat`. `stderr` carries the plan tree + trace; `stdout` carries only the final result.

## File Structure (locked before task decomposition)

```
crates/
├── agnes-llm/                            # NEW crate (Task 1)
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs                        # public re-exports
│       ├── error.rs                      # LlmError, PlannerError
│       ├── provider.rs                   # Provider trait, CompletionRequest, Message
│       ├── mock.rs                       # MockProvider (test-only helpers, always compiled)
│       ├── anthropic.rs                  # AnthropicProvider (Task 2)
│       ├── openai.rs                     # OpenAiCompatProvider (Task 3)
│       ├── resolve.rs                    # LlmCliOpts + Provider::resolve factory (Task 4)
│       └── planner.rs                    # Planner + Turn + system-prompt builder (Task 8)
├── agnes-session/                        # NEW crate (Task 9)
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs                        # public re-exports
│       ├── error.rs                      # SessionError
│       ├── events.rs                     # SessionEvent, EventSink, NodeKindTag
│       ├── plan_tree.rs                  # PlanTree data + Dag -> PlanTree builder
│       ├── tracer_bridge.rs              # runtime::Tracer -> EventSink adapter
│       └── session.rs                    # Session struct + run_turn
├── agnes-builtins/
│   └── src/tools.rs                      # MODIFIED (Task 6): native_dispatch(provider) + mocks
├── agnes-runtime/
│   ├── src/lib.rs                        # MODIFIED (Task 5): add Tracer, NoopTracer, execute_with
│   └── src/scheduler.rs                  # MODIFIED (Task 5): thread tracer through
└── agnes-cli/
    ├── Cargo.toml                        # MODIFIED (Task 10): add deps
    └── src/
        ├── main.rs                       # MODIFIED (Task 10): clap dispatch
        ├── cli.rs                        # NEW (Task 10): Args, Command
        ├── run_cmd.rs                    # NEW (Task 10): `agnes run <file>`
        ├── chat.rs                       # NEW (Task 11): REPL loop
        ├── input.rs                      # NEW (Task 11): line reader + `(`-balanced multiline
        ├── plan_view.rs                  # NEW (Task 10): PlanTree -> stderr tree
        └── sink_stderr.rs                # NEW (Task 10): StderrEventSink

examples/
└── chat-demo.md                          # NEW (Task 12): walkthrough
```

**Locked interfaces (referenced across tasks — every task's implementer sees only their own task, so signatures must match these exactly):**

- `agnes_llm::Provider` — trait; `async fn complete(&self, req: CompletionRequest) -> Result<String, LlmError>`.
- `agnes_llm::CompletionRequest { system: Option<String>, messages: Vec<Message>, max_tokens: u32 }`.
- `agnes_llm::Message { role: Role, content: String }`; `Role::User | Role::Assistant`.
- `agnes_llm::LlmError` — `thiserror` enum with `Http`, `Api { status, body }`, `Deserialize`, `MissingApiKey { env_var: &'static str }`, `MissingConfig { what: &'static str, env_var: &'static str, flag: &'static str }`.
- `agnes_llm::LlmCliOpts { provider: Option<String>, model: Option<String>, base_url: Option<String> }`.
- `agnes_llm::MockProvider::new(responses: Vec<String>) -> Self` — returns `responses[0]`, `responses[1]`, ... on successive `complete` calls; panics if exhausted. Cheap to clone; internally `Arc<Mutex<VecDeque<String>>>`.
- `agnes_llm::Planner::new(provider: Arc<dyn Provider>, registry: &Registry) -> Self`.
- `agnes_llm::Planner::plan(&mut self, nl: &str) -> Result<String, PlannerError>` — returns the extracted DSL source (fenced-block stripped).
- `agnes_llm::Planner::push_error_feedback(&mut self, bad_dsl: String, err: String)`.
- `agnes_llm::Planner::record_result(&mut self, dsl: String, result_preview: String)`.
- `agnes_runtime::Tracer` — trait, `Send + Sync`, with `fn node_start(&self, id: NodeId, kind: &NodeKind, args_preview: &str)` and `fn node_end(&self, id: NodeId, result: Result<&Value, &RuntimeError>, elapsed: Duration)`.
- `agnes_runtime::NoopTracer` — unit struct implementing `Tracer`.
- `agnes_runtime::execute_with(dag, reg, dispatch, tracer: &dyn Tracer) -> Result<Value, RuntimeError>` (async).
- `agnes_runtime::execute(dag, reg, dispatch) -> Result<Value, RuntimeError>` — kept, now a wrapper.
- `agnes_builtins::native_dispatch(provider: Arc<dyn Provider>) -> HashMap<String, ToolImpl>` — **signature change** from the current no-arg version.
- `agnes_session::Session::new(provider: Arc<dyn Provider>) -> Result<Self, SessionError>`.
- `agnes_session::Session::run_turn(&mut self, input: TurnInput, sink: &mut dyn EventSink) -> Result<Value, SessionError>` (async). `EventSink` is dispatched as `&mut dyn EventSink` (not generic) so `Session::run_turn` stays object-safe.
- `agnes_session::TurnInput::NaturalLanguage(String) | RawDsl(String)`.
- `agnes_session::SessionEvent` variants: `PlannerStart`, `PlannerRetry { attempt: u8, error: String }`, `DslProduced { source: String }`, `PlanReady { tree: PlanTree }`, `NodeStart { id: u32, kind: NodeKindTag, args: Vec<(String, String)> }`, `NodeEnd { id: u32, ok: bool, preview: String, elapsed_ms: u64 }`, `TurnResult { value_preview: String, value_type: String }`, `TurnFailed { error: String }`.
- `agnes_session::PlanTree { kind: String, label: String, provides: Option<String>, children: Vec<PlanTree> }`.
- `agnes_session::NodeKindTag::Tool { name: String } | Llm` — the two node kinds that emit events; keeps the CLI decoupled from `NodeKind`.

---

### Task 1: Scaffold `agnes-llm` crate + `Provider` trait + `MockProvider`

**Files:**
- Create: `crates/agnes-llm/Cargo.toml`
- Create: `crates/agnes-llm/src/lib.rs`
- Create: `crates/agnes-llm/src/error.rs`
- Create: `crates/agnes-llm/src/provider.rs`
- Create: `crates/agnes-llm/src/mock.rs`
- Modify: `Cargo.toml` (workspace root — add member + workspace deps `async-trait`, `reqwest`)
- Test: `crates/agnes-llm/tests/provider_smoke.rs`

**Interfaces:**
- Consumes: nothing (first task).
- Produces: `agnes_llm::{Provider, CompletionRequest, Message, Role, LlmError, MockProvider}`. Downstream tasks (2, 3, 6, 8) depend on these signatures.

- [ ] **Step 1: Add workspace deps and register the crate**

Edit `Cargo.toml` (workspace root). Under `members = [...]` append `"crates/agnes-llm"`. Under `[workspace.dependencies]` add:

```toml
agnes-llm   = { path = "crates/agnes-llm" }
reqwest     = { version = "0.12", default-features = false, features = ["json", "rustls-tls"] }
async-trait = "0.1"
```

- [ ] **Step 2: Create `crates/agnes-llm/Cargo.toml`**

```toml
[package]
name = "agnes-llm"
edition.workspace = true
version.workspace = true
license.workspace = true
authors.workspace = true

[dependencies]
async-trait.workspace = true
reqwest.workspace     = true
serde.workspace       = true
serde_json.workspace  = true
thiserror.workspace   = true
tokio.workspace       = true

[dev-dependencies]
tokio = { workspace = true, features = ["macros", "rt-multi-thread"] }
```

- [ ] **Step 3: Write the failing smoke test**

Create `crates/agnes-llm/tests/provider_smoke.rs`:

```rust
use agnes_llm::{CompletionRequest, MockProvider, Provider, Message, Role};
use std::sync::Arc;

#[tokio::test]
async fn mock_provider_returns_queued_responses_in_order() {
    let p: Arc<dyn Provider> =
        Arc::new(MockProvider::new(vec!["hello".into(), "world".into()]));
    let req1 = CompletionRequest {
        system: None,
        messages: vec![Message { role: Role::User, content: "a".into() }],
        max_tokens: 128,
    };
    let req2 = CompletionRequest {
        system: None,
        messages: vec![Message { role: Role::User, content: "b".into() }],
        max_tokens: 128,
    };
    let r1 = p.complete(req1).await.unwrap();
    let r2 = p.complete(req2).await.unwrap();
    assert_eq!(r1, "hello");
    assert_eq!(r2, "world");
}
```

- [ ] **Step 4: Run test to confirm it fails**

Run: `cargo test -p agnes-llm --test provider_smoke`
Expected: FAIL with "unresolved import `agnes_llm::...`" or "no crate named agnes_llm".

- [ ] **Step 5: Implement `error.rs`**

Create `crates/agnes-llm/src/error.rs`:

```rust
use thiserror::Error;

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("HTTP transport error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("provider API returned status {status}\n  body: {body}\n  Fix: verify api key, model id, and base_url; retry.")]
    Api { status: u16, body: String },

    #[error("failed to deserialize provider response: {0}")]
    Deserialize(String),

    #[error("Missing API key.\n  Why: `{env_var}` is not set.\n  Fix: export {env_var}=<key>, or add `{env_var}=<key>` to a .env file at the workspace root.")]
    MissingApiKey { env_var: &'static str },

    #[error("Missing {what}.\n  Why: neither the CLI flag `{flag}` nor the env var `{env_var}` is set.\n  Fix: pass {flag}, set {env_var}, or add it to .env.")]
    MissingConfig {
        what: &'static str,
        env_var: &'static str,
        flag: &'static str,
    },
}
```

- [ ] **Step 6: Implement `provider.rs`**

Create `crates/agnes-llm/src/provider.rs`:

```rust
use crate::error::LlmError;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct CompletionRequest {
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub max_tokens: u32,
}

#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    async fn complete(&self, req: CompletionRequest) -> Result<String, LlmError>;
}
```

- [ ] **Step 7: Implement `mock.rs`**

Create `crates/agnes-llm/src/mock.rs`:

```rust
use crate::error::LlmError;
use crate::provider::{CompletionRequest, Provider};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// Deterministic in-memory provider for tests and demos.
/// Returns queued responses in FIFO order. Records every request it saw
/// so tests can assert on them.
#[derive(Debug, Clone)]
pub struct MockProvider {
    inner: Arc<Mutex<MockInner>>,
}

#[derive(Debug)]
struct MockInner {
    responses: VecDeque<String>,
    seen: Vec<CompletionRequest>,
}

impl MockProvider {
    pub fn new(responses: Vec<String>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(MockInner {
                responses: responses.into(),
                seen: Vec::new(),
            })),
        }
    }

    /// Snapshot of every request the mock has served so far, in order.
    pub fn seen(&self) -> Vec<CompletionRequest> {
        self.inner.lock().unwrap().seen.clone()
    }
}

#[async_trait::async_trait]
impl Provider for MockProvider {
    async fn complete(&self, req: CompletionRequest) -> Result<String, LlmError> {
        let mut g = self.inner.lock().unwrap();
        g.seen.push(req);
        Ok(g
            .responses
            .pop_front()
            .expect("MockProvider: response queue exhausted; queue more responses"))
    }
}
```

- [ ] **Step 8: Implement `lib.rs`**

Create `crates/agnes-llm/src/lib.rs`:

```rust
//! LLM provider abstraction + Planner (NL -> agnes DSL).
//!
//! Two production providers live in `anthropic` and `openai` (added in
//! Tasks 2 and 3). `MockProvider` is available at all times for tests.

mod error;
mod mock;
mod provider;

pub use error::LlmError;
pub use mock::MockProvider;
pub use provider::{CompletionRequest, Message, Provider, Role};
```

- [ ] **Step 9: Run the smoke test — it must pass**

Run: `cargo test -p agnes-llm --test provider_smoke`
Expected: PASS (1 test).

- [ ] **Step 10: Commit**

```bash
jj describe -m "feat(llm): scaffold agnes-llm crate with Provider trait + MockProvider

Adds the Provider trait (async_trait, object-safe), CompletionRequest /
Message types, LlmError enum, and MockProvider for tests. Downstream
providers, planner, and session code depend on these signatures.

Co-Authored-By: Claude <noreply@anthropic.com>"
jj new
```

---

### Task 2: `AnthropicProvider` (real HTTP against Claude Messages API)

**Files:**
- Create: `crates/agnes-llm/src/anthropic.rs`
- Modify: `crates/agnes-llm/src/lib.rs` (add `mod anthropic; pub use anthropic::AnthropicProvider;`)
- Test: `crates/agnes-llm/tests/anthropic_shape.rs`

**Interfaces:**
- Consumes: `Provider`, `CompletionRequest`, `Message`, `Role`, `LlmError` from Task 1.
- Produces: `agnes_llm::AnthropicProvider::new(model: String, api_key: String, client: reqwest::Client) -> Self`. Task 4 (`resolve`) constructs it.

- [ ] **Step 1: Write the failing shape test**

Create `crates/agnes-llm/tests/anthropic_shape.rs`. This test validates the JSON body we build without making a real network call — we serialize the request body directly using an internal helper `AnthropicProvider::build_body`:

```rust
use agnes_llm::{AnthropicProvider, CompletionRequest, Message, Role};

#[test]
fn anthropic_body_has_expected_shape() {
    let p = AnthropicProvider::new("claude-haiku-4-5".into(), "sk-test".into(), reqwest::Client::new());
    let req = CompletionRequest {
        system: Some("you are helpful".into()),
        messages: vec![
            Message { role: Role::User, content: "hi".into() },
            Message { role: Role::Assistant, content: "hello".into() },
            Message { role: Role::User, content: "again".into() },
        ],
        max_tokens: 256,
    };
    let body = p.build_body(&req);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["model"], "claude-haiku-4-5");
    assert_eq!(v["max_tokens"], 256);
    assert_eq!(v["system"], "you are helpful");
    let msgs = v["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[0]["role"], "user");
    assert_eq!(msgs[0]["content"], "hi");
    assert_eq!(msgs[1]["role"], "assistant");
    assert_eq!(msgs[2]["role"], "user");
}

#[test]
fn anthropic_body_omits_system_when_none() {
    let p = AnthropicProvider::new("m".into(), "k".into(), reqwest::Client::new());
    let req = CompletionRequest {
        system: None,
        messages: vec![Message { role: Role::User, content: "x".into() }],
        max_tokens: 8,
    };
    let body = p.build_body(&req);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(v.get("system").is_none(), "system must be absent when None");
}
```

- [ ] **Step 2: Run — confirm failure**

Run: `cargo test -p agnes-llm --test anthropic_shape`
Expected: FAIL — `AnthropicProvider` not found.

- [ ] **Step 3: Implement `crates/agnes-llm/src/anthropic.rs`**

```rust
use crate::error::LlmError;
use crate::provider::{CompletionRequest, Provider, Role};
use serde::Serialize;

const ANTHROPIC_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";

pub struct AnthropicProvider {
    model: String,
    api_key: String,
    client: reqwest::Client,
}

#[derive(Serialize)]
struct WireMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Serialize)]
struct WireBody<'a> {
    model: &'a str,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<&'a str>,
    messages: Vec<WireMessage<'a>>,
}

impl AnthropicProvider {
    pub fn new(model: String, api_key: String, client: reqwest::Client) -> Self {
        Self { model, api_key, client }
    }

    /// Build the JSON body string. Exposed for shape tests.
    pub fn build_body(&self, req: &CompletionRequest) -> String {
        let msgs: Vec<WireMessage> = req
            .messages
            .iter()
            .map(|m| WireMessage {
                role: match m.role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                },
                content: &m.content,
            })
            .collect();
        let body = WireBody {
            model: &self.model,
            max_tokens: req.max_tokens,
            system: req.system.as_deref(),
            messages: msgs,
        };
        serde_json::to_string(&body).expect("serialize anthropic body")
    }
}

#[async_trait::async_trait]
impl Provider for AnthropicProvider {
    async fn complete(&self, req: CompletionRequest) -> Result<String, LlmError> {
        let body = self.build_body(&req);
        let resp = self
            .client
            .post(ANTHROPIC_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(LlmError::Api { status: status.as_u16(), body: text });
        }
        // Response shape: { "content": [ { "type": "text", "text": "..." }, ... ] }
        let v: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| LlmError::Deserialize(format!("{e}: body was {text}")))?;
        let content = v
            .get("content")
            .and_then(|c| c.as_array())
            .ok_or_else(|| LlmError::Deserialize("no `content` array in response".into()))?;
        let mut out = String::new();
        for part in content {
            if part.get("type").and_then(|t| t.as_str()) == Some("text") {
                if let Some(s) = part.get("text").and_then(|t| t.as_str()) {
                    out.push_str(s);
                }
            }
        }
        Ok(out)
    }
}
```

- [ ] **Step 4: Wire into `lib.rs`**

Edit `crates/agnes-llm/src/lib.rs` — add:

```rust
mod anthropic;
pub use anthropic::AnthropicProvider;
```

- [ ] **Step 5: Run the shape test — must pass**

Run: `cargo test -p agnes-llm --test anthropic_shape`
Expected: PASS (2 tests).

- [ ] **Step 6: Commit**

```bash
jj describe -m "feat(llm): AnthropicProvider (Messages API)

POST /v1/messages with anthropic-version 2023-06-01 header. Parses the
content[] array and concatenates text parts. Shape unit tests cover
body serialization; network calls are not exercised here.

Co-Authored-By: Claude <noreply@anthropic.com>"
jj new
```

---

### Task 3: `OpenAiCompatProvider` (Chat Completions shape, base_url overrideable)

**Files:**
- Create: `crates/agnes-llm/src/openai.rs`
- Modify: `crates/agnes-llm/src/lib.rs` (add `mod openai; pub use openai::OpenAiCompatProvider;`)
- Test: `crates/agnes-llm/tests/openai_shape.rs`

**Interfaces:**
- Consumes: `Provider`, `CompletionRequest`, `Message`, `Role`, `LlmError` from Task 1.
- Produces: `agnes_llm::OpenAiCompatProvider::new(model: String, api_key: String, base_url: String, client: reqwest::Client) -> Self`. `base_url` is a full origin like `https://api.openai.com/v1` — the provider appends `/chat/completions`.

- [ ] **Step 1: Write the failing shape test**

Create `crates/agnes-llm/tests/openai_shape.rs`:

```rust
use agnes_llm::{CompletionRequest, Message, OpenAiCompatProvider, Role};

#[test]
fn openai_body_folds_system_into_messages() {
    let p = OpenAiCompatProvider::new(
        "gpt-4o-mini".into(),
        "sk-test".into(),
        "https://api.openai.com/v1".into(),
        reqwest::Client::new(),
    );
    let req = CompletionRequest {
        system: Some("be terse".into()),
        messages: vec![Message { role: Role::User, content: "hi".into() }],
        max_tokens: 64,
    };
    let body = p.build_body(&req);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["model"], "gpt-4o-mini");
    assert_eq!(v["max_tokens"], 64);
    let msgs = v["messages"].as_array().unwrap();
    // system folded in as the first message with role=system
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0]["role"], "system");
    assert_eq!(msgs[0]["content"], "be terse");
    assert_eq!(msgs[1]["role"], "user");
    assert_eq!(msgs[1]["content"], "hi");
}

#[test]
fn openai_endpoint_appends_chat_completions() {
    let p = OpenAiCompatProvider::new(
        "m".into(),
        "k".into(),
        "https://ark.cn-beijing.volces.com/api/v3".into(),
        reqwest::Client::new(),
    );
    assert_eq!(
        p.endpoint(),
        "https://ark.cn-beijing.volces.com/api/v3/chat/completions"
    );
}
```

- [ ] **Step 2: Run — must fail**

Run: `cargo test -p agnes-llm --test openai_shape`
Expected: FAIL — `OpenAiCompatProvider` not found.

- [ ] **Step 3: Implement `crates/agnes-llm/src/openai.rs`**

```rust
use crate::error::LlmError;
use crate::provider::{CompletionRequest, Provider, Role};
use serde::Serialize;

pub struct OpenAiCompatProvider {
    model: String,
    api_key: String,
    base_url: String,
    client: reqwest::Client,
}

#[derive(Serialize)]
struct WireMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Serialize)]
struct WireBody<'a> {
    model: &'a str,
    max_tokens: u32,
    messages: Vec<WireMessage<'a>>,
}

impl OpenAiCompatProvider {
    pub fn new(model: String, api_key: String, base_url: String, client: reqwest::Client) -> Self {
        // Normalize: strip any trailing slash.
        let base_url = base_url.trim_end_matches('/').to_string();
        Self { model, api_key, base_url, client }
    }

    pub fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }

    pub fn build_body(&self, req: &CompletionRequest) -> String {
        let mut msgs: Vec<WireMessage> = Vec::with_capacity(req.messages.len() + 1);
        if let Some(sys) = &req.system {
            msgs.push(WireMessage { role: "system", content: sys });
        }
        for m in &req.messages {
            msgs.push(WireMessage {
                role: match m.role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                },
                content: &m.content,
            });
        }
        serde_json::to_string(&WireBody {
            model: &self.model,
            max_tokens: req.max_tokens,
            messages: msgs,
        })
        .expect("serialize openai body")
    }
}

#[async_trait::async_trait]
impl Provider for OpenAiCompatProvider {
    async fn complete(&self, req: CompletionRequest) -> Result<String, LlmError> {
        let body = self.build_body(&req);
        let resp = self
            .client
            .post(self.endpoint())
            .header("authorization", format!("Bearer {}", self.api_key))
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(LlmError::Api { status: status.as_u16(), body: text });
        }
        // { "choices": [ { "message": { "content": "..." } } ] }
        let v: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| LlmError::Deserialize(format!("{e}: body was {text}")))?;
        let content = v
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(|s| s.as_str())
            .ok_or_else(|| {
                LlmError::Deserialize(format!(
                    "no choices[0].message.content in response: {text}"
                ))
            })?;
        Ok(content.to_string())
    }
}
```

- [ ] **Step 4: Wire into `lib.rs`**

Edit `crates/agnes-llm/src/lib.rs`:

```rust
mod openai;
pub use openai::OpenAiCompatProvider;
```

- [ ] **Step 5: Run the shape test — must pass**

Run: `cargo test -p agnes-llm --test openai_shape`
Expected: PASS (2 tests).

- [ ] **Step 6: Commit**

```bash
jj describe -m "feat(llm): OpenAiCompatProvider (Chat Completions)

POST {base_url}/chat/completions with Bearer auth. One implementation
covers OpenAI + DeepSeek + 火山方舟 + 阿里百炼 via base_url override.
System prompt folded into messages[] as first entry.

Co-Authored-By: Claude <noreply@anthropic.com>"
jj new
```

---

### Task 4: `Provider::resolve` — CLI-flag / env / .env resolution

**Files:**
- Create: `crates/agnes-llm/src/resolve.rs`
- Modify: `crates/agnes-llm/src/lib.rs` (add `mod resolve; pub use resolve::{LlmCliOpts, resolve_provider};`)
- Modify: `crates/agnes-llm/Cargo.toml` — add `dotenvy = "0.15"` (also add `dotenvy` to workspace deps in root `Cargo.toml` if not present)
- Test: `crates/agnes-llm/tests/resolve.rs`

**Interfaces:**
- Consumes: `Provider`, `LlmError`, `AnthropicProvider`, `OpenAiCompatProvider` from Tasks 1–3.
- Produces:
  - `pub struct LlmCliOpts { pub provider: Option<String>, pub model: Option<String>, pub base_url: Option<String> }`.
  - `pub fn resolve_provider(cli: &LlmCliOpts) -> Result<Arc<dyn Provider>, LlmError>`.
  - `.env` loading is the caller's responsibility (CLI does it once at startup). Task 10 calls `dotenvy::dotenv().ok()` before `resolve_provider`.

- [ ] **Step 1: Add `dotenvy` to workspace root `Cargo.toml`**

Under `[workspace.dependencies]` add:

```toml
dotenvy = "0.15"
```

Then edit `crates/agnes-llm/Cargo.toml` — under `[dependencies]` add `dotenvy.workspace = true`.

- [ ] **Step 2: Write the failing test**

Create `crates/agnes-llm/tests/resolve.rs`:

```rust
use agnes_llm::{resolve_provider, LlmCliOpts, LlmError};

// Serialize these tests — they mutate process env.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn clear_env() {
    for k in ["AGNES_LLM_PROVIDER", "AGNES_LLM_MODEL", "AGNES_LLM_BASE_URL",
              "ANTHROPIC_API_KEY", "OPENAI_API_KEY"] {
        // SAFETY: process-serialized via ENV_LOCK.
        unsafe { std::env::remove_var(k); }
    }
}

#[test]
fn missing_provider_selection_errors() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_env();
    let cli = LlmCliOpts { provider: None, model: None, base_url: None };
    let err = resolve_provider(&cli).unwrap_err();
    assert!(matches!(err, LlmError::MissingConfig { flag: "--llm-provider", .. }), "got: {err}");
}

#[test]
fn anthropic_needs_key() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_env();
    let cli = LlmCliOpts { provider: Some("anthropic".into()), model: Some("m".into()), base_url: None };
    let err = resolve_provider(&cli).unwrap_err();
    assert!(matches!(err, LlmError::MissingApiKey { env_var: "ANTHROPIC_API_KEY" }), "got: {err}");
}

#[test]
fn anthropic_resolves_with_key_and_default_model() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_env();
    unsafe { std::env::set_var("ANTHROPIC_API_KEY", "sk-test"); }
    let cli = LlmCliOpts { provider: Some("anthropic".into()), model: None, base_url: None };
    // Model defaults to "claude-haiku-4-5" when neither flag nor env supplies one.
    let _ = resolve_provider(&cli).expect("should resolve");
}

#[test]
fn openai_needs_key_and_base_url() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_env();
    let cli = LlmCliOpts { provider: Some("openai".into()), model: Some("m".into()), base_url: None };
    let err = resolve_provider(&cli).unwrap_err();
    // First missing thing surfaced: key.
    assert!(matches!(err, LlmError::MissingApiKey { env_var: "OPENAI_API_KEY" }));

    unsafe { std::env::set_var("OPENAI_API_KEY", "sk-test"); }
    let err2 = resolve_provider(&cli).unwrap_err();
    assert!(matches!(err2, LlmError::MissingConfig { what: "base_url", flag: "--llm-base-url", .. }));
}

#[test]
fn cli_flag_beats_env() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_env();
    unsafe {
        std::env::set_var("AGNES_LLM_PROVIDER", "openai");
        std::env::set_var("AGNES_LLM_MODEL", "env-model");
        std::env::set_var("ANTHROPIC_API_KEY", "sk-test");
    }
    // CLI says anthropic — must win over env's "openai".
    let cli = LlmCliOpts { provider: Some("anthropic".into()), model: Some("cli-model".into()), base_url: None };
    let _ = resolve_provider(&cli).expect("should resolve as anthropic");
}
```

- [ ] **Step 3: Run — must fail**

Run: `cargo test -p agnes-llm --test resolve`
Expected: FAIL — `resolve_provider` not found.

- [ ] **Step 4: Implement `crates/agnes-llm/src/resolve.rs`**

```rust
use crate::anthropic::AnthropicProvider;
use crate::error::LlmError;
use crate::openai::OpenAiCompatProvider;
use crate::provider::Provider;
use std::sync::Arc;

const DEFAULT_ANTHROPIC_MODEL: &str = "claude-haiku-4-5";

#[derive(Debug, Clone, Default)]
pub struct LlmCliOpts {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub base_url: Option<String>,
}

fn pick(flag: &Option<String>, env: &str) -> Option<String> {
    flag.clone().or_else(|| std::env::var(env).ok().filter(|s| !s.is_empty()))
}

pub fn resolve_provider(cli: &LlmCliOpts) -> Result<Arc<dyn Provider>, LlmError> {
    let provider = pick(&cli.provider, "AGNES_LLM_PROVIDER").ok_or(LlmError::MissingConfig {
        what: "provider selection",
        env_var: "AGNES_LLM_PROVIDER",
        flag: "--llm-provider",
    })?;
    let model = pick(&cli.model, "AGNES_LLM_MODEL");
    let base_url = pick(&cli.base_url, "AGNES_LLM_BASE_URL");
    let client = reqwest::Client::new();

    match provider.as_str() {
        "anthropic" => {
            let key = std::env::var("ANTHROPIC_API_KEY").ok().filter(|s| !s.is_empty())
                .ok_or(LlmError::MissingApiKey { env_var: "ANTHROPIC_API_KEY" })?;
            let model = model.unwrap_or_else(|| DEFAULT_ANTHROPIC_MODEL.to_string());
            Ok(Arc::new(AnthropicProvider::new(model, key, client)))
        }
        "openai" => {
            let key = std::env::var("OPENAI_API_KEY").ok().filter(|s| !s.is_empty())
                .ok_or(LlmError::MissingApiKey { env_var: "OPENAI_API_KEY" })?;
            let base = base_url.ok_or(LlmError::MissingConfig {
                what: "base_url",
                env_var: "AGNES_LLM_BASE_URL",
                flag: "--llm-base-url",
            })?;
            let model = model.ok_or(LlmError::MissingConfig {
                what: "model",
                env_var: "AGNES_LLM_MODEL",
                flag: "--llm-model",
            })?;
            Ok(Arc::new(OpenAiCompatProvider::new(model, key, base, client)))
        }
        other => Err(LlmError::UnknownProvider { name: other.to_string() }),
    }
}
```

Also extend `crates/agnes-llm/src/error.rs` — add:

```rust
    #[error("Unknown provider `{name}`.\n  Fix: use one of: `anthropic`, `openai`.")]
    UnknownProvider { name: String },
```

- [ ] **Step 5: Wire `lib.rs`**

Edit `crates/agnes-llm/src/lib.rs`:

```rust
mod resolve;
pub use resolve::{resolve_provider, LlmCliOpts};
```

- [ ] **Step 6: Run — must pass**

Run: `cargo test -p agnes-llm --test resolve`
Expected: PASS (5 tests).

- [ ] **Step 7: Commit**

```bash
jj describe -m "feat(llm): resolve_provider from CLI flags -> env -> defaults

Missing key/model/base_url produces error variants whose messages name the
exact env var and CLI flag to set. Anthropic model has a sensible default
(claude-haiku-4-5); openai-compat requires model + base_url explicitly.

Co-Authored-By: Claude <noreply@anthropic.com>"
jj new
```

---

### Task 5: Add `Tracer` trait + `execute_with` (runtime hook, backward-compatible)

**Files:**
- Modify: `crates/agnes-runtime/src/lib.rs` (add `Tracer`, `NoopTracer`, `execute_with`)
- Modify: `crates/agnes-runtime/src/scheduler.rs` (thread tracer through, hook `Tool` + `Llm` nodes)
- Test: `crates/agnes-runtime/tests/tracer.rs`

**Interfaces:**
- Consumes: `agnes_compiler::{NodeId, NodeKind}`, `agnes_types::Value`, `agnes_runtime::RuntimeError`.
- Produces:
  - `pub trait Tracer: Send + Sync { fn node_start(&self, id: NodeId, kind: &NodeKind, args_preview: &str); fn node_end(&self, id: NodeId, result: Result<&Value, &RuntimeError>, elapsed: Duration); }`.
  - `pub struct NoopTracer;` implementing `Tracer`.
  - `pub async fn execute_with(dag: &Dag, reg: &Registry, dispatch: &HashMap<String, ToolImpl>, tracer: &dyn Tracer) -> Result<Value, RuntimeError>`.
  - Existing `execute(...)` is now `execute_with(..., &NoopTracer)`.
- Hooks fire only for `NodeKind::Tool { .. }` and `NodeKind::Llm`. All other node kinds run silently.

- [ ] **Step 1: Write the failing test**

Create `crates/agnes-runtime/tests/tracer.rs`:

```rust
use agnes_builtins::register_builtins;
use agnes_compiler::{compile, NodeKind};
use agnes_parser::parse;
use agnes_registry::Registry;
use agnes_runtime::{execute_with, NoopTracer, Tracer, RuntimeError};
use agnes_types::Value;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Default)]
struct RecordingTracer {
    events: Arc<Mutex<Vec<String>>>,
}

impl Tracer for RecordingTracer {
    fn node_start(&self, _id: agnes_compiler::NodeId, kind: &NodeKind, args: &str) {
        let label = match kind {
            NodeKind::Tool { name } => format!("start tool:{name} args={args}"),
            NodeKind::Llm => format!("start llm args={args}"),
            _ => return,
        };
        self.events.lock().unwrap().push(label);
    }
    fn node_end(&self, _id: agnes_compiler::NodeId, result: Result<&Value, &RuntimeError>, _elapsed: Duration) {
        self.events
            .lock()
            .unwrap()
            .push(format!("end ok={}", result.is_ok()));
    }
}

// A tiny stub dispatch that doesn't need a Provider — the current
// runtime tests already construct dispatch maps by hand.
fn stub_dispatch() -> std::collections::HashMap<String, agnes_builtins::ToolImpl> {
    use agnes_types::Value;
    use serde_json::Value as JsonValue;
    use std::sync::Arc;
    let mut m = std::collections::HashMap::new();
    m.insert(
        "read-file".to_string(),
        Arc::new(|_args| Box::pin(async {
            Ok(Value::typed(JsonValue::String("hello".into()), "PlainText"))
        })) as agnes_builtins::ToolImpl,
    );
    m.insert(
        "summarize".to_string(),
        Arc::new(|_args| Box::pin(async {
            Ok(Value::typed(JsonValue::String("[SUMMARY]".into()), "Summary"))
        })) as agnes_builtins::ToolImpl,
    );
    m
}

#[tokio::test]
async fn tracer_receives_start_and_end_per_tool_node() {
    let src = r#"(pipe (tool read-file :path "x") (tool summarize))"#;
    let mut r = Registry::new();
    register_builtins(&mut r).unwrap();
    let p = parse(src).unwrap();
    r.load(&p).unwrap();
    agnes_checker::check(&p, &r).unwrap();
    let dag = compile(&p, &r).unwrap();
    let dispatch = stub_dispatch();

    let tracer = RecordingTracer::default();
    let _ = execute_with(&dag, &r, &dispatch, &tracer).await.unwrap();

    let ev = tracer.events.lock().unwrap().clone();
    // read-file start, read-file end, summarize start, summarize end (order preserved by pipe).
    assert_eq!(ev.len(), 4, "expected 4 events, got {ev:?}");
    assert!(ev[0].starts_with("start tool:read-file"));
    assert_eq!(ev[1], "end ok=true");
    assert!(ev[2].starts_with("start tool:summarize"));
    assert_eq!(ev[3], "end ok=true");
}

#[tokio::test]
async fn existing_execute_still_works_as_noop() {
    // Verifies backward compat: agnes_runtime::execute(...) unchanged.
    let src = r#"(tool read-file :path "x")"#;
    let mut r = Registry::new();
    register_builtins(&mut r).unwrap();
    let p = parse(src).unwrap();
    r.load(&p).unwrap();
    agnes_checker::check(&p, &r).unwrap();
    let dag = compile(&p, &r).unwrap();
    let dispatch = stub_dispatch();
    let v = agnes_runtime::execute(&dag, &r, &dispatch).await.unwrap();
    assert_eq!(v.data.as_str().unwrap(), "hello");
}
```

- [ ] **Step 2: Run — must fail**

Run: `cargo test -p agnes-runtime --test tracer`
Expected: FAIL — `execute_with` / `Tracer` / `NoopTracer` don't exist.

- [ ] **Step 3: Add the trait + wrapper to `agnes-runtime/src/lib.rs`**

Replace the current file:

```rust
//! agnes runtime: tokio async executor with boundary validation.

pub mod boundary;
pub mod error;
mod scheduler;

pub use error::RuntimeError;

use std::collections::HashMap;
use std::time::Duration;

use agnes_builtins::ToolImpl;
use agnes_compiler::{Dag, NodeId, NodeKind};
use agnes_registry::Registry;
use agnes_types::Value;

/// Observer for tool + llm node execution. Hooks fire only for
/// `NodeKind::Tool { .. }` and `NodeKind::Llm` — control-flow nodes are
/// silent. `args_preview` is a caller-formatted, truncation-friendly
/// summary of the arg map (no exact contract; consumers must tolerate any
/// human string).
pub trait Tracer: Send + Sync {
    fn node_start(&self, id: NodeId, kind: &NodeKind, args_preview: &str);
    fn node_end(&self, id: NodeId, result: Result<&Value, &RuntimeError>, elapsed: Duration);
}

/// Default no-op tracer used by the plain `execute()` entry point.
pub struct NoopTracer;

impl Tracer for NoopTracer {
    fn node_start(&self, _id: NodeId, _kind: &NodeKind, _args_preview: &str) {}
    fn node_end(&self, _id: NodeId, _result: Result<&Value, &RuntimeError>, _elapsed: Duration) {}
}

pub async fn execute(
    dag: &Dag,
    reg: &Registry,
    dispatch: &HashMap<String, ToolImpl>,
) -> Result<Value, RuntimeError> {
    execute_with(dag, reg, dispatch, &NoopTracer).await
}

pub async fn execute_with(
    dag: &Dag,
    reg: &Registry,
    dispatch: &HashMap<String, ToolImpl>,
    tracer: &dyn Tracer,
) -> Result<Value, RuntimeError> {
    scheduler::run(dag, reg, dispatch, tracer).await
}
```

- [ ] **Step 4: Thread tracer through `agnes-runtime/src/scheduler.rs`**

Change `pub async fn run(...)` to take a `tracer: &dyn Tracer`:

```rust
pub async fn run(
    dag: &Dag,
    reg: &Registry,
    dispatch: &HashMap<String, ToolImpl>,
    tracer: &dyn crate::Tracer,
) -> Result<Value, RuntimeError> {
    let mut cache: HashMap<NodeId, Value> = HashMap::new();
    let mut env: HashMap<String, Value> = HashMap::new();
    eval_node(dag, dag.root, reg, dispatch, tracer, &mut cache, &mut env).await
}
```

Extend `eval_node` (and `eval_input`, `collect_kwargs`) with a `tracer: &dyn crate::Tracer` parameter — plumb it into every recursive call. On the two arms that call `call_native`, replace them with the traced version below.

Replace the two arms:

```rust
NodeKind::Llm => {
    let args = collect_kwargs(dag, &node.inputs, reg, dispatch, tracer, cache, env).await?;
    call_native_traced(id, &node.kind, "llm", args, dispatch, reg, &node.provides, tracer).await?
}
// ...
NodeKind::Tool { name } => {
    let args = collect_kwargs(dag, &node.inputs, reg, dispatch, tracer, cache, env).await?;
    call_native_traced(id, &node.kind, name, args, dispatch, reg, &node.provides, tracer).await?
}
```

Add the traced helper next to `call_native`:

```rust
async fn call_native_traced(
    id: NodeId,
    kind: &NodeKind,
    tool: &str,
    args: HashMap<String, Value>,
    dispatch: &HashMap<String, ToolImpl>,
    reg: &Registry,
    provides: &TypeExpr,
    tracer: &dyn crate::Tracer,
) -> Result<Value, RuntimeError> {
    let preview = args_preview(&args);
    tracer.node_start(id, kind, &preview);
    let start = std::time::Instant::now();
    let out = call_native(tool, args, dispatch, reg, provides).await;
    let elapsed = start.elapsed();
    match &out {
        Ok(v) => tracer.node_end(id, Ok(v), elapsed),
        Err(e) => tracer.node_end(id, Err(e), elapsed),
    }
    out
}

fn args_preview(args: &HashMap<String, Value>) -> String {
    let mut kvs: Vec<(&String, &Value)> = args.iter().collect();
    kvs.sort_by(|a, b| a.0.cmp(b.0));
    let mut out = String::new();
    for (i, (k, v)) in kvs.iter().enumerate() {
        if i > 0 { out.push(' '); }
        let val = if let Some(s) = v.data.as_str() {
            let trimmed: String = s.chars().take(40).collect();
            format!(":{k}={trimmed:?}")
        } else {
            format!(":{k}=<{}>", v.declared_type)
        };
        out.push_str(&val);
    }
    out
}
```

- [ ] **Step 5: Fix `dispatch_define` recursion**

`dispatch_define` and `eval_expr` in `scheduler.rs` recurse via `call_native` — they don't need the tracer (the tracer sees the outer tool call already; inner define-body calls stay silent to keep the trace at one level). Leave those paths using `call_native` (untraced). No signature change on the AST-interpreter side.

- [ ] **Step 6: Run — must pass**

Run: `cargo test -p agnes-runtime`
Expected: PASS — all previous runtime tests + the new tracer tests.

- [ ] **Step 7: Commit**

```bash
jj describe -m "feat(runtime): additive Tracer trait + execute_with

Tracer::node_start/node_end fire per Tool/Llm node in the DAG scheduler.
NoopTracer is the default; execute() is now a thin wrapper over
execute_with(..., &NoopTracer). Every existing runtime test still passes.

Co-Authored-By: Claude <noreply@anthropic.com>"
jj new
```

---

### Task 6: Rewire `native_dispatch` to take a `Provider`; wire the three LLM tools + mock the four I/O tools

**Files:**
- Modify: `crates/agnes-builtins/Cargo.toml` — add `agnes-llm.workspace = true`.
- Modify: `crates/agnes-builtins/src/tools.rs` — signature change on `native_dispatch`; new implementations for all seven tools.
- Modify: `crates/agnes-builtins/tests/register.rs` — update the two call sites to pass a `MockProvider`.
- Modify: `crates/agnes-runtime/Cargo.toml` — add `agnes-llm = { workspace = true }` under `[dev-dependencies]` (test-only, for constructing MockProvider).
- Modify: `crates/agnes-runtime/tests/execute.rs` — update the ~5 call sites to pass a `MockProvider`; the two write-tempfile tests can go away (see Step 4) since `read-file` no longer touches disk.
- Modify: `crates/agnes-cli/Cargo.toml` — add `agnes-llm.workspace = true`.
- Modify: `crates/agnes-cli/src/main.rs` — the temporary top-level main (Task 10 restructures it) passes a `MockProvider` for now so this task remains locally testable.
- Modify: `crates/agnes-cli/tests/acceptance.rs` — update call site.

**Interfaces:**
- Consumes: `agnes_llm::{Provider, CompletionRequest, Message, Role, MockProvider}` from Task 1.
- Produces: `agnes_builtins::native_dispatch(provider: Arc<dyn Provider>) -> HashMap<String, ToolImpl>`. All later code (Session, CLI) calls this.

- [ ] **Step 1: Update `crates/agnes-builtins/Cargo.toml`**

Add under `[dependencies]`:

```toml
agnes-llm.workspace = true
```

- [ ] **Step 2: Write the failing routing test**

Create `crates/agnes-builtins/tests/dispatch_routing.rs`:

```rust
use agnes_builtins::native_dispatch;
use agnes_llm::MockProvider;
use agnes_types::Value;
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::sync::Arc;

fn args(kvs: &[(&str, &str)]) -> HashMap<String, Value> {
    kvs.iter()
        .map(|(k, v)| ((*k).into(), Value::typed(JsonValue::String((*v).into()), "PlainText")))
        .collect()
}

#[tokio::test]
async fn translate_routes_through_provider() {
    let mock = Arc::new(MockProvider::new(vec!["こんにちは".into()]));
    let d = native_dispatch(mock.clone());
    let out = (d["translate"])(args(&[("input", "hello world"), ("lang", "ja")])).await.unwrap();
    assert_eq!(out.data.as_str().unwrap(), "こんにちは");
    let seen = mock.seen();
    assert_eq!(seen.len(), 1);
    let req = &seen[0];
    let sys = req.system.as_deref().unwrap_or("");
    assert!(sys.contains("translator"), "system prompt should identify translator, got: {sys}");
    assert_eq!(req.messages.len(), 1);
    let user = &req.messages[0].content;
    assert!(user.contains("Translate to ja"), "user prompt should name target lang, got: {user}");
    assert!(user.contains("hello world"), "user prompt should carry input, got: {user}");
}

#[tokio::test]
async fn summarize_routes_through_provider() {
    let mock = Arc::new(MockProvider::new(vec!["one-para summary".into()]));
    let d = native_dispatch(mock.clone());
    let out = (d["summarize"])(args(&[("input", "long body...")])).await.unwrap();
    assert_eq!(out.data.as_str().unwrap(), "one-para summary");
    assert_eq!(out.declared_type.to_string(), "Summary");
}

#[tokio::test]
async fn llm_routes_through_provider() {
    let mock = Arc::new(MockProvider::new(vec!["result".into()]));
    let d = native_dispatch(mock.clone());
    let out = (d["llm"])(args(&[("prompt", "answer this"), ("input", "context")])).await.unwrap();
    assert_eq!(out.data.as_str().unwrap(), "result");
    let seen = mock.seen();
    assert!(seen[0].system.is_none(), "llm tool sends no system prompt");
    assert!(seen[0].messages[0].content.contains("answer this"));
    assert!(seen[0].messages[0].content.contains("context"));
}

#[tokio::test]
async fn read_file_returns_mock_content_for_known_and_placeholder_for_unknown() {
    let mock = Arc::new(MockProvider::new(vec![]));
    let d = native_dispatch(mock);
    let known = (d["read-file"])(args(&[("path", "README.md")])).await.unwrap();
    assert!(known.data.as_str().unwrap().contains("agnes"), "seeded README should mention agnes");

    let unknown = (d["read-file"])(args(&[("path", "does-not-exist.md")])).await.unwrap();
    let s = unknown.data.as_str().unwrap();
    assert!(s.contains("[MOCK file at does-not-exist.md"), "got: {s}");
}

#[tokio::test]
async fn write_file_does_not_touch_disk_and_records_call() {
    use std::path::Path;
    let mock = Arc::new(MockProvider::new(vec![]));
    let d = native_dispatch(mock);
    let out = (d["write-file"])(args(&[("path", "/tmp/definitely-not-created-by-mock-agnes.txt"), ("content", "abc")])).await.unwrap();
    assert!(out.data.is_null(), "write-file returns Unit (null JSON)");
    assert!(!Path::new("/tmp/definitely-not-created-by-mock-agnes.txt").exists(),
        "mock write-file must not touch disk");
}

#[tokio::test]
async fn ocr_returns_fixed_placeholder() {
    let mock = Arc::new(MockProvider::new(vec![]));
    let d = native_dispatch(mock);
    let out = (d["ocr"])(args(&[("source", "any.pdf")])).await.unwrap();
    let s = out.data.as_str().unwrap();
    assert!(!s.is_empty(), "ocr must return some canned sentence");
    assert_eq!(out.declared_type.to_string(), "PlainText");
}
```

- [ ] **Step 3: Run — must fail with signature mismatch**

Run: `cargo test -p agnes-builtins --test dispatch_routing`
Expected: FAIL — `native_dispatch()` currently takes 0 args.

- [ ] **Step 4: Rewrite `crates/agnes-builtins/src/tools.rs`**

Replace the entire file:

```rust
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};

use agnes_llm::{CompletionRequest, Message, Provider, Role};
use agnes_types::Value;
use serde_json::Value as JsonValue;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
pub type ToolImpl =
    Arc<dyn Fn(HashMap<String, Value>) -> BoxFuture<'static, Result<Value, String>> + Send + Sync>;

/// Per-process recording of every mock write-file call, so a CLI/sink can
/// print a "would-have-written" summary at end of turn.
pub fn writes() -> &'static Mutex<Vec<(String, usize)>> {
    static WRITES: OnceLock<Mutex<Vec<(String, usize)>>> = OnceLock::new();
    WRITES.get_or_init(|| Mutex::new(Vec::new()))
}

const MAX_TOKENS: u32 = 1024;

const MOCK_README: &str = "# agnes\n\nA Lisp-style DSL and Rust runtime for LLM-planned agent workflows, with a TypeScript-style semantic type system.";
const MOCK_NOTES: &str = "TODO(agnes): example note fixtures live here so demos don't need real disk I/O.";
const MOCK_DRAFT: &str = "Draft: agnes lets an LLM plan a workflow as a small DSL and hand it to a typed Rust runtime.";

fn read_file_mock(path: &str) -> String {
    match path {
        "README.md" => MOCK_README.into(),
        "NOTES.md" => MOCK_NOTES.into(),
        "draft.md" => MOCK_DRAFT.into(),
        other => format!("[MOCK file at {other}: agnes is a Lisp-style DSL for LLM-planned agent workflows. Placeholder body — swap in seeded content by editing MOCK_* constants in agnes-builtins.]"),
    }
}

fn as_str(v: &Value) -> Option<String> {
    v.data.as_str().map(str::to_string)
}

fn arg_str(args: &HashMap<String, Value>, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(as_str)
        .ok_or_else(|| format!("missing :{key}"))
}

pub fn native_dispatch(provider: Arc<dyn Provider>) -> HashMap<String, ToolImpl> {
    let mut m: HashMap<String, ToolImpl> = HashMap::new();

    // read-file (mock, no disk)
    m.insert(
        "read-file".into(),
        Arc::new(|args| {
            Box::pin(async move {
                let path = arg_str(&args, "path")?;
                Ok(Value::typed(JsonValue::String(read_file_mock(&path)), "PlainText"))
            })
        }),
    );

    // write-file (mock: record and return Unit)
    m.insert(
        "write-file".into(),
        Arc::new(|args| {
            Box::pin(async move {
                let path = arg_str(&args, "path")?;
                let content = arg_str(&args, "content")?;
                writes().lock().unwrap().push((path, content.len()));
                Ok(Value::typed(JsonValue::Null, "Unit"))
            })
        }),
    );

    // ocr (mock: fixed sentence)
    m.insert(
        "ocr".into(),
        Arc::new(|args| {
            Box::pin(async move {
                let _ = arg_str(&args, "source")?;
                Ok(Value::typed(
                    JsonValue::String("Extracted text: agnes runtime dispatches LLM-planned workflows.".into()),
                    "PlainText",
                ))
            })
        }),
    );

    // join-lines (real, kept)
    m.insert(
        "join-lines".into(),
        Arc::new(|args| {
            Box::pin(async move {
                let lines = args
                    .get("lines")
                    .ok_or_else(|| "missing :lines".to_string())?
                    .data
                    .as_array()
                    .ok_or_else(|| "lines is not a JSON array".to_string())?
                    .iter()
                    .map(|v| v.as_str().unwrap_or("").to_string())
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(Value::typed(JsonValue::String(lines), "PlainText"))
            })
        }),
    );

    // llm (real provider call)
    {
        let p = provider.clone();
        m.insert(
            "llm".into(),
            Arc::new(move |args| {
                let p = p.clone();
                Box::pin(async move {
                    let prompt = arg_str(&args, "prompt")?;
                    let input = args.get("input").and_then(as_str).unwrap_or_default();
                    let user = if input.is_empty() {
                        prompt
                    } else {
                        format!("{prompt}\n\n{input}")
                    };
                    let out = p.complete(CompletionRequest {
                        system: None,
                        messages: vec![Message { role: Role::User, content: user }],
                        max_tokens: MAX_TOKENS,
                    }).await.map_err(|e| e.to_string())?;
                    Ok(Value::typed(JsonValue::String(out), "PlainText"))
                })
            }),
        );
    }

    // summarize (real provider call)
    {
        let p = provider.clone();
        m.insert(
            "summarize".into(),
            Arc::new(move |args| {
                let p = p.clone();
                Box::pin(async move {
                    let input = arg_str(&args, "input")?;
                    let out = p.complete(CompletionRequest {
                        system: Some("You are a concise summarizer. Return one paragraph.".into()),
                        messages: vec![Message {
                            role: Role::User,
                            content: format!("Summarize the following:\n\n{input}"),
                        }],
                        max_tokens: MAX_TOKENS,
                    }).await.map_err(|e| e.to_string())?;
                    Ok(Value::typed(JsonValue::String(out), "Summary"))
                })
            }),
        );
    }

    // translate (real provider call)
    {
        let p = provider.clone();
        m.insert(
            "translate".into(),
            Arc::new(move |args| {
                let p = p.clone();
                Box::pin(async move {
                    let input = arg_str(&args, "input")?;
                    let lang = arg_str(&args, "lang")?;
                    let out = p.complete(CompletionRequest {
                        system: Some("You are a professional translator.".into()),
                        messages: vec![Message {
                            role: Role::User,
                            content: format!("Translate to {lang}. Output only the translation.\n\n{input}"),
                        }],
                        max_tokens: MAX_TOKENS,
                    }).await.map_err(|e| e.to_string())?;
                    Ok(Value::typed(JsonValue::String(out), "PlainText"))
                })
            }),
        );
    }

    m
}
```

- [ ] **Step 5: Update existing call sites**

**`crates/agnes-builtins/tests/register.rs`** — the two `native_dispatch()` calls become:

```rust
use std::sync::Arc;
let mock = Arc::new(agnes_llm::MockProvider::new(vec![]));
let d = native_dispatch(mock);
```

(Test still just checks map keys — no LLM call happens.)

**`crates/agnes-runtime/tests/execute.rs`** — update all five call sites the same way, and:

- The two tests that touched disk (`runs_read_then_summarize`, `runs_a_defined_compound_tool`, `boundary_validates_list_of_union_at_runtime`) no longer need `tempfile_path()` / `tokio::fs::write` — `read-file` reads from the in-memory mock. Change the source strings to use seeded paths (`"README.md"` / `"NOTES.md"`) and delete the tempfile write/cleanup lines.
- `evaluates_list_literal` doesn't touch `read-file` at all — just swap `native_dispatch()` for `native_dispatch(mock)` and leave the rest alone.
- For `runs_read_then_summarize` and `runs_a_defined_compound_tool`, feed `MockProvider::new(vec!["[SUMMARY]".into()])` so `summarize` returns that literal, and adjust the assertion to `assert_eq!(s, "[SUMMARY]")`.
- Add `agnes-llm.workspace = true` to `crates/agnes-runtime/Cargo.toml` `[dev-dependencies]`.
- Delete `fn tempfile_path()` from the file — nothing calls it anymore.

Concrete updated body of `runs_read_then_summarize`:

```rust
#[tokio::test]
async fn runs_read_then_summarize() {
    let src = r#"(pipe (tool read-file :path "README.md") (tool summarize))"#;
    let mut r = agnes_registry::Registry::new();
    agnes_builtins::register_builtins(&mut r).unwrap();

    let p = agnes_parser::parse(src).unwrap();
    r.load(&p).unwrap();
    agnes_checker::check(&p, &r).unwrap();
    let dag = agnes_compiler::compile(&p, &r).unwrap();

    let mock = std::sync::Arc::new(agnes_llm::MockProvider::new(vec!["[SUMMARY]".into()]));
    let dispatch = agnes_builtins::native_dispatch(mock);
    let out = agnes_runtime::execute(&dag, &r, &dispatch).await.expect("run ok");
    assert_eq!(out.data.as_str().unwrap(), "[SUMMARY]");
}
```

Apply the analogous transformation to the other tests. Delete `tempfile_path()` from the file.

**`crates/agnes-cli/tests/acceptance.rs`** — update the one `native_dispatch()` call site the same way. Read the test first (`crates/agnes-cli/tests/acceptance.rs`) to see which source strings it uses; substitute mock paths (`README.md` etc.) if the test writes to disk.

**`crates/agnes-cli/src/main.rs`** — temporary shim so this task can be run in isolation (Task 10 rewrites this file):

```rust
use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).init();
    let path = std::env::args().nth(1).ok_or_else(|| anyhow::anyhow!("usage: agnes <file.agnes>"))?;
    let src = tokio::fs::read_to_string(std::path::PathBuf::from(&path)).await?;
    let mut reg = agnes_registry::Registry::new();
    agnes_builtins::register_builtins(&mut reg)?;
    let program = agnes_parser::parse(&src).map_err(|e| anyhow::anyhow!("{e}"))?;
    reg.load(&program).map_err(|e| anyhow::anyhow!("{e}"))?;
    agnes_checker::check(&program, &reg).map_err(|e| anyhow::anyhow!("{e}"))?;
    let dag = agnes_compiler::compile(&program, &reg).map_err(|e| anyhow::anyhow!("{e}"))?;
    // Temporary MockProvider so the workspace stays green through this task.
    // Task 10 replaces this with real provider resolution.
    let mock: Arc<dyn agnes_llm::Provider> = Arc::new(agnes_llm::MockProvider::new(vec![
        "[LLM output placeholder for CLI shim]".into();
        16
    ]));
    let dispatch = agnes_builtins::native_dispatch(mock);
    let result = agnes_runtime::execute(&dag, &reg, &dispatch).await.map_err(|e| anyhow::anyhow!("{e}"))?;
    println!("{}", result.data);
    Ok(())
}
```

- [ ] **Step 6: Run — must pass**

Run: `cargo test --workspace`
Expected: PASS — all pre-existing tests plus the 6 new routing tests.

- [ ] **Step 7: Commit**

```bash
jj describe -m "feat(builtins): native_dispatch takes Arc<dyn Provider>

Three LLM tools (llm/summarize/translate) now call the provider; four
non-LLM tools (read-file/write-file/ocr/join-lines) are in-memory mocks
so demos run without a filesystem. read-file returns a fixed placeholder
for unknown paths; write-file records to a process-global writes() log.
Existing tests updated to pass a MockProvider.

Co-Authored-By: Claude <noreply@anthropic.com>"
jj new
```

---

### Task 7: DSL extractor helper in `agnes-llm`

**Files:**
- Create: `crates/agnes-llm/src/dsl_extract.rs`
- Modify: `crates/agnes-llm/src/lib.rs` (add `mod dsl_extract; pub use dsl_extract::extract_dsl;`)
- Test: `crates/agnes-llm/tests/dsl_extract.rs`

**Interfaces:**
- Consumes: nothing external.
- Produces: `pub fn extract_dsl(raw: &str) -> String`. Used by `Planner` in Task 8 to peel the fenced code block off a raw LLM response.

- [ ] **Step 1: Write the failing test**

```rust
use agnes_llm::extract_dsl;

#[test]
fn peels_fenced_agnes_block() {
    let raw = "Sure! Here you go:\n\n```agnes\n(pipe (tool read-file :path \"README.md\"))\n```\n\nHope that helps.";
    let out = extract_dsl(raw);
    assert_eq!(out, "(pipe (tool read-file :path \"README.md\"))");
}

#[test]
fn peels_fenced_block_without_lang_tag() {
    let raw = "```\n(tool llm :prompt \"hi\")\n```";
    let out = extract_dsl(raw);
    assert_eq!(out, "(tool llm :prompt \"hi\")");
}

#[test]
fn passes_through_when_no_fence() {
    let raw = "(tool llm :prompt \"hi\")";
    assert_eq!(extract_dsl(raw), raw);
}

#[test]
fn picks_first_agnes_fence_when_multiple() {
    let raw = "```agnes\nA\n```\n```agnes\nB\n```";
    assert_eq!(extract_dsl(raw), "A");
}
```

Save as `crates/agnes-llm/tests/dsl_extract.rs`.

- [ ] **Step 2: Run — must fail**

Run: `cargo test -p agnes-llm --test dsl_extract`
Expected: FAIL — `extract_dsl` not found.

- [ ] **Step 3: Implement `crates/agnes-llm/src/dsl_extract.rs`**

```rust
/// Peel an ```agnes``` (or bare ```) fenced block out of an LLM response.
/// Preference: first ```agnes``` block, else first ``` block, else the
/// full string trimmed. Never errors — the parser downstream will
/// produce a proper error if the extracted content isn't valid agnes.
pub fn extract_dsl(raw: &str) -> String {
    if let Some(block) = fenced(raw, "```agnes") {
        return block;
    }
    if let Some(block) = fenced(raw, "```") {
        return block;
    }
    raw.trim().to_string()
}

fn fenced(raw: &str, open: &str) -> Option<String> {
    let start = raw.find(open)?;
    let after_open = &raw[start + open.len()..];
    // Skip an optional trailing tag line on the opener: "agnes\n" or just "\n".
    let after_line = match after_open.find('\n') {
        Some(nl) => &after_open[nl + 1..],
        None => after_open,
    };
    let end = after_line.find("```")?;
    Some(after_line[..end].trim().to_string())
}
```

- [ ] **Step 4: Wire and run — must pass**

Edit `crates/agnes-llm/src/lib.rs` — add `mod dsl_extract; pub use dsl_extract::extract_dsl;`.
Run: `cargo test -p agnes-llm --test dsl_extract`
Expected: PASS (4 tests).

- [ ] **Step 5: Commit**

```bash
jj describe -m "feat(llm): DSL fenced-block extractor for planner responses

extract_dsl peels the first \`\`\`agnes ... \`\`\` block from a raw LLM
response, falls back to any \`\`\` block, and passes through unchanged
if no fence is present. Used by the planner in Task 8.

Co-Authored-By: Claude <noreply@anthropic.com>"
jj new
```

---

### Task 8: `Planner` — NL → DSL with history + system-prompt catalogue

**Files:**
- Create: `crates/agnes-llm/src/planner.rs`
- Modify: `crates/agnes-llm/src/lib.rs` (add `mod planner; pub use planner::{Planner, PlannerError, Turn};`)
- Modify: `crates/agnes-llm/src/error.rs` — add `PlannerError`.
- Modify: `crates/agnes-llm/Cargo.toml` — add `agnes-registry.workspace = true` (planner reads tool sigs from the registry).
- Test: `crates/agnes-llm/tests/planner.rs`

**Interfaces:**
- Consumes: `agnes_registry::Registry`, `Provider`, `CompletionRequest`, `Message`, `Role`, `MockProvider`, `extract_dsl`.
- Produces:
  - `pub struct Turn { pub user_nl: String, pub assistant_dsl: String, pub result_preview: String }`.
  - `pub struct Planner { /* private */ }`.
  - `Planner::new(provider: Arc<dyn Provider>, registry: &Registry) -> Self`.
  - `Planner::plan(&mut self, nl: &str) -> Result<String, PlannerError>` — appends `(user_nl, response)` to a scratch buffer that `push_error_feedback` / `record_result` finalize into history.
  - `Planner::push_error_feedback(&mut self, bad_dsl: String, err: String)` — appends an `assistant(bad_dsl)` then a `user("That failed with: <err>. Fix and try again; output only the corrected DSL.")` to the scratch buffer.
  - `Planner::record_result(&mut self, dsl: String, result_preview: String)` — commits the current turn into `history` and clears scratch.
  - `Planner::history(&self) -> &[Turn]`.
  - `Planner::reset_history(&mut self)`.
- Cap: last **6 turns kept verbatim** = up to 12 (user, assistant) messages. Older turns collapse into a prefix line prepended to the system prompt.

- [ ] **Step 1: Extend `crates/agnes-llm/src/error.rs`**

Add:

```rust
#[derive(Debug, Error)]
pub enum PlannerError {
    #[error(transparent)]
    Llm(#[from] LlmError),

    #[error("planner produced empty response after DSL extraction")]
    EmptyResponse,
}
```

- [ ] **Step 2: Write the failing planner test**

Create `crates/agnes-llm/tests/planner.rs`:

```rust
use agnes_builtins::register_builtins;
use agnes_llm::{extract_dsl, MockProvider, Planner, Provider, Turn};
use agnes_registry::Registry;
use std::sync::Arc;

fn reg_with_builtins() -> Registry {
    let mut r = Registry::new();
    register_builtins(&mut r).unwrap();
    r
}

#[tokio::test]
async fn planner_returns_extracted_dsl() {
    let raw = "Sure:\n\n```agnes\n(tool read-file :path \"README.md\")\n```";
    let mock: Arc<dyn Provider> = Arc::new(MockProvider::new(vec![raw.into()]));
    let reg = reg_with_builtins();
    let mut p = Planner::new(mock, &reg);
    let dsl = p.plan("read the readme").await.unwrap();
    assert_eq!(dsl, "(tool read-file :path \"README.md\")");
}

#[tokio::test]
async fn planner_system_prompt_lists_every_tool() {
    let mock = Arc::new(MockProvider::new(vec!["```agnes\n(tool read-file :path \"a\")\n```".into()]));
    let reg = reg_with_builtins();
    let mut p = Planner::new(mock.clone(), &reg);
    let _ = p.plan("do stuff").await.unwrap();
    let seen = mock.seen();
    let sys = seen[0].system.as_deref().unwrap();
    for name in ["read-file", "write-file", "summarize", "translate", "ocr", "llm", "join-lines"] {
        assert!(sys.contains(name), "system prompt must list `{name}`; got: {sys}");
    }
}

#[tokio::test]
async fn planner_feeds_error_back_on_retry() {
    let mock = Arc::new(MockProvider::new(vec![
        "```agnes\nBROKEN\n```".into(),
        "```agnes\n(tool read-file :path \"README.md\")\n```".into(),
    ]));
    let reg = reg_with_builtins();
    let mut p = Planner::new(mock.clone(), &reg);

    let _ = p.plan("read readme").await.unwrap();
    p.push_error_feedback("BROKEN".into(), "syntax error at 1:1".into());
    let dsl2 = p.plan("read readme").await.unwrap();
    assert_eq!(dsl2, "(tool read-file :path \"README.md\")");

    let seen = mock.seen();
    let second = &seen[1];
    // The second call's message chain includes the previous bad DSL + the
    // "That failed with:" user turn.
    let chain: Vec<String> = second.messages.iter().map(|m| m.content.clone()).collect();
    let joined = chain.join("\n---\n");
    assert!(joined.contains("BROKEN"), "chain must carry the previous bad DSL; got: {joined}");
    assert!(joined.contains("That failed with"), "chain must carry the error hint; got: {joined}");
}

#[tokio::test]
async fn record_result_commits_a_turn_and_scratch_clears() {
    let mock = Arc::new(MockProvider::new(vec!["```agnes\n(tool ocr :source \"a.pdf\")\n```".into()]));
    let reg = reg_with_builtins();
    let mut p = Planner::new(mock, &reg);
    let _ = p.plan("ocr something").await.unwrap();
    p.record_result("(tool ocr :source \"a.pdf\")".into(), "Extracted text: ...".into());
    let hist = p.history();
    assert_eq!(hist.len(), 1);
    assert_eq!(hist[0].user_nl, "ocr something");
    assert!(hist[0].assistant_dsl.contains("ocr"));
}
```

Also update the crate's `[dev-dependencies]` in `crates/agnes-llm/Cargo.toml`:

```toml
[dev-dependencies]
tokio = { workspace = true, features = ["macros", "rt-multi-thread"] }
agnes-registry.workspace = true
agnes-builtins.workspace = true
```

- [ ] **Step 3: Run — must fail**

Run: `cargo test -p agnes-llm --test planner`
Expected: FAIL — `Planner` / `Turn` don't exist.

- [ ] **Step 4: Update `crates/agnes-llm/Cargo.toml` (regular deps)**

Under `[dependencies]` add:

```toml
agnes-registry.workspace = true
agnes-types.workspace    = true
```

- [ ] **Step 5: Implement `crates/agnes-llm/src/planner.rs`**

```rust
use crate::dsl_extract::extract_dsl;
use crate::error::PlannerError;
use crate::provider::{CompletionRequest, Message, Provider, Role};
use agnes_registry::Registry;
use agnes_types::ToolSignature;
use std::sync::Arc;

const MAX_TURNS_VERBATIM: usize = 6;
const PLANNER_MAX_TOKENS: u32 = 2048;

#[derive(Debug, Clone)]
pub struct Turn {
    pub user_nl: String,
    pub assistant_dsl: String,
    pub result_preview: String,
}

/// Draft-buffer entry: raw messages appended during this in-flight turn.
#[derive(Debug, Clone)]
enum Scratch {
    User(String),
    Assistant(String),
}

pub struct Planner {
    provider: Arc<dyn Provider>,
    system: String,
    history: Vec<Turn>,
    /// Uncommitted messages for the turn currently being planned.
    scratch: Vec<Scratch>,
    /// The natural-language prompt that started the current in-flight turn.
    pending_nl: Option<String>,
}

impl Planner {
    pub fn new(provider: Arc<dyn Provider>, registry: &Registry) -> Self {
        Self {
            provider,
            system: build_system_prompt(registry),
            history: Vec::new(),
            scratch: Vec::new(),
            pending_nl: None,
        }
    }

    pub fn history(&self) -> &[Turn] {
        &self.history
    }

    pub fn reset_history(&mut self) {
        self.history.clear();
        self.scratch.clear();
        self.pending_nl = None;
    }

    /// Plan the DSL for `nl`. Call again after `push_error_feedback` to
    /// retry with the previous bad DSL and error in the message chain.
    pub async fn plan(&mut self, nl: &str) -> Result<String, PlannerError> {
        if self.pending_nl.is_none() {
            self.pending_nl = Some(nl.to_string());
            self.scratch.push(Scratch::User(nl.to_string()));
        } else if !nl.is_empty() && self.pending_nl.as_deref() != Some(nl) {
            // Same turn, different NL text should not usually happen — treat as
            // a fresh user turn appended to the scratch.
            self.scratch.push(Scratch::User(nl.to_string()));
        }

        let req = CompletionRequest {
            system: Some(self.effective_system()),
            messages: self.build_messages(),
            max_tokens: PLANNER_MAX_TOKENS,
        };
        let raw = self.provider.complete(req).await?;
        let dsl = extract_dsl(&raw);
        if dsl.is_empty() {
            return Err(PlannerError::EmptyResponse);
        }
        self.scratch.push(Scratch::Assistant(dsl.clone()));
        Ok(dsl)
    }

    pub fn push_error_feedback(&mut self, bad_dsl: String, err: String) {
        // The most recent scratch entry is the assistant's DSL; the caller
        // may have already popped it via `plan`. Overwrite/replace as needed.
        // Simpler: append a fresh assistant echo and a user follow-up.
        self.scratch.push(Scratch::Assistant(bad_dsl));
        self.scratch.push(Scratch::User(format!(
            "That failed with: {err}\n\nFix and try again; output only the corrected DSL inside a ```agnes fenced block."
        )));
    }

    pub fn record_result(&mut self, dsl: String, result_preview: String) {
        let user_nl = self.pending_nl.take().unwrap_or_default();
        self.history.push(Turn { user_nl, assistant_dsl: dsl, result_preview });
        self.scratch.clear();
    }

    fn effective_system(&self) -> String {
        // Collapse anything beyond the last MAX_TURNS_VERBATIM into a
        // prefix line prepended to the system prompt.
        let n = self.history.len();
        if n <= MAX_TURNS_VERBATIM {
            return self.system.clone();
        }
        let extras: &[Turn] = &self.history[..n - MAX_TURNS_VERBATIM];
        let mut prefix = String::from("<prior context:\n");
        for t in extras {
            prefix.push_str(&format!(
                "  - user asked {:?}, produced {}-line DSL, result was {} chars\n",
                t.user_nl,
                t.assistant_dsl.lines().count(),
                t.result_preview.chars().count(),
            ));
        }
        prefix.push_str(">\n\n");
        prefix.push_str(&self.system);
        prefix
    }

    fn build_messages(&self) -> Vec<Message> {
        let mut out = Vec::new();
        // Verbatim slice of recent history.
        let n = self.history.len();
        let start = n.saturating_sub(MAX_TURNS_VERBATIM);
        for t in &self.history[start..] {
            out.push(Message { role: Role::User, content: t.user_nl.clone() });
            out.push(Message { role: Role::Assistant, content: format!("```agnes\n{}\n```", t.assistant_dsl) });
        }
        // Then the scratch buffer for the in-flight turn.
        for s in &self.scratch {
            match s {
                Scratch::User(c) => out.push(Message { role: Role::User, content: c.clone() }),
                Scratch::Assistant(c) => out.push(Message {
                    role: Role::Assistant,
                    content: format!("```agnes\n{c}\n```"),
                }),
            }
        }
        out
    }
}

fn build_system_prompt(registry: &Registry) -> String {
    let mut s = String::new();
    s.push_str("You are the agnes DSL planner. Given a user goal, produce an agnes program that achieves it using the registered tools.\n\n");
    s.push_str("Output ONLY an ```agnes fenced code block containing the program — no prose, no explanation.\n\n");
    s.push_str("agnes forms:\n");
    s.push_str("  (pipe expr1 expr2 ...)                 sequential flow; each step's output becomes the next step's implicit input\n");
    s.push_str("  (par branch1 branch2 ...)              parallel branches; each branch's value is discarded (use `let` inside)\n");
    s.push_str("  (let name expr)                        bind expr's value to `name` (or bind the piped-in value if expr omitted)\n");
    s.push_str("  (tool NAME :key value :key value ...)  call a tool; kwargs match the tool's `requires` param names\n");
    s.push_str("  (list e1 e2 ...)                       or bracket literal [e1 e2 ...]\n");
    s.push_str("  (if cond then else) / (match scrutinee (pat arm) ...) / (retry N body) / (catch body fallback)\n");
    s.push_str("  Literals: strings \"...\", ints, true/false, nil.\n\n");
    s.push_str("Registered tools:\n");
    // The registry doesn't expose iteration; we synthesize the catalog by
    // asking for each known tool name in a fixed order. In practice all
    // callers register the 7 builtins, so this list matches.
    for name in [
        "read-file", "write-file", "summarize", "translate", "ocr", "llm", "join-lines",
    ] {
        if let Some(sig) = registry.tool_signature(name) {
            s.push_str(&format!("  {name} :: {}\n", format_sig(sig)));
        }
    }
    s.push('\n');
    s.push_str("Examples:\n\n");
    s.push_str("  goal: read the readme and summarize it\n");
    s.push_str("  ```agnes\n  (pipe (tool read-file :path \"README.md\") (tool summarize))\n  ```\n\n");
    s.push_str("  goal: translate the readme into Japanese and English, then join\n");
    s.push_str("  ```agnes\n  (pipe\n    (par\n      (let ja (pipe (tool read-file :path \"README.md\") (tool translate :lang \"ja\")))\n      (let en (pipe (tool read-file :path \"README.md\") (tool translate :lang \"en\"))))\n    (tool join-lines :lines [ja en]))\n  ```\n");
    s
}

fn format_sig(sig: &ToolSignature) -> String {
    let params: Vec<String> = sig
        .requires
        .iter()
        .map(|(n, t)| format!("({n} {t})"))
        .collect();
    format!("{} -> {}", params.join(" "), sig.provides)
}
```

- [ ] **Step 6: Wire `lib.rs`**

Edit `crates/agnes-llm/src/lib.rs`:

```rust
mod planner;
pub use planner::{Planner, Turn};
pub use error::PlannerError;
```

- [ ] **Step 7: Run — must pass**

Run: `cargo test -p agnes-llm`
Expected: PASS — all agnes-llm tests including the 4 new planner tests.

- [ ] **Step 8: Commit**

```bash
jj describe -m "feat(llm): Planner (NL -> agnes DSL) with error-feedback retry

System prompt lists every registered tool sig plus DSL forms + two
few-shot examples. Multi-turn history capped at 6 recent turns
verbatim; older turns collapsed into a 'prior context' prefix.
push_error_feedback + a second plan() call form the retry primitive
used by Session::run_turn in Task 9.

Co-Authored-By: Claude <noreply@anthropic.com>"
jj new
```

---

### Task 9: `agnes-session` — headless engine

**Files:**
- Create: `crates/agnes-session/Cargo.toml`
- Create: `crates/agnes-session/src/lib.rs`
- Create: `crates/agnes-session/src/error.rs`
- Create: `crates/agnes-session/src/events.rs`
- Create: `crates/agnes-session/src/plan_tree.rs`
- Create: `crates/agnes-session/src/tracer_bridge.rs`
- Create: `crates/agnes-session/src/session.rs`
- Modify: `Cargo.toml` (workspace root) — add member `crates/agnes-session` and `agnes-session = { path = "crates/agnes-session" }` under workspace deps.
- Test: `crates/agnes-session/tests/session_end_to_end.rs`

**Interfaces:**
- Consumes: `Provider`, `MockProvider`, `Planner`, `PlannerError` (agnes-llm); `Registry`, `native_dispatch`, `register_builtins`, `Tracer`, `execute_with`; parser + checker + compiler.
- Produces:
  - `pub struct Session { /* provider, registry, dispatch, planner */ }`.
  - `pub enum TurnInput { NaturalLanguage(String), RawDsl(String) }`.
  - `pub enum SessionEvent { PlannerStart, PlannerRetry { attempt: u8, error: String }, DslProduced { source: String }, PlanReady { tree: PlanTree }, NodeStart { id: u32, kind: NodeKindTag, args: Vec<(String, String)> }, NodeEnd { id: u32, ok: bool, preview: String, elapsed_ms: u64 }, TurnResult { value_preview: String, value_type: String }, TurnFailed { error: String } }`.
  - `pub struct PlanTree { pub kind: String, pub label: String, pub provides: Option<String>, pub children: Vec<PlanTree> }`.
  - `pub enum NodeKindTag { Tool { name: String }, Llm }`.
  - `#[async_trait::async_trait] pub trait EventSink: Send { async fn emit(&mut self, ev: SessionEvent); }`.
  - `Session::new(provider: Arc<dyn Provider>) -> Result<Self, SessionError>`.
  - `Session::run_turn(&mut self, input: TurnInput, sink: &mut dyn EventSink) -> Result<Value, SessionError>` (async).
  - `Session::history(&self) -> &[Turn]` and `Session::reset_history(&mut self)` (thin wrappers over `Planner`).

- [ ] **Step 1: Create `crates/agnes-session/Cargo.toml`**

```toml
[package]
name = "agnes-session"
edition.workspace = true
version.workspace = true
license.workspace = true
authors.workspace = true

[dependencies]
agnes-llm.workspace       = true
agnes-builtins.workspace  = true
agnes-parser.workspace    = true
agnes-checker.workspace   = true
agnes-compiler.workspace  = true
agnes-registry.workspace  = true
agnes-runtime.workspace   = true
agnes-types.workspace     = true
agnes-ast.workspace       = true
async-trait.workspace     = true
thiserror.workspace       = true
tokio.workspace           = true
serde_json.workspace      = true

[dev-dependencies]
tokio = { workspace = true, features = ["macros", "rt-multi-thread"] }
```

Register the crate in workspace root `Cargo.toml`: append `"crates/agnes-session"` to `members`, and add `agnes-session = { path = "crates/agnes-session" }` under `[workspace.dependencies]`.

Also add `agnes-ast.workspace = true` under `[workspace.dependencies]` if not already present — check first.

- [ ] **Step 2: Write the failing end-to-end test**

Create `crates/agnes-session/tests/session_end_to_end.rs`:

```rust
use agnes_llm::{MockProvider, Provider};
use agnes_session::{EventSink, Session, SessionEvent, TurnInput};
use std::sync::Arc;

struct CollectSink(pub Vec<SessionEvent>);

#[async_trait::async_trait]
impl EventSink for CollectSink {
    async fn emit(&mut self, ev: SessionEvent) {
        self.0.push(ev);
    }
}

#[tokio::test]
async fn nl_turn_plans_and_executes_end_to_end() {
    // Planner sees the goal and returns a DSL. Then translate/summarize
    // return canned strings. read-file uses the mocked in-memory table.
    let planner_response = "```agnes\n(pipe (tool read-file :path \"README.md\") (tool summarize))\n```";
    let provider: Arc<dyn Provider> = Arc::new(MockProvider::new(vec![
        planner_response.into(),   // planner call
        "one-sentence summary".into(), // summarize call
    ]));
    let mut session = Session::new(provider).unwrap();
    let mut sink = CollectSink(vec![]);
    let out = session
        .run_turn(TurnInput::NaturalLanguage("summarize the readme".into()), &mut sink)
        .await
        .unwrap();
    assert_eq!(out.data.as_str().unwrap(), "one-sentence summary");

    // Sink event stream shape:
    let kinds: Vec<&str> = sink.0.iter().map(|e| match e {
        SessionEvent::PlannerStart => "planner-start",
        SessionEvent::DslProduced { .. } => "dsl",
        SessionEvent::PlanReady { .. } => "plan",
        SessionEvent::NodeStart { .. } => "node-start",
        SessionEvent::NodeEnd { .. } => "node-end",
        SessionEvent::TurnResult { .. } => "turn-result",
        _ => "other",
    }).collect();
    assert!(kinds.contains(&"planner-start"));
    assert!(kinds.contains(&"dsl"));
    assert!(kinds.contains(&"plan"));
    assert!(kinds.iter().filter(|k| **k == "node-start").count() >= 2, "read-file + summarize expected");
    assert!(kinds.iter().filter(|k| **k == "node-end").count() >= 2);
    assert!(kinds.contains(&"turn-result"));
}

#[tokio::test]
async fn raw_dsl_turn_skips_planner() {
    let provider: Arc<dyn Provider> = Arc::new(MockProvider::new(vec![]));  // no planner calls
    let mut session = Session::new(provider).unwrap();
    let mut sink = CollectSink(vec![]);
    let out = session
        .run_turn(
            TurnInput::RawDsl("(tool read-file :path \"README.md\")".into()),
            &mut sink,
        )
        .await
        .unwrap();
    assert!(out.data.as_str().unwrap().contains("agnes"));
    // No PlannerStart event when RawDsl.
    assert!(!sink.0.iter().any(|e| matches!(e, SessionEvent::PlannerStart)));
}

#[tokio::test]
async fn planner_retries_on_bad_dsl_then_succeeds() {
    let provider: Arc<dyn Provider> = Arc::new(MockProvider::new(vec![
        "```agnes\nBROKEN(\n```".into(),
        "```agnes\n(tool read-file :path \"README.md\")\n```".into(),
    ]));
    let mut session = Session::new(provider).unwrap();
    let mut sink = CollectSink(vec![]);
    let _ = session
        .run_turn(TurnInput::NaturalLanguage("read the readme".into()), &mut sink)
        .await
        .expect("should recover on retry");
    let retry_count = sink.0.iter().filter(|e| matches!(e, SessionEvent::PlannerRetry { .. })).count();
    assert_eq!(retry_count, 1, "one retry expected");
}
```

- [ ] **Step 3: Run — must fail**

Run: `cargo test -p agnes-session`
Expected: FAIL — crate scaffolding not in place.

- [ ] **Step 4: Implement `crates/agnes-session/src/error.rs`**

```rust
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SessionError {
    #[error(transparent)]
    Planner(#[from] agnes_llm::PlannerError),

    #[error("parse error: {0}")]
    Parse(String),

    #[error("check error: {0}")]
    Check(String),

    #[error("compile error: {0}")]
    Compile(String),

    #[error(transparent)]
    Runtime(#[from] agnes_runtime::RuntimeError),

    #[error("planner exhausted retries (attempts=3); last error: {last}")]
    RetriesExhausted { last: String },
}
```

- [ ] **Step 5: Implement `crates/agnes-session/src/events.rs`**

```rust
use crate::plan_tree::PlanTree;

#[derive(Debug, Clone)]
pub enum NodeKindTag {
    Tool { name: String },
    Llm,
}

#[derive(Debug, Clone)]
pub enum SessionEvent {
    PlannerStart,
    PlannerRetry { attempt: u8, error: String },
    DslProduced { source: String },
    PlanReady { tree: PlanTree },
    NodeStart { id: u32, kind: NodeKindTag, args: Vec<(String, String)> },
    NodeEnd { id: u32, ok: bool, preview: String, elapsed_ms: u64 },
    TurnResult { value_preview: String, value_type: String },
    TurnFailed { error: String },
}

#[async_trait::async_trait]
pub trait EventSink: Send {
    async fn emit(&mut self, ev: SessionEvent);
}
```

- [ ] **Step 6: Implement `crates/agnes-session/src/plan_tree.rs`**

```rust
use agnes_compiler::{Dag, Input, NodeKind};

#[derive(Debug, Clone)]
pub struct PlanTree {
    pub kind: String,
    pub label: String,
    pub provides: Option<String>,
    pub children: Vec<PlanTree>,
}

pub fn build_plan_tree(dag: &Dag) -> PlanTree {
    build(dag, dag.root)
}

fn build(dag: &Dag, id: agnes_compiler::NodeId) -> PlanTree {
    let node = dag.get(id);
    let (kind, label) = match &node.kind {
        NodeKind::Tool { name } => ("tool".into(), format!("tool {name}")),
        NodeKind::Llm => ("llm".into(), "llm".into()),
        NodeKind::Pipe => ("pipe".into(), "pipe".into()),
        NodeKind::Par => ("par".into(), "par".into()),
        NodeKind::Let { name } => ("let".into(), format!("let {name}")),
        NodeKind::If => ("if".into(), "if".into()),
        NodeKind::Match { .. } => ("match".into(), "match".into()),
        NodeKind::Foreach { item } => ("foreach".into(), format!("foreach {item}")),
        NodeKind::Retry { times, .. } => ("retry".into(), format!("retry {times}")),
        NodeKind::Catch { .. } => ("catch".into(), "catch".into()),
        NodeKind::Return => ("return".into(), "return".into()),
        NodeKind::Literal(lit) => ("lit".into(), format!("{lit:?}")),
        NodeKind::Var(n) => ("var".into(), n.clone()),
        NodeKind::List => ("list".into(), "list".into()),
    };
    let mut children = Vec::new();
    for inp in &node.inputs {
        if let Some(child_id) = child_id_of(inp) {
            children.push(build(dag, child_id));
        }
    }
    PlanTree {
        kind,
        label,
        provides: Some(node.provides.to_string()),
        children,
    }
}

fn child_id_of(inp: &Input) -> Option<agnes_compiler::NodeId> {
    match inp {
        Input::FromNode(id) => Some(*id),
        Input::Kw { source, .. } => child_id_of(source),
        Input::Literal(_) | Input::Var(_) => None,
    }
}
```

- [ ] **Step 7: Implement `crates/agnes-session/src/tracer_bridge.rs`**

```rust
use crate::events::{EventSink, NodeKindTag, SessionEvent};
use agnes_compiler::{NodeId, NodeKind};
use agnes_runtime::{RuntimeError, Tracer};
use agnes_types::Value;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::mpsc;

/// A tracer that forwards NodeStart/NodeEnd events over an in-memory
/// channel. The receiver side is drained by Session::run_turn and forwarded
/// to the user-supplied EventSink (which is `&mut dyn EventSink`, so it
/// cannot be shared across the sync callback boundary directly).
pub struct ChannelTracer {
    tx: Mutex<mpsc::UnboundedSender<SessionEvent>>,
}

impl ChannelTracer {
    pub fn new() -> (Self, mpsc::UnboundedReceiver<SessionEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self { tx: Mutex::new(tx) }, rx)
    }
}

impl Tracer for ChannelTracer {
    fn node_start(&self, id: NodeId, kind: &NodeKind, args_preview: &str) {
        let tag = match kind {
            NodeKind::Tool { name } => NodeKindTag::Tool { name: name.clone() },
            NodeKind::Llm => NodeKindTag::Llm,
            _ => return,
        };
        let args: Vec<(String, String)> = if args_preview.is_empty() {
            vec![]
        } else {
            vec![("preview".into(), args_preview.to_string())]
        };
        let _ = self
            .tx
            .lock()
            .unwrap()
            .send(SessionEvent::NodeStart { id: id.0 as u32, kind: tag, args });
    }

    fn node_end(&self, id: NodeId, result: Result<&Value, &RuntimeError>, elapsed: Duration) {
        let (ok, preview) = match result {
            Ok(v) => {
                let p = if let Some(s) = v.data.as_str() {
                    let take: String = s.chars().take(60).collect();
                    format!("{}({}) {take:?}", v.declared_type, s.len())
                } else {
                    format!("{}", v.declared_type)
                };
                (true, p)
            }
            Err(e) => (false, e.to_string()),
        };
        let _ = self.tx.lock().unwrap().send(SessionEvent::NodeEnd {
            id: id.0 as u32,
            ok,
            preview,
            elapsed_ms: elapsed.as_millis() as u64,
        });
    }
}

pub async fn drain(rx: &mut mpsc::UnboundedReceiver<SessionEvent>, sink: &mut dyn EventSink) {
    while let Ok(ev) = rx.try_recv() {
        sink.emit(ev).await;
    }
}
```

- [ ] **Step 8: Implement `crates/agnes-session/src/session.rs`**

```rust
use crate::error::SessionError;
use crate::events::{EventSink, SessionEvent};
use crate::plan_tree::build_plan_tree;
use crate::tracer_bridge::{drain, ChannelTracer};
use agnes_builtins::{native_dispatch, register_builtins, ToolImpl};
use agnes_llm::{Planner, Provider, Turn};
use agnes_registry::Registry;
use agnes_runtime::execute_with;
use agnes_types::Value;
use std::collections::HashMap;
use std::sync::Arc;

pub enum TurnInput {
    NaturalLanguage(String),
    RawDsl(String),
}

pub struct Session {
    /// Kept solely to seed the planner's system prompt at construction time
    /// and to hand a Registry reference to `native_dispatch` callers if they
    /// need one. Each `run_turn` builds a fresh per-turn Registry, so this
    /// is never mutated after construction.
    _template_registry: Registry,
    dispatch: HashMap<String, ToolImpl>,
    planner: Planner,
}

const MAX_PLAN_RETRIES: u8 = 2;

impl Session {
    pub fn new(provider: Arc<dyn Provider>) -> Result<Self, SessionError> {
        let mut registry = Registry::new();
        register_builtins(&mut registry).map_err(|e| SessionError::Check(e.to_string()))?;
        let dispatch = native_dispatch(provider.clone());
        let planner = Planner::new(provider, &registry);
        Ok(Self { _template_registry: registry, dispatch, planner })
    }

    pub fn history(&self) -> &[Turn] {
        self.planner.history()
    }

    pub fn reset_history(&mut self) {
        self.planner.reset_history();
    }

    pub async fn run_turn(
        &mut self,
        input: TurnInput,
        sink: &mut dyn EventSink,
    ) -> Result<Value, SessionError> {
        let dsl = match input {
            TurnInput::RawDsl(s) => s,
            TurnInput::NaturalLanguage(nl) => {
                sink.emit(SessionEvent::PlannerStart).await;
                self.plan_with_retries(&nl, sink).await?
            }
        };
        sink.emit(SessionEvent::DslProduced { source: dsl.clone() }).await;

        // parse -> check -> compile
        let program = agnes_parser::parse(&dsl).map_err(|e| SessionError::Parse(e.to_string()))?;
        // A registry mutation-per-turn: apply top-levels of this program.
        // For the MVP we do NOT persist `define`s across turns — each turn
        // gets a fresh registry seeded with the builtins. This trades the
        // "prior-turn defines visible to later turns" nicety for a much
        // simpler correctness story (no duplicate-define NameConflict when
        // the same DSL is re-run, no state that surprises the user).
        let mut turn_registry = Registry::new();
        register_builtins(&mut turn_registry).map_err(|e| SessionError::Check(e.to_string()))?;
        turn_registry.load(&program).map_err(|e| SessionError::Check(e.to_string()))?;
        agnes_checker::check(&program, &turn_registry).map_err(|e| SessionError::Check(e.to_string()))?;
        let dag = agnes_compiler::compile(&program, &turn_registry).map_err(|e| SessionError::Compile(e.to_string()))?;

        sink.emit(SessionEvent::PlanReady { tree: build_plan_tree(&dag) }).await;

        let (tracer, mut rx) = ChannelTracer::new();
        // Poll the channel while the runtime executes.
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
        // Final drain — pick up any events emitted after the last tick.
        drain(&mut rx, sink).await;

        match result {
            Ok(v) => {
                let preview = if let Some(s) = v.data.as_str() {
                    let t: String = s.chars().take(120).collect();
                    format!("{t}{}", if s.len() > 120 { "…" } else { "" })
                } else {
                    v.data.to_string()
                };
                sink.emit(SessionEvent::TurnResult {
                    value_preview: preview.clone(),
                    value_type: v.declared_type.to_string(),
                }).await;
                self.planner.record_result(dsl, preview);
                Ok(v)
            }
            Err(e) => {
                sink.emit(SessionEvent::TurnFailed { error: e.to_string() }).await;
                Err(SessionError::from(e))
            }
        }
    }

    async fn plan_with_retries(&mut self, nl: &str, sink: &mut dyn EventSink) -> Result<String, SessionError> {
        let mut last_err = String::new();
        for attempt in 0..=MAX_PLAN_RETRIES {
            let dsl = self.planner.plan(nl).await?;
            // Dry-run: parse/check/compile against a fresh registry so a
            // planner attempt with a bad `define` cannot break anything.
            let mut probe = Registry::new();
            register_builtins(&mut probe).map_err(|e| SessionError::Check(e.to_string()))?;
            match dry_run(&dsl, &mut probe) {
                Ok(()) => return Ok(dsl),
                Err(e) => {
                    last_err = e.clone();
                    if attempt < MAX_PLAN_RETRIES {
                        sink.emit(SessionEvent::PlannerRetry { attempt: attempt + 1, error: e.clone() }).await;
                        self.planner.push_error_feedback(dsl, e);
                    }
                }
            }
        }
        Err(SessionError::RetriesExhausted { last: last_err })
    }
}

fn dry_run(dsl: &str, probe: &mut Registry) -> Result<(), String> {
    let program = agnes_parser::parse(dsl).map_err(|e| e.to_string())?;
    probe.load(&program).map_err(|e| e.to_string())?;
    agnes_checker::check(&program, probe).map_err(|e| e.to_string())?;
    let _ = agnes_compiler::compile(&program, probe).map_err(|e| e.to_string())?;
    Ok(())
}
```

- [ ] **Step 9: Implement `crates/agnes-session/src/lib.rs`**

```rust
//! Headless session engine: NL -> DSL -> plan tree -> traced execution.
//! Emits SessionEvents to a caller-supplied EventSink. Frontends (CLI,
//! future GUI) plug in by implementing EventSink.

mod error;
mod events;
mod plan_tree;
mod session;
mod tracer_bridge;

pub use error::SessionError;
pub use events::{EventSink, NodeKindTag, SessionEvent};
pub use plan_tree::PlanTree;
pub use session::{Session, TurnInput};
```

- [ ] **Step 10: Run — must pass**

Run: `cargo test -p agnes-session`
Expected: PASS — all 3 tests.

- [ ] **Step 11: Commit**

```bash
jj describe -m "feat(session): headless engine driving planner + runtime

Session::run_turn walks NL -> planner -> parse/check/compile -> execute,
emitting SessionEvent variants over an EventSink (PlannerStart,
PlannerRetry, DslProduced, PlanReady, NodeStart/End, TurnResult/Failed).
A ChannelTracer bridges the runtime's Tracer hooks into the event stream.
Frontends (CLI now, future GUI) implement EventSink.

Co-Authored-By: Claude <noreply@anthropic.com>"
jj new
```

---

### Task 10: CLI scaffolding — clap `chat`/`run` commands + StderrEventSink + plan renderer

**Files:**
- Modify: `crates/agnes-cli/Cargo.toml` — add deps.
- Create: `crates/agnes-cli/src/cli.rs`
- Create: `crates/agnes-cli/src/plan_view.rs`
- Create: `crates/agnes-cli/src/sink_stderr.rs`
- Create: `crates/agnes-cli/src/run_cmd.rs`
- Modify: `crates/agnes-cli/src/main.rs` — clap dispatch.
- Modify: `Cargo.toml` (root) — add `clap = { version = "4", features = ["derive"] }` and `rustyline = "14"` to workspace deps.
- Test: `crates/agnes-cli/tests/plan_view_snapshot.rs`

**Interfaces:**
- Consumes: `agnes_session::{Session, TurnInput, EventSink, SessionEvent, PlanTree, NodeKindTag}`; `agnes_llm::{resolve_provider, LlmCliOpts, MockProvider}`.
- Produces:
  - `struct Args { cmd: Command, ... }` (clap derive) with subcommands `Chat`, `Run { file }` and shared `llm_provider / llm_model / llm_base_url` flags.
  - `pub fn render_plan(tree: &PlanTree, out: &mut impl std::io::Write) -> std::io::Result<()>`.
  - `pub struct StderrEventSink;` implementing `EventSink`, rendering the trace with `▶ / ✔ / ✘` glyphs.

- [ ] **Step 1: Update `crates/agnes-cli/Cargo.toml`**

Add under `[dependencies]`:

```toml
agnes-session.workspace = true
agnes-llm.workspace     = true
async-trait.workspace   = true
clap                    = { workspace = true }
dotenvy.workspace       = true
rustyline               = { workspace = true }
```

And add to workspace root `[workspace.dependencies]`:

```toml
clap      = { version = "4", features = ["derive"] }
rustyline = "14"
```

- [ ] **Step 2: Write the failing plan-view snapshot test**

Create `crates/agnes-cli/tests/plan_view_snapshot.rs`:

```rust
use agnes_session::PlanTree;

fn sample() -> PlanTree {
    PlanTree {
        kind: "pipe".into(), label: "pipe".into(), provides: Some("PlainText".into()),
        children: vec![
            PlanTree {
                kind: "par".into(), label: "par".into(), provides: Some("Unit".into()),
                children: vec![
                    PlanTree {
                        kind: "let".into(), label: "let ja".into(), provides: Some("PlainText".into()),
                        children: vec![
                            PlanTree {
                                kind: "pipe".into(), label: "pipe".into(), provides: Some("PlainText".into()),
                                children: vec![
                                    PlanTree { kind: "tool".into(), label: "tool read-file".into(), provides: Some("PlainText".into()), children: vec![] },
                                    PlanTree { kind: "tool".into(), label: "tool translate".into(), provides: Some("PlainText".into()), children: vec![] },
                                ],
                            },
                        ],
                    },
                ],
            },
            PlanTree { kind: "tool".into(), label: "tool join-lines".into(), provides: Some("PlainText".into()), children: vec![] },
        ],
    }
}

#[test]
fn render_plan_uses_indent_tree_glyphs() {
    let mut buf = Vec::new();
    agnes_cli::plan_view::render_plan(&sample(), &mut buf).unwrap();
    let out = String::from_utf8(buf).unwrap();
    // A handful of anchors — exact rendering is exercised by insta if enabled,
    // but this smoke check keeps things stable enough for TDD.
    assert!(out.contains("pipe"));
    assert!(out.contains("├── par"));
    assert!(out.contains("│   └── let ja"));
    assert!(out.contains("└── tool join-lines"));
    assert!(out.contains("→ PlainText"));
}
```

To make `plan_view` reachable as `agnes_cli::plan_view`, the binary crate needs a matching `lib.rs`. Alternative approach used here: move `plan_view` into a small internal library target inside `agnes-cli`.

Modify `crates/agnes-cli/Cargo.toml`:

```toml
[lib]
name = "agnes_cli"
path = "src/lib.rs"

[[bin]]
name = "agnes"
path = "src/main.rs"
```

- [ ] **Step 3: Create `crates/agnes-cli/src/lib.rs`**

```rust
//! Internal helpers for the `agnes` binary. Exposed as a library so
//! integration tests can hit them.
pub mod cli;
pub mod plan_view;
pub mod run_cmd;
pub mod sink_stderr;
```

- [ ] **Step 4: Run — must fail**

Run: `cargo test -p agnes-cli --test plan_view_snapshot`
Expected: FAIL — no `render_plan`.

- [ ] **Step 5: Implement `plan_view.rs`**

```rust
use agnes_session::PlanTree;
use std::io::{self, Write};

pub fn render_plan(tree: &PlanTree, out: &mut impl Write) -> io::Result<()> {
    render(tree, "", true, out, true)
}

fn render(node: &PlanTree, prefix: &str, is_last: bool, out: &mut impl Write, is_root: bool) -> io::Result<()> {
    if is_root {
        writeln!(out, "{}{}{}", node.label, provides_suffix(node), "")?;
    } else {
        let connector = if is_last { "└── " } else { "├── " };
        writeln!(out, "{prefix}{connector}{}{}", node.label, provides_suffix(node))?;
    }
    let child_prefix = if is_root {
        String::new()
    } else if is_last {
        format!("{prefix}    ")
    } else {
        format!("{prefix}│   ")
    };
    let n = node.children.len();
    for (i, ch) in node.children.iter().enumerate() {
        render(ch, &child_prefix, i + 1 == n, out, false)?;
    }
    Ok(())
}

fn provides_suffix(node: &PlanTree) -> String {
    match &node.provides {
        Some(t) if node.kind == "tool" || node.kind == "llm" || node.kind == "let" || node.kind == "pipe" || node.kind == "par" =>
            format!("  → {t}"),
        _ => String::new(),
    }
}
```

- [ ] **Step 6: Implement `sink_stderr.rs`**

```rust
use agnes_session::{EventSink, NodeKindTag, SessionEvent};
use std::io::Write;
use std::time::Instant;

/// Renders SessionEvents to stderr with a start-time-relative timestamp.
pub struct StderrEventSink {
    start: Instant,
    printed_plan_header: bool,
    printed_trace_header: bool,
}

impl Default for StderrEventSink {
    fn default() -> Self {
        Self { start: Instant::now(), printed_plan_header: false, printed_trace_header: false }
    }
}

impl StderrEventSink {
    pub fn new() -> Self { Self::default() }

    fn t(&self) -> String {
        let ms = self.start.elapsed().as_millis();
        format!("[+{}.{:03}s]", ms / 1000, ms % 1000)
    }
}

#[async_trait::async_trait]
impl EventSink for StderrEventSink {
    async fn emit(&mut self, ev: SessionEvent) {
        let e = &mut std::io::stderr().lock();
        match ev {
            SessionEvent::PlannerStart => {
                let _ = writeln!(e, "\n━━━ Planning ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
                self.start = Instant::now();
                self.printed_plan_header = false;
                self.printed_trace_header = false;
            }
            SessionEvent::PlannerRetry { attempt, error } => {
                let _ = writeln!(e, "  retry #{attempt}: {error}");
            }
            SessionEvent::DslProduced { source } => {
                let _ = writeln!(e, "━━━ Generated DSL ━━━━━━━━━━━━━━━━━━━━━━━━");
                let _ = writeln!(e, "{source}");
            }
            SessionEvent::PlanReady { tree } => {
                let _ = writeln!(e, "━━━ Plan ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
                let _ = crate::plan_view::render_plan(&tree, e);
            }
            SessionEvent::NodeStart { id: _, kind, args } => {
                if !self.printed_trace_header {
                    let _ = writeln!(e, "━━━ Trace ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
                    self.printed_trace_header = true;
                }
                let label = match kind {
                    NodeKindTag::Tool { name } => format!("tool {name}"),
                    NodeKindTag::Llm => "llm".into(),
                };
                let a = if args.is_empty() { String::new() } else { format!("  {}", args[0].1) };
                let _ = writeln!(e, "{} ▶ {label}{a}", self.t());
            }
            SessionEvent::NodeEnd { id: _, ok, preview, elapsed_ms: _ } => {
                let glyph = if ok { "✔" } else { "✘" };
                let _ = writeln!(e, "{} {glyph} {preview}", self.t());
            }
            SessionEvent::TurnResult { value_preview: _, value_type } => {
                let _ = writeln!(e, "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
                let _ = writeln!(e, "(result: {value_type})");
            }
            SessionEvent::TurnFailed { error } => {
                let _ = writeln!(e, "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
                let _ = writeln!(e, "✘ turn failed: {error}");
            }
        }
    }
}
```

- [ ] **Step 7: Implement `cli.rs`**

```rust
use agnes_llm::LlmCliOpts;
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "agnes", version, about = "agnes DSL runtime")]
pub struct Args {
    #[command(flatten)]
    pub llm: LlmFlags,

    #[command(subcommand)]
    pub cmd: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Interactive REPL: NL turns are planned into DSL by an LLM.
    Chat,
    /// Non-interactive: parse, compile, and execute a .agnes file.
    Run { file: String },
}

#[derive(Debug, clap::Args)]
pub struct LlmFlags {
    #[arg(long)]
    pub llm_provider: Option<String>,
    #[arg(long)]
    pub llm_model: Option<String>,
    #[arg(long)]
    pub llm_base_url: Option<String>,
}

impl LlmFlags {
    pub fn to_opts(&self) -> LlmCliOpts {
        LlmCliOpts {
            provider: self.llm_provider.clone(),
            model: self.llm_model.clone(),
            base_url: self.llm_base_url.clone(),
        }
    }
}
```

- [ ] **Step 8: Implement `run_cmd.rs`**

```rust
use crate::sink_stderr::StderrEventSink;
use agnes_llm::Provider;
use agnes_session::{Session, TurnInput};
use std::path::PathBuf;
use std::sync::Arc;

pub async fn run_file(file: &str, provider: Arc<dyn Provider>) -> anyhow::Result<()> {
    let src = tokio::fs::read_to_string(PathBuf::from(file)).await?;
    let mut session = Session::new(provider)?;
    let mut sink = StderrEventSink::new();
    let out = session.run_turn(TurnInput::RawDsl(src), &mut sink).await?;
    println!("{}", out.data);
    Ok(())
}
```

- [ ] **Step 9: Rewrite `crates/agnes-cli/src/main.rs`**

```rust
use agnes_cli::{cli::{Args, Command}, run_cmd};
use agnes_llm::resolve_provider;
use clap::Parser;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).init();
    let _ = dotenvy::dotenv();
    let args = Args::parse();
    let provider = resolve_provider(&args.llm.to_opts())
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    match args.cmd.unwrap_or(Command::Chat) {
        Command::Chat => {
            // Task 11 wires the REPL here.
            eprintln!("chat REPL not implemented yet — Task 11 adds it. Use `agnes run <file>` for now.");
            Ok(())
        }
        Command::Run { file } => run_cmd::run_file(&file, provider).await,
    }
}
```

- [ ] **Step 10: Run — must pass**

Run: `cargo test -p agnes-cli --test plan_view_snapshot`
Expected: PASS.

- [ ] **Step 11: Update `crates/agnes-cli/tests/acceptance.rs`**

The existing test now goes through `agnes-session`. Read the current file first; if it still calls `native_dispatch()` directly, replace the flow with:

```rust
let mock: Arc<dyn agnes_llm::Provider> = Arc::new(agnes_llm::MockProvider::new(vec![
    "[SUMMARY]".into(); 8
]));
let mut session = agnes_session::Session::new(mock).unwrap();
struct Silent;
#[async_trait::async_trait]
impl agnes_session::EventSink for Silent {
    async fn emit(&mut self, _ev: agnes_session::SessionEvent) {}
}
let mut sink = Silent;
let out = session.run_turn(agnes_session::TurnInput::RawDsl(src.into()), &mut sink).await.unwrap();
```

- [ ] **Step 12: Run everything**

Run: `cargo test --workspace`
Expected: PASS.

- [ ] **Step 13: Commit**

```bash
jj describe -m "feat(cli): clap dispatch + StderrEventSink + plan-tree renderer

Splits agnes-cli into a lib target (cli/plan_view/sink_stderr/run_cmd)
plus the `agnes` binary. \`agnes run <file>\` walks the same session
plumbing chat will use. \`agnes chat\` is a stub until Task 11.

Co-Authored-By: Claude <noreply@anthropic.com>"
jj new
```

---

### Task 11: `agnes chat` REPL — rustyline + slash commands + `(`-balanced multiline

**Files:**
- Create: `crates/agnes-cli/src/chat.rs`
- Create: `crates/agnes-cli/src/input.rs`
- Modify: `crates/agnes-cli/src/lib.rs` — add `pub mod chat; pub mod input;`.
- Modify: `crates/agnes-cli/src/main.rs` — call `chat::run(provider).await` under `Command::Chat`.
- Test: `crates/agnes-cli/tests/input_balance.rs`

**Interfaces:**
- Consumes: `agnes_session::{Session, TurnInput, SessionEvent, EventSink}`; `StderrEventSink`.
- Produces:
  - `pub async fn chat::run(provider: Arc<dyn agnes_llm::Provider>) -> anyhow::Result<()>`.
  - `pub fn input::is_balanced(s: &str) -> bool` — counts `(` vs `)` outside string literals.

- [ ] **Step 1: Write the failing balance test**

Create `crates/agnes-cli/tests/input_balance.rs`:

```rust
use agnes_cli::input::is_balanced;

#[test] fn one_liner_is_balanced()     { assert!(is_balanced("(tool foo)")); }
#[test] fn multiline_open_not_balanced(){ assert!(!is_balanced("(pipe\n  (tool")); }
#[test] fn parens_inside_string_ignored(){ assert!(is_balanced(r#"(tool x :s "(a)b")"#)); }
#[test] fn escaped_quote_in_string()   { assert!(is_balanced(r#"(tool x :s "a\"b")"#)); }
```

- [ ] **Step 2: Run — must fail**

Run: `cargo test -p agnes-cli --test input_balance`
Expected: FAIL.

- [ ] **Step 3: Implement `input.rs`**

```rust
/// Simple paren balancer that skips runs inside "..." string literals.
/// Handles \" escapes. Not a full parser — it just tells the REPL when
/// to submit the buffer to `agnes-parser`.
pub fn is_balanced(s: &str) -> bool {
    let mut depth: i32 = 0;
    let mut in_str = false;
    let mut esc = false;
    for ch in s.chars() {
        if in_str {
            if esc { esc = false; continue; }
            match ch {
                '\\' => esc = true,
                '"' => in_str = false,
                _ => {}
            }
            continue;
        }
        match ch {
            '"' => in_str = true,
            '(' => depth += 1,
            ')' => depth -= 1,
            _ => {}
        }
        if depth < 0 { return false; }
    }
    depth == 0 && !in_str
}
```

- [ ] **Step 4: Run — must pass**

Run: `cargo test -p agnes-cli --test input_balance`
Expected: PASS.

- [ ] **Step 5: Implement `chat.rs`**

```rust
use crate::input::is_balanced;
use crate::sink_stderr::StderrEventSink;
use agnes_llm::Provider;
use agnes_session::{Session, SessionEvent, TurnInput, EventSink};
use rustyline::error::ReadlineError;
use rustyline::{DefaultEditor, Result as RlResult};
use std::sync::Arc;

/// Prints the banner and enters the REPL. Ctrl-D exits cleanly.
pub async fn run(provider: Arc<dyn Provider>) -> anyhow::Result<()> {
    banner();
    let mut session = Session::new(provider)?;
    let mut rl: DefaultEditor = DefaultEditor::new()?;
    loop {
        match read_line_or_block(&mut rl) {
            Ok(Some(line)) => {
                if let Some(cmd) = line.strip_prefix('/') {
                    if !dispatch_slash(cmd, &mut session).await? { break; }
                    continue;
                }
                if line.trim().is_empty() { continue; }
                let mut sink = StderrEventSink::new();
                let input = if line.trim_start().starts_with('(') || line.trim_start().starts_with('[') {
                    // Direct DSL injection when the user types raw code.
                    TurnInput::RawDsl(line)
                } else {
                    TurnInput::NaturalLanguage(line)
                };
                match session.run_turn(input, &mut sink).await {
                    Ok(v) => println!("{}", v.data),
                    Err(e) => eprintln!("error: {e}"),
                }
            }
            Ok(None) => break, // EOF
            Err(e) => { eprintln!("readline: {e}"); break; }
        }
    }
    Ok(())
}

/// Reads one logical entry: either a single line ending on Enter, or a
/// multi-line entry when `(` opens; keeps reading with the continuation
/// prompt `... ` until the paren balance is zero.
fn read_line_or_block(rl: &mut DefaultEditor) -> RlResult<Option<String>> {
    let first = match rl.readline("agnes> ") {
        Ok(s) => s,
        Err(ReadlineError::Eof) => return Ok(None),
        Err(ReadlineError::Interrupted) => return Ok(Some(String::new())),
        Err(e) => return Err(e),
    };
    let _ = rl.add_history_entry(first.as_str());
    if !first.trim_start().starts_with('(') { return Ok(Some(first)); }
    let mut buf = first;
    while !is_balanced(&buf) {
        match rl.readline("     ...> ") {
            Ok(next) => {
                buf.push('\n');
                buf.push_str(&next);
                let _ = rl.add_history_entry(next.as_str());
            }
            Err(ReadlineError::Eof) | Err(ReadlineError::Interrupted) => return Ok(Some(buf)),
            Err(e) => return Err(e),
        }
    }
    Ok(Some(buf))
}

async fn dispatch_slash(cmd: &str, session: &mut Session) -> anyhow::Result<bool> {
    let cmd = cmd.trim();
    if cmd == "quit" || cmd == "exit" { return Ok(false); }
    if cmd == "reset" { session.reset_history(); println!("(history cleared)"); return Ok(true); }
    if cmd == "history" {
        for (i, t) in session.history().iter().enumerate() {
            println!("--- turn {i} ---");
            println!("user: {}", t.user_nl);
            println!("dsl:  {}", t.assistant_dsl);
            println!("out:  {}", t.result_preview);
        }
        return Ok(true);
    }
    if let Some(dsl) = cmd.strip_prefix("run ") {
        let mut sink = StderrEventSink::new();
        match session.run_turn(TurnInput::RawDsl(dsl.into()), &mut sink).await {
            Ok(v) => println!("{}", v.data),
            Err(e) => eprintln!("error: {e}"),
        }
        return Ok(true);
    }
    eprintln!("unknown command: /{cmd}. Try: /run <dsl>, /history, /reset, /quit");
    Ok(true)
}

fn banner() {
    eprintln!("━━━ agnes chat ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    eprintln!("type your goal, or /run <dsl>, /history, /reset, /quit");
    eprintln!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
}
```

- [ ] **Step 6: Wire `lib.rs` and `main.rs`**

Edit `crates/agnes-cli/src/lib.rs`:

```rust
pub mod chat;
pub mod input;
```

Edit the `Command::Chat` arm in `main.rs`:

```rust
Command::Chat => agnes_cli::chat::run(provider).await,
```

- [ ] **Step 7: Run — everything**

Run: `cargo test --workspace`
Expected: PASS. The REPL itself isn't unit-tested (rustyline needs a TTY); we exercise it manually in Task 12.

- [ ] **Step 8: Commit**

```bash
jj describe -m "feat(cli): agnes chat REPL — rustyline + slash commands

Line starting with '(' or '[' is treated as raw DSL; anything else is
planned by the LLM. Slash commands: /run <dsl>, /history, /reset,
/quit. Multi-line entry supported via paren balance. Ctrl-D exits.

Co-Authored-By: Claude <noreply@anthropic.com>"
jj new
```

---

### Task 12: End-to-end verification + demo doc

**Files:**
- Create: `examples/chat-demo.md`
- Modify: `README.md` — add a short section pointing at `agnes chat`.

**Interfaces:**
- Consumes: everything shipped in Tasks 1–11.
- Produces: doc + a manual verification checklist that the plan's Verification section anchored to.

- [ ] **Step 1: `cargo test --workspace` clean**

Run: `cargo test --workspace`
Expected: PASS across all crates. Any failing test blocks this task — fix in the task that owns it.

- [ ] **Step 2: `cargo clippy --workspace --all-targets --deny warnings`**

Run and fix any lints introduced by new files (typical: unused-imports, needless-borrow). Do not silence with `#[allow(...)]` unless a comment explains why.

- [ ] **Step 3: Manual missing-key path**

Ensure the shell has neither `ANTHROPIC_API_KEY` nor `OPENAI_API_KEY` set, and neither `--llm-provider` nor `AGNES_LLM_PROVIDER` supplied. Run:

```bash
env -u ANTHROPIC_API_KEY -u OPENAI_API_KEY -u AGNES_LLM_PROVIDER cargo run -p agnes-cli -- chat
```

Expected stderr:

```
Missing provider selection.
  Why: neither the CLI flag `--llm-provider` nor the env var `AGNES_LLM_PROVIDER` is set.
  Fix: pass --llm-provider, set AGNES_LLM_PROVIDER, or add it to .env.
```

Exit code non-zero.

- [ ] **Step 4: Manual real-key path (record the trace)**

With a real key:

```bash
ANTHROPIC_API_KEY=... cargo run -p agnes-cli -- chat \
  --llm-provider anthropic --llm-model claude-haiku-4-5
```

Then in the REPL:

1. `Translate the readme into Japanese`
2. `now do English too and join them`
3. `/run (tool llm :prompt "haiku about types" :input "")`
4. `/history`
5. `/quit`

Expected: plan tree + trace on stderr for each; final result on stdout; two `translate` node ends showing >200ms elapsed (real API latency).

- [ ] **Step 5: Write `examples/chat-demo.md`**

Include: quick-start command, example session transcript (paste from Step 4), and the note that non-LLM tools are in-memory mocks (link to `crates/agnes-builtins/src/tools.rs` `MOCK_README`/`MOCK_NOTES`/`MOCK_DRAFT`).

- [ ] **Step 6: Update `README.md`**

Add a section under "Try it":

```markdown
## Interactive chat

Set an API key and:

    ANTHROPIC_API_KEY=... cargo run -p agnes-cli -- chat --llm-provider anthropic

Each natural-language turn is planned into an agnes DSL program by the
LLM, executed by the runtime, and printed with a plan tree and per-tool
trace. `/run <dsl>` lets you inject a hand-written program. See
[examples/chat-demo.md](examples/chat-demo.md).
```

- [ ] **Step 7: Commit**

```bash
jj describe -m "docs: chat demo walkthrough + README quick-start

Manual verification confirms:
  - missing-key path errors with a What/Why/Fix message
  - real-key path plans + executes translate/summarize/llm with visible
    plan tree and per-node trace
  - /run injects hand-written DSL; /history dumps prior turns

Co-Authored-By: Claude <noreply@anthropic.com>"
jj new
```

---

## Self-review notes (for the executing agent)

If while executing any task the interfaces you produce drift from the "Interfaces" block, STOP and update this plan file first — later tasks depend on those signatures.

Common pitfalls surfaced by the design:

- **Per-turn registry, not persistent**. `Session` intentionally does NOT persist `define`s across turns. Each turn builds a fresh `Registry` seeded with builtins so re-running the same DSL never hits `NameConflict`. Prior-turn `define`s survive only through the planner's system-prompt history, not through the registry — the planner may re-emit the `define` in later turns if it wants to reuse it. That's the MVP contract; do not "improve" it into a persistent registry without changing the retry story too.
- **`native_dispatch` closures move `provider.clone()` into the `Arc<dyn Fn>`**. Because `Provider` requires `Send + Sync`, cloning the `Arc` inside the outer closure and again inside the inner async block is intentional; do not shortcut this.
- **`tokio::select!` polling of the tracer channel in `Session::run_turn`** uses a 10ms tick. That's fine for a REPL. If you notice tests hanging, check for a dropped receiver — the sender lives on the tracer, which lives on the stack for the duration of `execute_with`.
- **`env::set_var` in `Task 4`'s tests is `unsafe` on edition 2024**. The `unsafe { }` block is required — do not delete it.
- **`rustyline 14`'s `DefaultEditor::readline` returns `Err(ReadlineError::Interrupted)` on Ctrl-C**. The plan treats that as "cancel current line, keep REPL alive"; do not exit on it.
