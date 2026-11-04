//! Agent runtime example on `async-executor` (`--features async-executor`).
//!
//! The host drives the executor itself; the crate never builds a runtime.

mod support;

use std::sync::Arc;

use agnes_ecs::adapters::async_executor::ExecutorSpawner;
use agnes_ecs::demo::{AgentPlugin, ModelPlugin, PermissionPlugin, PermissionPolicy, ToolPlugin};
use agnes_ecs::{App, Update};
use futures_lite::future::block_on;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let executor = Arc::new(async_executor::Executor::new());
    let mut app = App::new();
    app.add_plugin(AgentPlugin)
        .add_plugin(ModelPlugin::new(Arc::new(support::EchoModel)))
        .add_plugin(ToolPlugin::new(Arc::new(support::EchoTool)))
        .add_plugin(PermissionPlugin::new(PermissionPolicy::default()))
        .init_resource::<support::SeedSent>()
        .add_systems(Update, support::seed_input)
        .add_systems(Update, support::report_turns)
        .set_spawner(ExecutorSpawner::new(executor.clone()));

    block_on(executor.run(async { app.run_async().await }))?;
    println!("runtime stopped");
    Ok(())
}
