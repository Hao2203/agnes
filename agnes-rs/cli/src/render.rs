//! 渲染层：[`Renderer`] 句柄、渲染系统，以及从 `Activity` / `RunEvent`
//! 到输出命令的投影。
//!
//! 实时统计属于 `agnes-agent` 的 [`Activity`]；本模块只读快照并决定怎么画，
//! 具体终端 I/O 在 [`crate::ui`] 的 plain/TUI 后端线程里完成。turn 收场摘要
//! 等落盘确认之后才打印：否则退出可能抢掉最后一批事件。

use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};

use agnes_agent::AgentRoot;
use agnes_agent::activity::{Activity, Phase};
use agnes_agent::model::{ModelSettings, ModelSettingsChanged};
use agnes_agent::runtime::{PendingUserMessages, TurnFinished};
use agnes_core::config::ModelName;
use agnes_core::llm::StopKind;
use agnes_core::message::{AssistantMessage, Message};
use agnes_core::turn::Outcome;
use agnes_ecs::{
    App, CancelToken, Entity, MessageReader, Plugin, Query, Res, ResMut, RuntimeSender, Update,
    With,
};
use agnes_io::llm::{LlmStreamEvent, LlmStreamUpdate, ModelsListed};
use agnes_plugin_async::AsyncExecutor;
use agnes_record::{Event as RecordEvent, Log, RunEvent, RunEventKind};
use jiff::Timestamp;

use crate::input::UserInput;
pub use crate::ui::Status;
use crate::ui::{UiCommand, plain, tui};

/// 每会话状态的只读快照对（都在 agent 根实体上）。
type Snapshot<'w> = Query<'w, (&'static Activity, &'static ModelSettings), With<AgentRoot>>;

struct RendererInner {
    tx: std_mpsc::Sender<UiCommand>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

/// A cloneable handle to the terminal backend.
#[derive(Clone)]
pub struct Renderer {
    inner: Arc<RendererInner>,
}

impl Renderer {
    /// Starts the line-based backend (pipes, non-TTY, `agnes run`).
    ///
    /// # Errors
    ///
    /// Returns the thread spawn error.
    pub fn plain() -> std::io::Result<Self> {
        let (tx, rx) = std_mpsc::channel();
        let worker = thread::Builder::new()
            .name("agnes-render".into())
            .spawn(move || plain::run(rx))?;
        Ok(Self {
            inner: Arc::new(RendererInner {
                tx,
                worker: Mutex::new(Some(worker)),
            }),
        })
    }

    /// Starts the ratatui backend and its input loop.
    ///
    /// # Errors
    ///
    /// Returns the thread spawn error; the terminal itself is initialized on
    /// the worker thread so a failed setup never leaves the host in raw mode.
    pub fn tui(
        input: Arc<UserInput>,
        sender: RuntimeSender,
        shutdown: CancelToken,
    ) -> std::io::Result<Self> {
        let (tx, rx) = std_mpsc::channel();
        let worker = thread::Builder::new()
            .name("agnes-tui".into())
            .spawn(move || tui::run(rx, input, sender, shutdown))?;
        Ok(Self {
            inner: Arc::new(RendererInner {
                tx,
                worker: Mutex::new(Some(worker)),
            }),
        })
    }

    /// Appends one line to the transcript.
    pub fn line(&self, text: impl Into<String>) {
        let _ = self.inner.tx.send(UiCommand::Line(text.into()));
    }

    /// Shows a plain-mode prompt. The TUI has a permanent input box, so this
    /// only marks the frontend as idle.
    pub fn prompt(&self, text: impl Into<String>) {
        let _ = self.inner.tx.send(UiCommand::Prompt(text.into()));
    }

    /// Starts (or restarts) the live stream buffer.
    pub fn stream_start(&self) {
        let _ = self.inner.tx.send(UiCommand::StreamStart);
    }

    /// Appends streamed assistant text.
    pub fn stream_delta(&self, text: &str) {
        let _ = self.inner.tx.send(UiCommand::StreamDelta(text.to_owned()));
    }

    /// Appends streamed reasoning text (observation only; never recorded).
    pub fn reasoning_delta(&self, text: &str) {
        let _ = self
            .inner
            .tx
            .send(UiCommand::ReasoningDelta(text.to_owned()));
    }

    /// Ends the live stream buffer.
    pub fn stream_end(&self) {
        let _ = self.inner.tx.send(UiCommand::StreamEnd);
    }

    /// Commits the authoritative assistant message: plain mode prints it only
    /// when nothing was streamed; the TUI replaces the live buffer with it.
    pub fn commit_assistant(&self, text: &str) {
        let _ = self
            .inner
            .tx
            .send(UiCommand::CommitAssistant(text.to_owned()));
    }

    /// Reports one finished step's token usage (plain prints a line; the TUI
    /// also gets the fresh `Activity` snapshot from `render`).
    pub fn step_stats(&self, step: u32, usage: agnes_core::message::TokenUsage) {
        let _ = self.inner.tx.send(UiCommand::StepStats { step, usage });
    }

    /// Replaces the status bar snapshot.
    pub fn status(&self, status: Status) {
        let _ = self.inner.tx.send(UiCommand::Status(status));
    }

    /// Hands the provider's model list to the frontend (picker or plain list).
    pub fn models(&self, current: ModelName, models: Vec<ModelName>) {
        let _ = self.inner.tx.send(UiCommand::Models { current, models });
    }

    /// Reports a failed model-list query.
    pub fn models_failed(&self, error: String) {
        let _ = self.inner.tx.send(UiCommand::ModelsFailed(error));
    }

    /// Shows (or clears) the approval prompt.
    pub fn approval(&self, prompt: Option<String>) {
        let _ = self.inner.tx.send(UiCommand::Approval(prompt));
    }

    /// Flushes pending plain output.
    pub fn flush(&self) {
        let _ = self.inner.tx.send(UiCommand::Flush);
    }
}

impl Drop for RendererInner {
    fn drop(&mut self) {
        let _ = self.tx.send(UiCommand::Shutdown);
        let worker = self
            .worker
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(worker) = worker {
            let _ = worker.join();
        }
    }
}

/// 渲染所需的宿主状态。
struct DisplayState {
    renderer: Renderer,
    sender: RuntimeSender,
    /// 渲染器服务的 agent 根实体：装配时绑定，消息与快照都按它寻址。
    agent: Entity,
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

/// 注册渲染系统：流式输出、[`RunEvent`] 流与 turn 收场报告。
pub struct RendererPlugin {
    renderer: Renderer,
    sender: RuntimeSender,
    agent: Entity,
    single_shot: bool,
    has_session: bool,
}

impl RendererPlugin {
    pub fn new(
        renderer: Renderer,
        sender: RuntimeSender,
        agent: Entity,
        single_shot: bool,
        has_session: bool,
    ) -> Self {
        Self {
            renderer,
            sender,
            agent,
            single_shot,
            has_session,
        }
    }
}

impl Plugin for RendererPlugin {
    fn name(&self) -> &'static str {
        "agnes-renderer"
    }

    /// # Panics
    ///
    /// Panics when `agnes_plugin_session::SessionPlugin` has not been
    /// registered before this plugin: its `session_ack` system must run
    /// before the final report within the same tick.
    fn build(&self, app: &mut App) {
        assert!(
            app.is_plugin_added::<agnes_plugin_session::SessionPlugin>(),
            "RendererPlugin requires SessionPlugin to be registered first: \
             session_ack must run before the final report within the same tick"
        );
        app.insert_resource(DisplayState {
            renderer: self.renderer.clone(),
            sender: self.sender.clone(),
            agent: self.agent,
            single_shot: self.single_shot,
            has_session: self.has_session,
        })
        .init_resource::<PendingReport>()
        .add_systems(Update, track_stream)
        .add_systems(Update, render_run_events)
        .add_systems(Update, render_model_list)
        .add_systems(Update, render_settings_changes)
        .add_systems(Update, render_rotations)
        .add_systems(Update, rotate_feedback)
        .add_systems(Update, report_finished);
    }
}

/// 把模型增量投递到输出层，并刷新状态栏；统计本身由 `Activity` 负责。
/// 增量流没有 agent 寻址（经在飞调用才能路由），本宿主只装配一个会话；
/// 状态栏快照按绑定的 agent 读取。
fn track_stream(
    mut events: MessageReader<LlmStreamEvent>,
    snapshot: Snapshot,
    state: Res<DisplayState>,
) {
    let events = events.read();
    if events.is_empty() {
        return;
    }
    for event in &events {
        match &event.update {
            LlmStreamUpdate::Started => state.renderer.stream_start(),
            LlmStreamUpdate::Delta { text } => state.renderer.stream_delta(text),
            LlmStreamUpdate::Reasoning { text } => state.renderer.reasoning_delta(text),
            LlmStreamUpdate::Usage { .. } => {}
        }
    }
    if let Some((activity, settings)) = snapshot.get(state.agent) {
        state.renderer.status(status_snapshot(&activity, &settings));
    }
}

fn render_run_events(
    mut events: MessageReader<RunEvent>,
    snapshot: Snapshot,
    state: Res<DisplayState>,
) {
    let events = events.read();
    if events.is_empty() {
        return;
    }
    for event in &events {
        if event.agent != state.agent {
            continue;
        }
        match &event.kind {
            RunEventKind::Started { tools, .. } => {
                let names: Vec<&str> = tools.iter().map(|tool| tool.as_str()).collect();
                state.renderer.line(format!(
                    "* model {}, tools: [{}]",
                    snapshot
                        .get(state.agent)
                        .map(|(_, settings)| settings.model().to_string())
                        .unwrap_or_default(),
                    names.join(", ")
                ));
            }
            RunEventKind::Recorded { event, .. } => render_recorded(event, &state),
            RunEventKind::ToolGated { denied, .. } => {
                state
                    .renderer
                    .line(format!("  ⛔ denied by {}: {}", denied.by, denied.reason));
            }
            // The approver task owns the interactive prompt: it must enter
            // answer mode before the question is visible.
            RunEventKind::ApprovalRequested { .. } => {}
            RunEventKind::ApprovalResolved { outcome, .. } => {
                if !outcome.allows() {
                    state
                        .renderer
                        .line(format!("  ✗ not approved ({outcome:?})"));
                }
            }
            RunEventKind::Aborted { by, reason, .. } => {
                state
                    .renderer
                    .line(format!("  ⛔ aborted by {by}: {reason}"));
            }
            RunEventKind::Finished { .. } | RunEventKind::Failed { .. } => {
                state.renderer.stream_end();
            }
            _ => {}
        }
    }
    if let Some((activity, settings)) = snapshot.get(state.agent) {
        state.renderer.status(status_snapshot(&activity, &settings));
    }
}

/// 模型列表查询的结果：TUI 用它填选择器，plain 直接打印。
fn render_model_list(
    mut listed: MessageReader<ModelsListed>,
    snapshot: Snapshot,
    state: Res<DisplayState>,
) {
    let Some((_, settings)) = snapshot.get(state.agent) else {
        return;
    };
    let current = settings.model().clone();
    for message in listed.read() {
        match &message.result {
            Ok(models) => state.renderer.models(current.clone(), models.clone()),
            Err(error) => state.renderer.models_failed(error.to_string()),
        }
    }
}

/// 设置变化：回显一行并立即刷新状态栏。
fn render_settings_changes(
    mut changed: MessageReader<ModelSettingsChanged>,
    snapshot: Snapshot,
    state: Res<DisplayState>,
) {
    if !changed
        .read()
        .iter()
        .any(|message| message.agent == state.agent)
    {
        return;
    }
    if let Some((activity, settings)) = snapshot.get(state.agent) {
        state.renderer.line(format!(
            "(model {} · thinking {})",
            settings.model(),
            settings.thinking()
        ));
        state.renderer.status(status_snapshot(&activity, &settings));
    }
}

fn render_recorded(event: &RecordEvent, state: &DisplayState) {
    match event {
        RecordEvent::MessageStart { .. } => {
            state.renderer.stream_start();
            state.renderer.line("→ model call");
        }
        RecordEvent::Message(Message::Assistant(message)) => {
            let text = match message {
                AssistantMessage::Text(content) => Some(content.as_str()),
                AssistantMessage::ToolCalls {
                    content: Some(content),
                    ..
                } => Some(content.as_str()),
                AssistantMessage::ToolCalls { content: None, .. } => None,
            };
            if let Some(text) = text {
                state.renderer.commit_assistant(text);
            }
        }
        RecordEvent::MessageEnd {
            step, usage, stop, ..
        } => {
            state.renderer.stream_end();
            state.renderer.step_stats(step.get(), *usage);
            if matches!(stop, StopKind::ToolUse) {
                state.renderer.line("← tool calls requested");
            }
        }
        _ => {
            if let Some(text) = recorded_text(event) {
                state.renderer.line(text);
            }
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
        if message.agent != state.agent {
            continue;
        }
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
    if rotations
        .read()
        .iter()
        .any(|rotation| rotation.agent == state.agent)
    {
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
    sessions: Query<
        (
            &Log,
            Option<&agnes_record::Durability>,
            &mut PendingUserMessages,
        ),
        With<AgentRoot>,
    >,
    mut finished: MessageReader<TurnFinished>,
    mut report: ResMut<PendingReport>,
    state: Res<DisplayState>,
) {
    if report.0.is_none()
        && let Some(message) = finished
            .read()
            .iter()
            .find(|message| message.agent == state.agent)
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
    let Some((log, durability, mut pending_msgs)) = sessions.get(state.agent) else {
        report.0 = Some(data);
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

/// 启动时回放的历史最多保留多少行，避免超长会话拖慢首帧。
pub(crate) const MAX_HISTORY_LINES: usize = 2_000;

/// TUI 启动时铺设恢复出的会话历史：只保留用户/助手可见消息与工具调用、
/// 失败标记，不铺开工具输出（与实时渲染保持一致）。
pub(crate) fn history_lines(log: &Log) -> Vec<String> {
    let mut lines = Vec::new();
    for event in log.events() {
        match event {
            RecordEvent::TurnStart => lines.push("─ turn ─".to_owned()),
            RecordEvent::Message(Message::User { content }) => {
                lines.push(format!("› {}", content.as_str()));
            }
            RecordEvent::Message(Message::Assistant(message)) => match message {
                AssistantMessage::Text(content) => lines.push(content.as_str().to_owned()),
                AssistantMessage::ToolCalls { content, calls } => {
                    if let Some(content) = content {
                        lines.push(content.as_str().to_owned());
                    }
                    for call in calls.iter() {
                        lines.push(format!("  ⚒ {} {}", call.name, call.arguments.as_str()));
                    }
                }
            },
            RecordEvent::Message(Message::ToolResult { output, .. }) if output.is_error() => {
                lines.push("  ✗ failed".to_owned());
            }
            _ => {}
        }
    }
    if lines.len() > MAX_HISTORY_LINES {
        let skip = lines.len() - MAX_HISTORY_LINES;
        lines.drain(..skip);
        lines.insert(0, "… (earlier history omitted)".to_owned());
    }
    lines
}

fn status_snapshot(activity: &Activity, settings: &ModelSettings) -> Status {
    Status {
        model: settings.model().to_string(),
        thinking: settings.thinking(),
        step: activity.step(),
        usage: activity.step_usage(),
        session_usage: activity.session_usage(),
        live_chars: activity.live_output_chars(),
        live_reasoning_chars: activity.live_reasoning_chars(),
        running: activity.is_running(),
        streaming: activity.phase() == Phase::Streaming,
        elapsed: activity.elapsed(),
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

#[cfg(test)]
fn event_text(model: &ModelName, event: &RunEvent) -> Option<String> {
    match &event.kind {
        RunEventKind::Started { tools, .. } => {
            let names: Vec<&str> = tools.iter().map(|tool| tool.as_str()).collect();
            Some(format!("* model {model}, tools: [{}]", names.join(", ")))
        }
        RunEventKind::ToolGated { denied, .. } => {
            Some(format!("  ⛔ denied by {}: {}", denied.by, denied.reason))
        }
        // The approver task owns the interactive prompt: it must enter
        // answer mode before the question is visible.
        RunEventKind::ApprovalRequested { .. } => None,
        RunEventKind::ApprovalResolved { outcome, .. } => {
            (!outcome.allows()).then(|| format!("  ✗ not approved ({outcome:?})"))
        }
        RunEventKind::Aborted { by, reason, .. } => Some(format!("  ⛔ aborted by {by}: {reason}")),
        RunEventKind::Recorded { event, .. } => recorded_text(event),
        RunEventKind::Finished { .. } => None,
        // The failure is reported with the turn summary; observers still see
        // this event.
        RunEventKind::Failed { .. } => None,
        _ => None,
    }
}

/// turn 收场摘要：结果与耗时。最终答复已经由流式输出/消息事件落地，
/// 这里不再重复打印。
fn print_turn_summary(
    renderer: &Renderer,
    outcome: Outcome,
    usage: agnes_core::message::TokenUsage,
    steps_used: u32,
    started_at: Timestamp,
    finished_at: Timestamp,
) {
    match outcome {
        Outcome::Answered => {}
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
    use agnes_core::content::Content;
    use agnes_core::message::{ToolArguments, ToolCall, ToolCallId, ToolOutput};
    use agnes_core::tool::ToolName;
    use agnes_core::turn::StepNumber;
    use agnes_util::NonEmpty;
    use std::str::FromStr;

    fn model() -> ModelName {
        ModelName::from_str("gpt-test").unwrap()
    }

    fn content(text: &str) -> Content {
        Content::try_from(text).unwrap()
    }

    #[test]
    fn resumed_history_renders_the_conversation() {
        let mut log = Log::default();
        log.record(RecordEvent::TurnStart);
        log.record(RecordEvent::Message(Message::User {
            content: content("hi"),
        }));
        log.record(RecordEvent::Message(Message::Assistant(
            AssistantMessage::Text(content("hello")),
        )));
        log.record(RecordEvent::Message(Message::Assistant(
            AssistantMessage::ToolCalls {
                content: Some(content("checking")),
                calls: NonEmpty::from_iter([ToolCall {
                    id: ToolCallId::new("c1"),
                    name: ToolName::from_str("read_file").unwrap(),
                    arguments: ToolArguments::new(r#"{"path":"x"}"#),
                }])
                .unwrap(),
            },
        )));
        log.record(RecordEvent::Message(Message::ToolResult {
            call_id: ToolCallId::new("c1"),
            output: ToolOutput::error("boom"),
        }));

        assert_eq!(
            history_lines(&log),
            vec![
                "─ turn ─".to_owned(),
                "› hi".to_owned(),
                "hello".to_owned(),
                "checking".to_owned(),
                r#"  ⚒ read_file {"path":"x"}"#.to_owned(),
                "  ✗ failed".to_owned(),
            ]
        );
    }

    #[test]
    fn long_histories_keep_the_tail() {
        let mut log = Log::default();
        for index in 0..MAX_HISTORY_LINES + 5 {
            log.record(RecordEvent::Message(Message::User {
                content: content(&format!("u{index}")),
            }));
        }

        let lines = history_lines(&log);
        assert_eq!(lines.len(), MAX_HISTORY_LINES + 1);
        assert_eq!(lines[0], "… (earlier history omitted)");
        let last = format!("› u{}", MAX_HISTORY_LINES + 4);
        assert_eq!(lines.last().map(String::as_str), Some(last.as_str()));
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
        let event = RunEvent {
            agent: agnes_ecs::Entity::PLACEHOLDER,
            kind: RunEventKind::Recorded {
                at: Timestamp::now(),
                event: RecordEvent::TurnStart,
            },
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
