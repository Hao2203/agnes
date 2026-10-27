//! 插件注册表与事件分发：注册、排序、可回收撤销，以及指令收件箱。

use agnes_core::message::ToolCallId;

use super::command::{QueuedMessage, ToolChange};
use super::decision::{Aborted, Blocked, Gate};
use super::outbox::Outbox;
use super::plugin::{Plugin, PluginEvent};

/// 一个插件槽位；`order` 小的先被问询，同序按注册先后。
struct Slot {
    order: i32,
    seq: u64,
    id: u64,
    plugin: Box<dyn Plugin>,
}

/// 一次成功注册的手柄。长期注册显式 [`Extensions::revoke`] 撤销；
/// 临时注册交给 [`Scope`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Registration {
    id: u64,
}

/// 有序、可回收的插件注册表与指令收件箱。
#[derive(Default)]
pub struct Extensions {
    slots: Vec<Slot>,
    next_seq: u64,
    next_id: u64,
    outbox: Outbox,
}

impl Extensions {
    pub fn new() -> Self {
        Self::default()
    }

    /// 以默认顺序（0）注册一个插件。
    pub fn register(&mut self, plugin: impl Plugin + 'static) -> Registration {
        self.register_ordered(0, plugin)
    }

    /// 注册一个插件；`order` 小的先被问询，同序保持注册先后。
    pub fn register_ordered(&mut self, order: i32, plugin: impl Plugin + 'static) -> Registration {
        let id = self.next_id;
        let seq = self.next_seq;
        self.next_seq += 1;
        self.next_id += 1;
        let slot = Slot {
            order,
            seq,
            id,
            plugin: Box::new(plugin),
        };
        let at = self
            .slots
            .partition_point(|slot| (slot.order, slot.seq) <= (order, seq));
        self.slots.insert(at, slot);
        Registration { id }
    }

    /// 撤销一次注册；已经不在表里则返回 `false`。
    pub fn revoke(&mut self, registration: Registration) -> bool {
        match self
            .slots
            .iter()
            .position(|slot| slot.id == registration.id)
        {
            Some(at) => {
                self.slots.remove(at);
                true
            }
            None => false,
        }
    }

    /// 是否没有任何插件。
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// 已注册插件的名字，按问询顺序。
    pub fn names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.slots.iter().map(|slot| slot.plugin.name())
    }

    /// 把一条事件按序分发给所有插件，指令进入各自的队列。
    ///
    /// 不再短路：即使前面的插件已经拒绝，后面的插件仍然看到这条事件。
    pub fn publish(&mut self, event: &PluginEvent<'_>) {
        let Self { slots, outbox, .. } = self;
        for slot in slots {
            let by = slot.plugin.name();
            let commands = slot.plugin.on_event(event);
            outbox.issue(by, commands);
        }
    }

    /// 门禁检查点：取走针对该调用的拒绝/审批指令，`Deny > Ask > Allow`。
    pub fn gate(&mut self, call: &ToolCallId) -> Gate {
        self.outbox.gate(call)
    }

    /// 结果检查点：取走针对该调用的结果替换，第一条生效。
    pub fn replacement(&mut self, call: &ToolCallId) -> Option<Blocked> {
        self.outbox.replacement(call)
    }

    /// 取走第一条中止指令。
    pub fn abort(&mut self) -> Option<Aborted> {
        self.outbox.abort()
    }

    /// 取走队首的注入消息。
    pub fn take_injection(&mut self) -> Option<QueuedMessage> {
        self.outbox.take_injection()
    }

    /// 取走全部剩余注入（收场时交还宿主）。
    pub(crate) fn take_pending_injections(&mut self) -> Vec<QueuedMessage> {
        self.outbox.take_injections()
    }

    /// 清空上次运行遗留的注入。
    pub fn clear_pending_injections(&mut self) {
        self.outbox.clear_injections();
    }

    /// 取走全部工具变更，保持发布顺序。
    pub fn take_tool_changes(&mut self) -> Vec<ToolChange> {
        self.outbox.take_tool_changes()
    }

    /// 是否有待执行的工具变更。
    pub(crate) fn has_tool_changes(&self) -> bool {
        self.outbox.has_tool_changes()
    }

    /// 丢弃收场后不可能再执行的定向指令。
    pub(crate) fn clear_pending_commands(&mut self) {
        self.outbox.clear_decisions();
    }

    /// 开一个临时注册作用域：离开时按注册的逆序撤销。
    pub fn scope(&mut self) -> Scope<'_> {
        Scope {
            extensions: self,
            tokens: Vec::new(),
        }
    }
}

/// 临时注册的作用域。
pub struct Scope<'a> {
    extensions: &'a mut Extensions,
    tokens: Vec<Registration>,
}

impl Scope<'_> {
    pub fn register(&mut self, plugin: impl Plugin + 'static) -> Registration {
        let token = self.extensions.register(plugin);
        self.tokens.push(token);
        token
    }

    pub fn register_ordered(&mut self, order: i32, plugin: impl Plugin + 'static) -> Registration {
        let token = self.extensions.register_ordered(order, plugin);
        self.tokens.push(token);
        token
    }
}

impl Drop for Scope<'_> {
    fn drop(&mut self) {
        for token in self.tokens.drain(..).rev() {
            self.extensions.revoke(token);
        }
    }
}
