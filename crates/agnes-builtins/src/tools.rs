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

const MOCK_README: &str = "# agnes\n\nA Lisp-style DSL and Rust runtime for LLM-planned agent workflows, with a TypeScript-style semantic type system.";
const MOCK_NOTES: &str =
    "TODO(agnes): example note fixtures live here so demos don't need real disk I/O.";
const MOCK_DRAFT: &str =
    "Draft: agnes lets an LLM plan a workflow as a small DSL and hand it to a typed Rust runtime.";

fn read_file_mock(path: &str) -> String {
    match path {
        "README.md" => MOCK_README.into(),
        "NOTES.md" => MOCK_NOTES.into(),
        "draft.md" => MOCK_DRAFT.into(),
        other => format!(
            "[MOCK file at {other}: agnes is a Lisp-style DSL for LLM-planned agent workflows. Placeholder body — swap in seeded content by editing MOCK_* constants in agnes-builtins.]"
        ),
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
                Ok(Value::typed(
                    JsonValue::String(read_file_mock(&path)),
                    "PlainText",
                ))
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
                    JsonValue::String(
                        "Extracted text: agnes runtime dispatches LLM-planned workflows.".into(),
                    ),
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
            }),
        );
    }

    m
}
