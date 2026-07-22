# Design: Real File I/O and User-Confirmed Shell Execution

## Overview

This extension enables Agnes to perform real code-writing tasks by adding:
1. Actual disk file reading and writing (sandboxed to an allowed directory)
2. User-confirmed shell command execution
3. After this change, an LLM can complete full programming tasks within the Agnes interactive chat loop — for example, starting from scratch and writing a simple working web server.

## Background

The current MVP of Agnes has mock `read-file` and `write-file` tools that only work with in-memory fixture data. This is fine for demo, but to actually use Agnes for LLM-planned coding workflows, we need real file system access.

Adding shell execution lets the LLM run build commands (like `cargo init`, `cargo add`, `cargo build`) to complete a full project.

## Design Decisions

### Security Model: Restricted Mode

We implement a **restricted (safe-by-default) mode**:
- All file operations are constrained to a configured "allow root" directory
- Any path that escapes this root directory (via `../`, symlinks, etc.) is rejected
- **All shell commands require explicit user confirmation before execution**
- Non-interactive mode disables shell execution by default

### Type System: `Path` Semantic Type

Add a new semantic type `Path` to the type registry in `agnes-types`:
- Represents a validated, absolute path within the allowed root
- `String` → `Path` implicit conversion happens during type checking
- Conversion includes path validation (checks boundary)
- For dynamically constructed paths, users explicitly call `(parse-path ...)`

### Tool API

All tools follow existing Lisp naming conventions (kebab-case) and require the `tool` prefix.

#### 1. `read-file`

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `path` | `Path` | Yes | Path to file to read |

**Returns:** `PlainText` — file contents as UTF-8 string.

**Changes:** Replaces the existing mock implementation with real disk I/O.

#### 2. `write-file`

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `path` | `Path` | Yes | Path to write to |
| `content` | `PlainText` | Yes | Content to write |

**Returns:** `Unit`

**Changes:** Replaces the existing mock implementation with real disk I/O. Parent directories are created if they don't exist.

#### 3. `parse-path`

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `path` | `String` | Yes | String path to parse and validate |

**Returns:** `Path` — validated Path value if path is within allowed root.

**Use case:** Dynamically constructing paths (e.g., string concatenation) where implicit conversion doesn't trigger automatically.

#### 4. `shell-run`

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `command` | `String` | Yes | Shell command to execute |

**Returns:** `CommandResult` (structured object):
```lisp
{
  stdout: PlainText,
  stderr: PlainText,
  exit-code: Int
}
```

**Behavior:**
1. Before execution, emits a `ShellConfirm` session event
2. Waits for user confirmation via the event channel
3. If user approves, executes the command via the system shell
4. Captures stdout, stderr, and exit code
5. Returns the structured result

### Component Changes

#### `agnes-types`
- Register the new `Path` type name in the type registry
- No other changes needed — the existing type system handles implicit conversion

#### `agnes-session`
- Add `allow_root: Option<PathBuf>` field to `Session`
  - `None` = defaults to current working directory
  - `Some(path)` = restrict to specified directory
- Add `resolve_path(&str) -> Result<PathBuf, String>` method
  - Performs canonicalization
  - Checks that path is within `allow_root`
  - Follows symlinks and checks the resolved path also stays within root
- Add `SessionEvent::ShellConfirm { command: String, responder: oneshot::Sender<bool> }`
- Add `allow_shell: bool` flag to control whether shell execution is permitted

#### `agnes-builtins`
- Modify `read-file` to use `Session::resolve_path` and real `tokio::fs::read_to_string`
- Modify `write-file` to use `Session::resolve_path` and real `tokio::fs::write` (creates parent dirs)
- Add `parse-path` tool implementation
- Add `shell-run` tool implementation with user confirmation flow
- Keep existing mock behavior optional behind a `mock` feature flag? No — replace mock with real I/O permanently. The mock data can still be used via the LLM tool.

#### `agnes-cli`
- Add `--allow-root <path>` CLI flag to set allowed root directory
- Add `--allow-shell` CLI flag to enable shell execution in non-interactive mode
- Handle `SessionEvent::ShellConfirm` by prompting the user in the terminal:
  ```
  [agnes] Confirm shell execution:
  Command: cargo build
  OK to run? [Y/n]
  ```
- Send the user's decision back via the responder channel

### Path Validation Algorithm

```rust
pub fn resolve_path(&self, input: &str) -> Result<PathBuf, String> {
    let allow_root = self.allow_root.as_ref()
        .unwrap_or(&std::env::current_dir().unwrap());

    // Resolve against current working directory
    let candidate = if input.is_absolute() {
        PathBuf::from(input)
    } else {
        std::env::current_dir().unwrap().join(input)
    };

    // Canonicalize to resolve symlinks and ..
    let canonical = candidate.canonicalize()?;

    // Check prefix
    if !canonical.starts_with(allow_root) {
        return Err(format!("Path '{}' is outside allowed root directory", input));
    }

    Ok(canonical)
}
```

### Usage Example

```lisp
;; Define a function that creates a simple Rust file
(define create-rs-file
  :params [(rel-path Path) (code PlainText)]
  :provides Unit
  (tool write-file :path rel-path :content code))

;; Create a basic web server using axum
(let src-path (parse-path "examples/web-server/src/main.rs"))
(tool write-file :path src-path
  :content "
use axum::Router;

#[tokio::main]
async fn main() {
    let app = Router::new()
        .route(\"/\", get(|| async { \"Hello, World!\" }));

    axum::Server::bind(\"127.0.0.1:3000\".parse().unwrap())
        .serve(app.into_make_svc())
        .await
        .unwrap();
}
")

;; Build it
(tool shell-run :command "cd examples/web-server && cargo build")
```

### Error Handling

- All I/O errors are converted to human-readable error strings and returned as `Err`
- Path boundary violations return clear error messages
- User cancellation of shell execution returns `Err("shell execution cancelled by user")`
- Shell execution disabled returns `Err("shell execution is not enabled in this session")`

### Testing

- Add unit tests for the path validation logic in `agnes-session`
- Test various escape attempts: `../`, multiple `../..`, symlinks pointing outside
- Integration tests for real file read/write within a temp directory

## Impact

- This is a backwards-compatible extension: existing DSL programs continue to work
- The change enables the main use case: "LLM writes full code project in Agnes chat loop"
- Security is maintained through restriction + explicit confirmation
- After this implementation, we can demonstrate creating a complete simple web server

## Dependencies

- Uses `tokio::fs` for async file I/O (already in workspace dependencies)
- Uses `tokio::process::Command` for async process execution (already in tokio features)
- Uses `oneshot` channel from tokio for confirmation (already available)

No new dependencies need to be added.
