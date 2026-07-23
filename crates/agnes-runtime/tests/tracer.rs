use agnes_builtins::{register_builtins, PathResolver, Sink, ToolCtx, ToolFn};
use agnes_compiler::{NodeKind, compile};
use agnes_parser::parse;
use agnes_registry::Registry;
use agnes_runtime::{NoopTracer, RuntimeError, Tracer, execute_with};
use agnes_types::Value;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::oneshot;

struct DummyResolver;
impl PathResolver for DummyResolver {
    fn resolve_path<'a>(&'a self, _input: &'a str) -> agnes_builtins::BoxFuture<'a, Result<PathBuf, String>> {
        panic!("dummy resolver should not be called in this test");
    }
}

/// No-op sink for tests that don't exercise shell-run.
struct DummySink;
impl Sink for DummySink {
    fn shell_confirm<'a>(
        &'a self,
        _command: String,
        responder: oneshot::Sender<bool>,
    ) -> agnes_builtins::BoxFuture<'a, ()> {
        Box::pin(async move {
            let _ = responder.send(false);
        })
    }
    fn shell_output<'a>(
        &'a self,
        _line: String,
        _is_stderr: bool,
    ) -> agnes_builtins::BoxFuture<'a, ()> {
        Box::pin(async {})
    }
}

static DUMMY_SINK: DummySink = DummySink;

fn ctx(resolver: &DummyResolver) -> ToolCtx<'_> {
    ToolCtx {
        resolver,
        sink: &DUMMY_SINK,
        allow_shell: false,
    }
}

#[derive(Default)]
struct RecordingTracer {
    events: Arc<Mutex<Vec<String>>>,
}

impl Tracer for RecordingTracer {
    fn node_start(&self, _id: agnes_compiler::NodeId, kind: &NodeKind, args: &str) {
        let label = match kind {
            NodeKind::Tool { name } => format!("start tool:{name} args={args}"),
            _ => return,
        };
        self.events.lock().unwrap().push(label);
    }
    fn node_end(
        &self,
        _id: agnes_compiler::NodeId,
        result: Result<&Value, &RuntimeError>,
        _elapsed: Duration,
    ) {
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

    let read_file: ToolFn = Box::new(move |_args, _ctx| {
            Box::pin(async { Ok(Value::typed(JsonValue::String("hello".into()), "String")) })
        });
    m.insert(
        "read-file".to_string(),
        Arc::new(read_file) as Arc<dyn agnes_builtins::Tool + Send + Sync>
    );

    let summarize: ToolFn = Box::new(move |_args, _ctx| {
            Box::pin(async {
                Ok(Value::typed(
                    JsonValue::String("[SUMMARY]".into()),
                    "String",
                ))
            })
        });
    m.insert(
        "summarize".to_string(),
        Arc::new(summarize) as Arc<dyn agnes_builtins::Tool + Send + Sync>
    );

    m
}

#[tokio::test]
async fn tracer_receives_start_and_end_per_tool_node() {
    let src = r#"(pipe (tool read-file "x") (tool summarize))"#;
    let mut r = Registry::new();
    register_builtins(&mut r).unwrap();
    let p = parse(src).unwrap();
    r.load(&p).unwrap();
    agnes_checker::check(&p, &r).unwrap();
    let dag = compile(&p, &r).unwrap();
    let dispatch = stub_dispatch();

    let tracer = RecordingTracer::default();
    let dummy = DummyResolver;
    let _ = execute_with(&dag, &r, &dispatch, &ctx(&dummy), &tracer).await.unwrap();

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
    let src = r#"(tool read-file "x")"#;
    let mut r = Registry::new();
    register_builtins(&mut r).unwrap();
    let p = parse(src).unwrap();
    r.load(&p).unwrap();
    agnes_checker::check(&p, &r).unwrap();
    let dag = compile(&p, &r).unwrap();
    let dispatch = stub_dispatch();
    let dummy = DummyResolver;
    let v = agnes_runtime::execute(&dag, &r, &dispatch, &ctx(&dummy)).await.unwrap();
    assert_eq!(v.data.as_str().unwrap(), "hello");
    let _ = NoopTracer; // touch to ensure it's exported.
}
