//! Building blocks shared by the agent runtime examples.
//!
//! The example is fully self-driving: `seed_input` injects one user prompt,
//! the demo pipeline runs model → permission → tool, and `report_turns`
//! prints the finished turn and cancels the app so the runner exits without
//! any timer.

use agnes_ecs::demo::{ModelError, ModelProvider, Tool, ToolError, Turn, UserInput};
use agnes_ecs::{AsyncExecutor, Commands, Entity, Query, Res, ResMut, Without};
use async_trait::async_trait;

pub struct EchoModel;

#[async_trait]
impl ModelProvider for EchoModel {
    fn name(&self) -> &str {
        "default"
    }

    async fn complete(&self, prompt: &str) -> Result<String, ModelError> {
        Ok(format!("tool:echo:{prompt}"))
    }
}

pub struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }

    async fn run(&self, arguments: &str) -> Result<String, ToolError> {
        Ok(format!("echoed {arguments}"))
    }
}

pub struct Reported;

#[derive(Default)]
pub struct SeedSent(pub bool);

pub fn seed_input(executor: Res<AsyncExecutor>, mut sent: ResMut<SeedSent>) {
    if sent.0 {
        return;
    }
    sent.0 = true;
    if let Err(error) = executor.send(UserInput {
        text: "hello from the example".to_owned(),
    }) {
        log::warn!("failed to seed input: {error}");
    }
}

pub fn report_turns(
    query: Query<(&Turn, Entity), Without<Reported>>,
    executor: Res<AsyncExecutor>,
    mut commands: Commands,
) {
    for (turn, entity) in query.iter() {
        if turn.finished() {
            println!("turn finished: {}", turn.last_text());
            commands.entity(entity).insert(Reported);
            executor.cancellation().cancel();
        }
    }
}
