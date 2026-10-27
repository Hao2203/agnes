//! 装配与实际运行：一次 `run` 或交互式 `chat`。

use std::collections::HashSet;
use std::io::{IsTerminal, Read};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use agnes_agent::approval::{ApprovalChannel, ApprovalRequest};
use agnes_agent::permission::{PermissionRules, PolicyBundle, Rule};
use agnes_agent::tools::FsBundle;
use agnes_agent::{Host, Log, RunRequest, RunSummary};
use agnes_core::config::{ApiKey, ModelName, StepBudget, SystemPrompt};
use agnes_core::content::Content;
use agnes_core::header::{Environment, Header};
use agnes_core::tool::ToolName;
use agnes_core::turn::AgentConfig;
use agnes_io::input::Input;
use agnes_io::tool::ToolRegistry;
use agnes_llm_openai::OpenAiClient;
use agnes_transport_reqwest::ReqwestTransport;

use crate::cli::{Cli, Command};
use crate::input::{self, Line, UserInput};
use crate::render::{EventPrinter, Renderer, handle_pending, print_summary};
use crate::session::{ChatSession, LoadedSession};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
const SYSTEM_PROMPT: &str = "You are agnes, a terminal coding assistant. Use the provided tools \
     to inspect and modify files in the working directory. Be concise.";

struct Wiring {
    llm: OpenAiClient<ReqwestTransport>,
    input: Arc<UserInput>,
    renderer: Renderer,
    rules: PermissionRules,
    root: PathBuf,
    model: ModelName,
    max_steps: StepBudget,
    allow_shell: bool,
    yes: bool,
    session_path: Option<PathBuf>,
}

pub async fn run(cli: Cli) -> Result<()> {
    let api_key = resolve_api_key(cli.api_key)?;
    let llm = OpenAiClient::new(
        ReqwestTransport::new(REQUEST_TIMEOUT)?,
        cli.base_url,
        api_key,
    );
    let root = std::env::current_dir().context("cannot determine the working directory")?;
    let wiring = Wiring {
        llm,
        input: UserInput::new(),
        renderer: Renderer::new().context("starting the renderer")?,
        rules: PermissionRules {
            deny: cli.deny,
            ask: ask_rules(cli.allow_shell),
        },
        root,
        model: cli.model,
        max_steps: cli.max_steps,
        allow_shell: cli.allow_shell,
        yes: cli.yes,
        session_path: cli.session,
    };
    match cli.command.unwrap_or(Command::Chat) {
        Command::Run { task } => run_once(&wiring, task).await,
        Command::Chat => chat(&wiring).await,
    }
}

async fn run_staged(
    wiring: &Wiring,
    host: &mut Host<'_>,
    session: Option<&mut ChatSession>,
    goal: Content,
) -> Result<RunSummary> {
    // 每轮从当前注册表重建 header：插件上一轮动态加入/移除的工具
    // 从这一轮开始对模型可见。
    let config = AgentConfig {
        header: session_header(host.tools()),
        goal,
        budget: wiring.max_steps,
    };
    let log = session
        .as_ref()
        .map_or_else(Log::default, |session| session.log().clone());
    let mut printer = EventPrinter::new(wiring.model.clone(), wiring.renderer.clone());
    let mut request = RunRequest::new(config, log, &mut printer);
    if let Some(session) = session {
        request = request.with_session(session.file_mut());
    }
    wiring.input.begin_run();
    let result = host.run(request).await;
    wiring.input.end_run();
    result.map_err(anyhow::Error::new)
}

async fn run_once(wiring: &Wiring, task: Option<String>) -> Result<()> {
    let goal = match task {
        Some(task) => Content::try_from(task.trim())
            .context("empty task: pass a task argument (`agnes run \"do X\"`)")?,
        None => read_goal(&wiring.renderer).await?,
    };

    let needs_approval = !wiring.rules.ask.is_empty() && !wiring.yes;
    if needs_approval && !std::io::stdin().is_terminal() {
        bail!(
            "`--allow-shell` needs a terminal for approval; pass --yes to auto-approve, \
             or `--deny run_shell`"
        );
    }
    let stdin_task = needs_approval.then(|| input::spawn_stdin_reader(&wiring.input));
    let ctrl_c = input::spawn_ctrl_c(&wiring.input);

    let (approvals, approver) = approvals(wiring);
    let mut host = build_host(wiring, approvals.as_ref())?;

    let mut session = match &wiring.session_path {
        Some(path) => {
            let loaded = ChatSession::open(path.clone()).await?;
            print_session_loaded(&loaded, &wiring.renderer, false);
            Some(loaded.session)
        }
        None => None,
    };
    let result = run_staged(wiring, &mut host, session.as_mut(), goal).await;

    if let Some(task) = stdin_task {
        task.abort();
    }
    ctrl_c.abort();
    if let Some(task) = approver {
        task.abort();
    }

    let flush = match session.as_mut() {
        Some(session) => session.flush().await,
        None => Ok(()),
    };
    let summary = match result {
        Ok(summary) => {
            flush?;
            summary
        }
        Err(err) => {
            if let Err(flush) = flush {
                wiring
                    .renderer
                    .line(format!("warning: could not flush the session: {flush:#}"));
            }
            return Err(err);
        }
    };

    print_summary(&summary, &wiring.renderer);
    handle_pending(summary.pending_messages, &wiring.renderer, |content| {
        wiring
            .renderer
            .line(format!("note: unused queued message: {content}"));
    });
    for content in wiring.input.drain() {
        wiring
            .renderer
            .line(format!("note: unused queued message: {content}"));
    }
    if let Some(session) = session.as_mut() {
        session.set_log(summary.log);
    }
    Ok(())
}

async fn chat(wiring: &Wiring) -> Result<()> {
    wiring.renderer.line(
        "interactive session — each line is a task with full session memory; \
         /new resets the context, Ctrl-C interrupts a running task, exit leaves",
    );
    let stdin_task = input::spawn_stdin_reader(&wiring.input);
    let ctrl_c = input::spawn_ctrl_c(&wiring.input);

    let (approvals, approver) = approvals(wiring);
    let mut host = build_host(wiring, approvals.as_ref())?;

    let mut session = match &wiring.session_path {
        Some(path) => {
            let loaded = ChatSession::open(path.clone()).await?;
            print_session_loaded(&loaded, &wiring.renderer, false);
            Some(loaded.session)
        }
        None => None,
    };

    loop {
        wiring.renderer.prompt("agnes> ");
        match wiring.input.next().await {
            Line::Exit => break,
            Line::Reset => {
                let old_path = session.as_ref().map(|session| session.path().to_path_buf());
                match session.take() {
                    Some(old) => match old.rotate().await {
                        Ok(loaded) => {
                            print_session_loaded(&loaded, &wiring.renderer, true);
                            session = Some(loaded.session);
                        }
                        Err(err) => {
                            wiring.renderer.line(format!("error: {err:#}"));
                            if let Some(path) = old_path {
                                match ChatSession::open(path).await {
                                    Ok(loaded) => session = Some(loaded.session),
                                    Err(reopen) => wiring.renderer.line(format!(
                                        "error: cannot reopen the session: {reopen:#}"
                                    )),
                                }
                            }
                        }
                    },
                    None => wiring.renderer.line("(session cleared)"),
                }
            }
            Line::Task(goal) => {
                let result = run_staged(wiring, &mut host, session.as_mut(), goal).await;
                match result {
                    Ok(summary) => {
                        print_summary(&summary, &wiring.renderer);
                        handle_pending(summary.pending_messages, &wiring.renderer, |content| {
                            wiring.input.push(Line::Task(content))
                        });
                        if let Some(session) = session.as_mut() {
                            session.set_log(summary.log);
                        }
                    }
                    Err(err) => {
                        wiring.renderer.line(format!("error: {err:#}"));
                    }
                }
            }
        }
    }

    stdin_task.abort();
    ctrl_c.abort();
    if let Some(approver) = approver {
        approver.abort();
    }
    if let Some(mut session) = session {
        session.flush().await?;
    }
    Ok(())
}

fn approvals(wiring: &Wiring) -> (Option<ApprovalChannel>, Option<JoinHandle<()>>) {
    if wiring.rules.ask.is_empty() {
        return (None, None);
    }
    let (channel, questions) = ApprovalChannel::channel(None);
    let task = spawn_approver(
        wiring.yes,
        wiring.renderer.clone(),
        Arc::clone(&wiring.input),
        questions,
    );
    (Some(channel), Some(task))
}

fn spawn_approver(
    auto_approve: bool,
    renderer: Renderer,
    input: Arc<UserInput>,
    mut questions: mpsc::Receiver<ApprovalRequest>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
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
                renderer.prompt(format!(
                    "\napprove `{}`: {}\n  [y] once  [n] reject  [a] always this session: ",
                    question.tool_name,
                    question.arguments.as_str()
                ));
                let answer = input.answer().await;
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
            let _ = question.reply.send(approved);
        }
    })
}

fn build_host<'a>(wiring: &'a Wiring, approvals: Option<&'a ApprovalChannel>) -> Result<Host<'a>> {
    let mut builder = Host::builder(&wiring.llm, &*wiring.input)
        .model(wiring.model.clone())
        .bundle(FsBundle::new(&wiring.root, wiring.allow_shell));
    if let Some(approvals) = approvals {
        builder = builder.approvals(Some(approvals));
    }
    builder
        .bundle(PolicyBundle::new(wiring.rules.clone()))
        .build()
        .context("assembling the host")
}

fn session_header(registry: &ToolRegistry) -> Header {
    Header::new(
        SystemPrompt::new(SYSTEM_PROMPT),
        Environment::new(registry.specs()),
    )
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

fn print_session_loaded(loaded: &LoadedSession, renderer: &Renderer, rotated: bool) {
    let path = loaded.session.path();
    if rotated {
        renderer.line(format!("(session cleared — new file: {})", path.display()));
    } else if loaded.repairs.is_empty() {
        renderer.line(format!("(session: {})", path.display()));
    } else {
        renderer.line(format!(
            "(resumed {}: repaired {} unfinished event(s))",
            path.display(),
            loaded.repairs.len()
        ));
    }
    if !loaded.notes.is_empty() {
        renderer.line(format!("(recovered {} audit note(s))", loaded.notes.len()));
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
