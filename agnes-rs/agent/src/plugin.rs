//! 插件接缝：宿主的策略层 —— 插件只处理事件、发出指令；指令由系统执行。
//!
//! 事件流[`PluginEvent`]：core 日志的镜像（[`PluginEvent::Core`]）、系统
//! 即将派发一次工具调用（[`PluginEvent::ToolCallRequested`]）、审批问询的
//! 答覆（[`PluginEvent::ApprovalAnswered`]）。插件看到事件后可发一条
//! [`Command`]（`None` = 不表态，交给后面的插件，链尾默认放行）；
//! 执行权在系统手里 —— 拒绝的合成结果、问询的构造与挂起、答覆的落定
//! 都由驱动完成。core 只看到一个普通的工具往返，封闭枚举不因插件扩展。

use agnes_core::event::Event as CoreEvent;
use agnes_core::message::ToolCall;

/// 插件发出、由系统执行的指令。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// 放行执行。
    Allow,
    /// 拒绝执行：理由会以合成错误工具结果喂回机器、模型可见。
    Deny(String),
    /// 走审批流：系统向审批服务发问询、挂起等待（挂起期间打断照常收场
    /// 整个 run）。
    Ask,
}

/// 插件处理的系统事件（core 词汇 + 驱动生命周期，core 侧不可见）。
#[derive(Debug, Clone)]
pub enum PluginEvent {
    /// 核心事件镜像 —— core 日志里的一条，按发生顺序流过。
    Core(CoreEvent),
    /// 系统即将派发一次工具调用 —— 指令在这个事件上说话才有效。
    ToolCallRequested { call: ToolCall },
    /// 一条待审批问询落定：批准与否由系统执行后续。
    ApprovalAnswered { call: ToolCall, approved: bool },
}

/// 参与事件处理的宿主插件。
pub trait Plugin: Send + Sync {
    fn name(&self) -> &'static str;

    /// 处理一条系统事件，可发指令。
    ///
    /// 指令只有在与 [`PluginEvent::ToolCallRequested`] 对应时才会被系统
    /// 执行；在其他事件上发指令属于实现错误（调试期会被断言拦下）。
    fn on_event(&mut self, event: &PluginEvent) -> Option<Command>;
}

/// 插件链：顺序处理 —— 第一个发指令的插件定案
/// （`None` 表示放行给后续插件；链上无人表态则系统默认放行）。
#[derive(Default)]
#[must_use]
pub struct PluginChain {
    plugins: Vec<Box<dyn Plugin>>,
}

impl PluginChain {
    pub fn new(plugins: impl IntoIterator<Item = Box<dyn Plugin>>) -> Self {
        Self {
            plugins: plugins.into_iter().collect(),
        }
    }

    pub fn register(&mut self, plugin: Box<dyn Plugin>) {
        self.plugins.push(plugin);
    }

    /// 是否没有任何插件。
    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    /// 把事件喂给整条链，交回发下的指令（无则 None）。
    pub fn on_event(&mut self, event: &PluginEvent) -> Option<Command> {
        for plugin in &mut self.plugins {
            match plugin.on_event(event) {
                None => continue,
                Some(command) => return Some(command),
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;
    use std::sync::{Arc, Mutex};

    fn log() -> Arc<Mutex<Vec<&'static str>>> {
        Arc::new(Mutex::new(Vec::new()))
    }

    fn call() -> ToolCall {
        ToolCall {
            id: agnes_core::message::ToolCallId::new("c1"),
            name: agnes_core::tool::ToolName::from_str("note").unwrap(),
            arguments: agnes_core::message::ToolArguments::new("{}"),
        }
    }

    fn call_event() -> PluginEvent {
        PluginEvent::ToolCallRequested { call: call() }
    }

    /// 记录看到的事件种类、放行。
    struct Runner(Arc<Mutex<Vec<&'static str>>>);

    impl Plugin for Runner {
        fn name(&self) -> &'static str {
            "runner"
        }

        fn on_event(&mut self, event: &PluginEvent) -> Option<Command> {
            match event {
                PluginEvent::ToolCallRequested { .. } => self.0.lock().unwrap().push("runner"),
                PluginEvent::Core(CoreEvent::TurnStart) => self.0.lock().unwrap().push("core"),
                PluginEvent::Core(_) | PluginEvent::ApprovalAnswered { .. } => {}
            }
            None
        }
    }

    /// 记录看到的事件、拒绝并给出理由。
    struct Denier(Arc<Mutex<Vec<&'static str>>>);

    impl Plugin for Denier {
        fn name(&self) -> &'static str {
            "denier"
        }

        fn on_event(&mut self, event: &PluginEvent) -> Option<Command> {
            match event {
                PluginEvent::ToolCallRequested { .. } => {
                    self.0.lock().unwrap().push("denier");
                    Some(Command::Deny("never on sundays".into()))
                }
                PluginEvent::Core(_) | PluginEvent::ApprovalAnswered { .. } => None,
            }
        }
    }

    #[test]
    fn an_empty_chain_issues_no_command() {
        let mut chain = PluginChain::default();
        assert!(chain.is_empty());
        assert_eq!(chain.on_event(&call_event()), None);
    }

    #[test]
    fn the_first_command_wins_and_stops_the_chain() {
        let log = log();
        let mut chain = PluginChain::default();
        chain.register(Box::new(Runner(Arc::clone(&log))));
        chain.register(Box::new(Denier(Arc::clone(&log))));
        chain.register(Box::new(Runner(Arc::clone(&log))));

        assert_eq!(
            chain.on_event(&call_event()),
            Some(Command::Deny("never on sundays".into()))
        );
        // 第三位没有被问询
        assert_eq!(*log.lock().unwrap(), vec!["runner", "denier"]);
    }

    #[test]
    fn core_events_flow_through_the_chain_without_commands() {
        let seen = log();
        let mut chain = PluginChain::default();
        chain.register(Box::new(Runner(Arc::clone(&seen))));
        chain.on_event(&PluginEvent::Core(CoreEvent::TurnStart));
        chain.on_event(&PluginEvent::ApprovalAnswered {
            call: call(),
            approved: false,
        });
        assert_eq!(*seen.lock().unwrap(), vec!["core"]);
    }
}
