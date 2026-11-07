//! 宿主侧 API：agent 的正确装配与 turn 请求的提交。
//!
//! [`AgentBuilder`] 一次 spawn 挂齐核心状态（[`AgentCore`]）、输入队列
//! （[`Inbox`]）与会话能力（`SessionPump` / `Durability`，可选 `Activity`、
//! `SessionStoreResource`），部分装配在 API 层面不可表示。
//!
//! [`submit_turn`] 在系统内直接生成请求实体；[`request_turn`] 供跨线程/
//! 外部客户端提交并 await 恰好一条 [`TurnResponse`]。

use std::sync::Arc;

use agnes_core::config::{ModelName, Thinking};
use agnes_core::turn::AgentConfig;
use agnes_ecs::{App, Commands, Entity, RuntimeSender};
use agnes_plugin_session::{SessionInitError, SessionState, session_state};
use agnes_record::{Event, SessionStore, SessionStoreResource};
use jiff::Timestamp;

use crate::activity::Activity;
use crate::model::ModelSettings;
use crate::runtime::TurnRequested;
use crate::runtime::request::{
    TurnQueued, TurnRequest, TurnRequestId, TurnRequestSeq, TurnResponder, TurnResponse,
};
use crate::{AgentCore, Inbox};

/// 一次完整的 agent 装配。
///
/// ```ignore
/// let agent = AgentBuilder::new(model, thinking)
///     .restore(events)
///     .store(store)
///     .spawn(&mut app)?;
/// ```
pub struct AgentBuilder {
    model: ModelName,
    thinking: Thinking,
    events: Vec<Event>,
    store: Option<Arc<dyn SessionStore>>,
    activity: bool,
}

impl AgentBuilder {
    /// 新建一个装配器；默认挂 `Activity`，无持久化 store。
    pub fn new(model: ModelName, thinking: Thinking) -> Self {
        Self {
            model,
            thinking,
            events: Vec::new(),
            store: None,
            activity: true,
        }
    }

    /// 用恢复出的会话事件重建日志与耐久基线。
    pub fn restore(mut self, events: Vec<Event>) -> Self {
        self.events = events;
        self
    }

    /// 挂上持久化 store；`None` 时是内存模式。
    pub fn store(mut self, store: Option<Arc<dyn SessionStore>>) -> Self {
        self.store = store;
        self
    }

    /// 是否挂观测组件 [`Activity`]。
    pub fn activity(mut self, enabled: bool) -> Self {
        self.activity = enabled;
        self
    }

    /// 生成 agent 实体并返回句柄。
    ///
    /// # Errors
    ///
    /// 恢复的历史有记录但没有 header 时返回 [`SessionInitError::NoHeader`]。
    pub fn spawn(self, app: &mut App) -> Result<AgentHandle, SessionInitError> {
        let SessionState {
            log,
            pump,
            durability,
        } = session_state(&self.events)?;
        app.init_resource::<TurnRequestSeq>();
        let entity = if self.activity {
            app.world_mut().spawn_bundle((
                AgentCore::new(log, ModelSettings::new(self.model.clone(), self.thinking)),
                Inbox::default(),
                pump,
                durability,
                Activity::default(),
            ))
        } else {
            app.world_mut().spawn_bundle((
                AgentCore::new(log, ModelSettings::new(self.model.clone(), self.thinking)),
                Inbox::default(),
                pump,
                durability,
            ))
        };
        if let Some(store) = self.store {
            app.world_mut()
                .insert(entity, SessionStoreResource::new(store));
        }
        Ok(AgentHandle { entity })
    }
}

/// 已装配 agent 的句柄。
#[derive(Clone, Copy, Debug)]
pub struct AgentHandle {
    /// agent 实体。
    pub entity: Entity,
}

impl AgentHandle {
    /// 这个 agent 用于提交请求的 id 发号器（与世界共享）。
    ///
    /// # Panics
    ///
    /// 世界里没有 `TurnRequestSeq` 时 panic；[`AgentBuilder::spawn`] 会安装它。
    pub fn request_seq(self, app: &App) -> TurnRequestSeq {
        app.world()
            .resource::<TurnRequestSeq>()
            .expect("AgentBuilder installs TurnRequestSeq")
            .clone()
    }
}

/// 在系统内提交一个 turn 请求：立即 reserve 请求实体。
///
/// 返回的实体在 `Commands` 应用后可见（同一 tick 的后续系统可见）。
pub fn submit_turn(
    commands: &mut Commands,
    id: TurnRequestId,
    agent: Entity,
    config: AgentConfig,
    responder: Option<TurnResponder>,
) -> Entity {
    let request = TurnRequest {
        id,
        agent,
        config,
        submitted_at: Timestamp::now(),
    };
    match responder {
        Some(responder) => commands.spawn_bundle((request, TurnQueued, responder)),
        None => commands.spawn_bundle((request, TurnQueued)),
    }
}

/// 跨线程/外部客户端提交请求，并等待恰好一条响应。
///
/// # Errors
///
/// 运行时已关闭，或响应通道在送达前被关闭。
pub async fn request_turn(
    sender: &RuntimeSender,
    seq: &TurnRequestSeq,
    agent: Entity,
    config: AgentConfig,
) -> Result<TurnResponse, RequestError> {
    let id = seq.next();
    let (tx, rx) = async_channel::bounded(1);
    sender
        .send_message(TurnRequested {
            id,
            agent,
            config,
            responder: Some(TurnResponder::new(tx)),
        })
        .map_err(|_| RequestError::RuntimeGone)?;
    rx.recv().await.map_err(|_| RequestError::Cancelled)
}

/// 跨线程请求的失败。
#[derive(Debug, thiserror::Error)]
pub enum RequestError {
    /// 运行时通道已关闭。
    #[error("the agent runtime is gone")]
    RuntimeGone,
    /// 响应通道在送达前关闭（通常是运行时被丢弃）。
    #[error("the response channel closed before a response arrived")]
    Cancelled,
}
