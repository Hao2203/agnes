//! 运行时状态：typestate 值落进组件，工具调用落成实体。

use std::time::Instant;

use agnes_core::agent::{Agent, AwaitingCall, AwaitingModel, AwaitingTools, Closed};
use agnes_core::config::ModelName;
use agnes_core::message::{ToolCall, ToolOutput};
use agnes_core::turn::StepNumber;
use agnes_ecs::{CancelToken, Entity};
use agnes_io::llm::LlmCallId;
use jiff::Timestamp;

/// 活跃 turn 的零大小标记：实体存在即“有 turn 在飞”。
///
/// 与 `TurnPhase` / `TurnClock` 在同一次 `spawn_turn` 插入、随 turn 实体一起
/// despawn；不暴露任何阶段内部结构。宿主用它做输入路由（`Query<&ActiveTurn>`）。
pub struct ActiveTurn;

/// 驱动侧动态阶段：机器在运行时以枚举持有，阶段本身提示下一步动作。
///
/// `Option` 只是转移机制的实现细节：系统取走阶段值做消耗式转移，结束后
/// 放回；取走期间其他系统看到 `None` 并跳过。`None` 且带 [`TurnFailed`]
/// 标记的 turn 由 `close_turn` 收场清理。
pub struct TurnPhase(pub(crate) Option<Staged>);

impl TurnPhase {
    pub(crate) fn new(staged: Staged) -> Self {
        Self(Some(staged))
    }
}

/// 阶段的载荷。转移是消耗式的：`call` / `respond` / `finish` / `interrupted`
/// 都消耗当前阶段交还下一个。
pub(crate) enum Staged {
    AwaitingCall(Agent<AwaitingCall>),
    AwaitingModel(Agent<AwaitingModel>),
    AwaitingTools(Agent<AwaitingTools>),
    Closed(Agent<Closed>),
}

/// turn 的时钟与打断令牌：异步任务持有令牌的克隆，打断即取消。
pub struct TurnClock {
    pub started_at: Timestamp,
    pub cancellation: CancelToken,
}

/// turn 实体的归属：它属于哪个 agent 实体。
///
/// 对齐 `ToolCallEntry::turn` 的父回引模式：begin_turn 的每 agent 守卫、
/// inject_pending 与 recorder 都靠它把 turn 路由回所属会话。
pub struct TurnOwner(pub Entity);

/// 挂在 turn 实体上的在飞模型调用：回复与取消按 id 路由回这里。
///
/// `issue_model_call` 在发出 [`LlmCall`](agnes_io::llm::LlmCall) 时挂上，
/// `process_llm_reply` 收到回复时取走；turn 收场随实体一起消失，迟到的
/// 回复落在已关闭的阶段上被安全忽略。
pub(crate) struct InFlightCall {
    pub(crate) id: LlmCallId,
    pub(crate) model: ModelName,
}

/// 失败收场的标记：阶段已丢弃（session repairs 负责补写收尾事件），
/// `close_turn` 按它发出 [`TurnFinished`](crate::runtime::TurnFinished)。
///
/// `kind` 保留失败类别供宿主程序化处理，`cause` 只用于展示。
pub struct TurnFailed {
    pub cause: String,
    pub kind: super::messages::TurnFailureKind,
}

/// 一个工具调用在其生命周期里的位置。
pub(crate) enum CallPhase {
    /// 从模型响应里落成，等待门禁。
    Pending,
    /// 门禁放行，等待派发。
    Allowed,
    /// 等待审批答覆；`deadline` 是可选的等待上限（超时按拒绝落定）。
    Asked { deadline: Option<Instant> },
    /// 被拒绝（门禁或审批）：合成的失败结果待落位。
    Denied(ToolOutput),
    /// 已派发，等异步任务回注。
    Running,
    /// 结果已回，待落位。
    Done(ToolOutput),
    /// 已落位进机器。
    Settled,
}

/// 一个工具调用实体：属于哪个 turn 的哪个 step、调用本身与阶段。
///
/// `index` 是调用在模型响应里的位置，用于确定性的结算顺序。
pub struct ToolCallEntry {
    pub turn: Entity,
    pub step: StepNumber,
    pub index: usize,
    pub call: ToolCall,
    pub phase: CallPhase,
}

agnes_ecs::component!(
    ActiveTurn,
    TurnPhase,
    TurnClock,
    TurnFailed,
    ToolCallEntry,
    TurnOwner,
    InFlightCall
);

/// 模型调用 id 的全局发放器：id 空间跨 agent 唯一，回复按 id 路由回
/// 挂在 turn 实体上的 [`InFlightCall`]。
#[derive(Default)]
pub(crate) struct LlmCallSeq(u64);

impl LlmCallSeq {
    /// 发放下一个调用 id。
    pub(crate) fn issue(&mut self) -> LlmCallId {
        let id = LlmCallId::new(self.0);
        self.0 = self
            .0
            .checked_add(1)
            .expect("model call id space exhausted");
        id
    }
}
