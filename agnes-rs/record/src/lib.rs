//! 会话记录词汇：一次 agent run 的持久化事件、宿主审计与内存日志。
//!
//! 记录格式归宿主所有 —— [`agnes_core`] 只持有领域状态（[`Turn`]），不产出
//! 事件。驱动/系统按发生顺序铸造这些事件并写入 [`Log`]；会话格式、插件
//! 观察、界面渲染都使用这一套词汇。
//!
//! - [`event::Event`]（再导出为 [`Event`]）是 run 的持久化事件序列；
//!   [`event::HostEvent`]（再导出为 [`HostEvent`]）是 core 不该知道的宿主
//!   审计语义（审批、中止、失败），与事件共享同一条顺序。
//! - [`run_event::RunEvent`]（再导出为 [`RunEvent`]）是运行期上报流：
//!   run 级生命周期、日志镜像与宿主语义，渲染与落盘各自消费。
//! - [`session::SessionLog`]（再导出为 [`SessionLog`]）是把记录追加到
//!   耐久存储的异步接缝。
//! - [`Log`] 是进程内的会话日志：历史即状态，[`Log::messages`] 与
//!   [`Log::header`] 是它的投影。
//! - [`AgentRoot`] 是 agent 会话根实体的标记：每会话状态以组件形式挂在
//!   各自的根实体上，一个世界可以同时运行多个 agent。
//!
//! [`Turn`]: agnes_core::turn::Turn

mod agent;
mod approval;
mod decision;
mod event;
mod log;
mod origin;
mod run_event;
mod session;

pub use agent::AgentRoot;
pub use approval::ApprovalOutcome;
pub use decision::Denied;
pub use event::{Event, HostEvent, StepOutcome};
pub use log::Log;
pub use origin::Origin;
pub use run_event::{RunEvent, Sink};
pub use session::{
    Durability, RotateSession, SessionError, SessionLog, SessionStore, SessionStoreResource,
};

// 会话状态以组件形式挂在各 agent 的根实体上（见 [`agent`] 模块文档），
// 这里集中声明本 crate 的组件词汇。
agnes_ecs::component!(AgentRoot, Log, Durability, SessionStoreResource);
