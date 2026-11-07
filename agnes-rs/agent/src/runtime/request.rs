//! 请求作为实体：请求、排队标记、终态响应与一次性回执。
//!
//! 一个 turn 请求从提交到收场是**同一个实体**：宿主/运行时先挂上
//! [`TurnRequest`] + [`TurnQueued`]，调度时在同一实体上插入 typestate 阶段与
//! 时钟，收场时写入 [`TurnResponse`]。因此请求 id 就是实体，响应不再依赖
//! 消息读者，也不会因为某个系统返回 `Err` 而消失。
//!
//! [`TurnTerminal`] 是唯一的终态意图：正常收场、模型/会话失败、打断、取消、
//! 超时都只写它，由 `finalize_requests` 统一落成 [`TurnResponse`]（恰好一次）。

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use agnes_core::turn::{AgentConfig, TurnResult};
use agnes_ecs::Entity;
use jiff::Timestamp;

use super::messages::TurnFailure;

/// 全局唯一的 turn 请求 id（跨 agent）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TurnRequestId(u64);

impl TurnRequestId {
    /// 用原始值构造（宿主可持久化/回放）。
    pub fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// 原始值。
    pub fn value(self) -> u64 {
        self.0
    }
}

impl fmt::Display for TurnRequestId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// 请求 id 的发号器。
///
/// 内部是 `Arc<AtomicU64>`：世界里的系统、宿主线程与外部客户端共享同一
/// id 空间。宿主在提交前先 [`next`](Self::next) 取号，就能在响应到达前
/// 持有稳定句柄（用于取消或日志关联）。
#[derive(Clone, Default)]
pub struct TurnRequestSeq(Arc<AtomicU64>);

impl TurnRequestSeq {
    /// 发放下一个请求 id。
    pub fn next(&self) -> TurnRequestId {
        TurnRequestId(self.0.fetch_add(1, Ordering::Relaxed))
    }
}

/// 请求实体上的稳定输入；同一实体随后长出运行期组件。
pub struct TurnRequest {
    /// 请求 id（与响应一一对应）。
    pub id: TurnRequestId,
    /// 这个请求属于哪个 agent 实体。
    pub agent: Entity,
    /// 本轮 turn 的配置（header、goal、预算）。
    pub config: AgentConfig,
    /// 提交时刻；排队等待时间由此可见。
    pub submitted_at: Timestamp,
}

/// 排队标记：实体已提交、尚未开跑。移除它即开始运行。
pub struct TurnQueued;

/// 请求的终态结果；实体上恰好出现一次。
///
/// 这是响应的权威记录：宿主查询组件即可拿到结果，不依赖消息读者。
/// 拉取式宿主处理完后负责 despawn 该请求实体；推送式请求由
/// [`TurnResponder`] 送达后自动回收。
#[derive(Clone, Debug)]
pub struct TurnResponse {
    /// 请求 id。
    pub id: TurnRequestId,
    /// 请求所属 agent。
    pub agent: Entity,
    /// 正常收场的结果，或失败原因。
    pub result: Result<TurnResult, TurnFailure>,
    /// 提交时刻。
    pub submitted_at: Timestamp,
    /// 开始运行的时刻（排队请求未运行时等于提交时刻）。
    pub started_at: Timestamp,
    /// 收场时刻。
    pub finished_at: Timestamp,
}

/// 一次性推送出口：跨线程/外部客户端可 await 一条响应。
///
/// 内部用 `AtomicBool` 保证即使多条终态路径意外重入也只送达一次。通道
/// 容量由调用方决定（通常为 1）；接收端消失时发送失败被忽略。
#[derive(Clone)]
pub struct TurnResponder {
    inner: Arc<ResponderInner>,
}

struct ResponderInner {
    sent: AtomicBool,
    tx: async_channel::Sender<TurnResponse>,
}

impl fmt::Debug for TurnResponder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TurnResponder")
            .field("sent", &self.inner.sent.load(Ordering::Relaxed))
            .finish()
    }
}

impl TurnResponder {
    /// 用一条响应通道构造。
    pub fn new(tx: async_channel::Sender<TurnResponse>) -> Self {
        Self {
            inner: Arc::new(ResponderInner {
                sent: AtomicBool::new(false),
                tx,
            }),
        }
    }

    pub(crate) fn send(&self, response: TurnResponse) {
        if !self.inner.sent.swap(true, Ordering::SeqCst) {
            let _ = self.inner.tx.try_send(response);
        }
    }
}

/// 终态意图：任何收场路径只写它，`finalize_requests` 据此落成响应。
pub(crate) struct TurnTerminal {
    pub(crate) result: Result<TurnResult, TurnFailure>,
    pub(crate) finished_at: Timestamp,
}

/// 已通过 [`TurnResponder`] 送达；下一 tick 的 reaper 会 despawn 实体。
pub(crate) struct TurnResponded;

agnes_ecs::component!(
    TurnRequest,
    TurnQueued,
    TurnResponse,
    TurnResponder,
    TurnTerminal,
    TurnResponded
);
