//! 输出层：所有 stdout 写入都交给一个独立线程，异步任务从不阻塞在终端上。

use std::io::{BufWriter, Write};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};

use agnes_agent::{Event, RunSummary, Sink};
use agnes_core::config::ModelName;
use agnes_core::content::Content;
use agnes_core::event::Event as CoreEvent;
use agnes_core::llm::StopKind;
use agnes_core::message::Message;
use agnes_core::turn::Outcome;

enum RenderCommand {
    Line(String),
    Prompt(String),
    Flush,
    Shutdown,
}

struct RendererInner {
    tx: Sender<RenderCommand>,
    writer: Mutex<Option<JoinHandle<()>>>,
}

/// A cloneable handle to the stdout writer thread.
#[derive(Clone)]
pub struct Renderer {
    inner: Arc<RendererInner>,
}

impl Renderer {
    pub fn new() -> std::io::Result<Self> {
        let (tx, rx) = mpsc::channel::<RenderCommand>();
        let writer = thread::Builder::new()
            .name("agnes-render".into())
            .spawn(move || {
                let stdout = std::io::stdout();
                let mut out = BufWriter::new(stdout.lock());
                while let Ok(command) = rx.recv() {
                    match command {
                        RenderCommand::Line(line) => {
                            let _ = out.write_all(line.as_bytes());
                            let _ = out.write_all(b"\n");
                            let _ = out.flush();
                        }
                        RenderCommand::Prompt(prompt) => {
                            let _ = out.write_all(prompt.as_bytes());
                            let _ = out.flush();
                        }
                        RenderCommand::Flush => {
                            let _ = out.flush();
                        }
                        RenderCommand::Shutdown => {
                            let _ = out.flush();
                            break;
                        }
                    }
                }
            })?;
        Ok(Self {
            inner: Arc::new(RendererInner {
                tx,
                writer: Mutex::new(Some(writer)),
            }),
        })
    }

    pub fn line(&self, text: impl Into<String>) {
        let _ = self.inner.tx.send(RenderCommand::Line(text.into()));
    }

    pub fn prompt(&self, text: impl Into<String>) {
        let _ = self.inner.tx.send(RenderCommand::Prompt(text.into()));
    }

    pub fn flush(&self) {
        let _ = self.inner.tx.send(RenderCommand::Flush);
    }
}

impl Drop for RendererInner {
    fn drop(&mut self) {
        let _ = self.tx.send(RenderCommand::Shutdown);
        let writer = self
            .writer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(writer) = writer {
            let _ = writer.join();
        }
    }
}

pub struct EventPrinter {
    model: ModelName,
    renderer: Renderer,
}

impl EventPrinter {
    pub fn new(model: ModelName, renderer: Renderer) -> Self {
        Self { model, renderer }
    }
}

fn recorded_text(event: &CoreEvent) -> Option<String> {
    match event {
        CoreEvent::Header(_) => None,
        CoreEvent::TurnStart => Some("─ turn ─".to_owned()),
        CoreEvent::MessageStart { .. } => Some("→ model call".to_owned()),
        CoreEvent::MessageEnd { stop, .. } => {
            matches!(stop, StopKind::ToolUse).then(|| "← tool calls requested".to_owned())
        }
        CoreEvent::ToolExecutionStart { call, .. } => {
            Some(format!("  ⚒ {} {}", call.name, call.arguments.as_str()))
        }
        CoreEvent::ToolExecutionEnd { .. } => None,
        CoreEvent::Message(Message::ToolResult { output, .. }) if output.is_error() => {
            Some("  ✗ failed".to_owned())
        }
        CoreEvent::Message(_) => None,
        CoreEvent::MessagesInjected { count } => Some(format!(
            "  ↪ {count} queued message(s) will be injected next step"
        )),
        CoreEvent::StepStart { .. } | CoreEvent::StepEnd { .. } | CoreEvent::TurnEnd { .. } => None,
    }
}

fn event_text(model: &ModelName, event: &Event) -> Option<String> {
    match event {
        Event::Started { tools, .. } => {
            let names: Vec<&str> = tools.iter().map(|tool| tool.as_str()).collect();
            Some(format!("* model {model}, tools: [{}]", names.join(", ")))
        }
        Event::ToolGated { denied, .. } => {
            Some(format!("  ⛔ denied by {}: {}", denied.by, denied.reason))
        }
        // The approver task owns the interactive prompt: it must enter
        // answer mode before the question is visible.
        Event::ApprovalRequested { .. } => None,
        Event::ApprovalResolved { outcome, .. } => {
            (!outcome.allows()).then(|| format!("  ✗ not approved ({outcome:?})"))
        }
        Event::Recorded { event, .. } => recorded_text(event),
        Event::Finished { .. } => None,
        // The app reports the run error once; observers still see this.
        Event::Failed { .. } => None,
        _ => None,
    }
}

impl Sink for EventPrinter {
    fn event(&mut self, event: &Event) {
        if let Some(text) = event_text(&self.model, event) {
            self.renderer.line(text);
        }
    }
}

pub fn print_summary(summary: &RunSummary, renderer: &Renderer) {
    match summary.outcome {
        Outcome::Answered => {
            renderer.line("");
            if let Some(reply) = summary.reply() {
                renderer.line(reply.as_str());
            }
        }
        Outcome::Interrupted => {
            renderer.line("");
            renderer.line("(stopped: interrupted by the user — session kept)");
        }
        Outcome::BudgetExhausted => {
            renderer.line("");
            renderer.line("(stopped: step budget exhausted — raise --max-steps)");
        }
        Outcome::LengthExceeded => {
            renderer.line("");
            renderer.line("(stopped: model output hit the token limit)");
        }
        Outcome::ContentFiltered => {
            renderer.line("");
            renderer.line("(stopped: the response was blocked by the content filter)");
        }
    }
    let seconds = summary.duration().as_secs_f64();
    renderer.line(format!(
        "[{seconds:.1}s, {} step(s), {}]",
        summary.steps_used, summary.usage
    ));
    renderer.flush();
}

pub fn handle_pending<F>(messages: Vec<Message>, renderer: &Renderer, mut on_user: F)
where
    F: FnMut(Content),
{
    for message in messages {
        match message {
            Message::User { content } => on_user(content),
            other => renderer.line(format!("note: unused injected message: {other:?}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agnes_core::message::{ToolCallId, ToolOutput};
    use agnes_core::turn::StepNumber;
    use jiff::Timestamp;
    use std::str::FromStr;

    fn model() -> ModelName {
        ModelName::from_str("gpt-test").unwrap()
    }

    #[test]
    fn core_lifecycle_events_map_to_one_line() {
        assert_eq!(
            recorded_text(&CoreEvent::TurnStart).as_deref(),
            Some("─ turn ─")
        );
        assert_eq!(
            recorded_text(&CoreEvent::MessageStart {
                step: StepNumber::FIRST,
            })
            .as_deref(),
            Some("→ model call")
        );
        assert_eq!(
            recorded_text(&CoreEvent::MessagesInjected { count: 2 }).as_deref(),
            Some("  ↪ 2 queued message(s) will be injected next step")
        );
    }

    #[test]
    fn recorded_wraps_core_text() {
        let event = Event::Recorded {
            at: Timestamp::now(),
            event: CoreEvent::TurnStart,
        };
        assert_eq!(event_text(&model(), &event).as_deref(), Some("─ turn ─"));
    }

    #[test]
    fn failed_tool_results_are_rendered() {
        let event = CoreEvent::Message(Message::ToolResult {
            call_id: ToolCallId::new("c1"),
            output: ToolOutput::error("boom"),
        });
        assert_eq!(recorded_text(&event).as_deref(), Some("  ✗ failed"));
    }
}
