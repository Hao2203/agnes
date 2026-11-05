//! Async task execution as ECS plugins.
//!
//! Each supported runtime is one plugin, and each installs the shared
//! [`AsyncExecutor`] resource:
//!
//! - [`TokioPlugin`] (feature `tokio`, enabled by default) spawns on the Tokio
//!   runtime that is active when a task is submitted;
//! - [`ExecutorPlugin`] (feature `async-executor`) spawns on a shared
//!   `async_executor::Executor` that the host drives itself.
//!
//! Systems that submit async work take `Res<AsyncExecutor>`; without either
//! plugin that parameter fails loudly when the schedule prepares, so a missing
//! capability never turns into a silently dropped task. The runner itself
//! lives in `agnes-ecs` and only needs a channel and a cancellation token, so
//! it runs under any executor.

#[cfg(feature = "async-executor")]
use std::sync::Arc;

use agnes_ecs::{App, Plugin};

mod executor;

pub use executor::{AsyncExecutor, SpawnError};

#[cfg(not(any(feature = "tokio", feature = "async-executor")))]
compile_error!("enable the `tokio` or `async-executor` feature of `agnes-plugin-async`");

/// Spawns tasks on the Tokio runtime that is active when the task is
/// submitted.
#[cfg(feature = "tokio")]
pub struct TokioPlugin;

#[cfg(feature = "tokio")]
impl Plugin for TokioPlugin {
    fn name(&self) -> &'static str {
        "agnes-async-tokio"
    }

    fn build(&self, app: &mut App) {
        app.insert_resource(AsyncExecutor::new(
            app.runtime_sender(),
            app.shutdown_handle(),
            executor::Runtime::Tokio,
        ));
    }
}

/// Spawns tasks on a shared `async-executor` executor.
///
/// The host is responsible for driving the executor, typically with
/// `futures_lite::future::block_on(executor.run(...))`.
#[cfg(feature = "async-executor")]
pub struct ExecutorPlugin {
    executor: Arc<async_executor::Executor<'static>>,
}

#[cfg(feature = "async-executor")]
impl ExecutorPlugin {
    /// Creates the plugin for the given executor.
    pub fn new(executor: Arc<async_executor::Executor<'static>>) -> Self {
        Self { executor }
    }
}

#[cfg(feature = "async-executor")]
impl Plugin for ExecutorPlugin {
    fn name(&self) -> &'static str {
        "agnes-async-executor"
    }

    fn build(&self, app: &mut App) {
        app.insert_resource(AsyncExecutor::new(
            app.runtime_sender(),
            app.shutdown_handle(),
            executor::Runtime::Executor(Arc::clone(&self.executor)),
        ));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use agnes_ecs::App;

    use super::AsyncExecutor;
    #[cfg(feature = "tokio")]
    use super::TokioPlugin;

    #[test]
    fn a_bare_app_has_no_executor() {
        let app = App::new();
        assert!(
            app.world().resource::<AsyncExecutor>().is_none(),
            "task execution is opt-in"
        );
    }

    fn executor(app: &App) -> AsyncExecutor {
        let executor = app
            .world()
            .resource::<AsyncExecutor>()
            .expect("executor resource");
        (*executor).clone()
    }

    #[cfg(feature = "tokio")]
    #[test]
    fn tokio_plugin_installs_the_executor_and_spawns() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime builds");
        let mut app = App::new();
        app.add_plugin(TokioPlugin);

        let executor = executor(&app);
        let flag = Arc::new(AtomicBool::new(false));
        let task_flag = flag.clone();
        runtime.block_on(async move {
            executor
                .spawn(async move { task_flag.store(true, Ordering::SeqCst) })
                .expect("tokio runtime is active");
            for _ in 0..100 {
                if flag.load(Ordering::SeqCst) {
                    return;
                }
                tokio::task::yield_now().await;
            }
            panic!("spawned task did not run");
        });
    }

    #[cfg(feature = "tokio")]
    #[test]
    fn spawning_outside_a_runtime_reports_an_error() {
        let mut app = App::new();
        app.add_plugin(TokioPlugin);
        assert!(executor(&app).spawn(async {}).is_err());
    }

    #[cfg(feature = "async-executor")]
    #[test]
    fn executor_plugin_installs_the_executor_and_spawns() {
        use futures_lite::future::block_on;

        let handle = Arc::new(async_executor::Executor::new());
        let mut app = App::new();
        app.add_plugin(super::ExecutorPlugin::new(handle.clone()));

        let executor = executor(&app);
        let flag = Arc::new(AtomicBool::new(false));
        let task_flag = flag.clone();
        executor
            .spawn(async move { task_flag.store(true, Ordering::SeqCst) })
            .expect("executor accepts the task");

        let loop_flag = flag.clone();
        block_on(handle.run(async move {
            while !loop_flag.load(Ordering::SeqCst) {
                futures_lite::future::yield_now().await;
            }
        }));
        assert!(flag.load(Ordering::SeqCst));
    }
}
