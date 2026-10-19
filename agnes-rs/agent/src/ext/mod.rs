//! 宿主扩展内核：可回收的插件注册与三类折叠点。
//!
//! 设计参照 dsh 的 Cordis 内核，但只取 Rust 里成立的部分：
//!
//! - 扩展点是**闭集**（门禁 / 结果 / 观察），不是字符串寻址的服务表；
//! - 门禁用「拒绝单调」的折叠，而不是 `next` 式瀑布 —— [`Verdict`] 里
//!   没有 `Allow`，注册顺序无法把拒绝变回放行；
//! - 注册可回收：长期注册拿 [`Registration`] 手柄显式撤销，临时注册用
//!   [`Extensions::scope`]，离开作用域按注册的逆序撤销。

mod decision;
mod plugin;

pub use decision::{ApprovalOutcome, Blocked, Denied, Gate, Settlement, Verdict};
pub use plugin::{Notice, Plugin};

use agnes_core::message::{ToolCall, ToolOutput};

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

/// 有序、可回收的插件注册表。
#[derive(Default)]
pub struct Extensions {
    slots: Vec<Slot>,
    next_seq: u64,
    next_id: u64,
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
        match self.slots.iter().position(|slot| slot.id == registration.id) {
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

    /// 门禁折叠：全部问询，取最强 —— Deny > Ask > 无人表态即放行。
    ///
    /// 一旦出现 Deny 就短路：更强的结果不存在，后续插件仍会通过
    /// [`Plugin::observe`] 看到这次拒绝。
    pub fn gate(&mut self, call: &ToolCall) -> Gate {
        let mut ask = false;
        for slot in &mut self.slots {
            match slot.plugin.gate(call) {
                None => {}
                Some(Verdict::Ask) => ask = true,
                Some(Verdict::Deny { reason }) => {
                    return Gate::Deny(Denied {
                        by: slot.plugin.name(),
                        reason,
                    });
                }
            }
        }
        if ask { Gate::Ask } else { Gate::Allow }
    }

    /// 结果折叠：按序询问，首个挡回生效（被挡的结果是终局）。
    pub fn settle(&mut self, call: &ToolCall, output: &ToolOutput) -> Option<Blocked> {
        for slot in &mut self.slots {
            if let Settlement::Block { feedback } = slot.plugin.settle(call, output) {
                return Some(Blocked {
                    by: slot.plugin.name(),
                    feedback,
                });
            }
        }
        None
    }

    /// 把一条通知送给每个插件。
    pub fn observe(&mut self, notice: &Notice<'_>) {
        for slot in &mut self.slots {
            slot.plugin.observe(notice);
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use agnes_core::message::{ToolArguments, ToolCallId};
    use agnes_core::tool::ToolName;
    use std::str::FromStr;
    use std::sync::{Arc, Mutex};

    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: ToolCallId::new("c1"),
            name: ToolName::from_str(name).unwrap(),
            arguments: ToolArguments::new("{}"),
        }
    }

    /// 一个可配置的探针：记录被问询的顺序，按配置表态。
    struct Probe {
        name: &'static str,
        verdict: Option<Verdict>,
        settlement: Settlement,
        seen: Arc<Mutex<Vec<&'static str>>>,
    }

    impl Probe {
        fn new(name: &'static str, seen: &Arc<Mutex<Vec<&'static str>>>) -> Self {
            Self {
                name,
                verdict: None,
                settlement: Settlement::Accept,
                seen: Arc::clone(seen),
            }
        }

        fn verdict(mut self, verdict: Verdict) -> Self {
            self.verdict = Some(verdict);
            self
        }

        fn settlement(mut self, settlement: Settlement) -> Self {
            self.settlement = settlement;
            self
        }
    }

    impl Plugin for Probe {
        fn name(&self) -> &'static str {
            self.name
        }

        fn gate(&mut self, _call: &ToolCall) -> Option<Verdict> {
            self.seen.lock().unwrap().push(self.name);
            self.verdict.clone()
        }

        fn settle(
            &mut self,
            _call: &ToolCall,
            _output: &agnes_core::message::ToolOutput,
        ) -> Settlement {
            self.seen.lock().unwrap().push(self.name);
            self.settlement.clone()
        }
    }

    fn deny(reason: &str) -> Verdict {
        Verdict::Deny {
            reason: reason.into(),
        }
    }

    #[test]
    fn nobody_speaking_is_allow() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut extensions = Extensions::new();
        extensions.register(Probe::new("silent", &seen));
        assert_eq!(extensions.gate(&call("note")), Gate::Allow);
    }

    #[test]
    fn ask_is_overridden_by_a_deny() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut extensions = Extensions::new();
        extensions.register(Probe::new("asker", &seen).verdict(Verdict::Ask));
        let denied = deny("never on sundays");
        let expected = Gate::Deny(Denied {
            by: "denier",
            reason: "never on sundays".into(),
        });
        extensions.register(Probe::new("denier", &seen).verdict(denied));

        assert_eq!(extensions.gate(&call("note")), expected);
    }

    #[test]
    fn a_deny_is_final_and_stops_the_fold() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut extensions = Extensions::new();
        extensions.register(Probe::new("first", &seen).verdict(deny("no")));
        extensions.register(Probe::new("second", &seen));

        assert!(matches!(extensions.gate(&call("note")), Gate::Deny(_)));
        // deny 已是最强，后面的插件不再被问询
        assert_eq!(*seen.lock().unwrap(), vec!["first"]);
    }

    #[test]
    fn ordering_is_by_order_then_registration() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut extensions = Extensions::new();
        extensions.register_ordered(10, Probe::new("late", &seen));
        extensions.register_ordered(-10, Probe::new("early", &seen));
        extensions.register(Probe::new("middle", &seen));

        let _ = extensions.gate(&call("note"));
        assert_eq!(*seen.lock().unwrap(), vec!["early", "middle", "late"]);
    }

    #[test]
    fn revoke_removes_a_registration() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut extensions = Extensions::new();
        let token = extensions.register(Probe::new("gone", &seen).verdict(deny("no")));
        assert!(!extensions.is_empty());

        assert!(extensions.revoke(token));
        assert!(extensions.is_empty());
        assert_eq!(extensions.gate(&call("note")), Gate::Allow);
        // 二次撤销无效
        assert!(!extensions.revoke(token));
    }

    #[test]
    fn a_scope_unwinds_its_registrations() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut extensions = Extensions::new();
        let permanent = extensions.register(Probe::new("permanent", &seen));

        {
            let mut scope = extensions.scope();
            scope.register(Probe::new("temporary", &seen));
            scope.register_ordered(5, Probe::new("another", &seen));
        }

        assert_eq!(extensions.names().collect::<Vec<_>>(), vec!["permanent"]);
        assert!(extensions.revoke(permanent));
    }

    #[test]
    fn the_first_block_wins() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut extensions = Extensions::new();
        extensions.register(Probe::new("accepting", &seen));
        extensions.register(Probe::new("blocking", &seen).settlement(Settlement::Block {
            feedback: "looks wrong".into(),
        }));
        extensions.register(Probe::new("later", &seen).settlement(Settlement::Block {
            feedback: "also wrong".into(),
        }));

        let output = agnes_core::message::ToolOutput::success("original");
        let blocked = extensions.settle(&call("note"), &output).unwrap();
        assert_eq!(blocked.by, "blocking");
        assert_eq!(blocked.feedback, "looks wrong");
        // 首个挡回即终局
        assert_eq!(*seen.lock().unwrap(), vec!["accepting", "blocking"]);
    }

    #[test]
    fn every_plugin_sees_the_notice() {
        struct Counter(Arc<Mutex<usize>>);
        impl Plugin for Counter {
            fn name(&self) -> &'static str {
                "counter"
            }
            fn observe(&mut self, notice: &Notice<'_>) {
                if matches!(notice, Notice::Core(_)) {
                    *self.0.lock().unwrap() += 1;
                }
            }
        }

        let count = Arc::new(Mutex::new(0));
        let mut extensions = Extensions::new();
        extensions.register(Counter(Arc::clone(&count)));
        extensions.register(Counter(Arc::clone(&count)));

        let event = agnes_core::event::Event::TurnStart;
        extensions.observe(&Notice::Core(&event));
        assert_eq!(*count.lock().unwrap(), 2);
    }
}
