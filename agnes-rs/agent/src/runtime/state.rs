//! 运行时状态：typestate 值落进组件，工具调用落成实体。

use std::collections::HashMap;
use std::time::Instant;

use agnes_core::agent::{Agent, AwaitingCall, AwaitingModel, AwaitingTools, Closed};
use agnes_core::config::ModelName;
use agnes_core::message::{ToolCall, ToolOutput};
use agnes_core::turn::StepNumber;
use agnes_ecs::{CancelToken, Entity};
use agnes_io::llm::LlmCallId;
use jiff::Timestamp;

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

/// 失败收场的标记：阶段已丢弃（session repairs 负责补写收尾事件），
/// `close_turn` 按它发出 [`TurnFinished`](crate::runtime::TurnFinished)。
pub struct TurnFailed {
    pub cause: String,
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

/// 在飞的模型调用：调用 id 与它归属的 turn、模型名。
#[derive(Default)]
pub(crate) struct PendingCalls {
    next: u64,
    by_id: HashMap<LlmCallId, PendingCall>,
}

/// 一条在飞调用的归属信息。
pub(crate) struct PendingCall {
    pub turn: Entity,
    pub model: ModelName,
}

impl PendingCalls {
    /// 发一个调用：分配 id 并记住归属。
    pub fn issue(&mut self, turn: Entity, model: ModelName) -> LlmCallId {
        let id = LlmCallId::new(self.next);
        self.next = self
            .next
            .checked_add(1)
            .expect("model call id space exhausted");
        self.by_id.insert(id, PendingCall { turn, model });
        id
    }

    /// 收走一条回复的归属信息；未知 id（迟到或已取消）返回 `None`。
    pub fn take(&mut self, id: LlmCallId) -> Option<PendingCall> {
        self.by_id.remove(&id)
    }

    /// 取走某个 turn 的全部在飞调用（打断时取消）。
    pub fn take_for(&mut self, turn: Entity) -> Vec<LlmCallId> {
        let mut ids = Vec::new();
        self.by_id.retain(|id, call| {
            if call.turn == turn {
                ids.push(*id);
                false
            } else {
                true
            }
        });
        ids
    }
}

/// 活跃 turn 存在时到达的 turn 请求，等收场后依次开跑。
#[derive(Default)]
pub struct PendingTurns(pub(crate) Vec<std::sync::Arc<super::messages::TurnRequested>>);

impl PendingTurns {
    /// 多少个 turn 请求在等待。
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// 是否没有 turn 请求在等待。
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// 还没有并入会话的用户消息：只在待调用阶段生效，其余场合在这里等待。
#[derive(Default)]
pub struct PendingUserMessages(pub(crate) Vec<agnes_core::content::Content>);

impl PendingUserMessages {
    /// 多少条用户消息在等待注入。
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// 是否没有用户消息在等待。
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// 取走全部等待中的用户消息（宿主收场时归还给输入侧）。
    pub fn drain(&mut self) -> Vec<agnes_core::content::Content> {
        std::mem::take(&mut self.0)
    }
}
