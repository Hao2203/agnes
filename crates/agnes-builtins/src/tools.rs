use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use agnes_types::Value;
use serde_json::Value as JsonValue;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
pub type ToolImpl =
    Arc<dyn Fn(HashMap<String, Value>) -> BoxFuture<'static, Result<Value, String>> + Send + Sync>;

pub fn native_dispatch() -> HashMap<String, ToolImpl> {
    let mut m: HashMap<String, ToolImpl> = HashMap::new();

    m.insert(
        "read-file".into(),
        Arc::new(|args| {
            Box::pin(async move {
                let path = args.get("path").ok_or("missing :path")?;
                let s = path.data.as_str().ok_or("path not string")?;
                let bytes = tokio::fs::read(s).await.map_err(|e| format!("read: {e}"))?;
                let text = String::from_utf8(bytes).map_err(|e| format!("utf8: {e}"))?;
                Ok(Value::typed(JsonValue::String(text), "PlainText"))
            })
        }),
    );

    m.insert(
        "write-file".into(),
        Arc::new(|args| {
            Box::pin(async move {
                let path = args
                    .get("path")
                    .ok_or("missing :path")?
                    .data
                    .as_str()
                    .ok_or("path not string")?
                    .to_string();
                let content = args
                    .get("content")
                    .ok_or("missing :content")?
                    .data
                    .as_str()
                    .ok_or("content not string")?
                    .to_string();
                tokio::fs::write(&path, content)
                    .await
                    .map_err(|e| format!("write: {e}"))?;
                Ok(Value::typed(JsonValue::Null, "Unit"))
            })
        }),
    );

    m.insert(
        "summarize".into(),
        Arc::new(|args| {
            Box::pin(async move {
                let input = extract_input(&args)?;
                let summary = format!("[SUMMARY of {} chars]", input.len());
                Ok(Value::typed(JsonValue::String(summary), "Summary"))
            })
        }),
    );

    m.insert(
        "translate".into(),
        Arc::new(|args| {
            Box::pin(async move {
                let input = extract_input(&args)?;
                let lang = args
                    .get("lang")
                    .ok_or("missing :lang")?
                    .data
                    .as_str()
                    .ok_or("lang not string")?
                    .to_string();
                let out = format!("[TRANSLATED to {lang}]\n{input}");
                Ok(Value::typed(JsonValue::String(out), "PlainText"))
            })
        }),
    );

    m.insert(
        "ocr".into(),
        Arc::new(|args| {
            Box::pin(async move {
                let _ = args.get("source").ok_or("missing :source")?;
                Ok(Value::typed(
                    JsonValue::String("[OCR-EXTRACTED-TEXT]".into()),
                    "PlainText",
                ))
            })
        }),
    );

    m.insert(
        "llm".into(),
        Arc::new(|args| {
            Box::pin(async move {
                let prompt = args
                    .get("prompt")
                    .ok_or("missing :prompt")?
                    .data
                    .as_str()
                    .unwrap_or("")
                    .to_string();
                let input = args
                    .get("input")
                    .map(|v| v.data.as_str().unwrap_or(""))
                    .unwrap_or("");
                let out = format!("[LLM prompt={prompt} input_len={}]", input.len());
                Ok(Value::typed(JsonValue::String(out), "PlainText"))
            })
        }),
    );

    m
}

/// Try either :input (kw form) or the sole positional (flowed-in).
fn extract_input(args: &HashMap<String, Value>) -> Result<String, String> {
    if let Some(v) = args.get("input") {
        return Ok(v.data.as_str().unwrap_or("").to_string());
    }
    // Flowed-in value is passed under the tool's declared sole-param name.
    // In MVP the runtime binds it to the parameter name; we look for a
    // "_flowed" convention as fallback if it wasn't rekeyed.
    args.iter()
        .find_map(|(_, v)| v.data.as_str().map(str::to_string))
        .ok_or_else(|| "no input".into())
}
