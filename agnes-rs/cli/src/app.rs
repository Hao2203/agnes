//! 装配与运行：把各组件的 Plugin 装进一个 ECS App，宿主任务（stdin、
//! Ctrl-C、审批）只经 `RuntimeSender` 注入消息。
//!
//! 每个组件 crate 导出 Plugin 与状态构造器；本 crate 是唯一知道具体
//! 实现的装配点 —— 宿主按需创建 agent 实体并挂上核心状态与输入队列，插件只
//! 注册系统；换 HTTP 栈、换 provider、换持久化 = 换一行装配代码。

use std::io::{IsTerminal, Read};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use agnes_agent::activity::ActivityPlugin;
use agnes_agent::approval::{ApprovalChannel, ApprovalRequest};
use agnes_agent::model::ModelPlugin;
use agnes_agent::permission::{PermissionRules, Rule};
use agnes_agent::runtime::{
    ActiveTurn, AgentPlugin, RequestBuilderResource, ToolsPlugin, TurnOwner, TurnQueued,
    TurnRequest, TurnRequestSeq, UserMessage,
};
use agnes_agent::tools::builtin_tools;
use agnes_agent::{AgentBuilder, AgentCore, StandardRequestBuilder, submit_turn};
use agnes_core::config::{ApiKey, StepBudget, SystemPrompt};
use agnes_core::content::Content;
use agnes_core::header::{Environment, Header};
use agnes_core::tool::ToolName;
use agnes_core::turn::AgentConfig;
use agnes_ecs::{
    App, CancelToken, Commands, Entity, MessageReader, MessageWriter, Query, Res, RuntimeSender,
    Update,
};
use agnes_io::tool::ToolRegistry;
use agnes_plugin_async::TokioPlugin;
use agnes_plugin_http::HttpPlugin;
use agnes_plugin_openai::OpenAiPlugin;
use agnes_plugin_session::SessionPlugin;
use agnes_plugin_session_file::FileStore;
use agnes_record::{Event, SessionStore};
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

/// 宿主侧的固定配置：step 预算与本轮 header（工具表在启动时已定型）。
struct HostConfig {
    budget: StepBudget,
    header: Header,
}

/// 把用户输入路由进管线：目标 agent 没有活跃 turn 时开新 turn，否则交给
/// 注入管线。
///
/// 目标 agent 是宿主装配时绑定的 agent 实体句柄，而不是"世界里的第一个"；
/// `turns` 只匹配带 [`ActiveTurn`] 标记、且归属该 agent 的 turn 实体。
#[allow(clippy::too_many_arguments)]
fn dispatch_lines(
    mut lines: MessageReader<TaskLine>,
    turns: Query<(&ActiveTurn, &TurnOwner)>,
    queued: Query<(&TurnRequest, &TurnQueued)>,
    input: Res<Arc<UserInput>>,
    config: Res<HostConfig>,
    seq: Res<TurnRequestSeq>,
    mut commands: Commands,
    mut injections: MessageWriter<UserMessage>,
) {
    let Some(agent) = input.agent() else {
        return;
    };
    let busy = turns.iter().any(|(_, owner)| owner.0 == agent)
        || queued.iter().any(|(request, _)| request.agent == agent);
    for line in lines.read() {
        if !busy {
            submit_turn(
                &mut commands,
                seq.next(),
                agent,
                AgentConfig {
                    header: config.header.clone(),
                    goal: Some(line.content.clone()),
                    budget: config.budget,
                },
                None,
            );
        } else {
            injections.write(UserMessage {
                agent,
                content: line.content.clone(),
            });
        }
    }
}

/// turn 在飞或排队时维护输入侧的 running 旗标，Ctrl-C 与 stdin 路由都看它。
/// 只跟踪绑定 agent 的请求，别的 agent 收场不清这份旗标。
fn track_running(
    turns: Query<(&ActiveTurn, &TurnOwner)>,
    queued: Query<(&TurnRequest, &TurnQueued)>,
    input: Res<Arc<UserInput>>,
) {
    let Some(agent) = input.agent() else {
        return;
    };
    let busy = turns.iter().any(|(_, owner)| owner.0 == agent)
        || queued.iter().any(|(request, _)| request.agent == agent);
    input.set_running(busy);
}

/// 会话侧装配：恢复的历史事件 + 可选的持久化文件。
struct SessionAssembly {
    events: Vec<Event>,
    file: Option<SessionFile>,
}

impl SessionAssembly {
    fn new(events: Vec<Event>, file: Option<SessionFile>) -> Self {
        Self { events, file }
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

/// 把各组件的 Plugin 装配成一个 App，返回装配好的 agent 实体句柄。
///
/// 宿主先创建 agent 实体并挂上核心状态、输入队列与能力组件（构造器来自各属主
/// crate），再注册插件；插件不再 spawn 实体、不挂组件。插件间强依赖
/// 由各自 build 内的断言强制；注册顺序仍即每 tick 的系统执行顺序 ——
/// 会话系统必须先于渲染/agent 系统注册，让落盘确认在同一 tick 内先于
/// 耐久门禁与收场报告更新。
fn build_app(mut app: App, cli: &Cli, host: HostAssembly) -> Result<(App, Entity)> {
    let HostAssembly {
        renderer,
        input,
        rules,
        approvals,
        session,
        single_shot,
    } = host;
    let SessionAssembly { events, file } = session;
    let has_session = file.is_some();
    let transport = Arc::new(ReqwestTransport::new(REQUEST_TIMEOUT)?);
    let root = std::env::current_dir().context("cannot determine the working directory")?;

    // 核心状态、输入队列与能力组件由 AgentBuilder 一次挂齐；持久化 store
    // 是可选能力，随装配一起交给 builder。
    let store: Option<Arc<dyn SessionStore>> =
        file.map(|file| Arc::new(FileStore::new(file)) as Arc<dyn SessionStore>);
    let agent = AgentBuilder::new(cli.model.clone(), cli.think)
        .restore(events)
        .store(store)
        .spawn(&mut app)
        .context("loading the session")?
        .entity;

    app.add_plugin(HttpPlugin::new(transport))
        .add_plugin(OpenAiPlugin::new(
            cli.base_url.clone(),
            resolve_api_key(cli.api_key.clone())?,
        ))
        .add_plugin(ToolsPlugin::empty().with_tools(builtin_tools(root, cli.allow_shell)))
        .add_plugin(ActivityPlugin)
        .add_plugin(ModelPlugin)
        .insert_resource(rules)
        .insert_resource(RequestBuilderResource::new(Box::new(
            StandardRequestBuilder,
        )))
        .insert_resource(Arc::clone(&input))
        .add_systems(Update, dispatch_lines)
        .add_systems(Update, track_running);
    // 工具表在注册 ToolsPlugin 后固定；header 只构造一次。
    let header = {
        let registry = app
            .world()
            .resource::<ToolRegistry>()
            .expect("ToolsPlugin installs the registry");
        session_header(&SystemPrompt::new(SYSTEM_PROMPT), &registry)
    };
    app.insert_resource(HostConfig {
        budget: cli.max_steps,
        header,
    });
    if let Some(approvals) = approvals {
        app.insert_resource(approvals);
    }
    app.add_plugin(SessionPlugin);
    let sender = app.runtime_sender();
    app.add_plugin(RendererPlugin::new(
        renderer,
        sender,
        agent,
        single_shot,
        has_session,
    ));
    app.add_plugin(AgentPlugin);
    app.add_plugin(TokioPlugin);
    Ok((app, agent))
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
        thinking: cli.think,
        ..Status::default()
    });

    // 会话打开需要 await：在进 App 之前完成；恢复出的历史与文件句柄
    // 交给装配（宿主自己挂状态与存储）。
    let mut events = Vec::new();
    let mut session_file = None;
    if let Some(path) = &cli.session {
        let Opened {
            session,
            events: restored,
            notes,
            repairs,
        } = SessionFile::open_or_create(path.clone())
            .await
            .with_context(|| format!("opening session `{}`", path.display()))?;
        print_session_loaded(path, notes.len(), repairs.len(), false, &renderer);
        events = restored;
        session_file = Some(session);
    }

    let (approvals, questions) = match wires_ask {
        true => {
            let (channel, questions) = ApprovalChannel::channel(None);
            (Some(channel), Some(questions))
        }
        false => (None, None),
    };

    let (app, agent) = build_app(
        app,
        &cli,
        HostAssembly {
            renderer: renderer.clone(),
            input: Arc::clone(&input),
            rules,
            approvals,
            session: SessionAssembly::new(events, session_file),
            single_shot,
        },
    )?;
    // 宿主任务（/model、Ctrl-C）的目标 agent：装配时创建并保管句柄。
    input.set_agent(agent);

    // TUI 的备用屏幕没有终端回滚；把恢复出的会话历史铺进消息区，供滚动查看。
    if tui && let Some(core) = app.world().get::<AgentCore>(agent) {
        for line in crate::render::history_lines(core.log()) {
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
    let stdin_task = (needs_line_reader && !tui).then(|| {
        input::spawn_stdin_reader(Arc::clone(&input), Arc::clone(&channels), renderer.clone())
    });

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
                agent: question.agent,
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
