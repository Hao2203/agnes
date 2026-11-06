//! 宿主侧模型设置：当前模型与思考模式。
//!
//! [`ModelSettings`] 是唯一真相，以组件形式挂在 agent 根实体上，只有
//! [`ModelPlugin`] 的 `apply_settings` 系统会写它；其余系统与界面都只读。
//! 切换在下一份请求生效：模型名随 `LlmCall` 一起离开，因此 in-flight
//! 调用不会被改到。多个 agent 各自持有自己的设置。组件由宿主在根实体
//! 上挂载（[`ModelSettings::new`]）；插件只注册投影系统。

use agnes_core::config::{ModelName, Thinking};
use agnes_ecs::{App, Entity, MessageReader, MessageWriter, Plugin, Query, Update};

agnes_ecs::component!(ModelSettings);

/// 当前生效的请求设置（agent 根实体上的组件）。
#[derive(Debug, Clone)]
pub struct ModelSettings {
    model: ModelName,
    thinking: Thinking,
}

impl ModelSettings {
    pub fn new(model: ModelName, thinking: Thinking) -> Self {
        Self { model, thinking }
    }

    pub fn model(&self) -> &ModelName {
        &self.model
    }

    pub fn thinking(&self) -> Thinking {
        self.thinking
    }
}

/// 切换目标 agent 的模型；下一份请求生效。
#[derive(Debug)]
pub struct SetModel {
    pub agent: Entity,
    pub model: ModelName,
}

/// 设置目标 agent 的思考模式与强度；下一份请求生效。
#[derive(Debug)]
pub struct SetThinking {
    pub agent: Entity,
    pub thinking: Thinking,
}

/// 设置真的发生了变化（同值不发，避免界面刷屏）。零大小标记事件：新值读
/// 目标 agent 根实体上的 [`ModelSettings`] 组件，事件发布时已是终态。
#[derive(Debug)]
pub struct ModelSettingsChanged {
    /// 设置变化的 agent 根实体。
    pub agent: Entity,
}

/// 注册设置投影系统：读宿主意图，写 [`ModelSettings`]。
///
/// 必须先于 `AgentPlugin` 注册（由它断言）：同一个 tick 内设置先于模型
/// 调用生效。设置组件由宿主挂载，本插件不创建。
#[derive(Default)]
pub struct ModelPlugin;

impl Plugin for ModelPlugin {
    fn name(&self) -> &'static str {
        "agnes-model"
    }

    fn build(&self, app: &mut App) {
        app.add_systems(Update, apply_settings);
    }
}

/// 唯一的写者：读意图消息，按目标 agent 更新设置；一个 agent 在同一 tick
/// 内多次变化只广播一次变更（同值不发，避免界面刷屏）。
fn apply_settings(
    mut sessions: Query<(Entity, &mut ModelSettings)>,
    mut models: MessageReader<SetModel>,
    mut thinking: MessageReader<SetThinking>,
    mut changed: MessageWriter<ModelSettingsChanged>,
) {
    let mut dirty: Vec<Entity> = Vec::new();
    for message in models.read() {
        let Some((_, mut settings)) = sessions.get_mut(message.agent) else {
            continue;
        };
        if settings.model != message.model {
            settings.model = message.model.clone();
            if !dirty.contains(&message.agent) {
                dirty.push(message.agent);
            }
        }
    }
    for message in thinking.read() {
        let Some((_, mut settings)) = sessions.get_mut(message.agent) else {
            continue;
        };
        if settings.thinking != message.thinking {
            settings.thinking = message.thinking;
            if !dirty.contains(&message.agent) {
                dirty.push(message.agent);
            }
        }
    }
    for agent in dirty {
        changed.write(ModelSettingsChanged { agent });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agnes_core::config::ReasoningEffort;
    use agnes_ecs::ResMut;
    use std::str::FromStr;

    #[derive(Default, Clone)]
    struct Captured {
        changes: usize,
    }

    fn capture(mut reader: MessageReader<ModelSettingsChanged>, mut captured: ResMut<Captured>) {
        for _ in reader.read() {
            captured.changes += 1;
        }
    }

    fn model(name: &str) -> ModelName {
        ModelName::from_str(name).expect("static model name is valid")
    }

    fn app() -> App {
        let mut app = App::new();
        // 宿主装配：根实体与设置组件先建，插件只注册系统。
        app.world_mut().spawn_bundle((
            crate::AgentRoot,
            ModelSettings::new(model("a"), Thinking::Off),
        ));
        app.add_plugin(ModelPlugin)
            .insert_resource(Captured::default())
            .add_systems(Update, capture);
        app
    }

    fn agent(app: &App) -> Entity {
        app.world()
            .entity_with::<crate::AgentRoot>()
            .expect("agent root exists")
    }

    fn settings(app: &App) -> (String, Thinking) {
        let settings = app
            .world()
            .get::<ModelSettings>(agent(app))
            .expect("settings are installed");
        (settings.model().as_str().to_owned(), settings.thinking())
    }

    fn captured(app: &App) -> Captured {
        app.world()
            .resource::<Captured>()
            .expect("captured is installed")
            .clone()
    }

    #[test]
    fn setting_the_model_and_thinking_updates_the_component() {
        let mut app = app();
        let agent = agent(&app);
        app.world_mut().send_message(SetModel {
            agent,
            model: model("b"),
        });
        app.world_mut().send_message(SetThinking {
            agent,
            thinking: Thinking::On(ReasoningEffort::Low),
        });
        app.pump().expect("schedule runs");

        assert_eq!(
            settings(&app),
            ("b".to_owned(), Thinking::On(ReasoningEffort::Low))
        );
        assert_eq!(captured(&app).changes, 1);
    }

    #[test]
    fn the_same_values_emit_no_change() {
        let mut app = app();
        let agent = agent(&app);
        app.world_mut().send_message(SetModel {
            agent,
            model: model("a"),
        });
        app.world_mut().send_message(SetThinking {
            agent,
            thinking: Thinking::Off,
        });
        app.pump().expect("schedule runs");

        assert_eq!(captured(&app).changes, 0);
        assert_eq!(settings(&app), ("a".to_owned(), Thinking::Off));
    }
}
