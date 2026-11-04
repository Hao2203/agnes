//! Agent runtime example on Tokio (`--features tokio`).

mod support;

use std::sync::Arc;

use agnes_ecs::adapters::tokio::{TokioSpawner, run};
use agnes_ecs::demo::{AgentPlugin, ModelPlugin, PermissionPlugin, PermissionPolicy, ToolPlugin};
use agnes_ecs::{App, Update};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut app = App::new();
    app.add_plugin(AgentPlugin)
        .add_plugin(ModelPlugin::new(Arc::new(support::EchoModel)))
        .add_plugin(ToolPlugin::new(Arc::new(support::EchoTool)))
        .add_plugin(PermissionPlugin::new(PermissionPolicy::default()))
        .init_resource::<support::SeedSent>()
        .add_systems(Update, support::seed_input)
        .add_systems(Update, support::report_turns)
        .set_spawner(TokioSpawner);

    run(app)?;
    println!("runtime stopped");
    Ok(())
}
