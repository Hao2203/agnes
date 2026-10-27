//! Core of the agnes agent：领域类型与 sans-io 类型状态机。
//!
//! 本 crate 是纯计算 —— 同步、无 I/O、无异步、无时钟。一个 agent run
//! 就是一个 turn（[`turn::Turn`]）：由若干 step（一次模型往返）组成，
//! 状态即类型 —— 阶段由 [`turn::AwaitingModel`]、[`turn::AwaitingTools`]、
//! [`turn::Closed`] 表示，每阶段只提供推进自己的方法；驱动把 IO 结果经
//! 这些方法喂回机器，机器只持有领域状态（[`context::Context`]），从不
//! 记录日志、也从不产出事件或执行 IO。事件与记录格式完全属于下游。
//!
//! 异步 I/O 的缝在 `agnes-io`；宿主把接缝接到任意运行时上，并自行决定
//! 「上下文如何变成模型请求」。插件式扩展（如工具权限审批）也全在宿主侧
//! —— core 的词汇不随插件扩展。

pub mod config;
pub mod content;
pub mod context;
pub mod header;
pub mod llm;
pub mod message;
pub mod tool;
pub mod turn;
