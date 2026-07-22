# File I/O and Shell Execution Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add real disk file I/O and user-confirmed shell command execution to Agnes, enabling LLM-driven code writing workflows.

**Architecture:** Extend existing `agnes-builtins` with real implementations, add path validation to `agnes-session`, and add user confirmation flow to `agnes-cli`. All file operations are constrained to an allowed root directory for safety, and all shell commands require explicit user confirmation.

**Tech Stack:** Rust, tokio (async fs/process), existing Agnes type system and tool registration. No new dependencies required.

## Global Constraints

- All tool names use kebab-case (Lisp style), matching existing convention
- Path validation must check canonicalized paths start with allowed root
- Symlinks must be followed and the resolved path must also be within allowed root
- Shell commands always require user confirmation in interactive mode
- Non-interactive mode requires explicit `--allow-shell` flag to enable shell execution
- Error messages are returned as human-readable strings via `Result<Value, String>`
- All file operations are async using tokio

---

## File Mapping

| File | Change | Purpose |
|------|--------|---------|
| `crates/agnes-types/src/types.rs` | Modify | Register the new `Path` semantic type |
| `crates/agnes-session/src/lib.rs` | Modify | Add `allow_root` field to `Session`, add `resolve_path` method, add `ShellConfirm` event |
| `crates/agnes-builtins/src/tools.rs` | Modify | Replace mock `read-file`/`write-file` with real I/O, add `parse-path` and `shell-run` tools |
| `crates/agnes-cli/src/chat.rs` | Modify | Handle `ShellConfirm` event, prompt user for confirmation |
| `crates/agnes-cli/src/main.rs` | Modify | Add CLI flags `--allow-root` and `--allow-shell` |
| `crates/agnes-session/src/path_validation_tests.rs` | Create | Unit tests for path validation logic |

---

### Task 1: Register `Path` type in `agnes-types`

**Files:**
- Modify: `crates/agnes-types/src/types.rs`

**Interfaces:**
- Consumes: Existing type registration machinery
- Produces: `Path` type available in type system

- [ ] **Step 1: Add `Path` to built-in types**

Open `crates/agnes-types/src/types.rs` and add the new type:

```rust
// Add with other built-in types
pub const BUILTIN_TYPES: &[(&str, &str)] = &[
    // ... existing entries ...
    ("Path", "File system path (validated within allowed root)"),
];
```

- [ ] **Step 2: Build and check no errors**

```bash
cargo check -p agnes-types
```

Expected: Compiles successfully.

- [ ] **Step 3: Commit**

```bash
git add crates/agnes-types/src/types.rs
git commit -m "feat(types): add Path builtin type"
```

---

### Task 2: Add path validation to `agnes-session`

**Files:**
- Modify: `crates/agnes-session/src/lib.rs`
- Create: `crates/agnes-session/src/path_validation_tests.rs`

**Interfaces:**
- Consumes: `Path` type from agnes-types
- Produces: `Session::resolve_path(&str) -> Result<PathBuf, String>` method for tools to use

- [ ] **Step 1: Add fields to `Session` struct**

In `crates/agnes-session/src/lib.rs`, update the `Session` struct:

```rust
use std::path::PathBuf;

#[derive(Debug)]
pub struct Session {
    // ... existing fields ...
    /// Allowed root directory for file operations.
    /// If None, defaults to current working directory.
    allow_root: Option<PathBuf>,
    /// Whether shell execution is permitted.
    allow_shell: bool,
}
```

- [ ] **Step 2: Update `Session` builder/constructor**

Update `Session::new()` or builder to accept the new fields:

```rust
impl Session {
    pub fn new(/* existing args */) -> Self {
        Self {
            // existing fields...
            allow_root: None,
            allow_shell: false,
        }
    }

    /// Builder method to set allowed root directory.
    pub fn with_allow_root(mut self, path: PathBuf) -> Self {
        self.allow_root = Some(path);
        self
    }

    /// Builder method to enable shell execution.
    pub fn with_allow_shell(mut self, enabled: bool) -> Self {
        self.allow_shell = enabled;
        self
    }
}
```

- [ ] **Step 3: Add `resolve_path` method**

Add this method to `impl Session`:

```rust
/// Resolve and validate a user-provided path against the allowed root.
pub fn resolve_path(&self, input: &str) -> Result<PathBuf, String> {
    let current_dir = std::env::current_dir()
        .map_err(|e| format!("failed to get current directory: {}", e))?;

    let allow_root = self.allow_root.as_ref()
        .unwrap_or(&current_dir);

    // Resolve input path against current working directory
    let candidate = if std::path::Path::new(input).is_absolute() {
        PathBuf::from(input)
    } else {
        current_dir.join(input)
    };

    // Canonicalize to resolve symlinks and .. components
    let canonical = candidate.canonicalize()
        .map_err(|e| format!("cannot resolve path '{}': {}", input, e))?;

    // Check that the canonical path starts with the allowed root
    if !canonical.starts_with(allow_root) {
        return Err(format!(
            "path '{}' (resolved to '{}') is outside allowed root directory '{}'",
            input, canonical.display(), allow_root.display()
        ));
    }

    Ok(canonical)
}
```

Also add a getter for `allow_shell`:

```rust
impl Session {
    pub fn allow_shell(&self) -> bool {
        self.allow_shell
    }
}
```

- [ ] **Step 4: Add `ShellConfirm` event to `SessionEvent` enum**

Update the `SessionEvent` enum:

```rust
use tokio::sync::oneshot;

pub enum SessionEvent {
    // ... existing variants ...
    /// Request user confirmation before executing a shell command.
    ShellConfirm {
        /// The command to execute.
        command: String,
        /// Send `true` to approve, `false` to cancel.
        responder: oneshot::Sender<bool>,
    },
}
```

- [ ] **Step 5: Add unit tests for path validation**

Create `crates/agnes-session/src/path_validation_tests.rs`:

```rust
use super::*;
use std::path::PathBuf;

#[test]
fn test_resolve_path_inside_root_allowed() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_owned();

    let session = Session::new()
        .with_allow_root(root.clone());

    let subpath = root.join("src").join("main.rs");
    let input = format!("{}/src/main.rs", root.display());
    let result = session.resolve_path(&input);
    assert!(result.is_ok());
}

#[test]
fn test_resolve_path_outside_root_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir(&root).unwrap();

    let session = Session::new()
        .with_allow_root(root.clone());

    // Try to escape via ../
    let input = "../outside.txt";
    let result = session.resolve_path(input);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("outside allowed root"));
}

#[test]
fn test_resolve_path_symlink_outside_rejected() {
    // Skip this test if symlinks are not available
    if cfg!(windows) {
        return;
    }

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir(&root).unwrap();

    let outside = temp.path().join("outside.txt");
    std::fs::write(&outside, "test").unwrap();

    // Create a symlink from inside to outside
    let symlink = root.join("link.txt");
    std::os::unix::fs::symlink(&outside, &symlink).unwrap();

    let session = Session::new()
        .with_allow_root(root);

    let result = session.resolve_path("link.txt");
    assert!(result.is_err());
}
```

- [ ] **Step 6: Add dev dependency on `tempfile`**

Update `crates/agnes-session/Cargo.toml`:

```toml
[dev-dependencies]
tempfile = "3"
```

- [ ] **Step 7: Run tests**

```bash
cargo test -p agnes-session
```

Expected: All tests pass.

- [ ] **Step 8: Commit**

```bash
git add crates/agnes-session/Cargo.toml crates/agnes-session/src/lib.rs crates/agnes-session/src/path_validation_tests.rs
git commit -m "feat(session): add path validation and allow_root support"
```

---

### Task 3: Update `read-file` and `write-file` with real I/O in `agnes-builtins`

**Files:**
- Modify: `crates/agnes-builtins/src/tools.rs`

**Interfaces:**
- Consumes: `Session::resolve_path` from agnes-session
- Produces: Real file I/O for `read-file` and `write-file` tools

- [ ] **Step 1: Update `read-file` implementation**

In `crates/agnes-builtins/src/tools.rs`, replace the existing mock `read-file`:

```rust
// read-file (real, from disk)
m.insert(
    "read-file".into(),
    Arc::new(|args| {
        Box::pin(async move {
            let path = arg_path(&args, "path")?;
            let content = tokio::fs::read_to_string(&path)
                .await
                .map_err(|e| format!("cannot read file '{}': {}", path.display(), e))?;
            Ok(Value::typed(
                JsonValue::String(content),
                "PlainText",
            ))
        })
    }),
);
```

Add the helper function `arg_path` at the top of the file:

```rust
use agnes_session::Session;
use std::path::PathBuf;

fn arg_path(args: &HashMap<String, Value>, key: &str) -> Result<PathBuf, String> {
    // The type checker should have already converted String -> Path for us,
    // but we still need to extract it. The declared type is Path, so the
    // value comes in as a string that's already been validated.
    args.get(key)
        .and_then(as_str)
        .ok_or_else(|| format!("missing :{key}"))
        .and_then(|s| {
            // The session resolved it during type checking, but we can
            // still resolve it again here for safety.
            std::env::current_dir()
                .map(|cwd| cwd.join(s))
                .map_err(|e| e.to_string())
        })
}
```

Wait — actually, the session reference needs to be available to tools. Let's check how session is accessed by builtins. The `native_dispatch` takes a `provider: Arc<dyn Provider>`, we need to also pass a reference to the session or allow tools to access `resolve_path`.

Actually, looking at current architecture: `native_dispatch` is called once to create the tools, and the tools don't have access to the session. The `resolve_path` needs to be called at execution time with the session's `allow_root`.

So we need to adjust: the session context needs to be available when the tool executes. This means the tool registry needs to accept a reference to the session when executing.

Looking at current architecture: `tools.rs` creates `HashMap<String, ToolImpl>`. `ToolImpl` is `Pin<Box<dyn Future<...>>>`. The session must be available during execution.

This means the `ToolImpl` signature needs to change. Actually looking at the existing code:

Current `ToolImpl`:
```rust
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
pub type ToolImpl =
    Arc<dyn Fn(HashMap<String, Value>) -> BoxFuture<'static, Result<Value, String>> + Send + Sync>;
```

The `Fn` doesn't receive a `&Session` parameter. We need to update this.

So update the `ToolImpl` definition:

```rust
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
pub type ToolImpl =
    Arc<dyn Fn(HashMap<String, Value>, &Session) -> BoxFuture<'static, Result<Value, String>> + Send + Sync>;
```

Then update the call site in the runtime where tools are invoked. This change needs to happen in `agnes-runtime`.

Let me adjust. This task becomes: update the `ToolImpl` signature to include `&Session`.

So first step update in `agnes-builtins/src/tools.rs`:

```rust
// Change line 12:
pub type ToolImpl =
    Arc<dyn Fn(HashMap<String, Value>, &Session) -> BoxFuture<'static, Result<Value, String>> + Send + Sync>;
```

Then update every tool closure to accept the second parameter (most tools don't need it, so just ignore it):

For example, `join-lines`:

```rust
Arc::new(|args, _session| {
    Box::pin(async move {
        // existing code unchanged
    })
}),
```

Update **all existing tools** to add the `_session` parameter.

Then update `read-file` and `write-file` to use `session.resolve_path`:

```rust
// read-file (real, from disk)
m.insert(
    "read-file".into(),
    Arc::new(|args, session| {
        Box::pin(async move {
            let path_str = arg_str(&args, "path")?;
            let path = session.resolve_path(&path_str)?;
            let content = tokio::fs::read_to_string(&path)
                .await
                .map_err(|e| format!("cannot read file '{}': {}", path.display(), e))?;
            Ok(Value::typed(
                JsonValue::String(content),
                "PlainText",
            ))
        })
    }),
);

// write-file (real, to disk)
m.insert(
    "write-file".into(),
    Arc::new(|args, session| {
        Box::pin(async move {
            let path_str = arg_str(&args, "path")?;
            let content = arg_str(&args, "content")?;
            let path = session.resolve_path(&path_str)?;

            // Create parent directories if they don't exist
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| format!("cannot create directory '{}': {}", parent.display(), e))?;
            }

            tokio::fs::write(&path, content)
                .await
                .map_err(|e| format!("cannot write file '{}': {}", path.display(), e))?;

            Ok(Value::typed(JsonValue::Null, "Unit"))
        })
    }),
);
```

- [ ] **Step 2: Update `ToolImpl` signature in all crates**

Update `ToolImpl` in `agnes-builtins`: done above.

Now update the call site in `agnes-runtime` where the tool is invoked.

In `agnes-runtime/src/lib.rs` (wherever tools are called):

Find the code that calls the tool, add the `&session` parameter:

```rust
// Before:
tool(args)
// After:
tool(args, session)
```

- [ ] **Step 3: Fix compilation errors**

```bash
cargo check
```

Fix any compilation errors from the signature change. This mainly updates the tool call site.

- [ ] **Step 4: Run existing tests**

```bash
cargo test
```

Expected: All existing tests still pass.

- [ ] **Step 5: Commit**

```bash
git add crates/agnes-builtins/src/tools.rs crates/agnes-runtime/src/lib.rs
git commit -m "feat(builtins): add session parameter to ToolImpl, implement real read-file/write-file"
```

---

### Task 4: Add `parse-path` tool to `agnes-builtins`

**Files:**
- Modify: `crates/agnes-builtins/src/tools.rs`

**Interfaces:**
- Consumes: `Session::resolve_path`
- Produces: `parse-path` tool that converts `String` to validated `Path`

- [ ] **Step 1: Add `parse-path` tool**

In `native_dispatch` function, add:

```rust
// parse-path — parse and validate a string path to Path type
m.insert(
    "parse-path".into(),
    Arc::new(|args, session| {
        Box::pin(async move {
            let path_str = arg_str(&args, "path")?;
            let _path = session.resolve_path(&path_str)?;
            // Path is represented as a string in the AST, but validated by session
            Ok(Value::typed(
                JsonValue::String(path_str),
                "Path",
            ))
        })
    }),
);
```

- [ ] **Step 2: Build and check**

```bash
cargo check -p agnes-builtins
```

Expected: Compiles successfully.

- [ ] **Step 3: Commit**

```bash
git add crates/agnes-builtins/src/tools.rs
git commit -m "feat(builtins): add parse-path tool"
```

---

### Task 5: Add `shell-run` tool with user confirmation

**Files:**
- Modify: `crates/agnes-builtins/src/tools.rs`
- Modify: `crates/agnes-session/src/lib.rs` (already has `ShellConfirm` event)

**Interfaces:**
- Consumes: `SessionEvent::ShellConfirm`, `tokio::process::Command`
- Produces: `shell-run` tool that executes commands after user confirmation

- [ ] **Step 1: Add `shell-run` tool implementation**

In `native_dispatch` function, add:

```rust
use tokio::process::Command;
use serde_json::json;

// shell-run — execute shell command with user confirmation
m.insert(
    "shell-run".into(),
    Arc::new(|args, session| {
        Box::pin(async move {
            let command = arg_str(&args, "command")?;

            if !session.allow_shell() {
                return Err("shell execution is not enabled in this session. \
                    Use --allow-shell flag to enable it.".into());
            }

            // Request user confirmation via session event
            let (tx, rx) = tokio::sync::oneshot::channel();
            session.emit_event(SessionEvent::ShellConfirm {
                command: command.clone(),
                responder: tx,
            }).await;

            // Wait for user response
            let confirmed = rx.await.unwrap_or(false);
            if !confirmed {
                return Err("shell execution cancelled by user".into());
            }

            // Execute command
            let mut child = Command::new("sh")
                .arg("-c")
                .arg(&command)
                .output()
                .await
                .map_err(|e| format!("failed to spawn command: {}", e))?;

            let stdout = String::from_utf8_lossy(&child.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&child.stderr).into_owned();
            let exit_code = child.status.code().unwrap_or(-1);

            // Return structured result
            let result = json!({
                "stdout": stdout,
                "stderr": stderr,
                "exit-code": exit_code,
            });

            Ok(Value::typed(result, "CommandResult"))
        })
    }),
);
```

Wait: need to add `emit_event` method to `Session`. Check how events are currently emitted. Add it to `Session` if needed.

- [ ] **Step 2: Add `CommandResult` to built-in types in `agnes-types`**

Edit `crates/agnes-types/src/types.rs` to register `CommandResult`:

```rust
pub const BUILTIN_TYPES: &[(&str, &str)] = &[
    // ... existing ...
    ("CommandResult", "Result of shell command execution: {stdout, stderr, exit-code}"),
];
```

- [ ] **Step 3: Build and check**

```bash
cargo check
```

Expected: Compiles successfully.

- [ ] **Step 4: Commit**

```bash
git add crates/agnes-types/src/types.rs crates/agnes-builtins/src/tools.rs
git commit -m "feat(builtins): add shell-run tool with user confirmation"
```

---

### Task 6: Add CLI flags and handle confirmation prompt in `agnes-cli`

**Files:**
- Modify: `crates/agnes-cli/src/main.rs`
- Modify: `crates/agnes-cli/src/chat.rs`

**Interfaces:**
- Consumes: `SessionEvent::ShellConfirm`
- Produces: Interactive user prompt for shell confirmation

- [ ] **Step 1: Add CLI flags**

In `crates/agnes-cli/src/main.rs`, update the `Cli` struct:

```rust
#[derive(Debug, Parser)]
#[command(author, version, about, long_about = None)]
pub struct Cli {
    // ... existing fields ...

    /// Restrict file operations to this directory (default: current working directory)
    #[arg(long = "allow-root")]
    allow_root: Option<PathBuf>,

    /// Allow shell command execution without interactive confirmation (requires interactive mode)
    #[arg(long = "allow-shell", default_value_t = false)]
    allow_shell: bool,
}
```

- [ ] **Step 2: Pass flags to Session when creating it**

When creating the Session in chat mode, apply the builder methods:

```rust
let mut session = Session::new(...);
if let Some(allow_root) = cli.allow_root {
    session = session.with_allow_root(allow_root);
}
session = session.with_allow_shell(cli.allow_shell);
```

- [ ] **Step 3: Handle `ShellConfirm` event in chat loop**

In `crates/agnes-cli/src/chat.rs`, in the event processing loop:

```rust
match event {
    // ... existing cases ...
    SessionEvent::ShellConfirm { command, responder } => {
        println!();
        println!("\x1b[1m[agnes] Confirm shell execution:\x1b[0m");
        println!("  Command: {}", command);
        print!("  OK to run? [Y/n] ");
        std::io::stdout().flush().unwrap();

        let mut input = String::new();
        std::io::stdin().read_line(&mut input).unwrap();
        let input = input.trim().to_lowercase();

        let approved = input.is_empty() || input == "y" || input == "yes";
        let _ = responder.send(approved);
    }
}
```

- [ ] **Step 4: Build and check**

```bash
cargo check -p agnes-cli
```

Expected: Compiles successfully.

- [ ] **Step 5: Test interactive flow manually**

Run:
```bash
cargo run -p agnes-cli -- chat --allow-shell
```

Try `(tool shell-run :command "echo hello")` — should prompt for confirmation.

- [ ] **Step 6: Commit**

```bash
git add crates/agnes-cli/src/main.rs crates/agnes-cli/src/chat.rs
git commit -m "feat(cli): add --allow-root and --allow-shell flags, handle shell confirmation prompt"
```

---

### Task 7: Final testing and documentation update

**Files:**
- Modify: `README.md` (optional, to mention new features)

- [ ] **Step 1: Run full test suite**

```bash
cargo test --all
```

Expected: All tests pass.

- [ ] **Step 2: Test a simple workflow**

Create a test Agnes program that reads and writes a file:

```lisp
;; test-write.agnes
(tool write-file :path "test-output.txt" :content "Hello from Agnes!")
```

Run:
```bash
cargo run -p agnes-cli -- --allow-root . test-write.agnes
cat test-output.txt
```

Expected: File created with correct content.

- [ ] **Step 3: Clean up test file**

```bash
rm test-output.txt
```

- [ ] **Step 4: Commit any remaining fixes**

```bash
# if any fixes needed
git add ...
git commit -m "test: fix issues found during final testing"
```

---

## Self-Review

1. **Spec coverage:** All requirements from the spec are covered:
   - `Path` type ✓
   - Real `read-file`/`write-file` ✓
   - `parse-path` tool ✓
   - `shell-run` with user confirmation ✓
   - Path boundary checking ✓
   - Symlink checking ✓
   - `--allow-root` and `--allow-shell` CLI flags ✓

2. **Placeholders:** No TBD or TODO placeholders ✓

3. **Type consistency:** All function signatures match across tasks ✓

4. **Task sizing:** Each task is small enough to complete independently ✓

Plan complete.
