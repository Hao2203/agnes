//! Cordis 教程 1–6 章的 Rust 等价测试：异步生命周期 + 类型级依赖 + 独立服务。

use std::any::TypeId;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures_lite::future::block_on;

use crate::event::next;
use crate::plugin::{Adapter, ApplyError, ErasedPlugin};
use crate::{
    BailKey, Ctx, EffectGuard, EventKey, Events, FiberInfo, FiberState, Listener, Mounts, Plugin,
    PluginError, ServiceDep, ServiceError, ServiceHost, ServiceParam, Services, WaterfallKey,
    async_trait,
};

type Log = Arc<Mutex<Vec<String>>>;

fn log() -> Log {
    Arc::new(Mutex::new(Vec::new()))
}

fn entries(log: &Log) -> Vec<String> {
    log.lock().unwrap().clone()
}

fn named(host: &ServiceHost, name: &str) -> FiberInfo {
    host.fibers()
        .into_iter()
        .find(|info| info.name == name)
        .unwrap()
}

struct TestGuard {
    cleanup: Option<Box<dyn FnOnce() + Send>>,
}

impl TestGuard {
    fn new(cleanup: impl FnOnce() + Send + 'static) -> Self {
        Self {
            cleanup: Some(Box::new(cleanup)),
        }
    }

    fn run(&mut self) {
        if let Some(cleanup) = self.cleanup.take() {
            cleanup();
        }
    }
}

#[async_trait]
impl EffectGuard for TestGuard {
    async fn close(&mut self) {
        self.run();
    }
}

impl Drop for TestGuard {
    fn drop(&mut self) {
        self.run();
    }
}

struct Hello {
    log: Log,
}

#[async_trait]
impl Plugin for Hello {
    type Deps = ();

    async fn apply(&mut self, _ctx: &Ctx<'_>, _deps: ()) -> Result<(), PluginError> {
        self.log.lock().unwrap().push("hello".into());
        Ok(())
    }
}

#[test]
fn first_plugin_runs_apply() {
    block_on(async {
        let log = log();
        let host = ServiceHost::new();
        let fiber = host
            .mount(Hello {
                log: Arc::clone(&log),
            })
            .await;

        assert_eq!(entries(&log), ["hello"]);
        assert_eq!(host.fiber(fiber).unwrap().state, FiberState::Active);
        host.shutdown().await;
    });
}

struct SlowApply {
    log: Log,
}

#[async_trait]
impl Plugin for SlowApply {
    type Deps = ();

    async fn apply(&mut self, ctx: &Ctx<'_>, _deps: ()) -> Result<(), PluginError> {
        let log = Arc::clone(&self.log);
        ctx.effect(async move {
            (
                (),
                TestGuard::new(move || {
                    log.lock().unwrap().push("cancelled-apply-cleanup".into());
                }),
            )
        })
        .await;
        futures_lite::future::pending::<()>().await;
        Ok(())
    }
}

#[test]
fn cancelling_apply_restores_pending_and_unwinds_effects() {
    block_on(async {
        let log = log();
        let host = ServiceHost::new();
        let outcome = futures_lite::future::poll_once(Box::pin(host.mount(SlowApply {
            log: Arc::clone(&log),
        })))
        .await;
        assert!(outcome.is_none());
        assert_eq!(entries(&log), ["cancelled-apply-cleanup"]);

        let fiber = host.fibers()[0].fiber;
        assert_eq!(host.fiber(fiber).unwrap().state, FiberState::Pending);
        assert!(host.unmount(fiber).await);
        assert_eq!(host.fiber(fiber).unwrap().state, FiberState::Disposed);
        host.shutdown().await;
    });
}

struct SlowParent {
    log: Log,
}

#[async_trait]
impl Plugin for SlowParent {
    type Deps = Arc<Mounts>;

    fn name(&self) -> &str {
        "slow-parent"
    }

    async fn apply(&mut self, ctx: &Ctx<'_>, mounts: Arc<Mounts>) -> Result<(), PluginError> {
        mounts
            .mount(
                ctx,
                Child {
                    log: Arc::clone(&self.log),
                },
            )
            .await;
        futures_lite::future::pending::<()>().await;
        Ok(())
    }
}

#[test]
fn cancelling_parent_disposes_mounted_child() {
    block_on(async {
        let host = ServiceHost::new();
        let outcome =
            futures_lite::future::poll_once(Box::pin(host.mount(SlowParent { log: log() }))).await;
        assert!(outcome.is_none());
        assert_eq!(named(&host, "slow-parent").state, FiberState::Pending);
        assert_eq!(named(&host, "child").state, FiberState::Disposed);
        host.shutdown().await;
    });
}

struct Failing;

#[async_trait]
impl Plugin for Failing {
    type Deps = ();

    async fn apply(&mut self, _ctx: &Ctx<'_>, _deps: ()) -> Result<(), PluginError> {
        Err("boom".into())
    }
}

#[test]
fn apply_failure_marks_failed() {
    block_on(async {
        let host = ServiceHost::new();
        let fiber = host.mount(Failing).await;
        let info = host.fiber(fiber).unwrap();

        assert_eq!(info.state, FiberState::Failed);
        assert_eq!(
            info.error.as_ref().map(|error| error.to_string()),
            Some("boom".to_string())
        );

        assert!(host.unmount(fiber).await);
        assert_eq!(host.fiber(fiber).unwrap().state, FiberState::Disposed);
        assert!(!host.unmount(fiber).await);
        host.shutdown().await;
    });
}

struct Effects {
    log: Log,
}

#[async_trait]
impl Plugin for Effects {
    type Deps = ();

    async fn apply(&mut self, ctx: &Ctx<'_>, _deps: ()) -> Result<(), PluginError> {
        for label in ["first", "second"] {
            let log = Arc::clone(&self.log);
            ctx.effect(async move {
                log.lock().unwrap().push(label.to_string());
                let cleanup = Arc::clone(&log);
                (
                    (),
                    TestGuard::new(move || {
                        cleanup.lock().unwrap().push(format!("{label}-cleanup"));
                    }),
                )
            })
            .await;
        }
        Ok(())
    }
}

#[test]
fn effects_unwind_in_reverse() {
    block_on(async {
        let log = log();
        let host = ServiceHost::new();
        let fiber = host
            .mount(Effects {
                log: Arc::clone(&log),
            })
            .await;

        assert!(host.unmount(fiber).await);
        assert!(!host.unmount(fiber).await);
        assert_eq!(
            entries(&log),
            ["first", "second", "second-cleanup", "first-cleanup"]
        );
        assert_eq!(host.fiber(fiber).unwrap().state, FiberState::Disposed);
        host.shutdown().await;
    });
}

#[test]
fn dropping_the_host_unwinds_effects() {
    let log = log();
    {
        let host = ServiceHost::new();
        block_on(async {
            host.mount(Effects {
                log: Arc::clone(&log),
            })
            .await;
        });
    }

    assert_eq!(
        entries(&log),
        ["first", "second", "second-cleanup", "first-cleanup"]
    );
}

struct Child {
    log: Log,
}

#[async_trait]
impl Plugin for Child {
    type Deps = ();

    fn name(&self) -> &str {
        "child"
    }

    async fn apply(&mut self, ctx: &Ctx<'_>, _deps: ()) -> Result<(), PluginError> {
        self.log.lock().unwrap().push("child".into());
        let log = Arc::clone(&self.log);
        ctx.effect(async move {
            (
                (),
                TestGuard::new(move || {
                    log.lock().unwrap().push("child-cleanup".into());
                }),
            )
        })
        .await;
        Ok(())
    }
}

struct Parent {
    log: Log,
}

#[async_trait]
impl Plugin for Parent {
    type Deps = Arc<Mounts>;

    fn name(&self) -> &str {
        "parent"
    }

    async fn apply(&mut self, ctx: &Ctx<'_>, mounts: Arc<Mounts>) -> Result<(), PluginError> {
        self.log.lock().unwrap().push("parent".into());
        mounts
            .mount(
                ctx,
                Child {
                    log: Arc::clone(&self.log),
                },
            )
            .await;
        let log = Arc::clone(&self.log);
        ctx.effect(async move {
            (
                (),
                TestGuard::new(move || {
                    log.lock().unwrap().push("parent-cleanup".into());
                }),
            )
        })
        .await;
        Ok(())
    }
}

#[test]
fn child_plugins_dispose_with_parent() {
    block_on(async {
        let log = log();
        let host = ServiceHost::new();
        let parent = host
            .mount(Parent {
                log: Arc::clone(&log),
            })
            .await;

        assert_eq!(named(&host, "child").state, FiberState::Active);
        assert!(host.unmount(parent).await);
        assert_eq!(named(&host, "child").state, FiberState::Disposed);
        assert_eq!(
            entries(&log),
            ["parent", "child", "child-cleanup", "parent-cleanup"]
        );
        host.shutdown().await;
    });
}

trait Greeter: Send + Sync {
    fn greet(&self) -> &'static str;
}

struct English;

impl Greeter for English {
    fn greet(&self) -> &'static str {
        "hello"
    }
}

struct GreeterService {
    greeting: &'static str,
}

impl GreeterService {
    fn greet(&self) -> &'static str {
        self.greeting
    }
}

struct ProvideKeys;

#[async_trait]
impl Plugin for ProvideKeys {
    type Deps = Arc<Services>;

    async fn apply(&mut self, ctx: &Ctx<'_>, services: Arc<Services>) -> Result<(), PluginError> {
        let english = Arc::new(English);
        services.provide(ctx, Arc::clone(&english)).await?;
        services.provide::<dyn Greeter>(ctx, english).await?;
        Ok(())
    }
}

#[test]
fn services_are_keyed_by_type_id() {
    block_on(async {
        let host = ServiceHost::new();
        host.mount(ProvideKeys).await;

        assert_eq!(host.resolve::<English>().unwrap().greet(), "hello");
        assert_eq!(host.resolve::<dyn Greeter>().unwrap().greet(), "hello");

        let names: Vec<&str> = host
            .services()
            .iter()
            .map(|service| service.type_name)
            .collect();
        assert!(names.iter().any(|name| name.contains("English")));
        assert!(names.iter().any(|name| name.contains("Greeter")));
        host.shutdown().await;
    });
}

struct Provider;

#[async_trait]
impl Plugin for Provider {
    type Deps = Arc<Services>;

    async fn apply(&mut self, ctx: &Ctx<'_>, services: Arc<Services>) -> Result<(), PluginError> {
        services
            .provide(ctx, Arc::new(GreeterService { greeting: "hello" }))
            .await?;
        Ok(())
    }
}

#[test]
fn duplicate_service_fails() {
    block_on(async {
        let host = ServiceHost::new();
        host.mount(Provider).await;
        let second = host.mount(Provider).await;
        let info = host.fiber(second).unwrap();

        assert_eq!(info.state, FiberState::Failed);
        assert!(matches!(
            info.error
                .as_ref()
                .and_then(|error| error.downcast_ref::<ServiceError>()),
            Some(ServiceError::ServiceTaken { .. })
        ));
        assert!(host.resolve::<GreeterService>().is_some());
        host.shutdown().await;
    });
}

struct Consumer {
    log: Log,
    applies: Arc<AtomicUsize>,
}

#[async_trait]
impl Plugin for Consumer {
    type Deps = Arc<GreeterService>;

    fn name(&self) -> &str {
        "consumer"
    }

    async fn apply(
        &mut self,
        _ctx: &Ctx<'_>,
        service: Arc<GreeterService>,
    ) -> Result<(), PluginError> {
        self.applies.fetch_add(1, Ordering::SeqCst);
        self.log.lock().unwrap().push(service.greet().to_string());
        Ok(())
    }
}

#[test]
fn inject_order_does_not_matter() {
    block_on(async {
        let first_log = log();
        let host = ServiceHost::new();
        let consumer = host
            .mount(Consumer {
                log: Arc::clone(&first_log),
                applies: Arc::new(AtomicUsize::new(0)),
            })
            .await;
        assert_eq!(host.fiber(consumer).unwrap().state, FiberState::Pending);

        host.mount(Provider).await;
        assert_eq!(host.fiber(consumer).unwrap().state, FiberState::Active);
        assert_eq!(entries(&first_log), ["hello"]);

        let second_log = log();
        let host = ServiceHost::new();
        host.mount(Provider).await;
        let consumer = host
            .mount(Consumer {
                log: Arc::clone(&second_log),
                applies: Arc::new(AtomicUsize::new(0)),
            })
            .await;
        assert_eq!(host.fiber(consumer).unwrap().state, FiberState::Active);
        assert_eq!(entries(&second_log), ["hello"]);
    });
}

#[test]
fn missing_service_stays_pending() {
    block_on(async {
        let host = ServiceHost::new();
        let consumer = host
            .mount(Consumer {
                log: log(),
                applies: Arc::new(AtomicUsize::new(0)),
            })
            .await;
        let info = host.fiber(consumer).unwrap();

        assert_eq!(info.state, FiberState::Pending);
        assert_eq!(info.missing.len(), 1);
        assert!(info.missing[0].name().contains("GreeterService"));
        host.shutdown().await;
    });
}

#[test]
fn provider_loss_pends_and_reapplies_same_instance() {
    block_on(async {
        let log = log();
        let applies = Arc::new(AtomicUsize::new(0));
        let host = ServiceHost::new();
        let consumer = host
            .mount(Consumer {
                log: Arc::clone(&log),
                applies: Arc::clone(&applies),
            })
            .await;
        let provider = host.mount(Provider).await;
        assert_eq!(host.fiber(consumer).unwrap().state, FiberState::Active);
        assert_eq!(applies.load(Ordering::SeqCst), 1);

        assert!(host.unmount(provider).await);
        assert_eq!(host.fiber(consumer).unwrap().state, FiberState::Pending);
        assert_eq!(applies.load(Ordering::SeqCst), 1);
        assert!(
            !host
                .services()
                .iter()
                .any(|service| service.type_name.contains("GreeterService"))
        );

        host.mount(Provider).await;
        assert_eq!(host.fiber(consumer).unwrap().state, FiberState::Active);
        assert_eq!(applies.load(Ordering::SeqCst), 2);
        assert_eq!(entries(&log), ["hello", "hello"]);
        host.shutdown().await;
    });
}

struct ShutdownPlugin {
    applies: Arc<AtomicUsize>,
    shutdowns: Arc<AtomicUsize>,
}

#[async_trait]
impl Plugin for ShutdownPlugin {
    type Deps = ();

    async fn apply(&mut self, _ctx: &Ctx<'_>, _deps: ()) -> Result<(), PluginError> {
        self.applies.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn shutdown(&mut self) -> Result<(), PluginError> {
        self.shutdowns.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[test]
fn shutdown_is_awaited_on_unmount() {
    block_on(async {
        let applies = Arc::new(AtomicUsize::new(0));
        let shutdowns = Arc::new(AtomicUsize::new(0));
        let host = ServiceHost::new();
        let fiber = host
            .mount(ShutdownPlugin {
                applies: Arc::clone(&applies),
                shutdowns: Arc::clone(&shutdowns),
            })
            .await;

        assert_eq!(applies.load(Ordering::SeqCst), 1);
        assert!(host.unmount(fiber).await);
        assert_eq!(shutdowns.load(Ordering::SeqCst), 1);

        host.shutdown().await;
        assert_eq!(shutdowns.load(Ordering::SeqCst), 1);
    });
}

struct FailingWithShutdown {
    shutdowns: Arc<AtomicUsize>,
}

#[async_trait]
impl Plugin for FailingWithShutdown {
    type Deps = ();

    async fn apply(&mut self, _ctx: &Ctx<'_>, _deps: ()) -> Result<(), PluginError> {
        Err("boom".into())
    }

    async fn shutdown(&mut self) -> Result<(), PluginError> {
        self.shutdowns.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[test]
fn shutdown_is_awaited_on_apply_failure() {
    block_on(async {
        let shutdowns = Arc::new(AtomicUsize::new(0));
        let host = ServiceHost::new();
        let fiber = host
            .mount(FailingWithShutdown {
                shutdowns: Arc::clone(&shutdowns),
            })
            .await;

        assert_eq!(host.fiber(fiber).unwrap().state, FiberState::Failed);
        assert_eq!(shutdowns.load(Ordering::SeqCst), 1);
        host.shutdown().await;
    });
}

struct ShutdownConsumer {
    applies: Arc<AtomicUsize>,
    shutdowns: Arc<AtomicUsize>,
}

#[async_trait]
impl Plugin for ShutdownConsumer {
    type Deps = Arc<GreeterService>;

    async fn apply(
        &mut self,
        _ctx: &Ctx<'_>,
        _service: Arc<GreeterService>,
    ) -> Result<(), PluginError> {
        self.applies.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn shutdown(&mut self) -> Result<(), PluginError> {
        self.shutdowns.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[test]
fn dependency_loss_shuts_down_and_reapplies_same_instance() {
    block_on(async {
        let applies = Arc::new(AtomicUsize::new(0));
        let shutdowns = Arc::new(AtomicUsize::new(0));
        let host = ServiceHost::new();
        let consumer = host
            .mount(ShutdownConsumer {
                applies: Arc::clone(&applies),
                shutdowns: Arc::clone(&shutdowns),
            })
            .await;
        assert_eq!(host.fiber(consumer).unwrap().state, FiberState::Pending);

        let provider = host.mount(Provider).await;
        assert_eq!(host.fiber(consumer).unwrap().state, FiberState::Active);
        assert_eq!(applies.load(Ordering::SeqCst), 1);
        assert_eq!(shutdowns.load(Ordering::SeqCst), 0);

        assert!(host.unmount(provider).await);
        assert_eq!(host.fiber(consumer).unwrap().state, FiberState::Pending);
        assert_eq!(shutdowns.load(Ordering::SeqCst), 1);

        host.mount(Provider).await;
        assert_eq!(host.fiber(consumer).unwrap().state, FiberState::Active);
        assert_eq!(applies.load(Ordering::SeqCst), 2);
        assert_eq!(shutdowns.load(Ordering::SeqCst), 1);

        assert!(host.unmount(consumer).await);
        assert_eq!(shutdowns.load(Ordering::SeqCst), 2);
        host.shutdown().await;
    });
}

struct NeedsMissing;

#[async_trait]
impl Plugin for NeedsMissing {
    type Deps = Arc<GreeterService>;

    async fn apply(
        &mut self,
        _ctx: &Ctx<'_>,
        _service: Arc<GreeterService>,
    ) -> Result<(), PluginError> {
        Ok(())
    }
}

#[test]
fn missing_dependency_resolve_reports_missing_not_plugin_error() {
    block_on(async {
        let host = ServiceHost::new();
        let ctx = host.ctx();
        let mut adapter = Adapter::new(NeedsMissing);
        let result = ErasedPlugin::apply(&mut adapter, &ctx).await;
        assert!(matches!(result, Err(ApplyError::MissingDeps)));
    });
}

struct Optional {
    log: Log,
}

#[async_trait]
impl Plugin for Optional {
    type Deps = Option<Arc<GreeterService>>;

    async fn apply(
        &mut self,
        _ctx: &Ctx<'_>,
        service: Option<Arc<GreeterService>>,
    ) -> Result<(), PluginError> {
        assert!(service.is_none());
        self.log.lock().unwrap().push("optional".into());
        Ok(())
    }
}

#[test]
fn optional_dependency_resolves_to_none() {
    block_on(async {
        let log = log();
        let host = ServiceHost::new();
        let fiber = host
            .mount(Optional {
                log: Arc::clone(&log),
            })
            .await;

        assert_eq!(host.fiber(fiber).unwrap().state, FiberState::Active);
        assert_eq!(entries(&log), ["optional"]);
        host.shutdown().await;
    });
}

struct ProvideBoth;

#[async_trait]
impl Plugin for ProvideBoth {
    type Deps = Arc<Services>;

    async fn apply(&mut self, ctx: &Ctx<'_>, services: Arc<Services>) -> Result<(), PluginError> {
        services
            .provide(ctx, Arc::new(GreeterService { greeting: "hello" }))
            .await?;
        services.provide(ctx, Arc::new(English)).await?;
        Ok(())
    }
}

struct Pair {
    log: Log,
}

#[async_trait]
impl Plugin for Pair {
    type Deps = (Arc<GreeterService>, Option<Arc<English>>);

    async fn apply(
        &mut self,
        _ctx: &Ctx<'_>,
        (service, english): (Arc<GreeterService>, Option<Arc<English>>),
    ) -> Result<(), PluginError> {
        self.log
            .lock()
            .unwrap()
            .push(format!("{}:{}", service.greet(), english.is_some()));
        Ok(())
    }
}

#[test]
fn tuple_dependencies_resolve() {
    block_on(async {
        let log = log();
        let host = ServiceHost::new();
        host.mount(ProvideBoth).await;
        let pair = host
            .mount(Pair {
                log: Arc::clone(&log),
            })
            .await;

        assert_eq!(host.fiber(pair).unwrap().state, FiberState::Active);
        assert_eq!(entries(&log), ["hello:true"]);
        host.shutdown().await;
    });
}

#[test]
fn dependencies_are_computed_from_the_deps_type() {
    let deps = <(Arc<GreeterService>, Option<Arc<English>>) as ServiceParam>::deps();
    assert_eq!(deps.len(), 1);
    assert_eq!(deps[0].id(), TypeId::of::<GreeterService>());
}

const PING: EventKey<u32> = EventKey::new("ping");

struct Emitter {
    log: Log,
}

#[async_trait]
impl Plugin for Emitter {
    type Deps = Arc<Events>;

    async fn apply(&mut self, ctx: &Ctx<'_>, events: Arc<Events>) -> Result<(), PluginError> {
        let log = Arc::clone(&self.log);
        events
            .on(ctx, PING, move |value: &u32| {
                log.lock().unwrap().push(format!("a:{value}"));
            })
            .await;
        let log = Arc::clone(&self.log);
        events
            .on(ctx, PING, move |value: &u32| {
                log.lock().unwrap().push(format!("b:{value}"));
            })
            .await;
        Ok(())
    }
}

#[test]
fn emit_observes_in_registration_order() {
    block_on(async {
        let log = log();
        let host = ServiceHost::new();
        let fiber = host
            .mount(Emitter {
                log: Arc::clone(&log),
            })
            .await;
        let events = host.resolve::<Events>().unwrap();

        events.emit(PING, &7);
        assert_eq!(entries(&log), ["a:7", "b:7"]);

        assert!(host.unmount(fiber).await);
        events.emit(PING, &8);
        assert_eq!(entries(&log), ["a:7", "b:7"]);
        host.shutdown().await;
    });
}

struct Prepender {
    log: Log,
}

#[async_trait]
impl Plugin for Prepender {
    type Deps = Arc<Events>;

    async fn apply(&mut self, ctx: &Ctx<'_>, events: Arc<Events>) -> Result<(), PluginError> {
        let log = Arc::clone(&self.log);
        events
            .prepend(ctx, PING, move |value: &u32| {
                log.lock().unwrap().push(format!("pre:{value}"));
            })
            .await;
        Ok(())
    }
}

#[test]
fn prepend_runs_before_existing_listeners() {
    block_on(async {
        let log = log();
        let host = ServiceHost::new();
        host.mount(Emitter {
            log: Arc::clone(&log),
        })
        .await;
        host.mount(Prepender {
            log: Arc::clone(&log),
        })
        .await;
        let events = host.resolve::<Events>().unwrap();

        events.emit(PING, &1);
        assert_eq!(entries(&log), ["pre:1", "a:1", "b:1"]);
        host.shutdown().await;
    });
}

struct Removable {
    token: Arc<Mutex<Option<Listener>>>,
    log: Log,
}

#[async_trait]
impl Plugin for Removable {
    type Deps = Arc<Events>;

    async fn apply(&mut self, ctx: &Ctx<'_>, events: Arc<Events>) -> Result<(), PluginError> {
        let log = Arc::clone(&self.log);
        let token = events
            .on(ctx, PING, move |value: &u32| {
                log.lock().unwrap().push(format!("removable:{value}"));
            })
            .await;
        *self.token.lock().unwrap() = Some(token);
        Ok(())
    }
}

#[test]
fn off_removes_a_listener() {
    block_on(async {
        let log = log();
        let token = Arc::new(Mutex::new(None));
        let host = ServiceHost::new();
        host.mount(Removable {
            token: Arc::clone(&token),
            log: Arc::clone(&log),
        })
        .await;
        let events = host.resolve::<Events>().unwrap();

        events.emit(PING, &1);
        let listener = token.lock().unwrap().unwrap();
        assert!(events.off(listener));
        events.emit(PING, &2);
        assert_eq!(entries(&log), ["removable:1"]);
        host.shutdown().await;
    });
}

const DECIDE: BailKey<i32, &'static str> = BailKey::new("decide");

struct Decider {
    log: Log,
}

#[async_trait]
impl Plugin for Decider {
    type Deps = Arc<Events>;

    async fn apply(&mut self, ctx: &Ctx<'_>, events: Arc<Events>) -> Result<(), PluginError> {
        let log = Arc::clone(&self.log);
        events
            .on_bail(ctx, DECIDE, move |value: &i32| {
                log.lock().unwrap().push(format!("first:{value}"));
                None
            })
            .await;
        let log = Arc::clone(&self.log);
        events
            .on_bail(ctx, DECIDE, move |value: &i32| {
                log.lock().unwrap().push(format!("second:{value}"));
                Some("second")
            })
            .await;
        let log = Arc::clone(&self.log);
        events
            .on_bail(ctx, DECIDE, move |value: &i32| {
                log.lock().unwrap().push(format!("third:{value}"));
                Some("third")
            })
            .await;
        Ok(())
    }
}

#[test]
fn bail_first_answer_wins() {
    block_on(async {
        let log = log();
        let host = ServiceHost::new();
        host.mount(Decider {
            log: Arc::clone(&log),
        })
        .await;
        let events = host.resolve::<Events>().unwrap();

        assert_eq!(events.bail(DECIDE, &1), Some("second"));
        assert_eq!(entries(&log), ["first:1", "second:1"]);
        host.shutdown().await;
    });
}

const TRANSFORM: WaterfallKey<String, String> = WaterfallKey::new("transform");

struct Wrapper;

#[async_trait]
impl Plugin for Wrapper {
    type Deps = Arc<Events>;

    async fn apply(&mut self, ctx: &Ctx<'_>, events: Arc<Events>) -> Result<(), PluginError> {
        events
            .on_waterfall(ctx, TRANSFORM, |_input: &String, continuation| {
                next(async move { continuation.await.to_uppercase() })
            })
            .await;
        events
            .on_waterfall(ctx, TRANSFORM, |input: &String, continuation| {
                next(async move {
                    if input.contains("blocked") {
                        "** blocked **".to_string()
                    } else {
                        continuation.await
                    }
                })
            })
            .await;
        Ok(())
    }
}

#[test]
fn waterfall_wraps_and_short_circuits() {
    block_on(async {
        let host = ServiceHost::new();
        host.mount(Wrapper).await;
        let events = host.resolve::<Events>().unwrap();

        let hello = "hello".to_string();
        assert_eq!(
            events
                .waterfall(TRANSFORM, &hello, || "hello".to_string())
                .await,
            "HELLO"
        );

        let blocked = "blocked words".to_string();
        assert_eq!(
            events
                .waterfall(TRANSFORM, &blocked, || "blocked words".to_string())
                .await,
            "** BLOCKED **"
        );
        host.shutdown().await;
    });
}

const INNER: EventKey<u32> = EventKey::new("inner");

struct Reentrant {
    log: Log,
}

#[async_trait]
impl Plugin for Reentrant {
    type Deps = Arc<Events>;

    async fn apply(&mut self, ctx: &Ctx<'_>, events: Arc<Events>) -> Result<(), PluginError> {
        let log = Arc::clone(&self.log);
        events
            .on(ctx, INNER, move |value: &u32| {
                log.lock().unwrap().push(format!("inner:{value}"));
            })
            .await;
        let log = Arc::clone(&self.log);
        let inner = Arc::clone(&events);
        events
            .on_waterfall(ctx, TRANSFORM, move |_input: &String, continuation| {
                let log = Arc::clone(&log);
                let events = Arc::clone(&inner);
                next(async move {
                    events.emit(INNER, &9);
                    log.lock().unwrap().push("outer".into());
                    continuation.await
                })
            })
            .await;
        Ok(())
    }
}

#[test]
fn waterfall_listener_can_emit_reentrantly() {
    block_on(async {
        let log = log();
        let host = ServiceHost::new();
        host.mount(Reentrant {
            log: Arc::clone(&log),
        })
        .await;
        let events = host.resolve::<Events>().unwrap();

        let value = "x".to_string();
        assert_eq!(
            events
                .waterfall(TRANSFORM, &value, || "x".to_string())
                .await,
            "x"
        );
        assert_eq!(entries(&log), ["inner:9", "outer"]);
        host.shutdown().await;
    });
}

#[test]
fn shutdown_is_idempotent_and_disposes_all() {
    block_on(async {
        let host = ServiceHost::new();
        let fiber = host.mount(Hello { log: log() }).await;

        host.shutdown().await;
        assert_eq!(host.fiber(fiber).unwrap().state, FiberState::Disposed);
        host.shutdown().await;

        let after = host.mount(Hello { log: log() }).await;
        assert_eq!(host.fiber(after).unwrap().state, FiberState::Disposed);
    });
}

#[test]
fn tokens_do_not_cross_hosts() {
    block_on(async {
        let a = ServiceHost::new();
        let b = ServiceHost::new();
        let fiber = a.mount(Hello { log: log() }).await;

        assert!(b.fiber(fiber).is_none());
        assert!(!b.unmount(fiber).await);
        assert_eq!(a.fiber(fiber).unwrap().state, FiberState::Active);
        a.shutdown().await;
        b.shutdown().await;
    });
}

struct FailAfterEffect {
    log: Log,
}

#[async_trait]
impl Plugin for FailAfterEffect {
    type Deps = ();

    async fn apply(&mut self, ctx: &Ctx<'_>, _deps: ()) -> Result<(), PluginError> {
        let log = Arc::clone(&self.log);
        ctx.effect(async move {
            (
                (),
                TestGuard::new(move || {
                    log.lock().unwrap().push("cleanup".into());
                }),
            )
        })
        .await;
        Err("boom".into())
    }
}

#[test]
fn a_failed_apply_unwinds_its_effects() {
    block_on(async {
        let log = log();
        let host = ServiceHost::new();
        let fiber = host
            .mount(FailAfterEffect {
                log: Arc::clone(&log),
            })
            .await;

        assert_eq!(host.fiber(fiber).unwrap().state, FiberState::Failed);
        assert_eq!(entries(&log), ["cleanup"]);
        host.shutdown().await;
    });
}

#[test]
fn service_dep_is_type_id_based() {
    let dep = ServiceDep::of::<GreeterService>();
    assert_eq!(dep.id(), TypeId::of::<GreeterService>());
}

#[test]
fn host_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<ServiceHost>();
}
