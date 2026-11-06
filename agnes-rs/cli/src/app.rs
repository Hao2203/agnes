//! 装配与运行：把各组件的 Plugin 装进一个 ECS App，宿主任务（stdin、
//! Ctrl-C、审批）只经 `RuntimeSender` 注入消息。
//!
//! 每个组件 crate 导出一个 Plugin；本 crate 是唯一知道具体实现的装配点
//! —— 换 HTTP 栈、换 provider、换持久化 = 换一行 `add_plugin`。

use std::io::{IsTerminal, Read};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use agnes_agent::StandardRequestBuilder;
use agnes_agent::activity::ActivityPlugin;
use agnes_agent::approval::{ApprovalChannel, ApprovalRequest};
use agnes_agent::permission::{PermissionRules, Rule};
use agnes_agent::runtime::{
    AgentPlugin, RequestBuilderResource, ToolsPlugin, TurnFinished, TurnRequested, UserMessage,
};
use agnes_agent::tools::builtin_tools;
use agnes_core::config::{ApiKey, StepBudget, SystemPrompt};
use agnes_core::content::Content;
use agnes_core::header::{Environment, Header};
use agnes_core::tool::ToolName;
use agnes_core::turn::AgentConfig;
use agnes_ecs::{
    App, CancelToken, MessageReader, MessageWriter, Query, Res, RuntimeSender, Update,
};
use agnes_io::tool::ToolRegistry;
use agnes_plugin_async::TokioPlugin;
use agnes_plugin_http::HttpPlugin;
use agnes_plugin_openai::OpenAiPlugin;
use agnes_plugin_session::SessionPlugin;
use agnes_plugin_session_file::SessionFilePlugin;
use agnes_session_file::{Opened, SessionFile};
use agnes_transport_reqwest::ReqwestTransport;
use anyhow::{Context, Result, bail};
use tokio::task::JoinHandle;

use crate::cli::{Cli, Command};
use crate::input::{self, TaskLine, UserInput};
use crate::render::{Renderer, RendererPlugin, Status};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
const SYSTEM_PROMPT: &str = "You are agnes, a terminal coding assistant. Use the provided tools \
     to inspect and modify files in the working directory. Be concise.";

/// 宿主任务共用的运行时句柄。
pub struct HostChannels {
    pub sender: RuntimeSender,
    pub shutdown: CancelToken,
}

/// 宿主侧的固定配置：step 预算与系统提示。
struct HostConfig {
    budget: StepBudget,
    system_prompt: SystemPrompt,
}

/// 把用户输入路由进管线：没有活跃 turn 时开新 turn，否则交给注入管线。
fn dispatch_lines(
    mut lines: MessageReader<TaskLine>,
    turns: Query<agnes_ecs::Entity>,
    registry: Res<ToolRegistry>,
    config: Res<HostConfig>,
    mut turn_requests: MessageWriter<TurnRequested>,
    mut injections: MessageWriter<UserMessage>,
) {
    for line in lines.read() {
        if turns.is_empty() {
            turn_requests.write(TurnRequested {
                config: AgentConfig {
                    header: session_header(&config.system_prompt, &registry),
                    goal: Some(line.content.clone()),
                    budget: config.budget,
                },
            });
        } else {
            injections.write(UserMessage {
                content: line.content.clone(),
            });
        }
    }
}

/// turn 在飞时维护输入侧的 running 旗标，Ctrl-C 与 stdin 路由都看它。
fn track_running(
    mut requests: MessageReader<TurnRequested>,
    mut finished: MessageReader<TurnFinished>,
    input: Res<Arc<UserInput>>,
) {
    if !requests.read().is_empty() {
        input.set_running(true);
    }
    if !finished.read().is_empty() {
        input.set_running(false);
    }
}

/// 会话侧装配：状态插件（带系统）+ 可选的持久化插件（不带系统）。
struct SessionAssembly {
    state: SessionPlugin,
    file: Option<SessionFilePlugin>,
}

impl SessionAssembly {
    fn new(state: SessionPlugin, file: Option<SessionFilePlugin>) -> Self {
        Self { state, file }
    }

    /// 是否有持久化后端（决定 `/new` 的反馈文案）。
    fn has_store(&self) -> bool {
        self.file.is_some()
    }
}

/// 宿主侧装配输入：渲染句柄、输入状态、权限与会话。
struct HostAssembly {
    renderer: Renderer,
    input: Arc<UserInput>,
    rules: PermissionRules,
    approvals: Option<ApprovalChannel>,
    session: SessionAssembly,
    single_shot: bool,
}

/// 把各组件的 Plugin 装配成一个 App。
///
/// 注册顺序即每 tick 的系统执行顺序；会话系统必须在渲染/agent 系统之前，
/// 让落盘确认在同一 tick 内先于耐久门禁与收场报告更新。
fn build_app(mut app: App, cli: &Cli, host: HostAssembly) -> Result<App> {
    let HostAssembly {
        renderer,
        input,
        rules,
        approvals,
        session,
        single_shot,
    } = host;
    let transport = Arc::new(ReqwestTransport::new(REQUEST_TIMEOUT)?);
    let root = std::env::current_dir().context("cannot determine the working directory")?;

    app.add_plugin(HttpPlugin::new(transport))
        .add_plugin(OpenAiPlugin::new(
            cli.base_url.clone(),
            resolve_api_key(cli.api_key.clone())?,
        ))
        .add_plugin(ToolsPlugin::empty().with_tools(builtin_tools(root, cli.allow_shell)))
        .add_plugin(ActivityPlugin::new(cli.model.clone()))
        .insert_resource(rules)
        .insert_resource(HostConfig {
            budget: cli.max_steps,
            system_prompt: SystemPrompt::new(SYSTEM_PROMPT),
        })
        .insert_resource(RequestBuilderResource::new(Box::new(
            StandardRequestBuilder::new(cli.model.clone()),
        )))
        .insert_resource(Arc::clone(&input))
        .add_systems(Update, dispatch_lines)
        .add_systems(Update, track_running);
    if let Some(approvals) = approvals {
        app.insert_resource(approvals);
    }
    // 会话注册：状态插件带系统，必须排在渲染/agent 之前，让 session_ack
    // 的落盘确认在同一 tick 内先于耐久门禁与收场报告生效。文件插件不带
    // 系统，顺序无关。
    let has_session = session.has_store();
    if let Some(file) = session.file {
        app.add_plugin(file);
    }
    app.add_plugin(session.state);
    let sender = app.runtime_sender();
    app.add_plugin(RendererPlugin::new(
        renderer,
        sender,
        single_shot,
        has_session,
    ));
    app.add_plugin(AgentPlugin);
    app.add_plugin(TokioPlugin);
    Ok(app)
}

pub async fn run(cli: Cli) -> Result<()> {
    let rules = PermissionRules {
        deny: cli.deny.clone(),
        ask: ask_rules(cli.allow_shell),
    };
    let single_shot = matches!(cli.command, Some(Command::Run { .. }));
    // ask 规则命中就必须有审批口；--yes 时口还在，只是自动批准（审计
    // 照记）。真正交互式问答案才需要终端。
    let wires_ask = !rules.ask.is_empty();
    if wires_ask && !cli.yes && !std::io::stdin().is_terminal() {
        bail!(
            "`--allow-shell` needs a terminal for approval; pass --yes to auto-approve, \
             or `--deny run_shell`"
        );
    }
    let input = UserInput::new();

    // 交互式 chat 在真实终端上使用 TUI；`run`、管道与非 TTY 保持行式
    // 输出（一次性运行的答复不能随备用屏幕消失）。
    let app = App::new();
    let host_sender = app.runtime_sender();
    let host_shutdown = app.shutdown_handle();
    let tui = !single_shot
        && !cli.plain
        && std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal();
    let renderer = if tui {
        Renderer::tui(
            Arc::clone(&input),
            host_sender.clone(),
            host_shutdown.clone(),
        )
        .context("starting the TUI")?
    } else {
        Renderer::plain().context("starting the renderer")?
    };
    renderer.status(Status {
        model: cli.model.to_string(),
        ..Status::default()
    });

    // 会话打开需要 await：在进 App 之前完成，恢复历史交给状态插件。
    let mut session_plugin = SessionPlugin::new();
    let mut session_file = None;
    if let Some(path) = &cli.session {
        let Opened {
            session,
            events,
            notes,
            repairs,
        } = SessionFile::open_or_create(path.clone())
            .await
            .with_context(|| format!("opening session `{}`", path.display()))?;
        print_session_loaded(path, notes.len(), repairs.len(), false, &renderer);
        session_plugin = SessionPlugin::resumed(events).context("loading the session")?;
        session_file = Some(SessionFilePlugin::new(session));
    }

    let (approvals, questions) = match wires_ask {
        true => {
            let (channel, questions) = ApprovalChannel::channel(None);
            (Some(channel), Some(questions))
        }
        false => (None, None),
    };

    let app = build_app(
        app,
        &cli,
        HostAssembly {
            renderer: renderer.clone(),
            input: Arc::clone(&input),
            rules,
            approvals,
            session: SessionAssembly::new(session_plugin, session_file),
            single_shot,
        },
    )?;

    // TUI 的备用屏幕没有终端回滚；把恢复出的会话历史铺进消息区，供滚动查看。
    if tui && let Some(log) = app.world().resource::<agnes_record::Log>() {
        for line in crate::render::history_lines(&log) {
            renderer.line(line);
        }
    }

    let channels = Arc::new(HostChannels {
        sender: app.runtime_sender(),
        shutdown: app.shutdown_handle(),
    });
    // TUI 在 raw mode 下自己接收 Ctrl-C；信号处理器只服务行式模式。
    let ctrl_c = (!tui).then(|| input::spawn_ctrl_c(Arc::clone(&input), Arc::clone(&channels)));
    let approver = questions.map(|questions| {
        spawn_approver(
            cli.yes,
            renderer.clone(),
            Arc::clone(&input),
            questions,
            channels.sender.clone(),
        )
    });

    // 逐行阅读任务只在有活干时启动：chat 的会话输入，或交互式审批应答。
    // 单次运行不需要它 —— 管道 EOF 会被它当成退出信号，误杀进行中的
    // turn；goal 用参数或 read_goal（独占读）获取。
    let needs_line_reader = !single_shot || (wires_ask && !cli.yes);
    let goal = match &cli.command {
        Some(Command::Run { task }) => Some(match task {
            Some(task) => Content::try_from(task.trim())
                .context("empty task: pass a task argument (`agnes run \"do X\"`)")?,
            None => read_goal(&renderer).await?,
        }),
        _ => {
            renderer.prompt("agnes> ");
            None
        }
    };
    let stdin_task = (needs_line_reader && !tui)
        .then(|| input::spawn_stdin_reader(Arc::clone(&input), Arc::clone(&channels)));

    if let Some(goal) = goal {
        channels
            .sender
            .send_message(crate::input::TaskLine { content: goal })
            .context("the runtime is gone")?;
    }

    let run_result = app.run_async().await;

    if let Some(stdin_task) = stdin_task {
        stdin_task.abort();
    }
    if let Some(ctrl_c) = ctrl_c {
        ctrl_c.abort();
    }
    if let Some(approver) = approver {
        approver.abort();
    }
    // TUI 的最后一份句柄释放时恢复终端；之后才能安全打印错误。
    drop(renderer);

    run_result.map_err(anyhow::Error::new)?;
    Ok(())
}

/// 终端审批：问询经 [`ApprovalChannel`] 进来，答覆经 `RuntimeSender` 回注
/// 为 [`agnes_agent::runtime::ApprovalDecided`]。
fn spawn_approver(
    auto_approve: bool,
    renderer: Renderer,
    input: Arc<UserInput>,
    mut questions: tokio::sync::mpsc::Receiver<ApprovalRequest>,
    sender: RuntimeSender,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        use std::collections::HashSet;
        let mut always: HashSet<ToolName> = HashSet::new();
        while let Some(question) = questions.recv().await {
            let approved = if auto_approve {
                renderer.line(format!("  ✓ auto-approved {}", question.tool_name));
                true
            } else if always.contains(&question.tool_name) {
                true
            } else {
                // Enter answer mode before the prompt becomes visible.
                let mode = input.begin_answer();
                renderer.approval(Some(format!(
                    "\napprove `{}`: {}\n  [y] once  [n] reject  [a] always this session: ",
                    question.tool_name,
                    question.arguments.as_str()
                )));
                let answer = input.answer().await;
                renderer.approval(None);
                drop(mode);
                match answer.trim() {
                    "a" | "A" => {
                        always.insert(question.tool_name.clone());
                        true
                    }
                    "y" | "Y" | "yes" => true,
                    _ => false,
                }
            };
            // 答覆经 ECS 消息回注；落定与审计由门禁与 session 插件负责。
            let _ = sender.send_message(agnes_agent::runtime::ApprovalDecided {
                call_id: question.call_id.clone(),
                approved,
            });
        }
    })
}

fn session_header(system_prompt: &SystemPrompt, registry: &ToolRegistry) -> Header {
    Header::new(system_prompt.clone(), Environment::new(registry.specs()))
}

fn ask_rules(allow_shell: bool) -> Vec<Rule> {
    if !allow_shell {
        return Vec::new();
    }
    vec![Rule::Name(
        ToolName::from_str("run_shell").expect("`run_shell` is a valid tool name"),
    )]
}

fn resolve_api_key(flag: Option<ApiKey>) -> Result<ApiKey> {
    if let Some(key) = flag {
        return Ok(key);
    }
    match std::env::var("OPENAI_API_KEY") {
        Ok(key) => ApiKey::try_from(key).context("OPENAI_API_KEY is empty"),
        Err(std::env::VarError::NotPresent) => {
            bail!("no API key: pass --api-key, set AGNES_API_KEY, or set OPENAI_API_KEY")
        }
        Err(std::env::VarError::NotUnicode(_)) => bail!("OPENAI_API_KEY is not valid UTF-8"),
    }
}

async fn read_goal(renderer: &Renderer) -> Result<Content> {
    if std::io::stdin().is_terminal() {
        renderer.prompt("> ");
        let line = tokio::task::spawn_blocking(|| {
            let mut line = String::new();
            std::io::stdin().read_line(&mut line).map(|_| line)
        })
        .await
        .context("reading the task")??;
        return Content::try_from(line.trim()).context("the task is empty");
    }
    let raw = tokio::task::spawn_blocking(|| {
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text).map(|_| text)
    })
    .await
    .context("reading the task from stdin")??;
    Content::try_from(raw.trim())
        .context("empty task: pass a task argument (`agnes run \"do X\"`) or pipe one in")
}

fn print_session_loaded(
    path: &std::path::Path,
    notes: usize,
    repairs: usize,
    rotated: bool,
    renderer: &Renderer,
) {
    if rotated {
        renderer.line(format!("(session cleared — new file: {})", path.display()));
    } else if repairs == 0 {
        renderer.line(format!("(session: {})", path.display()));
    } else {
        renderer.line(format!(
            "(resumed {}: repaired {repairs} unfinished event(s))",
            path.display()
        ));
    }
    if notes > 0 {
        renderer.line(format!("(recovered {notes} audit note(s))"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ask_rules_follow_shell_not_yes() {
        assert!(ask_rules(false).is_empty());
        assert_eq!(
            ask_rules(true),
            vec![Rule::Name(ToolName::from_str("run_shell").unwrap())]
        );
    }
}
