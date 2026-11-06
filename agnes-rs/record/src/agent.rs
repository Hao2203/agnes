//! agent 会话实体的标记与身份。
//!
//! 一个 ECS 世界里可以同时运行多个 agent；每个 agent 的会话状态
//! （[`Log`](crate::Log)、[`Durability`](crate::Durability)、待处理队列等）
//! 以组件形式挂在它自己的根实体上，而不是全局资源。[`AgentRoot`] 是这些
//! 实体的零大小标记：插件在安装期 find-or-create 一个根实体并把自己的
//! 状态挂上去。本 crate 依赖 `agnes-ecs` 只为把状态类型注册成组件
//! （`component!`）；事件词汇本身保持与 ECS 无关。

/// 标记一个 agent 会话的根实体。
///
/// 每个根实体代表一个独立运行的 agent 会话：会话日志、耐久进度、模型
/// 设置、活动快照等每会话状态都以组件形式挂在它上面。零大小标记，配合
/// `With<AgentRoot>` 过滤或 `World::entity_with::<AgentRoot>` 查找使用。
pub struct AgentRoot;
