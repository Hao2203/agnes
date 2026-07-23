use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::path::PathBuf;

use agnes_llm::{CompletionRequest, Message, Provider, Role};
use agnes_types::Value;
use serde_json::Value as JsonValue;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Resolve and validate paths against an allowed root. This is the only
/// capability a path-handling tool needs; shell gating and sink emission
/// live on separate traits (ISP - tools no longer depend on methods they
/// don't use).
pub trait PathResolver: Send + Sync + std::any::Any {
    /// Resolve a user-provided path, ensuring it's within the allowed root directory.
    fn resolve_path<'a>(&'a self, input: &'a str) -> BoxFuture<'a, Result<PathBuf, String>>;
}

/// Emit shell-related events to the session sink. Kept separate from
/// `PathResolver` so non-shell tools don't depend on these methods, and
/// so adding a path capability doesn't touch shell plumbing (and vice
/// versa). Implementations forward to the shared sink without holding a
/// lock across a long await.
pub trait Sink: Send + Sync {
    /// Request user confirmation before running a shell command. The
    /// `responder` receives `true` to approve, `false` to cancel.
    fn shell_confirm<'a>(
        &'a self,
        command: String,
        responder: tokio::sync::oneshot::Sender<bool>,
    ) -> BoxFuture<'a, ()>;

    /// Forward one line of live output from a running shell command.
    /// `is_stderr` marks the stream origin. Streamed as produced (not
    /// buffered until exit) so the user can watch long-running commands.
    fn shell_output<'a>(&'a self, line: String, is_stderr: bool) -> BoxFuture<'a, ()>;
}

/// Everything a tool needs from the session at call time: path
/// resolution, shell-event emission, and the shell-gating flag. Passed
/// as a single borrowed context so the scheduler threads one value
/// instead of three, and so each trait stays narrow.
pub struct ToolCtx<'a> {
    pub resolver: &'a (dyn PathResolver + Send + Sync),
    pub sink: &'a (dyn Sink + Send + Sync),
    pub allow_shell: bool,
}

/// Tool trait defines the interface that all tools must implement.
pub trait Tool: Send + Sync {
    /// Call the tool with the given arguments and context.
    fn call<'a>(&'a self, args: HashMap<String, Value>, ctx: &'a ToolCtx<'a>) -> BoxFuture<'a, Result<Value, String>>;
}

/// Blanket implementation for any correctly-typed function.
impl<F> Tool for F
where
    F: for<'a> Fn(HashMap<String, Value>, &'a ToolCtx<'a>) -> BoxFuture<'a, Result<Value, String>>
    + Send
    + Sync
    + 'static,
{
    fn call<'a>(&'a self, args: HashMap<String, Value>, ctx: &'a ToolCtx<'a>) -> BoxFuture<'a, Result<Value, String>> {
        self(args, ctx)
    }
}

pub type ToolImpl = Arc<dyn Tool + Send + Sync>;

/// Concrete type of a native tool closure: `Fn(args, &ToolCtx) -> BoxFuture`.
/// Spelled out once here so tool registrations read cleanly and clippy's
/// `very_complex_type` stays quiet.
pub type ToolFn =
    Box<dyn for<'a> Fn(HashMap<String, Value>, &'a ToolCtx<'a>) -> BoxFuture<'a, Result<Value, String>> + Send + Sync + 'static>;

/// Per-process recording of every mock write-file call, drained by
/// `agnes_session::Session::run_turn` at the end of each turn and emitted
/// as a `SessionEvent::WriteSummary`. Call sites should NOT rely on this
/// list accumulating across turns — Session takes ownership of the entries
/// on both the success and failure paths.
pub fn writes() -> &'static Mutex<Vec<(String, usize)>> {
    static WRITES: OnceLock<Mutex<Vec<(String, usize)>>> = OnceLock::new();
    WRITES.get_or_init(|| Mutex::new(Vec::new()))
}

const MAX_TOKENS: u32 = 1024;

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

    // read-file (real, from disk)
    let read_file: ToolFn =
        Box::new(|args, ctx| {
            Box::pin(async move {
                let path_str = arg_str(&args, "path")?;
                let path = ctx.resolver.resolve_path(&path_str).await?;
                let content = tokio::fs::read_to_string(&path)
                    .await
                    .map_err(|e| format!("cannot read file '{}': {}", path.display(), e))?;
                Ok(Value::typed(
                    JsonValue::String(content),
                    "String",
                ))
            })
        });
    m.insert("read-file".into(), Arc::new(read_file));

    // write-file (real, to disk)
    let write_file: ToolFn =
        Box::new(|args, ctx| {
            Box::pin(async move {
                let path_str = arg_str(&args, "path")?;
                let content = arg_str(&args, "content")?;
                let content_len = content.len();
                let path = ctx.resolver.resolve_path(&path_str).await?;

                // Create parent directories if they don't exist
                if let Some(parent) = path.parent() {
                    tokio::fs::create_dir_all(parent)
                        .await
                        .map_err(|e| format!("cannot create directory '{}': {}", parent.display(), e))?;
                }

                tokio::fs::write(&path, content)
                    .await
                    .map_err(|e| format!("cannot write file '{}': {}", path.display(), e))?;

                // Record write for WriteSummary event
                writes().lock().unwrap().push((path.display().to_string(), content_len));

                Ok(Value::typed(JsonValue::Null, "Unit"))
            })
        });
    m.insert("write-file".into(), Arc::new(write_file));

    // join-lines (real, kept)
    let join_lines: ToolFn =
        Box::new(|args, _ctx| {
            Box::pin(async move {
                let lines = args
                    .get("lines")
                    .ok_or_else(|| "missing `lines` parameter".to_string())?
                    .data
                    .as_array()
                    .ok_or_else(|| "lines is not a JSON array".to_string())?
                    .iter()
                    .map(|v| v.as_str().unwrap_or("").to_string())
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(Value::typed(JsonValue::String(lines), "String"))
            })
        });
    m.insert("join-lines".into(), Arc::new(join_lines));

    // llm (real provider call)
    {
        let p = provider.clone();
        let llm: ToolFn =
            Box::new(move |args, _ctx| {
                let p = p.clone();
                Box::pin(async move {
                    let prompt = arg_str(&args, "prompt")?;
                    let input = args.get("input").and_then(as_str).unwrap_or_default();
                    let user = if input.is_empty() {
                        prompt
                    } else {
                        format!("{prompt}\n\n{input}")
                    };
                    let out = p
                        .complete(CompletionRequest {
                            system: None,
                            messages: vec![Message {
                                role: Role::User,
                                content: user,
                            }],
                            max_tokens: MAX_TOKENS,
                        })
                        .await
                        .map_err(|e| e.to_string())?;
                    Ok(Value::typed(JsonValue::String(out), "String"))
                })
            });
        m.insert("llm".into(), Arc::new(llm));
    }

    // summarize (real provider call)
    {
        let p = provider.clone();
        let summarize: ToolFn =
            Box::new(move |args, _ctx| {
                let p = p.clone();
                Box::pin(async move {
                    let input = arg_str(&args, "input")?;
                    let out = p
                        .complete(CompletionRequest {
                            system: Some(
                                "You are a concise summarizer. Return one paragraph.".into(),
                            ),
                            messages: vec![Message {
                                role: Role::User,
                                content: format!("Summarize the following:\n\n{input}"),
                            }],
                            max_tokens: MAX_TOKENS,
                        })
                        .await
                        .map_err(|e| e.to_string())?;
                    Ok(Value::typed(JsonValue::String(out), "String"))
                })
            });
        m.insert("summarize".into(), Arc::new(summarize));
    }

    // translate (real provider call)
    {
        let p = provider.clone();
        let translate: ToolFn =
            Box::new(move |args, _ctx| {
                let p = p.clone();
                Box::pin(async move {
                    let input = arg_str(&args, "input")?;
                    let lang = arg_str(&args, "lang")?;
                    let out = p
                        .complete(CompletionRequest {
                            system: Some("You are a professional translator.".into()),
                            messages: vec![Message {
                                role: Role::User,
                                content: format!(
                                    "Translate to {lang}. Output only the translation.\n\n{input}"
                                ),
                            }],
                            max_tokens: MAX_TOKENS,
                        })
                        .await
                        .map_err(|e| e.to_string())?;
                    Ok(Value::typed(JsonValue::String(out), "String"))
                })
            });
        m.insert("translate".into(), Arc::new(translate));
    }

    // `finish` and `observe` are not registered as tools: they are
    // Expr::Finish / Expr::Observe special forms dispatched by the compiler
    // and runtime directly. See `agnes-runtime::scheduler::NodeKind::Finish`
    // and `NodeKind::Observe`.

    // parse-path - parse and validate a string path to Path type
    let parse_path: ToolFn =
        Box::new(|args, ctx| {
            Box::pin(async move {
                let path_str = arg_str(&args, "path")?;
                let _path = ctx.resolver.resolve_path(&path_str).await?;
                // Path is represented as a string in the AST, but validated by session
                Ok(Value::typed(
                    JsonValue::String(path_str),
                    "Path",
                ))
            })
        });
    m.insert("parse-path".into(), Arc::new(parse_path));

    // shell-run - execute shell command with user confirmation
    use tokio::process::Command;
    use tokio::io::{AsyncBufReadExt, BufReader};
    use std::process::Stdio;
    use serde_json::json;

    let shell_run: ToolFn =
        Box::new(|args, ctx| {
            Box::pin(async move {
                let command = arg_str(&args, "command")?;

                if !ctx.allow_shell {
                    return Err("shell execution is not enabled in this session. \
                        Use --allow-shell flag to enable it.".into());
                }

                // Request user confirmation via the sink
                let (tx, rx) = tokio::sync::oneshot::channel();
                ctx.sink.shell_confirm(command.clone(), tx).await;

                // Wait for user response
                let confirmed = rx.await.unwrap_or(false);
                if !confirmed {
                    return Err("shell execution cancelled by user".into());
                }

                // Spawn with piped stdout/stderr so we can stream each line
                // live as it is produced, instead of buffering everything
                // until exit (which made long builds look hung).
                let mut child = Command::new("sh")
                    .arg("-c")
                    .arg(&command)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .map_err(|e| format!("failed to spawn command: {}", e))?;

                let stdout = child.stdout.take().unwrap();
                let stderr = child.stderr.take().unwrap();

                // Drain both streams concurrently: forward each line to the
                // sink for live display and accumulate into buffers for the
                // returned CommandResult. Both share `ctx.sink` by ref.
                let stdout_task = async {
                    let mut buf = String::new();
                    let mut lines = BufReader::new(stdout).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        ctx.sink.shell_output(line.clone(), false).await;
                        buf.push_str(&line);
                        buf.push('\n');
                    }
                    buf
                };
                let stderr_task = async {
                    let mut buf = String::new();
                    let mut lines = BufReader::new(stderr).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        ctx.sink.shell_output(line.clone(), true).await;
                        buf.push_str(&line);
                        buf.push('\n');
                    }
                    buf
                };
                let (stdout_str, stderr_str) = tokio::join!(stdout_task, stderr_task);

                let status = child
                    .wait()
                    .await
                    .map_err(|e| format!("failed to wait for command: {}", e))?;
                let exit_code = status.code().unwrap_or(-1);

                // Return structured result
                let result = json!({
                    "stdout": stdout_str,
                    "stderr": stderr_str,
                    "exit-code": exit_code,
                });

                Ok(Value::typed(result, "CommandResult"))
            })
        });
    m.insert("shell-run".into(), Arc::new(shell_run));

    m
}
