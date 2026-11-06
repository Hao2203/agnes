//! 运行期可观测状态：agent 当前在做什么、这一步用了多少 token。
//!
//! [`Activity`] 是派生状态，不是会话日志的一部分：它从不落盘、不可恢复，
//! 只从 [`RunEvent`] 与 [`LlmStreamEvent`] 投影而来。它以组件形式挂在
//! agent 根实体上，每个 agent 一份快照，由宿主挂载（`Activity::default()`）。
//! 权威用量仍在 `MessageEnd.usage` 与 `TurnResult.usage`；流尾的 `Usage`
//! 只是让界面提前看到同一个值。
//!
//! [`ActivityPlugin`] 只注册投影系统；界面（CLI、未来的 GUI/web）只读
//! 快照，不重复计数。会话插件仍然只管日志与耐久，不持有任何实时状态。

use std::time::{Duration, Instant};

use agnes_core::message::TokenUsage;
use agnes_ecs::{App, MessageReader, Plugin, Query, Update};
use agnes_io::llm::{LlmStreamEvent, LlmStreamUpdate};
use agnes_record::{Event as RecordEvent, RotateSession, RunEvent, RunEventKind};

use crate::runtime::{InFlightCall, TurnOwner};

agnes_ecs::component!(Activity);

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

/// 一次运行的实时快照。派生自消息流，永不落盘。模型与思考设置属于
/// 配置，由 `ModelSettings` 持有，不在快照里重复。
#[derive(Debug, Clone, Default)]
pub struct Activity {
    phase: Phase,
    step: u32,
    step_usage: TokenUsage,
    session_usage: TokenUsage,
    live_output_chars: u64,
    live_reasoning_chars: u64,
    started: Option<Instant>,
    finished: Option<Duration>,
}

impl Activity {
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

    /// 当前流已收到的思考字符数（界面可据此估算思考 token）。
    pub fn live_reasoning_chars(&self) -> u64 {
        self.live_reasoning_chars
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
        self.live_reasoning_chars = 0;
        self.started = Some(Instant::now());
        self.finished = None;
    }

    fn end_turn(&mut self) {
        self.finished = self.started.map(|started| started.elapsed());
        self.phase = Phase::Idle;
        self.live_output_chars = 0;
        self.live_reasoning_chars = 0;
    }

    fn record(&mut self, event: &RecordEvent) {
        match event {
            RecordEvent::MessageStart { step } => {
                self.step = step.get();
                self.step_usage = TokenUsage::default();
                self.live_output_chars = 0;
                self.live_reasoning_chars = 0;
                self.phase = Phase::Running;
            }
            RecordEvent::MessageEnd { usage, .. } => {
                self.step_usage = *usage;
                self.session_usage = self.session_usage.saturating_add(*usage);
                self.live_output_chars = 0;
                self.live_reasoning_chars = 0;
                self.phase = Phase::Running;
            }
            _ => {}
        }
    }

    fn stream_started(&mut self) {
        self.phase = Phase::Streaming;
        self.live_output_chars = 0;
        self.live_reasoning_chars = 0;
    }

    fn stream_delta(&mut self, text: &str) {
        self.ensure_streaming();
        Self::count_chars(&mut self.live_output_chars, text);
    }

    fn stream_reasoning(&mut self, text: &str) {
        self.ensure_streaming();
        Self::count_chars(&mut self.live_reasoning_chars, text);
    }

    fn ensure_streaming(&mut self) {
        if self.phase != Phase::Streaming {
            self.stream_started();
        }
    }

    fn count_chars(counter: &mut u64, text: &str) {
        *counter = counter.saturating_add(u64::try_from(text.chars().count()).unwrap_or(u64::MAX));
    }

    fn note_usage(&mut self, usage: TokenUsage) {
        self.step_usage = usage;
    }

    fn reset_session(&mut self) {
        self.session_usage = TokenUsage::default();
        self.step = 0;
        self.step_usage = TokenUsage::default();
        self.live_output_chars = 0;
        self.live_reasoning_chars = 0;
    }
}

/// 注册活动投影系统：把运行期消息投影到宿主挂载的 [`Activity`] 上。
#[derive(Default)]
pub struct ActivityPlugin;

impl Plugin for ActivityPlugin {
    fn name(&self) -> &'static str {
        "agnes-activity"
    }

    fn build(&self, app: &mut App) {
        app.add_systems(Update, track_run_events)
            .add_systems(Update, track_stream)
            .add_systems(Update, track_rotation);
    }
}

/// 运行事件按 `agent` 路由到所属 agent 的快照；每个 agent 只看到自己的
/// 事件流。
fn track_run_events(mut events: MessageReader<RunEvent>, mut activities: Query<&mut Activity>) {
    for event in events.read() {
        let Some(mut activity) = activities.get_mut(event.agent) else {
            continue;
        };
        match &event.kind {
            RunEventKind::Started { .. } => activity.begin_turn(),
            RunEventKind::Recorded { event, .. } => activity.record(event),
            RunEventKind::Finished { .. } | RunEventKind::Failed { .. } => activity.end_turn(),
            _ => {}
        }
    }
}

/// 流式事件按在飞调用路由回所属 agent 的快照；调用已收场时迟到的增量
/// 直接丢弃（权威用量随 `MessageEnd` 走）。
fn track_stream(
    mut events: MessageReader<LlmStreamEvent>,
    mut activities: Query<&mut Activity>,
    calls: Query<(&InFlightCall, &TurnOwner)>,
) {
    for event in events.read() {
        let Some((_, owner)) = calls.iter().find(|(call, _)| call.id == event.id) else {
            continue;
        };
        let Some(mut activity) = activities.get_mut(owner.0) else {
            continue;
        };
        match &event.update {
            LlmStreamUpdate::Started => activity.stream_started(),
            LlmStreamUpdate::Delta { text } => activity.stream_delta(text),
            LlmStreamUpdate::Reasoning { text } => activity.stream_reasoning(text),
            LlmStreamUpdate::Usage { usage } => activity.note_usage(*usage),
        }
    }
}

fn track_rotation(
    mut rotations: MessageReader<RotateSession>,
    mut activities: Query<&mut Activity>,
) {
    for rotation in rotations.read() {
        if let Some(mut activity) = activities.get_mut(rotation.agent) {
            activity.reset_session();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agnes_core::config::ModelName;
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
        // 宿主装配：根实体与快照组件先建，插件只注册系统。
        app.world_mut()
            .spawn_bundle((crate::AgentRoot, Activity::default()));
        app.add_plugin(ActivityPlugin);
        app
    }

    fn root(app: &App) -> agnes_ecs::Entity {
        app.world()
            .entity_with::<crate::AgentRoot>()
            .expect("agent root exists")
    }

    fn activity(app: &App) -> Activity {
        app.world()
            .get::<Activity>(root(app))
            .expect("activity component")
            .clone()
    }

    fn started(agent: agnes_ecs::Entity) -> RunEvent {
        RunEvent {
            agent,
            kind: RunEventKind::Started {
                at: Timestamp::now(),
                goal: None,
                tools: Vec::new(),
            },
        }
    }

    fn recorded(agent: agnes_ecs::Entity, event: RecordEvent) -> RunEvent {
        RunEvent {
            agent,
            kind: RunEventKind::Recorded {
                at: Timestamp::now(),
                event,
            },
        }
    }

    fn stream(update: LlmStreamUpdate) -> LlmStreamEvent {
        LlmStreamEvent {
            id: LlmCallId::new(1),
            update,
        }
    }

    /// 在飞调用实体：流式事件经它路由回所属 agent 的快照。
    fn install_stream_call(app: &mut App) {
        let agent = root(app);
        app.world_mut().spawn_bundle((
            InFlightCall {
                id: LlmCallId::new(1),
                model: model(),
            },
            TurnOwner(agent),
        ));
    }

    #[test]
    fn a_run_moves_through_running_and_streaming() {
        let mut app = app();
        let agent = root(&app);
        app.world_mut().send_message(started(agent));
        app.pump().expect("schedule runs");
        {
            let activity = activity(&app);
            assert_eq!(activity.phase(), Phase::Running);
            assert!(activity.is_running());
            assert_eq!(activity.step(), 0);
        }

        install_stream_call(&mut app);
        app.world_mut().send_message(recorded(
            agent,
            RecordEvent::MessageStart {
                step: StepNumber::FIRST,
            },
        ));
        app.world_mut().send_message(stream(LlmStreamUpdate::Delta {
            text: "hello".to_owned(),
        }));
        app.pump().expect("schedule runs");
        {
            let activity = activity(&app);
            assert_eq!(activity.phase(), Phase::Streaming);
            assert_eq!(activity.step(), 1);
            assert_eq!(activity.live_output_chars(), 5);
        }

        app.world_mut().send_message(recorded(
            agent,
            RecordEvent::MessageEnd {
                step: StepNumber::FIRST,
                model: model(),
                stop: StopKind::Completed,
                usage: TokenUsage {
                    input: 3,
                    output: 2,
                },
            },
        ));
        app.pump().expect("schedule runs");
        {
            let activity = activity(&app);
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

        app.world_mut().send_message(RunEvent {
            agent,
            kind: RunEventKind::Finished {
                at: Timestamp::now(),
                outcome: agnes_core::turn::Outcome::Answered,
            },
        });
        app.pump().expect("schedule runs");
        {
            let activity = activity(&app);
            assert_eq!(activity.phase(), Phase::Idle);
            assert!(!activity.is_running());
        }
    }

    #[test]
    fn the_stream_tail_usage_is_visible_before_the_message_ends() {
        let mut app = app();
        let agent = root(&app);
        install_stream_call(&mut app);
        app.world_mut().send_message(started(agent));
        app.world_mut()
            .send_message(stream(LlmStreamUpdate::Started));
        app.world_mut().send_message(stream(LlmStreamUpdate::Usage {
            usage: TokenUsage {
                input: 7,
                output: 4,
            },
        }));
        app.pump().expect("schedule runs");

        let activity = activity(&app);
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
    fn reasoning_chars_are_counted_and_cleared_with_the_step() {
        let mut app = app();
        let agent = root(&app);
        install_stream_call(&mut app);
        app.world_mut()
            .send_message(stream(LlmStreamUpdate::Reasoning {
                text: "think".to_owned(),
            }));
        app.pump().expect("schedule runs");
        {
            let activity = activity(&app);
            assert_eq!(activity.phase(), Phase::Streaming);
            assert_eq!(activity.live_reasoning_chars(), 5);
            assert_eq!(activity.live_output_chars(), 0);
        }

        app.world_mut().send_message(stream(LlmStreamUpdate::Delta {
            text: "answer".to_owned(),
        }));
        app.pump().expect("schedule runs");
        {
            let activity = activity(&app);
            assert_eq!(activity.live_reasoning_chars(), 5);
            assert_eq!(activity.live_output_chars(), 6);
        }

        app.world_mut().send_message(recorded(
            agent,
            RecordEvent::MessageEnd {
                step: StepNumber::FIRST,
                model: model(),
                stop: StopKind::Completed,
                usage: TokenUsage {
                    input: 3,
                    output: 2,
                },
            },
        ));
        app.pump().expect("schedule runs");
        assert_eq!(activity(&app).live_reasoning_chars(), 0);
    }

    #[test]
    fn rotation_clears_the_session_total() {
        let mut app = app();
        let agent = root(&app);
        app.world_mut().send_message(started(agent));
        app.world_mut().send_message(recorded(
            agent,
            RecordEvent::MessageEnd {
                step: StepNumber::FIRST,
                model: model(),
                stop: StopKind::Completed,
                usage: TokenUsage {
                    input: 9,
                    output: 5,
                },
            },
        ));
        app.pump().expect("schedule runs");
        assert_eq!(
            activity(&app).session_usage(),
            TokenUsage {
                input: 9,
                output: 5
            }
        );

        app.world_mut().send_message(RotateSession { agent });
        app.pump().expect("schedule runs");
        assert_eq!(activity(&app).session_usage(), TokenUsage::default());
    }

    #[test]
    fn foreign_events_do_not_touch_the_snapshot() {
        let mut app = App::new();
        // 本 agent 的根实体 + 一个别的 agent 的根实体；事件都发给对方。
        app.world_mut()
            .spawn_bundle((crate::AgentRoot, Activity::default()));
        app.add_plugin(ActivityPlugin);
        let other = app.world_mut().spawn(crate::AgentRoot);

        app.world_mut().send_message(started(other));
        app.world_mut().send_message(RunEvent {
            agent: other,
            kind: RunEventKind::Finished {
                at: Timestamp::now(),
                outcome: agnes_core::turn::Outcome::Answered,
            },
        });
        app.pump().expect("schedule runs");

        assert_eq!(
            activity(&app).phase(),
            Phase::Idle,
            "foreign events are ignored"
        );
    }
}
