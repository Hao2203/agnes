use crate::commands::Commands;
use crate::entity::Entity;
use crate::message::{MessageReader, MessageWriter};
use crate::query::Query;
use crate::resource::{Res, ResMut};
use crate::runner::{AsyncExecutor, CancelToken};
use futures_lite::future::race;

use super::{
    AgentRun, ApprovalLog, ApprovalRequested, ApprovalResolved, CancelRun, ModelCompleted,
    ModelRegistry, PermissionPolicy, ToolApproved, ToolCompleted, ToolRegistry, ToolRequested,
    Turn, UserInput,
};

pub(super) fn handle_user_input(
    mut inputs: MessageReader<UserInput>,
    models: Res<ModelRegistry>,
    executor: Res<AsyncExecutor>,
    mut commands: Commands,
) {
    for input in inputs.read() {
        let Some(provider) = models.default_provider().cloned() else {
            log::warn!("no model provider is registered");
            continue;
        };
        let cancellation = CancelToken::new();
        let run = commands.spawn(AgentRun::new(0, cancellation.clone()));
        commands
            .entity(run)
            .insert(Turn::new(run, 0, input.text.clone()));

        let executor = (*executor).clone();
        let task_executor = executor.clone();
        let prompt = input.text.clone();
        if let Err(error) = executor.spawn(async move {
            race(
                async {
                    cancellation.cancelled().await;
                },
                async {
                    let result = provider.complete(&prompt).await;
                    let text = match result {
                        Ok(text) => text,
                        Err(error) => format!("model error: {error}"),
                    };
                    if let Err(error) = task_executor.send(ModelCompleted {
                        run,
                        version: 0,
                        text,
                    }) {
                        log::warn!("failed to send model completion: {error}");
                    }
                },
            )
            .await;
        }) {
            log::warn!("failed to spawn model task: {error}");
        }
    }
}

pub(super) fn process_model_response(
    mut results: MessageReader<ModelCompleted>,
    mut runs: Query<&mut AgentRun>,
    mut turns: Query<&mut Turn>,
    mut requests: MessageWriter<ToolRequested>,
) {
    for result in results.read() {
        let Some(run) = runs.get_mut(result.run) else {
            log::debug!("dropping completion for a missing run {:?}", result.run);
            continue;
        };
        if run.version() != result.version || run.is_cancelled() {
            log::debug!("dropping stale completion for run {:?}", result.run);
            continue;
        }
        drop(run);

        let Some(mut turn) = turns.get_mut(result.run) else {
            continue;
        };
        if turn.version() != result.version {
            continue;
        }
        turn.set_text(result.text.clone());
        match parse_tool_call(&result.text) {
            Some((name, arguments)) => requests.write(ToolRequested {
                run: result.run,
                version: result.version,
                name,
                arguments,
            }),
            None => turn.finish(),
        }
    }
}

pub(super) fn permission_system(
    mut requests: MessageReader<ToolRequested>,
    policy: Res<PermissionPolicy>,
    mut completed: MessageWriter<ToolCompleted>,
    mut approvals: MessageWriter<ApprovalRequested>,
) {
    for request in requests.read() {
        if policy.denies(&request.name) {
            completed.write(ToolCompleted {
                run: request.run,
                version: request.version,
                name: request.name.clone(),
                output: "denied by permission policy".to_owned(),
                denied: true,
            });
        } else if policy.asks(&request.name) {
            approvals.write(ApprovalRequested {
                run: request.run,
                version: request.version,
                name: request.name.clone(),
                arguments: request.arguments.clone(),
            });
        }
    }
}

pub(super) fn execute_tools(
    mut requests: MessageReader<ToolRequested>,
    mut approved: MessageReader<ToolApproved>,
    runs: Query<&AgentRun>,
    policy: Res<PermissionPolicy>,
    tools: Res<ToolRegistry>,
    executor: Res<AsyncExecutor>,
    mut completed: MessageWriter<ToolCompleted>,
) {
    let mut dispatcher = Dispatcher {
        tools: &tools,
        executor: &executor,
        completed: &mut completed,
    };

    for request in requests.read() {
        if policy.denies(&request.name) || policy.asks(&request.name) {
            continue;
        }
        let Some(run) = runs.get(request.run) else {
            continue;
        };
        if run.version() != request.version || run.is_cancelled() {
            continue;
        }
        dispatcher.dispatch(
            request.run,
            request.version,
            &request.name,
            &request.arguments,
            run.cancellation(),
        );
    }

    for request in approved.read() {
        let Some(run) = runs.get(request.run) else {
            continue;
        };
        if run.version() != request.version || run.is_cancelled() {
            continue;
        }
        dispatcher.dispatch(
            request.run,
            request.version,
            &request.name,
            &request.arguments,
            run.cancellation(),
        );
    }
}

struct Dispatcher<'a, 'm> {
    tools: &'a ToolRegistry,
    executor: &'a AsyncExecutor,
    completed: &'a mut MessageWriter<'m, ToolCompleted>,
}

impl Dispatcher<'_, '_> {
    fn dispatch(
        &mut self,
        run: Entity,
        version: u64,
        name: &str,
        arguments: &str,
        cancellation: CancelToken,
    ) {
        let Some(tool) = self.tools.get(name).cloned() else {
            self.completed.write(ToolCompleted {
                run,
                version,
                name: name.to_owned(),
                output: format!("tool `{name}` is not registered"),
                denied: true,
            });
            return;
        };
        let executor = self.executor.clone();
        let task_executor = executor.clone();
        let name = name.to_owned();
        let arguments = arguments.to_owned();
        if let Err(error) = executor.spawn(async move {
            race(
                async {
                    cancellation.cancelled().await;
                },
                async {
                    let result = tool.run(&arguments).await;
                    let (output, denied) = match result {
                        Ok(output) => (output, false),
                        Err(error) => (format!("tool error: {error}"), true),
                    };
                    if let Err(error) = task_executor.send(ToolCompleted {
                        run,
                        version,
                        name,
                        output,
                        denied,
                    }) {
                        log::warn!("failed to send tool result: {error}");
                    }
                },
            )
            .await;
        }) {
            log::warn!("failed to spawn tool task: {error}");
        }
    }
}

pub(super) fn handle_approval(
    mut answers: MessageReader<ApprovalResolved>,
    runs: Query<&AgentRun>,
    mut approved: MessageWriter<ToolApproved>,
    mut completed: MessageWriter<ToolCompleted>,
) {
    for answer in answers.read() {
        let Some(run) = runs.get(answer.run) else {
            continue;
        };
        if run.version() != answer.version || run.is_cancelled() {
            continue;
        }
        if answer.approved {
            approved.write(ToolApproved {
                run: answer.run,
                version: answer.version,
                name: answer.name.clone(),
                arguments: answer.arguments.clone(),
            });
        } else {
            completed.write(ToolCompleted {
                run: answer.run,
                version: answer.version,
                name: answer.name.clone(),
                output: "approval rejected".to_owned(),
                denied: true,
            });
        }
    }
}

pub(super) fn handle_tool_result(
    mut results: MessageReader<ToolCompleted>,
    runs: Query<&AgentRun>,
    mut turns: Query<&mut Turn>,
) {
    for result in results.read() {
        let Some(run) = runs.get(result.run) else {
            continue;
        };
        if run.version() != result.version || run.is_cancelled() {
            continue;
        }
        let Some(mut turn) = turns.get_mut(result.run) else {
            continue;
        };
        if turn.version() != result.version {
            continue;
        }
        turn.set_text(result.output.clone());
        turn.finish();
    }
}

pub(super) fn cancel_runs(
    mut cancels: MessageReader<CancelRun>,
    mut runs: Query<&mut AgentRun>,
    mut turns: Query<&mut Turn>,
) {
    for cancel in cancels.read() {
        let Some(mut run) = runs.get_mut(cancel.run) else {
            continue;
        };
        run.cancel();
        drop(run);
        if let Some(mut turn) = turns.get_mut(cancel.run) {
            turn.finish();
        }
    }
}

pub(super) fn record_approvals(
    mut requests: MessageReader<ApprovalRequested>,
    mut log: ResMut<ApprovalLog>,
) {
    for request in requests.read() {
        log.requests.push((*request).clone());
    }
}

fn parse_tool_call(text: &str) -> Option<(String, String)> {
    let rest = text.strip_prefix("tool:")?;
    let (name, arguments) = rest.split_once(':')?;
    Some((name.to_owned(), arguments.to_owned()))
}
