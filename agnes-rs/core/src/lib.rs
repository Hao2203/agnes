//! Core of the agnes agent：领域类型与事件循环驱动的 sans-io 状态机。
//!
//! 本 crate 是纯计算 —— 同步、无 I/O、无异步、无时钟。一个 agent run
//! 就是一个 turn（[`turn::Turn`]）：由若干 step（[`step::Step`]，一次模型
//! 往返）组成的事件循环状态机 —— 外层驱动把结果作为 [`turn::Input`] 喂给
//! 机器，机器只接收输入、消费事件、更新状态、产生待执行的动作
//! （[`turn::Action`]），从不执行 IO。发生的一切都写进
//! [`context::Context`] 的有序日志（消息也是事件，[`event::Event`]）。
//!
//! 异步 I/O 的缝在 `agnes-io`；宿主把接缝接到任意运行时上，并自行决定
//! 「Context 如何变成模型请求」（[`llm::RequestBuilder`]）。

pub mod config;
pub mod content;
pub mod context;
pub mod event;
pub mod llm;
pub mod message;
pub mod step;
pub mod tool;
pub mod turn;
