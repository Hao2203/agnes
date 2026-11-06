//! 运行期可观测状态：agent 当前在做什么、这一步用了多少 token。
//!
//! [`Activity`] 是派生状态，不是会话日志的一部分：它从不落盘、不可恢复，
//! 只从 [`RunEvent`] 与 [`LlmStreamEvent`] 投影而来。权威用量仍在
//! `MessageEnd.usage` 与 `TurnResult.usage`；流尾的 `Usage` 只是让界面
//! 提前看到同一个值。
//!
//! [`ActivityPlugin`] 把它装进 ECS；界面（CLI、未来的 GUI/web）只读快照，
//! 不重复计数。会话插件仍然只管日志与耐久，不持有任何实时状态。

use std::time::{Duration, Instant};

use agnes_core::config::ModelName;
use agnes_core::message::TokenUsage;
use agnes_ecs::{App, MessageReader, Plugin, ResMut, Update};
use agnes_io::llm::{LlmStreamEvent, LlmStreamUpdate};
use agnes_record::{Event as RecordEvent, RotateSession, RunEvent};

/// agent 当前所处的阶段。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Phase {
    /// 没有活跃 turn。
    #[default]
    Idle,
    /// 有活跃 turn，但当前没有模型流在接收。
    Running,
    /// 正在接收一次模型响应。
    Streaming,
}

/// 一次运行的实时快照。派生自消息流，永不落盘。
#[derive(Debug)]
pub struct Activity {
    model: ModelName,
    phase: Phase,
    step: u32,
    step_usage: TokenUsage,
    session_usage: TokenUsage,
    live_output_chars: u64,
    started: Option<Instant>,
    finished: Option<Duration>,
}

impl Activity {
    /// 创建一个空闲快照；模型名在启动时固定。
    pub fn new(model: ModelName) -> Self {
        Self {
            model,
            phase: Phase::Idle,
            step: 0,
            step_usage: TokenUsage::default(),
            session_usage: TokenUsage::default(),
            live_output_chars: 0,
            started: None,
            finished: None,
        }
    }

    /// 本轮使用的模型。
    pub fn model(&self) -> &ModelName {
        &self.model
    }

    /// 当前阶段。
    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// 是否有活跃 turn。
    pub fn is_running(&self) -> bool {
        self.phase != Phase::Idle
    }

    /// 当前 step（1 起始；0 表示尚未开始）。
    pub fn step(&self) -> u32 {
        self.step
    }

    /// 当前 step 的用量（流尾 usage 或 `MessageEnd` 的权威值）。
    pub fn step_usage(&self) -> TokenUsage {
        self.step_usage
    }

    /// 本会话累计用量（`/new` 后清零）。
    pub fn session_usage(&self) -> TokenUsage {
        self.session_usage
    }

    /// 当前流已收到的输出字符数（界面可据此估算 token）。
    pub fn live_output_chars(&self) -> u64 {
        self.live_output_chars
    }

    /// turn 已运行的时间；收场后冻结在收场时刻。
    pub fn elapsed(&self) -> Duration {
        self.finished
            .or_else(|| self.started.map(|started| started.elapsed()))
            .unwrap_or_default()
    }

    fn begin_turn(&mut self) {
        self.phase = Phase::Running;
        self.step = 0;
        self.step_usage = TokenUsage::default();
        self.live_output_chars = 0;
        self.started = Some(Instant::now());
        self.finished = None;
    }

    fn end_turn(&mut self) {
        self.finished = self.started.map(|started| started.elapsed());
        self.phase = Phase::Idle;
        self.live_output_chars = 0;
    }

    fn record(&mut self, event: &RecordEvent) {
        match event {
            RecordEvent::MessageStart { step } => {
                self.step = step.get();
                self.step_usage = TokenUsage::default();
                self.live_output_chars = 0;
                self.phase = Phase::Running;
            }
            RecordEvent::MessageEnd { usage, .. } => {
                self.step_usage = *usage;
                self.session_usage = self.session_usage.saturating_add(*usage);
                self.live_output_chars = 0;
                self.phase = Phase::Running;
            }
            _ => {}
        }
    }

    fn stream_started(&mut self) {
        self.phase = Phase::Streaming;
        self.live_output_chars = 0;
    }

    fn stream_delta(&mut self, text: &str) {
        if self.phase != Phase::Streaming {
            self.stream_started();
        }
        self.live_output_chars = self
            .live_output_chars
            .saturating_add(u64::try_from(text.chars().count()).unwrap_or(u64::MAX));
    }

    fn note_usage(&mut self, usage: TokenUsage) {
        self.step_usage = usage;
    }

    fn reset_session(&mut self) {
        self.session_usage = TokenUsage::default();
        self.step = 0;
        self.step_usage = TokenUsage::default();
        self.live_output_chars = 0;
    }
}

/// 安装 [`Activity`] 并把运行期消息投影进去。
pub struct ActivityPlugin {
    model: ModelName,
}

impl ActivityPlugin {
    /// Creates the plugin for the model whose runs are observed.
    pub fn new(model: ModelName) -> Self {
        Self { model }
    }
}

impl Plugin for ActivityPlugin {
    fn name(&self) -> &'static str {
        "agnes-activity"
    }

    fn build(&self, app: &mut App) {
        app.insert_resource(Activity::new(self.model.clone()))
            .add_systems(Update, track_run_events)
            .add_systems(Update, track_stream)
            .add_systems(Update, track_rotation);
    }
}

fn track_run_events(mut events: MessageReader<RunEvent>, mut activity: ResMut<Activity>) {
    for event in events.read() {
        match event.as_ref() {
            RunEvent::Started { .. } => activity.begin_turn(),
            RunEvent::Recorded { event, .. } => activity.record(event),
            RunEvent::Finished { .. } | RunEvent::Failed { .. } => activity.end_turn(),
            _ => {}
        }
    }
}

fn track_stream(mut events: MessageReader<LlmStreamEvent>, mut activity: ResMut<Activity>) {
    for event in events.read() {
        match &event.update {
            LlmStreamUpdate::Started => activity.stream_started(),
            LlmStreamUpdate::Delta { text } => activity.stream_delta(text),
            LlmStreamUpdate::Usage { usage } => activity.note_usage(*usage),
        }
    }
}

fn track_rotation(mut rotations: MessageReader<RotateSession>, mut activity: ResMut<Activity>) {
    if !rotations.read().is_empty() {
        activity.reset_session();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agnes_core::llm::StopKind;
    use agnes_core::turn::StepNumber;
    use agnes_io::llm::LlmCallId;
    use jiff::Timestamp;
    use std::str::FromStr;

    fn model() -> ModelName {
        ModelName::from_str("gpt-test").unwrap()
    }

    fn app() -> App {
        let mut app = App::new();
        app.add_plugin(ActivityPlugin::new(model()));
        app
    }

    fn started() -> RunEvent {
        RunEvent::Started {
            at: Timestamp::now(),
            goal: None,
            tools: Vec::new(),
        }
    }

    fn recorded(event: RecordEvent) -> RunEvent {
        RunEvent::Recorded {
            at: Timestamp::now(),
            event,
        }
    }

    fn stream(update: LlmStreamUpdate) -> LlmStreamEvent {
        LlmStreamEvent {
            id: LlmCallId::new(1),
            update,
        }
    }

    #[test]
    fn a_run_moves_through_running_and_streaming() {
        let mut app = app();
        app.world_mut().send_message(started());
        app.pump().expect("schedule runs");
        {
            let activity = app.world().resource::<Activity>().expect("activity");
            assert_eq!(activity.phase(), Phase::Running);
            assert!(activity.is_running());
            assert_eq!(activity.step(), 0);
        }

        app.world_mut()
            .send_message(recorded(RecordEvent::MessageStart {
                step: StepNumber::FIRST,
            }));
        app.world_mut().send_message(stream(LlmStreamUpdate::Delta {
            text: "hello".to_owned(),
        }));
        app.pump().expect("schedule runs");
        {
            let activity = app.world().resource::<Activity>().expect("activity");
            assert_eq!(activity.phase(), Phase::Streaming);
            assert_eq!(activity.step(), 1);
            assert_eq!(activity.live_output_chars(), 5);
        }

        app.world_mut()
            .send_message(recorded(RecordEvent::MessageEnd {
                step: StepNumber::FIRST,
                model: model(),
                stop: StopKind::Completed,
                usage: TokenUsage {
                    input: 3,
                    output: 2,
                },
            }));
        app.pump().expect("schedule runs");
        {
            let activity = app.world().resource::<Activity>().expect("activity");
            assert_eq!(activity.phase(), Phase::Running);
            assert_eq!(
                activity.step_usage(),
                TokenUsage {
                    input: 3,
                    output: 2
                }
            );
            assert_eq!(
                activity.session_usage(),
                TokenUsage {
                    input: 3,
                    output: 2
                }
            );
            assert_eq!(activity.live_output_chars(), 0);
        }

        app.world_mut().send_message(RunEvent::Finished {
            at: Timestamp::now(),
            outcome: agnes_core::turn::Outcome::Answered,
        });
        app.pump().expect("schedule runs");
        {
            let activity = app.world().resource::<Activity>().expect("activity");
            assert_eq!(activity.phase(), Phase::Idle);
            assert!(!activity.is_running());
        }
    }

    #[test]
    fn the_stream_tail_usage_is_visible_before_the_message_ends() {
        let mut app = app();
        app.world_mut().send_message(started());
        app.world_mut()
            .send_message(stream(LlmStreamUpdate::Started));
        app.world_mut().send_message(stream(LlmStreamUpdate::Usage {
            usage: TokenUsage {
                input: 7,
                output: 4,
            },
        }));
        app.pump().expect("schedule runs");

        let activity = app.world().resource::<Activity>().expect("activity");
        assert_eq!(
            activity.step_usage(),
            TokenUsage {
                input: 7,
                output: 4
            }
        );
        assert_eq!(activity.phase(), Phase::Streaming);
    }

    #[test]
    fn rotation_clears_the_session_total() {
        let mut app = app();
        app.world_mut().send_message(started());
        app.world_mut()
            .send_message(recorded(RecordEvent::MessageEnd {
                step: StepNumber::FIRST,
                model: model(),
                stop: StopKind::Completed,
                usage: TokenUsage {
                    input: 9,
                    output: 5,
                },
            }));
        app.pump().expect("schedule runs");
        assert_eq!(
            app.world()
                .resource::<Activity>()
                .expect("activity")
                .session_usage(),
            TokenUsage {
                input: 9,
                output: 5
            }
        );

        app.world_mut().send_message(RotateSession);
        app.pump().expect("schedule runs");
        assert_eq!(
            app.world()
                .resource::<Activity>()
                .expect("activity")
                .session_usage(),
            TokenUsage::default()
        );
    }
}
