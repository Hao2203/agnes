use std::sync::Arc;

use async_executor::Executor;
use futures_lite::future::block_on;

use crate::schedule::Schedule;
use crate::world::World;
use crate::{
    App, CancelToken, Commands, Entity, MessageReader, MessageWriter, Plugin, Query, Res, ResMut,
    ScheduleError, SystemError, Update,
};

#[derive(Debug, PartialEq, Eq)]
struct Config {
    value: u32,
}

#[derive(Debug, Default)]
struct State {
    value: u32,
}

fn copy_config(config: Res<Config>, mut state: ResMut<State>) {
    state.value = config.value;
}

fn conflicting_resources(_first: ResMut<Config>, _second: Res<Config>) {}

fn conflicting_queries(_first: Query<&mut Health>, _second: Query<&Health>) {}

fn conflicting_tuple_query(_query: Query<(&mut Health, &Health)>) {}

fn conflicting_messages(_writer: MessageWriter<Ping>, _reader: MessageReader<Ping>) {}

fn missing_resource(_config: Res<Config>, _state: ResMut<State>) {}

fn three_params(config: Res<Config>, mut state: ResMut<State>, _optional: Option<Res<u64>>) {
    state.value = config.value + 1;
}

fn no_params() {}

#[test]
fn a_system_can_have_no_parameters() {
    let mut world = World::new();

    let mut schedule = Schedule::new();
    schedule
        .add_system(no_params)
        .expect("no parameters are supported");
    schedule.run(&mut world).expect("run succeeds");
}

#[allow(clippy::too_many_arguments)]
fn eight_params(
    _a: Res<Config>,
    _b: Res<Config>,
    _c: Res<Config>,
    _d: Res<Config>,
    _e: Res<Config>,
    _f: Res<Config>,
    _g: Res<Config>,
    _h: Res<Config>,
) {
}

#[test]
fn a_system_can_declare_eight_parameters() {
    let mut world = World::new();
    world.insert_resource(Config { value: 1 });

    let mut schedule = Schedule::new();
    schedule
        .add_system(eight_params)
        .expect("eight parameters are supported");
    schedule.run(&mut world).expect("run succeeds");
}

#[test]
fn res_and_res_mut_are_extracted() {
    let mut world = World::new();
    world.insert_resource(Config { value: 7 });
    world.insert_resource(State::default());

    let mut schedule = Schedule::new();
    schedule
        .add_system(copy_config)
        .expect("copy_config has no conflicts");
    schedule.run(&mut world).expect("resources are present");

    assert_eq!(world.resource::<State>().expect("state exists").value, 7);
}

#[test]
fn a_missing_resource_is_reported() {
    let mut world = World::new();
    world.insert_resource(State::default());

    let mut schedule = Schedule::new();
    schedule
        .add_system(missing_resource)
        .expect("missing_resource has no static conflicts");
    let error = schedule.run(&mut world).expect_err("config is missing");

    assert!(matches!(
        error,
        SystemError::Param {
            source: crate::ParamError::MissingResource { .. },
            ..
        }
    ));
}

#[test]
fn conflicting_parameters_are_rejected_when_added() {
    let mut schedule = Schedule::new();
    let error = schedule
        .add_system(conflicting_resources)
        .expect_err("read and write of the same resource conflict");

    assert!(matches!(error, ScheduleError::ConflictingAccess { .. }));
    assert!(schedule.is_empty());
}

#[test]
fn query_read_write_conflicts_are_rejected() {
    let mut schedule = Schedule::new();
    let error = schedule
        .add_system(conflicting_queries)
        .expect_err("two queries conflict on the same component");

    assert!(matches!(error, ScheduleError::ConflictingAccess { .. }));
}

#[test]
fn tuple_query_read_write_conflicts_are_rejected() {
    let mut schedule = Schedule::new();
    let error = schedule
        .add_system(conflicting_tuple_query)
        .expect_err("one tuple query conflicts with itself");

    assert!(matches!(error, ScheduleError::ConflictingAccess { .. }));
}

#[test]
fn message_read_write_conflicts_are_rejected() {
    let mut schedule = Schedule::new();
    let error = schedule
        .add_system(conflicting_messages)
        .expect_err("reading and writing the same message type conflicts");

    assert!(matches!(error, ScheduleError::ConflictingAccess { .. }));
}

#[test]
fn tuple_parameters_with_an_empty_option_run() {
    let mut world = World::new();
    world.insert_resource(Config { value: 3 });
    world.insert_resource(State::default());

    let mut schedule = Schedule::new();
    schedule
        .add_system(three_params)
        .expect("tuple parameters are valid");
    schedule.run(&mut world).expect("resources are present");

    assert_eq!(world.resource::<State>().expect("state exists").value, 4);
}

#[derive(Debug, PartialEq, Eq)]
struct Ping(u32);

#[derive(Default)]
struct Seen(Vec<u32>);

#[derive(Default)]
struct SeenAgain(Vec<u32>);

fn write_ping(mut writer: MessageWriter<Ping>) {
    writer.write(Ping(1));
}

fn read_ping(mut reader: MessageReader<Ping>, mut seen: ResMut<Seen>) {
    for ping in reader.read() {
        seen.0.push(ping.0);
    }
}

fn read_ping_again(mut reader: MessageReader<Ping>, mut seen: ResMut<SeenAgain>) {
    for ping in reader.read() {
        seen.0.push(ping.0);
    }
}

#[test]
fn writer_messages_are_visible_next_run() {
    let mut world = World::new();
    world.insert_resource(Seen::default());

    let mut schedule = Schedule::new();
    schedule.add_system(write_ping).expect("writer is valid");
    schedule.add_system(read_ping).expect("reader is valid");

    schedule.run(&mut world).expect("first run succeeds");
    assert!(world.resource::<Seen>().expect("seen exists").0.is_empty());

    schedule.run(&mut world).expect("second run succeeds");
    assert_eq!(world.resource::<Seen>().expect("seen exists").0, [1]);
}

#[test]
fn host_messages_are_visible_in_the_same_run() {
    let mut world = World::new();
    world.insert_resource(Seen::default());
    world.send_message(Ping(7));

    let mut schedule = Schedule::new();
    schedule.add_system(read_ping).expect("reader is valid");
    schedule.run(&mut world).expect("run succeeds");

    assert_eq!(world.resource::<Seen>().expect("seen exists").0, [7]);
}

#[test]
fn readers_have_independent_cursors() {
    let mut world = World::new();
    world.insert_resource(Seen::default());
    world.insert_resource(SeenAgain::default());
    world.send_message(Ping(3));

    let mut schedule = Schedule::new();
    schedule
        .add_system(read_ping)
        .expect("first reader is valid");
    schedule
        .add_system(read_ping_again)
        .expect("second reader is valid");
    schedule.run(&mut world).expect("run succeeds");

    assert_eq!(world.resource::<Seen>().expect("seen exists").0, [3]);
    assert_eq!(
        world.resource::<SeenAgain>().expect("seen again exists").0,
        [3]
    );
}

#[test]
fn a_new_reader_does_not_replay_history() {
    let mut world = World::new();
    world.insert_resource(Seen::default());
    world.insert_resource(SeenAgain::default());

    let mut schedule = Schedule::new();
    schedule.add_system(write_ping).expect("writer is valid");
    schedule.add_system(read_ping).expect("reader is valid");
    schedule.run(&mut world).expect("first run succeeds");
    schedule.run(&mut world).expect("second run succeeds");

    schedule
        .add_system(read_ping_again)
        .expect("late reader is valid");
    schedule.run(&mut world).expect("third run succeeds");

    assert!(
        world
            .resource::<SeenAgain>()
            .expect("seen again exists")
            .0
            .is_empty()
    );
}

#[test]
fn consumed_messages_are_dropped() {
    let mut world = World::new();
    world.insert_resource(Seen::default());

    let mut schedule = Schedule::new();
    schedule.add_system(write_ping).expect("writer is valid");
    schedule.add_system(read_ping).expect("reader is valid");

    schedule.run(&mut world).expect("first run succeeds");
    assert_eq!(world.published_message_len::<Ping>(), 1);

    schedule.run(&mut world).expect("second run succeeds");
    assert_eq!(world.published_message_len::<Ping>(), 1);
}

#[derive(Debug, PartialEq, Eq)]
struct Health(u32);

#[derive(Default)]
struct SpawnProbe {
    reserved: Option<Entity>,
    seen_during_system: usize,
}

fn spawn_with_probe(mut commands: Commands, query: Query<Entity>, mut probe: ResMut<SpawnProbe>) {
    let entity = commands.spawn(Health(10));
    probe.reserved = Some(entity);
    probe.seen_during_system = query.iter().count();
}

#[test]
fn commands_spawn_is_invisible_until_applied() {
    let mut world = World::new();
    world.insert_resource(SpawnProbe::default());

    let mut schedule = Schedule::new();
    schedule
        .add_system(spawn_with_probe)
        .expect("spawn system is valid");
    schedule.run(&mut world).expect("run succeeds");

    let probe = world.resource::<SpawnProbe>().expect("probe exists");
    let entity = probe.reserved.expect("entity was reserved");
    assert_eq!(probe.seen_during_system, 0);
    drop(probe);

    assert!(world.is_alive(entity));
    assert_eq!(world.get::<Health>(entity).expect("health exists").0, 10);
}

fn despawn_all(mut commands: Commands, query: Query<Entity>) {
    for entity in query.iter() {
        commands.entity(entity).despawn();
    }
}

#[test]
fn despawn_via_commands_cleans_components() {
    let mut world = World::new();
    let entity = world.spawn(Health(1));

    let mut schedule = Schedule::new();
    schedule.add_system(despawn_all).expect("despawn is valid");
    schedule.run(&mut world).expect("run succeeds");

    assert!(!world.is_alive(entity));
    assert!(world.get::<Health>(entity).is_none());
}

fn insert_and_remove(mut commands: Commands) {
    let entity = commands.spawn(Health(1));
    commands.entity(entity).insert(Ping(2)).remove::<Health>();
}

#[test]
fn commands_can_chain_insert_and_remove() {
    let mut world = World::new();

    let mut schedule = Schedule::new();
    schedule
        .add_system(insert_and_remove)
        .expect("commands are valid");
    schedule.run(&mut world).expect("run succeeds");

    let entity = world.alive_entities()[0];
    assert!(world.get::<Health>(entity).is_none());
    assert_eq!(world.get::<Ping>(entity).expect("ping exists").0, 2);
}

#[test]
fn spawn_bundle_attaches_all_components() {
    let mut world = World::new();
    let entity = world.spawn_bundle((Health(1), Ping(2)));

    assert_eq!(world.get::<Health>(entity).expect("health exists").0, 1);
    assert_eq!(world.get::<Ping>(entity).expect("ping exists").0, 2);
}

#[test]
fn spawn_bundle_empty_matches_spawn_empty() {
    let mut world = World::new();
    let entity = world.spawn_bundle(());

    assert!(world.is_alive(entity));
    assert!(world.get::<Health>(entity).is_none());
}

fn spawn_bundle_with_commands(mut commands: Commands, query: Query<&Health>) {
    let entity = commands.spawn_bundle((Health(1), Ping(2)));
    assert!(query.get(entity).is_none());
}

#[test]
fn commands_spawn_bundle_applies_all_components() {
    let mut world = World::new();

    let mut schedule = Schedule::new();
    schedule
        .add_system(spawn_bundle_with_commands)
        .expect("commands are valid");
    schedule.run(&mut world).expect("run succeeds");

    let entity = world.alive_entities()[0];
    assert_eq!(world.get::<Health>(entity).expect("health exists").0, 1);
    assert_eq!(world.get::<Ping>(entity).expect("ping exists").0, 2);
}

fn insert_bundle_with_commands(mut commands: Commands) {
    let entity = commands.spawn_empty();
    commands.entity(entity).insert_bundle((Health(1), Ping(2)));
}

#[test]
fn entity_commands_insert_bundle_applies_all_components() {
    let mut world = World::new();

    let mut schedule = Schedule::new();
    schedule
        .add_system(insert_bundle_with_commands)
        .expect("commands are valid");
    schedule.run(&mut world).expect("run succeeds");

    let entity = world.alive_entities()[0];
    assert_eq!(world.get::<Health>(entity).expect("health exists").0, 1);
    assert_eq!(world.get::<Ping>(entity).expect("ping exists").0, 2);
}

#[test]
fn inserting_twice_replaces_the_component() {
    let mut world = World::new();
    let entity = world.spawn(Health(1));
    world.insert(entity, Health(9));

    assert_eq!(world.get::<Health>(entity).expect("health exists").0, 9);
}

#[test]
fn removing_one_entity_keeps_others_intact() {
    let mut world = World::new();
    let first = world.spawn(Health(1));
    let second = world.spawn(Health(2));
    let third = world.spawn(Health(3));

    assert_eq!(world.remove::<Health>(second), Some(Health(2)));

    assert_eq!(world.get::<Health>(first).expect("first survives").0, 1);
    assert_eq!(world.get::<Health>(third).expect("third survives").0, 3);
    assert!(world.get::<Health>(second).is_none());
}

#[test]
fn a_respawned_slot_hides_the_old_generation() {
    let mut world = World::new();
    let first = world.spawn(Health(1));
    world.despawn(first);
    let second = world.spawn(Health(2));

    assert_eq!(first.index(), second.index());
    assert!(world.get::<Health>(first).is_none());
    assert_eq!(world.get::<Health>(second).expect("new health exists").0, 2);
}

fn stale_command(mut commands: Commands) {
    commands
        .entity(Entity::PLACEHOLDER)
        .insert(Health(1))
        .despawn();
}

#[test]
fn a_stale_entity_command_is_skipped() {
    let mut world = World::new();

    let mut schedule = Schedule::new();
    schedule
        .add_system(stale_command)
        .expect("commands are valid");
    schedule
        .run(&mut world)
        .expect("stale commands do not panic");
    assert_eq!(world.alive_len(), 0);
}

#[test]
fn mutable_items_on_different_entities_can_coexist() {
    let mut world = World::new();
    let first = world.spawn(Health(1));
    let second = world.spawn(Health(2));

    let mut query = Query::<&mut Health>::new(&world);
    let mut first_item = query.get_mut(first).expect("first exists");
    let mut second_item = query.get_mut(second).expect("second exists");
    first_item.0 = 3;
    second_item.0 = 4;
    drop(first_item);
    drop(second_item);

    assert_eq!(world.get::<Health>(first).expect("first").0, 3);
    assert_eq!(world.get::<Health>(second).expect("second").0, 4);
}

fn mutate_health_with_filter(
    mut health: Query<&mut Health>,
    _ping: Query<&Ping, crate::With<Health>>,
) {
    for mut item in health.iter_mut() {
        item.0 = 9;
    }
}

#[test]
fn a_with_filter_does_not_block_a_mutable_query() {
    let mut world = World::new();
    let entity = world.spawn(Health(1));
    world.insert(entity, Ping(1));

    let mut schedule = Schedule::new();
    schedule
        .add_system(mutate_health_with_filter)
        .expect("filters do not create read access");
    schedule.run(&mut world).expect("run succeeds");

    assert_eq!(world.get::<Health>(entity).expect("health").0, 9);
}

#[derive(Default)]
struct Counter(u32);

fn increment(mut counter: ResMut<Counter>) {
    counter.0 += 1;
}

struct CounterPlugin;

impl Plugin for CounterPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Counter>()
            .add_systems(Update, increment);
    }
}

#[test]
fn a_plugin_registers_resources_and_systems() {
    let mut app = App::new();
    app.add_plugin(CounterPlugin);
    app.update().expect("update succeeds");
    assert_eq!(app.world().resource::<Counter>().expect("counter").0, 1);
}

#[test]
#[should_panic(expected = "is already registered")]
fn a_duplicate_plugin_panics() {
    let mut app = App::new();
    app.add_plugin(CounterPlugin).add_plugin(CounterPlugin);
}

#[test]
fn a_schedule_access_conflict_is_reported_from_app() {
    let mut app = App::new();
    let error = app
        .try_add_systems(Update, conflicting_resources)
        .expect_err("conflicting system is rejected");
    assert!(matches!(error, ScheduleError::ConflictingAccess { .. }));
}

struct RunnerSeen {
    values: Vec<u32>,
    notify: async_channel::Sender<()>,
}

fn read_ping_notifying(mut reader: MessageReader<Ping>, mut seen: ResMut<RunnerSeen>) {
    for ping in reader.read() {
        seen.values.push(ping.0);
        let _ = seen.notify.try_send(());
    }
}

#[test]
fn runtime_events_drive_the_runner_until_shutdown() {
    let executor = Arc::new(Executor::new());
    let mut app = App::new();
    let (notify_tx, notify_rx) = async_channel::unbounded();
    app.insert_resource(RunnerSeen {
        values: Vec::new(),
        notify: notify_tx,
    });
    app.add_systems(Update, read_ping_notifying);

    let sender = app.runtime_sender();
    let shutdown = app.shutdown_handle();
    let runner = app.take_runner();

    let producer = executor.spawn(async move {
        sender.send_message(Ping(9)).expect("runner is alive");
        for _ in 0..1000 {
            if notify_rx.try_recv().is_ok() {
                break;
            }
            futures_lite::future::yield_now().await;
        }
        shutdown.cancel();
    });

    block_on(executor.run(async {
        runner.run(&mut app).await.expect("runner exits cleanly");
        producer.await;
    }));

    assert_eq!(
        app.world().resource::<RunnerSeen>().expect("seen").values,
        [9]
    );
}

#[test]
fn shutdown_exits_the_runner() {
    let app = App::new();
    app.shutdown_handle().cancel();

    block_on(async {
        app.run_async().await.expect("runner exits cleanly");
    });
}

#[test]
fn a_cancelled_token_resolves() {
    let token = CancelToken::new();
    assert!(!token.is_cancelled());
    token.cancel();
    assert!(token.is_cancelled());
    block_on(async {
        token.cancelled().await;
    });
}

#[derive(Debug)]
struct Boom(&'static str);

impl std::fmt::Display for Boom {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for Boom {}

fn failing_system(_config: Res<Config>) -> Result<(), Boom> {
    Err(Boom("boom"))
}

#[test]
fn a_fallible_system_propagates_its_error() {
    let mut world = World::new();
    world.insert_resource(Config { value: 1 });

    let mut schedule = Schedule::new();
    schedule
        .add_system(failing_system)
        .expect("fallible systems are supported");
    let error = schedule.run(&mut world).expect_err("the system fails");

    let SystemError::Failed { system, source } = error else {
        panic!("unexpected error: {error:?}");
    };
    assert!(system.ends_with("failing_system"));
    let boom = source
        .downcast_ref::<Boom>()
        .expect("the original error is preserved");
    assert_eq!(boom.0, "boom");
}

fn fallible_ok(_config: Res<Config>) -> Result<(), Boom> {
    Ok(())
}

#[test]
fn a_fallible_system_that_succeeds_runs_following_systems() {
    let mut world = World::new();
    world.insert_resource(Config { value: 1 });
    world.insert_resource(State::default());

    let mut schedule = Schedule::new();
    schedule
        .add_system(fallible_ok)
        .expect("fallible systems are supported");
    schedule
        .add_system(copy_config)
        .expect("copy_config has no conflicts");
    schedule.run(&mut world).expect("the schedule succeeds");

    assert_eq!(world.resource::<State>().expect("state exists").value, 1);
}

fn mark_first(mut state: ResMut<State>) -> Result<(), Boom> {
    state.value += 1;
    Ok(())
}

fn fail_middle(_state: Res<State>) -> Result<(), Boom> {
    Err(Boom("middle"))
}

fn mark_third(mut state: ResMut<State>) {
    state.value += 10;
}

#[test]
fn a_failing_system_stops_the_schedule() {
    let mut world = World::new();
    world.insert_resource(State::default());

    let mut schedule = Schedule::new();
    schedule
        .add_system(mark_first)
        .expect("mark_first is valid");
    schedule
        .add_system(fail_middle)
        .expect("fail_middle is valid");
    schedule
        .add_system(mark_third)
        .expect("mark_third is valid");
    let error = schedule
        .run(&mut world)
        .expect_err("the middle system fails");

    assert!(matches!(error, SystemError::Failed { .. }));
    assert_eq!(world.resource::<State>().expect("state exists").value, 1);
}

fn spawn_then_fail(mut commands: Commands) -> Result<(), Boom> {
    commands.spawn_empty();
    Err(Boom("after spawn"))
}

#[test]
fn a_failing_systems_commands_are_still_applied() {
    let mut world = World::new();

    let mut schedule = Schedule::new();
    schedule
        .add_system(spawn_then_fail)
        .expect("spawn_then_fail is valid");
    let error = schedule.run(&mut world).expect_err("the system fails");

    assert!(matches!(error, SystemError::Failed { .. }));
    assert_eq!(world.alive_len(), 1);
}

fn failing_app_system() -> Result<(), Boom> {
    Err(Boom("app"))
}

#[test]
fn an_app_update_propagates_a_system_error() {
    let mut app = App::new();
    app.add_systems(Update, failing_app_system);
    let error = app.update().expect_err("the system fails");

    assert!(matches!(
        error,
        SystemError::Failed { system, .. } if system.ends_with("failing_app_system")
    ));
}

#[test]
fn cancelling_wakes_a_registered_waiter() {
    let token = CancelToken::new();
    let executor = Arc::new(Executor::new());
    let waiter = executor.spawn({
        let token = token.clone();
        async move {
            token.cancelled().await;
        }
    });
    let canceller = executor.spawn({
        let token = token.clone();
        async move {
            futures_lite::future::yield_now().await;
            token.cancel();
        }
    });
    block_on(executor.run(async {
        waiter.await;
        canceller.await;
    }));
}
