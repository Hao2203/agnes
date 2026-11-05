//! 输出层：所有 stdout 写入都交给一个独立线程，异步任务从不阻塞在终端上。
//! [`RendererPlugin`] 把 [`RunEvent`] 流渲染成行，并在 turn 收场时打印
//! 摘要、把没生效的用户消息还给输入队列。

use std::io::{BufWriter, Write};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};

use agnes_agent::runtime::{PendingUserMessages, TurnFinished};
use agnes_core::config::ModelName;
use agnes_core::llm::StopKind;
use agnes_core::message::{AssistantMessage, Message};
use agnes_core::turn::Outcome;
use agnes_ecs::{App, MessageReader, Plugin, Res, ResMut, RuntimeSender, Update};
use agnes_plugin_async::AsyncExecutor;
use agnes_record::{Event as RecordEvent, Log, RunEvent};
use jiff::Timestamp;

enum RenderCommand {
    Line(String),
    Prompt(String),
    Flush,
    Shutdown,
}

struct RendererInner {
    tx: std_mpsc::Sender<RenderCommand>,
    writer: Mutex<Option<JoinHandle<()>>>,
}

/// A cloneable handle to the stdout writer thread.
#[derive(Clone)]
pub struct Renderer {
    inner: Arc<RendererInner>,
}

impl Renderer {
    pub fn new() -> std::io::Result<Self> {
        let (tx, rx) = std_mpsc::channel::<RenderCommand>();
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

/// 渲染所需的宿主状态。
struct DisplayState {
    model: ModelName,
    renderer: Renderer,
    sender: RuntimeSender,
    /// 单次运行模式：收场即退出，未生效的用户消息只打印提示。
    single_shot: bool,
    /// 有会话文件时，轮转反馈由 `SessionRotated`（新文件路径）给出；
    /// 无会话时轮转只清上下文，由 `rotate_feedback` 直接确认。
    has_session: bool,
}

/// 已收场、等待落盘确认的 turn 报告。摘要必须在会话耐久之后打印 ——
/// 否则退出可能抢掉最后一批事件。
#[derive(Default)]
struct PendingReport(Option<ReportData>);

struct ReportData {
    outcome: Outcome,
    usage: agnes_core::message::TokenUsage,
    steps_used: u32,
    started_at: Timestamp,
    finished_at: Timestamp,
    failure: Option<String>,
}

/// 注册渲染系统：[`RunEvent`] 流 + turn 收场报告。
pub struct RendererPlugin {
    model: ModelName,
    renderer: Renderer,
    sender: RuntimeSender,
    single_shot: bool,
    has_session: bool,
}

impl RendererPlugin {
    pub fn new(
        model: ModelName,
        renderer: Renderer,
        sender: RuntimeSender,
        single_shot: bool,
        has_session: bool,
    ) -> Self {
        Self {
            model,
            renderer,
            sender,
            single_shot,
            has_session,
        }
    }
}

impl Plugin for RendererPlugin {
    fn name(&self) -> &'static str {
        "agnes-renderer"
    }

    fn build(&self, app: &mut App) {
        app.insert_resource(DisplayState {
            model: self.model.clone(),
            renderer: self.renderer.clone(),
            sender: self.sender.clone(),
            single_shot: self.single_shot,
            has_session: self.has_session,
        })
        .init_resource::<PendingReport>()
        .add_systems(Update, render_run_events)
        .add_systems(Update, render_rotations)
        .add_systems(Update, rotate_feedback)
        .add_systems(Update, report_finished);
    }
}

fn render_run_events(mut events: MessageReader<RunEvent>, state: Res<DisplayState>) {
    for event in events.read() {
        if let Some(text) = event_text(&state.model, event.as_ref()) {
            state.renderer.line(text);
        }
    }
}

/// 会话轮转（`/new`）：确认新文件并重画提示符，否则用户看到的只是沉默。
/// 仅在有会话文件时触发（`SessionRotated` 由 writer 任务回注）。
fn render_rotations(
    mut rotations: MessageReader<agnes_plugin_session::SessionRotated>,
    state: Res<DisplayState>,
) {
    for message in rotations.read() {
        state.renderer.line(format!(
            "(session cleared — new file: {})",
            message.path.display()
        ));
        if !state.single_shot {
            state.renderer.prompt("agnes> ");
        }
    }
}

/// 无会话文件的轮转确认：上下文即刻重置，没有 writer 回注可等。
fn rotate_feedback(
    mut rotations: MessageReader<agnes_record::RotateSession>,
    state: Res<DisplayState>,
) {
    if state.has_session {
        return;
    }
    if !rotations.read().is_empty() {
        state.renderer.line("(context cleared)");
        if !state.single_shot {
            state.renderer.prompt("agnes> ");
        }
    }
}

/// turn 收场：等落盘确认后打印摘要、把未生效的用户消息还给输入侧；
/// 单次运行模式随后请求退出。
fn report_finished(
    executor: Res<AsyncExecutor>,
    log: Res<Log>,
    durability: Option<Res<agnes_record::Durability>>,
    mut pending_msgs: ResMut<PendingUserMessages>,
    mut finished: MessageReader<TurnFinished>,
    mut report: ResMut<PendingReport>,
    state: Res<DisplayState>,
) {
    if report.0.is_none()
        && let Some(message) = finished.read().first()
    {
        let data = match &message.result {
            Ok(result) => ReportData {
                outcome: result.outcome,
                usage: result.usage,
                steps_used: result.steps_used,
                started_at: message.started_at,
                finished_at: message.finished_at,
                failure: None,
            },
            Err(failure) => ReportData {
                outcome: Outcome::Interrupted,
                usage: agnes_core::message::TokenUsage::default(),
                steps_used: 0,
                started_at: message.started_at,
                finished_at: message.finished_at,
                failure: Some(failure.cause.clone()),
            },
        };
        report.0 = Some(data);
    }

    let Some(data) = report.0.take() else {
        return;
    };
    // 会话失败也是终态：没有可等的落盘了。否则要求「已记录的事件全部
    // 落盘确认」—— 记录与本 tick 的转发有先后，序号比较覆盖两头。
    let durable = durability.is_none_or(|d| {
        d.failed.is_some()
            || (d.is_durable()
                && u64::try_from(log.events().len()).is_ok_and(|count| count <= d.flushed_seq))
    });
    if !durable {
        // 落盘确认未到：报告放回，下一 tick 再试。
        report.0 = Some(data);
        return;
    }
    match data.failure {
        Some(cause) => {
            state.renderer.line(format!("error: {cause}"));
            state.renderer.flush();
        }
        None => print_turn_summary(
            &state.renderer,
            &log,
            data.outcome,
            data.usage,
            data.steps_used,
            data.started_at,
            data.finished_at,
        ),
    }
    for content in pending_msgs.drain() {
        if state.single_shot {
            state
                .renderer
                .line(format!("note: unused queued message: {content}"));
        } else {
            // 排队的任务在空闲时自动依次开跑（与旧 chat 循环一致）。
            let _ = state
                .sender
                .send_message(crate::input::TaskLine { content });
        }
    }
    if state.single_shot {
        executor.cancellation().cancel();
    } else {
        state.renderer.prompt("agnes> ");
    }
}

fn recorded_text(event: &RecordEvent) -> Option<String> {
    match event {
        RecordEvent::Header(_) => None,
        RecordEvent::TurnStart => Some("─ turn ─".to_owned()),
        RecordEvent::MessageStart { .. } => Some("→ model call".to_owned()),
        RecordEvent::MessageEnd { stop, .. } => {
            matches!(stop, StopKind::ToolUse).then(|| "← tool calls requested".to_owned())
        }
        RecordEvent::ToolExecutionStart { call, .. } => {
            Some(format!("  ⚒ {} {}", call.name, call.arguments.as_str()))
        }
        RecordEvent::ToolExecutionEnd { .. } => None,
        RecordEvent::Message(Message::ToolResult { output, .. }) if output.is_error() => {
            Some("  ✗ failed".to_owned())
        }
        RecordEvent::Message(_) => None,
        RecordEvent::MessagesInjected { count } => Some(format!(
            "  ↪ {count} queued message(s) will be injected next step"
        )),
        RecordEvent::StepStart { .. }
        | RecordEvent::StepEnd { .. }
        | RecordEvent::TurnEnd { .. } => None,
    }
}

fn event_text(model: &ModelName, event: &RunEvent) -> Option<String> {
    match event {
        RunEvent::Started { tools, .. } => {
            let names: Vec<&str> = tools.iter().map(|tool| tool.as_str()).collect();
            Some(format!("* model {model}, tools: [{}]", names.join(", ")))
        }
        RunEvent::ToolGated { denied, .. } => {
            Some(format!("  ⛔ denied by {}: {}", denied.by, denied.reason))
        }
        // The approver task owns the interactive prompt: it must enter
        // answer mode before the question is visible.
        RunEvent::ApprovalRequested { .. } => None,
        RunEvent::ApprovalResolved { outcome, .. } => {
            (!outcome.allows()).then(|| format!("  ✗ not approved ({outcome:?})"))
        }
        RunEvent::Aborted { by, reason, .. } => Some(format!("  ⛔ aborted by {by}: {reason}")),
        RunEvent::Recorded { event, .. } => recorded_text(event),
        RunEvent::Finished { .. } => None,
        // The failure is reported with the turn summary; observers still see
        // this event.
        RunEvent::Failed { .. } => None,
        _ => None,
    }
}

/// turn 收场摘要：结果、最终答复与耗时。
fn print_turn_summary(
    renderer: &Renderer,
    log: &Log,
    outcome: Outcome,
    usage: agnes_core::message::TokenUsage,
    steps_used: u32,
    started_at: Timestamp,
    finished_at: Timestamp,
) {
    let reply = log
        .messages()
        .filter_map(|message| match message {
            Message::Assistant(AssistantMessage::Text(content)) => Some(content),
            _ => None,
        })
        .last()
        .cloned();
    match outcome {
        Outcome::Answered => {
            renderer.line("");
            if let Some(reply) = reply {
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
    let seconds = finished_at.duration_since(started_at).as_secs_f64();
    renderer.line(format!("[{seconds:.1}s, {steps_used} step(s), {usage}]"));
    renderer.flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use agnes_core::message::{ToolCallId, ToolOutput};
    use agnes_core::turn::StepNumber;
    use std::str::FromStr;

    fn model() -> ModelName {
        ModelName::from_str("gpt-test").unwrap()
    }

    #[test]
    fn record_lifecycle_events_map_to_one_line() {
        assert_eq!(
            recorded_text(&RecordEvent::TurnStart).as_deref(),
            Some("─ turn ─")
        );
        assert_eq!(
            recorded_text(&RecordEvent::MessageStart {
                step: StepNumber::FIRST,
            })
            .as_deref(),
            Some("→ model call")
        );
        assert_eq!(
            recorded_text(&RecordEvent::MessagesInjected { count: 2 }).as_deref(),
            Some("  ↪ 2 queued message(s) will be injected next step")
        );
    }

    #[test]
    fn recorded_wraps_event_text() {
        let event = RunEvent::Recorded {
            at: Timestamp::now(),
            event: RecordEvent::TurnStart,
        };
        assert_eq!(event_text(&model(), &event).as_deref(), Some("─ turn ─"));
    }

    #[test]
    fn failed_tool_results_are_rendered() {
        let event = RecordEvent::Message(Message::ToolResult {
            call_id: ToolCallId::new("c1"),
            output: ToolOutput::error("boom"),
        });
        assert_eq!(recorded_text(&event).as_deref(), Some("  ✗ failed"));
    }
}
