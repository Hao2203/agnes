//! Core of the agnes agent：领域类型与事件循环驱动的 sans-io 状态机。
//!
//! 本 crate 是纯计算 —— 同步、无 I/O、无异步、无时钟。一个 agent run
//! 就是一个 turn（[`turn::Turn`]）：由若干 step（一次模型往返）组成，
//! 状态即类型 —— 阶段由 [`turn::AwaitingMessage`]、[`turn::AwaitingTools`]、
//! [`turn::Closed`] 表示，每阶段只提供推进自己的方法；驱动把 IO 结果经
//! 这些方法喂回机器，机器只接收输入、消费事件、更新状态、产生待执行的
//! 动作（[`turn::Action`]），从不执行 IO。发生的一切都作为事件动作交给
//! 下游，由驱动按顺序记入日志 —— 消息、系统提示与工具环境
//! （[`event::Event::Header`]）都是事件，所以日志是唯一状态；只有能推进
//! 阶段的信息进机器，流式增量由上层适配器直报界面。
//!
//! 异步 I/O 的缝在 `agnes-io`；宿主把接缝接到任意运行时上，并自行决定
//! 「日志如何变成模型请求」。插件式扩展
//! （如工具权限审批）也全在宿主侧 —— core 的词汇不随插件扩展。

pub mod config;
pub mod content;
pub mod event;
pub mod header;
pub mod llm;
pub mod message;
pub mod tool;
pub mod turn;
