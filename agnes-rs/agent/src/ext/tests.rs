use super::*;
use crate::record::Event as RecordEvent;
use crate::test_support::{assistant, user};
use agnes_core::message::{Message, ToolArguments, ToolCall, ToolCallId, ToolOutput};
use agnes_core::tool::ToolName;
use std::cell::Cell;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

fn call(id: &str, name: &str) -> ToolCall {
    ToolCall {
        id: ToolCallId::new(id),
        name: ToolName::from_str(name).unwrap(),
        arguments: ToolArguments::new("{}"),
    }
}

fn output() -> ToolOutput {
    ToolOutput::success("original")
}

/// 记录收到的事件，不表态。
struct Seen {
    name: &'static str,
    seen: Arc<Mutex<Vec<&'static str>>>,
}

impl Plugin for Seen {
    fn name(&self) -> &str {
        self.name
    }

    fn on_event(&mut self, _event: &PluginEvent<'_>) -> Vec<Command> {
        self.seen.lock().unwrap().push(self.name);
        Vec::new()
    }
}

struct Denier {
    by: &'static str,
    reason: &'static str,
}

impl Plugin for Denier {
    fn name(&self) -> &str {
        self.by
    }

    fn on_event(&mut self, event: &PluginEvent<'_>) -> Vec<Command> {
        match event {
            PluginEvent::ToolCallRequested { call } => vec![Command::Deny {
                call: call.id.clone(),
                reason: self.reason.into(),
            }],
            _ => Vec::new(),
        }
    }
}

struct Asker;

impl Plugin for Asker {
    fn name(&self) -> &str {
        "asker"
    }

    fn on_event(&mut self, event: &PluginEvent<'_>) -> Vec<Command> {
        match event {
            PluginEvent::ToolCallRequested { call } => {
                vec![Command::RequireApproval {
                    call: call.id.clone(),
                }]
            }
            _ => Vec::new(),
        }
    }
}

struct Blocker {
    by: &'static str,
    feedback: &'static str,
}

impl Plugin for Blocker {
    fn name(&self) -> &str {
        self.by
    }

    fn on_event(&mut self, event: &PluginEvent<'_>) -> Vec<Command> {
        match event {
            PluginEvent::ToolCallFinished { call, .. } => vec![Command::ReplaceResult {
                call: call.id.clone(),
                feedback: self.feedback.into(),
            }],
            _ => Vec::new(),
        }
    }
}

struct Aborter {
    reason: &'static str,
}

impl Plugin for Aborter {
    fn name(&self) -> &str {
        "aborter"
    }

    fn on_event(&mut self, _event: &PluginEvent<'_>) -> Vec<Command> {
        vec![Command::Abort {
            reason: self.reason.into(),
        }]
    }
}

struct Injector;

impl Plugin for Injector {
    fn name(&self) -> &str {
        "injector"
    }

    fn on_event(&mut self, event: &PluginEvent<'_>) -> Vec<Command> {
        match event {
            PluginEvent::Record(RecordEvent::TurnStart) => vec![
                Command::InjectMessage {
                    message: user("first"),
                },
                Command::InjectMessage {
                    message: user("second"),
                },
            ],
            _ => Vec::new(),
        }
    }
}

/// 注入一条非用户消息：角色必须原样保留。
struct AssistantInjector;

impl Plugin for AssistantInjector {
    fn name(&self) -> &str {
        "assistant-injector"
    }

    fn on_event(&mut self, event: &PluginEvent<'_>) -> Vec<Command> {
        match event {
            PluginEvent::Record(RecordEvent::TurnStart) => vec![Command::InjectMessage {
                message: assistant("prefilled"),
            }],
            _ => Vec::new(),
        }
    }
}

/// 运行时装上的插件：名字在装载时才确定。
struct RuntimeNamed {
    name: String,
}

impl Plugin for RuntimeNamed {
    fn name(&self) -> &str {
        &self.name
    }

    fn on_event(&mut self, event: &PluginEvent<'_>) -> Vec<Command> {
        match event {
            PluginEvent::ToolCallRequested { call } => vec![Command::Deny {
                call: call.id.clone(),
                reason: "denied at runtime".into(),
            }],
            _ => Vec::new(),
        }
    }
}

/// 名字在运行时可变：内核应继续使用注册时捕获的名字。
struct Renamable {
    name: Cell<&'static str>,
}

impl Plugin for Renamable {
    fn name(&self) -> &str {
        self.name.get()
    }

    fn on_event(&mut self, event: &PluginEvent<'_>) -> Vec<Command> {
        match event {
            PluginEvent::Record(_) => {
                self.name.set("renamed");
                Vec::new()
            }
            PluginEvent::ToolCallRequested { call } => vec![Command::Deny {
                call: call.id.clone(),
                reason: "no".into(),
            }],
            _ => Vec::new(),
        }
    }
}

/// 运行期命名，一次返回注入、挡回与中止。
struct RuntimeEverything {
    name: String,
}

impl Plugin for RuntimeEverything {
    fn name(&self) -> &str {
        &self.name
    }

    fn on_event(&mut self, event: &PluginEvent<'_>) -> Vec<Command> {
        match event {
            PluginEvent::ToolCallRequested { call } => vec![
                Command::InjectMessage {
                    message: user("note"),
                },
                Command::ReplaceResult {
                    call: call.id.clone(),
                    feedback: "blocked".into(),
                },
                Command::Abort {
                    reason: "stop".into(),
                },
            ],
            _ => Vec::new(),
        }
    }
}

fn requested<'a>(call: &'a ToolCall) -> PluginEvent<'a> {
    PluginEvent::ToolCallRequested { call }
}

#[test]
fn nobody_speaking_is_allow() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut extensions = Extensions::new();
    extensions.register(Seen {
        name: "silent",
        seen: Arc::clone(&seen),
    });
    let call = call("c1", "note");
    extensions.publish(&requested(&call));
    assert_eq!(extensions.gate(&call.id), Gate::Allow);
}

#[test]
fn ask_is_overridden_by_a_deny() {
    let mut extensions = Extensions::new();
    extensions.register(Asker);
    extensions.register(Denier {
        by: "denier",
        reason: "never on sundays",
    });
    let call = call("c1", "note");
    extensions.publish(&requested(&call));

    assert_eq!(
        extensions.gate(&call.id),
        Gate::Deny(Denied {
            by: "denier".into(),
            reason: "never on sundays".into(),
        })
    );
}

#[test]
fn a_deny_does_not_short_circuit_the_event_stream() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut extensions = Extensions::new();
    extensions.register(Denier {
        by: "first",
        reason: "no",
    });
    extensions.register(Seen {
        name: "second",
        seen: Arc::clone(&seen),
    });
    let call = call("c1", "note");
    extensions.publish(&requested(&call));

    assert!(matches!(extensions.gate(&call.id), Gate::Deny(_)));
    // 拒绝不再阻止后续插件看到事件
    assert_eq!(*seen.lock().unwrap(), vec!["second"]);
}

#[test]
fn the_first_deny_by_order_wins() {
    let mut extensions = Extensions::new();
    extensions.register(Denier {
        by: "first",
        reason: "first reason",
    });
    extensions.register(Denier {
        by: "second",
        reason: "second reason",
    });
    let call = call("c1", "note");
    extensions.publish(&requested(&call));

    assert_eq!(
        extensions.gate(&call.id),
        Gate::Deny(Denied {
            by: "first".into(),
            reason: "first reason".into(),
        })
    );
}

#[test]
fn ordering_is_by_order_then_registration() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut extensions = Extensions::new();
    extensions.register_ordered(
        10,
        Seen {
            name: "late",
            seen: Arc::clone(&seen),
        },
    );
    extensions.register_ordered(
        -10,
        Seen {
            name: "early",
            seen: Arc::clone(&seen),
        },
    );
    extensions.register(Seen {
        name: "middle",
        seen: Arc::clone(&seen),
    });

    let call = call("c1", "note");
    extensions.publish(&requested(&call));
    assert_eq!(*seen.lock().unwrap(), vec!["early", "middle", "late"]);
}

#[test]
fn revoke_removes_a_registration() {
    let mut extensions = Extensions::new();
    let token = extensions.register(Denier {
        by: "gone",
        reason: "no",
    });
    assert!(!extensions.is_empty());

    assert!(extensions.revoke(token));
    assert!(extensions.is_empty());
    let call = call("c1", "note");
    extensions.publish(&requested(&call));
    assert_eq!(extensions.gate(&call.id), Gate::Allow);
    // 二次撤销无效
    assert!(!extensions.revoke(token));
}

#[test]
fn a_scope_unwinds_its_registrations() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut extensions = Extensions::new();
    let permanent = extensions.register(Seen {
        name: "permanent",
        seen: Arc::clone(&seen),
    });

    {
        let mut scope = extensions.scope();
        scope.register(Seen {
            name: "temporary",
            seen: Arc::clone(&seen),
        });
        scope.register_ordered(
            5,
            Seen {
                name: "another",
                seen: Arc::clone(&seen),
            },
        );
    }

    assert_eq!(extensions.names().collect::<Vec<_>>(), vec!["permanent"]);
    assert!(extensions.revoke(permanent));
}

#[test]
fn the_first_block_wins() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut extensions = Extensions::new();
    extensions.register(Seen {
        name: "accepting",
        seen,
    });
    extensions.register(Blocker {
        by: "blocking",
        feedback: "looks wrong",
    });
    extensions.register(Blocker {
        by: "later",
        feedback: "also wrong",
    });

    let call = call("c1", "note");
    let output = output();
    extensions.publish(&PluginEvent::ToolCallFinished {
        call: &call,
        output: &output,
    });
    let blocked = extensions.replacement(&call.id).unwrap();
    assert_eq!(blocked.by.as_str(), "blocking");
    assert_eq!(blocked.feedback, "looks wrong");
    // 其他调用的结果替换不会被这条事件消费
    assert_eq!(extensions.replacement(&ToolCallId::new("other")), None);
}

#[test]
fn commands_for_other_calls_stay_queued() {
    let mut extensions = Extensions::new();
    extensions.register(Denier {
        by: "denier",
        reason: "no",
    });
    let first = call("c1", "note");
    let second = call("c2", "note");
    extensions.publish(&requested(&second));

    // 针对 c2 的拒绝不会被 c1 的检查点吃掉
    assert_eq!(extensions.gate(&first.id), Gate::Allow);
    assert_eq!(
        extensions.gate(&second.id),
        Gate::Deny(Denied {
            by: "denier".into(),
            reason: "no".into(),
        })
    );
}

#[test]
fn injections_are_fifo() {
    let mut extensions = Extensions::new();
    extensions.register(Injector);
    extensions.publish(&PluginEvent::Record(&RecordEvent::TurnStart));

    let first = extensions.take_injection().unwrap();
    assert_eq!(first.by().as_str(), "injector");
    assert!(matches!(first.message(), Message::User { content } if content.as_str() == "first"));
    let second = extensions.take_injection().unwrap();
    assert!(matches!(second.message(), Message::User { content } if content.as_str() == "second"));
    assert!(extensions.take_injection().is_none());
}

#[test]
fn an_injection_keeps_the_message_role() {
    let mut extensions = Extensions::new();
    extensions.register(AssistantInjector);
    extensions.publish(&PluginEvent::Record(&RecordEvent::TurnStart));

    let queued = extensions.take_injection().unwrap();
    assert_eq!(queued.by().as_str(), "assistant-injector");
    assert_eq!(queued.message(), &assistant("prefilled"));
}

#[test]
fn the_first_abort_wins() {
    let mut extensions = Extensions::new();
    extensions.register_ordered(
        -1,
        Aborter {
            reason: "first reason",
        },
    );
    extensions.register_ordered(
        1,
        Aborter {
            reason: "second reason",
        },
    );
    let call = call("c1", "note");
    extensions.publish(&requested(&call));

    let aborted = extensions.abort().unwrap();
    assert_eq!(aborted.by.as_str(), "aborter");
    assert_eq!(aborted.reason, "first reason");
    assert!(extensions.abort().is_none());
}

#[test]
fn a_consumed_deny_does_not_fire_again() {
    let mut extensions = Extensions::new();
    extensions.register(Denier {
        by: "denier",
        reason: "no",
    });
    let call = call("c1", "note");
    extensions.publish(&requested(&call));
    assert!(matches!(extensions.gate(&call.id), Gate::Deny(_)));
    // 同一个检查点不会重复消费同一条拒绝
    assert_eq!(extensions.gate(&call.id), Gate::Allow);
}

#[test]
fn an_origin_holds_both_static_and_runtime_names() {
    let literal: Origin = "denier".into();
    let owned: Origin = String::from("denier").into();
    assert_eq!(literal, owned);
    assert_eq!(literal.as_str(), "denier");
    assert_eq!(literal.to_string(), "denier");
    assert_eq!(format!("{literal:?}"), r#"Origin("denier")"#);
    assert_eq!(literal.clone(), literal);

    // 长名字引用计数：克隆共享同一份分配。
    let long: Origin = "a runtime plugin name longer than one word".into();
    assert_eq!(long.as_str().as_ptr(), long.clone().as_str().as_ptr());
}

#[test]
fn a_runtime_name_reaches_the_gate() {
    let mut extensions = Extensions::new();
    extensions.register(RuntimeNamed {
        name: "memory@1.2".into(),
    });
    assert_eq!(extensions.names().collect::<Vec<_>>(), vec!["memory@1.2"]);

    let call = call("c1", "note");
    extensions.publish(&requested(&call));
    assert_eq!(
        extensions.gate(&call.id),
        Gate::Deny(Denied {
            by: "memory@1.2".into(),
            reason: "denied at runtime".into(),
        })
    );
}

#[test]
fn the_name_is_fixed_at_registration() {
    let mut extensions = Extensions::new();
    extensions.register(Renamable {
        name: Cell::new("original"),
    });

    // 事件把插件内部的名字改掉，注册名不该跟着变。
    extensions.publish(&PluginEvent::Record(&RecordEvent::TurnStart));
    let call = call("c1", "note");
    extensions.publish(&requested(&call));

    assert_eq!(extensions.names().collect::<Vec<_>>(), vec!["original"]);
    assert_eq!(
        extensions.gate(&call.id),
        Gate::Deny(Denied {
            by: "original".into(),
            reason: "no".into(),
        })
    );
}

#[test]
fn a_runtime_name_reaches_every_queue() {
    let mut extensions = Extensions::new();
    extensions.register(RuntimeEverything {
        name: "memory@1.2".into(),
    });
    let call = call("c1", "note");
    extensions.publish(&requested(&call));

    assert_eq!(
        extensions.take_injection().unwrap().by().as_str(),
        "memory@1.2"
    );
    assert_eq!(
        extensions.replacement(&call.id).unwrap().by.as_str(),
        "memory@1.2"
    );
    assert_eq!(extensions.abort().unwrap().by.as_str(), "memory@1.2");
}
