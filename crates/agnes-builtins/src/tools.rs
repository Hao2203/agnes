use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::path::PathBuf;

use agnes_llm::{CompletionRequest, Message, Provider, Role};
use agnes_types::{TypeExpr, TypeName, Value};
use serde_json::Value as JsonValue;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
/// Trait for types that can resolve and validate paths against an allowed root.
pub trait PathResolver: Send + Sync {
    /// Resolve a user-provided path, ensuring it's within the allowed root directory.
    fn resolve_path<'a>(&'a self, input: &'a str) -> BoxFuture<'a, Result<PathBuf, String>>;
}

/// Tool trait defines the interface that all tools must implement.
pub trait Tool: Send + Sync {
    /// Call the tool with the given arguments and path resolver.
    fn call<'a>(&'a self, args: HashMap<String, Value>, resolver: &'a (dyn PathResolver + Send + Sync)) -> BoxFuture<'a, Result<Value, String>>;
}

/// Blanket implementation for any correctly-typed function.
impl<F> Tool for F
where
    F: for<'a> Fn(HashMap<String, Value>, &'a (dyn PathResolver + Send + Sync)) -> BoxFuture<'a, Result<Value, String>>
    + Send
    + Sync
    + 'static,
{
    fn call<'a>(&'a self, args: HashMap<String, Value>, resolver: &'a (dyn PathResolver + Send + Sync)) -> BoxFuture<'a, Result<Value, String>> {
        self(args, resolver)
    }
}

pub type ToolImpl = Arc<dyn Tool + Send + Sync>;

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
    let read_file: Box<dyn for<'a> Fn(HashMap<String, Value>, &'a (dyn PathResolver + Send + Sync)) -> BoxFuture<'a, Result<Value, String>> + Send + Sync + 'static> =
        Box::new(|args, resolver| {
            Box::pin(async move {
                let path_str = arg_str(&args, "path")?;
                let path = resolver.resolve_path(&path_str).await?;
                let content = tokio::fs::read_to_string(&path)
                    .await
                    .map_err(|e| format!("cannot read file '{}': {}", path.display(), e))?;
                Ok(Value::typed(
                    JsonValue::String(content),
                    "PlainText",
                ))
            })
        });
    m.insert("read-file".into(), Arc::new(read_file));

    // write-file (real, to disk)
    let write_file: Box<dyn for<'a> Fn(HashMap<String, Value>, &'a (dyn PathResolver + Send + Sync)) -> BoxFuture<'a, Result<Value, String>> + Send + Sync + 'static> =
        Box::new(|args, resolver| {
            Box::pin(async move {
                let path_str = arg_str(&args, "path")?;
                let content = arg_str(&args, "content")?;
                let path = resolver.resolve_path(&path_str).await?;

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
        });
    m.insert("write-file".into(), Arc::new(write_file));

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

    // join-lines (real, kept)
    let join_lines: Box<dyn for<'a> Fn(HashMap<String, Value>, &'a (dyn PathResolver + Send + Sync)) -> BoxFuture<'a, Result<Value, String>> + Send + Sync + 'static> =
        Box::new(|args, _resolver| {
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
        });
    m.insert("join-lines".into(), Arc::new(join_lines));

    // llm (real provider call)
    {
        let p = provider.clone();
        let llm: Box<dyn for<'a> Fn(HashMap<String, Value>, &'a (dyn PathResolver + Send + Sync)) -> BoxFuture<'a, Result<Value, String>> + Send + Sync + 'static> =
            Box::new(move |args, _resolver| {
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
                    Ok(Value::typed(JsonValue::String(out), "PlainText"))
                })
            });
        m.insert("llm".into(), Arc::new(llm));
    }

    // summarize (real provider call)
    {
        let p = provider.clone();
        let summarize: Box<dyn for<'a> Fn(HashMap<String, Value>, &'a (dyn PathResolver + Send + Sync)) -> BoxFuture<'a, Result<Value, String>> + Send + Sync + 'static> =
            Box::new(move |args, _resolver| {
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
                    Ok(Value::typed(JsonValue::String(out), "Summary"))
                })
            });
        m.insert("summarize".into(), Arc::new(summarize));
    }

    // translate (real provider call)
    {
        let p = provider.clone();
        let translate: Box<dyn for<'a> Fn(HashMap<String, Value>, &'a (dyn PathResolver + Send + Sync)) -> BoxFuture<'a, Result<Value, String>> + Send + Sync + 'static> =
            Box::new(move |args, _resolver| {
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
                    Ok(Value::typed(JsonValue::String(out), "PlainText"))
                })
            });
        m.insert("translate".into(), Arc::new(translate));
    }

    // --- Loop control: finish / observe ---
    // Both are identity on data but rewrite declared_type at the outer
    // layer so Session::run_turn can classify the root shape.
    let finish: Box<dyn for<'a> Fn(HashMap<String, Value>, &'a (dyn PathResolver + Send + Sync)) -> BoxFuture<'a, Result<Value, String>> + Send + Sync + 'static> =
        Box::new(|mut kw: HashMap<String, Value>, _resolver| {
            Box::pin(async move {
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
            })
        });
    m.insert("finish".into(), Arc::new(finish));

    let observe: Box<dyn for<'a> Fn(HashMap<String, Value>, &'a (dyn PathResolver + Send + Sync)) -> BoxFuture<'a, Result<Value, String>> + Send + Sync + 'static> =
        Box::new(|mut kw: HashMap<String, Value>, _resolver| {
            Box::pin(async move {
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
            })
        });
    m.insert("observe".into(), Arc::new(observe));

    m
}
