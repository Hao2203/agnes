//! agent 会话根实体的标记与身份。
//!
//! 一个 ECS 世界里可以同时运行多个 agent；每个 agent 的会话状态（
//! [`Log`](agnes_record::Log)、[`Durability`](agnes_record::Durability)、
//! 待处理队列等）以组件形式挂在它自己的根实体上，而不是全局资源。
//! [`AgentRoot`] 是这些实体的零大小标记：宿主 spawn 根实体、挂上状态并
//! 保管句柄，插件与系统只按标记过滤、按消息里的 `agent: Entity` 寻址。
//! 状态组件的构造器（`agnes_plugin_session::session_state`）与注册归
//! 各属主 crate；本模块只拥有"这是一个会话"这个身份。

/// 标记一个 agent 会话的根实体。
///
/// 每个根实体代表一个独立运行的 agent 会话：会话日志、耐久进度、模型
/// 设置、活动快照等每会话状态都以组件形式挂在它上面，消息里的
/// `agent: Entity` 字段以它为地址。零大小标记，配合 `With<AgentRoot>`
/// 过滤或 `World::entity_with::<AgentRoot>` 查找使用。
pub struct AgentRoot;

agnes_ecs::component!(AgentRoot);
