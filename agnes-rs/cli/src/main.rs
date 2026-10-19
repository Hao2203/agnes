mod input;

use std::collections::HashSet;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, bail};
use clap::{Parser, Subcommand};
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;

use agnes_agent::approval::{ApprovalChannel, ApprovalRequest};
use agnes_agent::permission::{PolicyBundle, Rule};
use agnes_agent::tools::FsBundle;
use agnes_agent::{Event, Sink};
use agnes_core::config::{ApiKey, BaseUrl, ModelName, StepBudget, SystemPrompt};
use agnes_core::content::Content;
use agnes_core::context::{Context, Environment};
use agnes_core::event::Event as CoreEvent;
use agnes_core::llm::StopReason;
use agnes_core::tool::ToolName;
use agnes_core::turn::{AgentConfig, Outcome};
use agnes_io::input::Input;
use agnes_io::llm::Llm;
use agnes_io::session::SessionLog;
use agnes_io::tool::ToolRegistry;
use agnes_llm_openai::OpenAiClient;
use agnes_session_file::SessionFile;
use agnes_transport_reqwest::ReqwestTransport;

use crate::input::{Line, UserInput};

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const DEFAULT_MODEL: &str = "gpt-4o-mini";
const DEFAULT_MAX_STEPS: u32 = 32;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
const SYSTEM_PROMPT: &str = "You are agnes, a terminal coding assistant. Use the provided tools \
     to inspect and modify files in the working directory. Be concise.";

#[derive(Parser)]
#[command(
    name = "agnes",
    version,
    about = "A terminal AI agent (OpenAI-compatible APIs)"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Model name (e.g. gpt-4o-mini)
    #[arg(long, global = true)]
    model: Option<String>,

    /// OpenAI-compatible API base URL
    #[arg(long, global = true)]
    base_url: Option<String>,

    /// API key (defaults to $AGNES_API_KEY, then $OPENAI_API_KEY)
    #[arg(long, global = true, env = "AGNES_API_KEY", hide_env_values = true)]
    api_key: Option<String>,

    /// Maximum number of model round trips (steps) per task
    #[arg(long, global = true, default_value_t = DEFAULT_MAX_STEPS)]
    max_steps: u32,

    /// Offer the run_shell tool to the model
    #[arg(long, global = true)]
    allow_shell: bool,

    /// Keep the chat session in this append-only file; resumes when it exists
    #[arg(long, global = true)]
    session: Option<PathBuf>,

    /// Never prompt for approval (tools with ask rules run directly)
    #[arg(long, global = true)]
    yes: bool,

    /// Deny a tool by exact name, or by prefix with a trailing `*` (repeatable)
    #[arg(long = "deny", global = true, value_name = "TOOL")]
    deny: Vec<String>,
}

#[derive(Subcommand)]
enum Command {
    /// Run a single task and exit
    Run {
        /// The task; read from stdin when omitted
        task: Option<String>,
    },
    /// Interactive session: one agent task per line, full session memory,
    /// `/new` resets the context, `exit` leaves
    Chat,
}

#[tokio::main]
async fn main() -> ExitCode {
    match real_main().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

async fn real_main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let model: ModelName = cli
        .model
        .as_deref()
        .unwrap_or(DEFAULT_MODEL)
        .parse()
        .context("invalid --model")?;
    let base_url: BaseUrl = cli
        .base_url
        .as_deref()
        .unwrap_or(DEFAULT_BASE_URL)
        .parse()
        .context("invalid --base-url")?;
    let api_key = match &cli.api_key {
        Some(key) => ApiKey::new(key).context("invalid --api-key")?,
        None => {
            let key = std::env::var("OPENAI_API_KEY").unwrap_or_default();
            if key.is_empty() {
                bail!("no API key: pass --api-key, set AGNES_API_KEY, or set OPENAI_API_KEY");
            }
            ApiKey::new(key)?
        }
    };

    let root = std::env::current_dir().context("cannot determine working directory")?;

    let transport = ReqwestTransport::new(REQUEST_TIMEOUT);
    let client = OpenAiClient::new(transport, base_url, api_key);

    match &cli.command {
        Some(Command::Run { task }) => {
            let goal = match task {
                Some(task) => task.clone(),
                None => read_goal_from_stdin().await?,
            };
            let goal = Content::try_from(goal.trim()).context(
                "empty task: pass a task argument (`agnes run \"do X\"`) or pipe one in",
            )?;
            let input = UserInput::new();
            input::spawn_ctrl_c(&input);
            input.begin_run();
            let ask = ask_rules(&cli);
            let approvals = approvals_for(&input, &ask);
            let deny = cli
                .deny
                .iter()
                .map(|spec| parse_rule(spec))
                .collect::<anyhow::Result<Vec<_>>>()?;
            let mut printer = EventPrinter::new(model.clone());
            let mut host = build_host(
                &client,
                &*input,
                model.clone(),
                &root,
                cli.allow_shell,
                approvals.as_ref(),
                deny,
                ask,
            )?;
            let config = agent_config(goal, session_context(host.tools()), cli.max_steps)?;
            let summary = host.run(config, &mut printer, None).await?;
            print_summary(&summary);
        }
        Some(Command::Chat) | None => {
            println!(
                "interactive session — each line is a task with full session memory; \
                 /new resets the context, Ctrl-C interrupts a running task, exit leaves"
            );
            let input = UserInput::new();
            input::spawn_stdin_reader(&input);
            input::spawn_ctrl_c(&input);

            let ask = ask_rules(&cli);
            let approvals = approvals_for(&input, &ask);
            let deny = cli
                .deny
                .iter()
                .map(|spec| parse_rule(spec))
                .collect::<anyhow::Result<Vec<_>>>()?;
            let mut host = build_host(
                &client,
                &*input,
                model.clone(),
                &root,
                cli.allow_shell,
                approvals.as_ref(),
                deny,
                ask,
            )?;

            // 会话文件（可选）：存在即恢复，不存在即新建。
            let mut session_file = None;
            let mut restored = None;
            if let Some(path) = cli.session.as_deref() {
                let opened = open_session(path, &host).await?;
                if opened.repairs.is_empty() {
                    println!("(session: {})", path.display());
                } else {
                    println!(
                        "(resumed {}: repaired {} unfinished event(s))",
                        path.display(),
                        opened.repairs.len()
                    );
                }
                restored = Some(opened.events);
                session_file = Some(opened.session);
            }
            let mut context = match restored {
                Some(events) => Context::restore(
                    SystemPrompt::new(SYSTEM_PROMPT),
                    Environment::new(host.tools().specs()),
                    events,
                ),
                None => session_context(host.tools()),
            };

            loop {
                print!("agnes> ");
                std::io::stdout().flush().ok();
                match input.next().await {
                    Line::Exit => break,
                    Line::Reset => {
                        context.reset();
                        match cli.session.as_deref() {
                            // 日志只追加：`/new` 轮转到下一个空闲的兄弟文件。
                            Some(path) => {
                                if let Some(mut file) = session_file.take() {
                                    file.flush().await.context("closing the session")?;
                                }
                                let next = rotate(path);
                                let opened = open_session(&next, &host).await?;
                                println!("(session cleared — new file: {})", next.display());
                                context = Context::restore(
                                    SystemPrompt::new(SYSTEM_PROMPT),
                                    Environment::new(host.tools().specs()),
                                    opened.events,
                                );
                                session_file = Some(opened.session);
                            }
                            None => println!("(session cleared)"),
                        }
                    }
                    Line::Task(goal) => {
                        input.begin_run();
                        let result = match agent_config(goal, context.clone(), cli.max_steps) {
                            Ok(config) => {
                                let session = session_file
                                    .as_mut()
                                    .map(|file| file as &mut dyn SessionLog);
                                host.run(config, &mut EventPrinter::new(model.clone()), session)
                                    .await
                                    .map_err(anyhow::Error::new)
                            }
                            Err(err) => Err(err),
                        };
                        input.end_run();
                        match result {
                            Ok(summary) => {
                                print_summary(&summary);
                                context = summary.context;
                                // 没来得及生效的插话排回队列，下一行接着跑
                                for message in summary.pending_user_messages {
                                    input.push(crate::input::Line::Task(message));
                                }
                            }
                            Err(err) => eprintln!("error: {err:#}"),
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// A fresh session: the system prompt plus the tools this host offers, which
/// are recorded on the context as its environment.
fn session_context(registry: &ToolRegistry) -> Context {
    Context::new(
        SystemPrompt::new(SYSTEM_PROMPT),
        Environment::new(registry.specs()),
    )
}

/// The task for a one-shot `run`: the first line on a terminal, or all of a
/// piped stdin.
async fn read_goal_from_stdin() -> anyhow::Result<String> {
    if std::io::stdin().is_terminal() {
        print!("enter task (finish with Enter): ");
        std::io::stdout().flush().ok();
        let mut reader = tokio::io::BufReader::new(tokio::io::stdin());
        Ok(input::read_line_lossy(&mut reader)
            .await?
            .unwrap_or_default())
    } else {
        let mut buf = String::new();
        tokio::io::stdin()
            .read_to_string(&mut buf)
            .await
            .context("reading task from stdin")?;
        Ok(buf.trim().to_owned())
    }
}

/// The fixed wiring every run needs: the model client, the tools, the policy
/// and the request transform.
#[allow(clippy::too_many_arguments)]
fn build_host<'a>(
    llm: &'a dyn Llm,
    input: &'a dyn Input,
    model: ModelName,
    root: &Path,
    allow_shell: bool,
    approvals: Option<&'a ApprovalChannel>,
    deny: Vec<Rule>,
    ask: Vec<Rule>,
) -> anyhow::Result<agnes_agent::Host<'a>> {
    let mut builder = agnes_agent::Host::builder(llm, input)
        .model(model)
        .bundle(FsBundle::new(root, allow_shell));
    if let Some(approvals) = approvals {
        builder = builder.approvals(Some(approvals));
    }
    builder
        .bundle(PolicyBundle::new(deny, ask))
        .build()
        .context("assembling the host")
}

/// 默认策略：`run_shell` 需要一次审批（除非 `--yes` 或 shell 不在场）。
fn ask_rules(cli: &Cli) -> Vec<Rule> {
    if cli.yes || !cli.allow_shell {
        return Vec::new();
    }
    vec![Rule::Name(
        ToolName::from_str("run_shell").expect("`run_shell` is a valid tool name"),
    )]
}

/// 有 ask 规则才有审批口；没有审批口的 ask 会在装载期被拒绝。
fn approvals_for(input: &Arc<UserInput>, ask: &[Rule]) -> Option<ApprovalChannel> {
    if ask.is_empty() {
        return None;
    }
    let (channel, questions) = ApprovalChannel::channel(None);
    spawn_approver(Arc::clone(input), questions);
    Some(channel)
}

/// `--deny` 的语法：`name` 精确匹配，`prefix*` 前缀匹配。
fn parse_rule(spec: &str) -> anyhow::Result<Rule> {
    match spec.strip_suffix('*') {
        Some(prefix) if !prefix.is_empty() => Ok(Rule::Prefix(prefix.to_owned())),
        Some(_) => bail!("empty prefix in --deny `{spec}`"),
        None => Ok(Rule::Name(
            ToolName::from_str(spec).with_context(|| format!("invalid --deny `{spec}`"))?,
        )),
    }
}

async fn open_session(
    path: &Path,
    host: &agnes_agent::Host<'_>,
) -> anyhow::Result<agnes_session_file::Opened> {
    SessionFile::open_or_create(
        path,
        SystemPrompt::new(SYSTEM_PROMPT),
        Environment::new(host.tools().specs()),
    )
    .await
    .with_context(|| format!("opening session `{}`", path.display()))
}

/// 会话文件不可重写：`/new` 轮转到下一个空闲的兄弟文件。
fn rotate(path: &Path) -> PathBuf {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("session");
    let extension = path.extension().and_then(|s| s.to_str()).unwrap_or("jsonl");
    (1..)
        .map(|n| path.with_file_name(format!("{stem}.{n}.{extension}")))
        .find(|candidate| !candidate.exists())
        .expect("an unused session name exists")
}

/// 审批界面：问询期间读到的行不分类，直接作为答覆（空行 = 拒绝）。
fn spawn_approver(input: Arc<UserInput>, mut questions: mpsc::UnboundedReceiver<ApprovalRequest>) {
    tokio::spawn(async move {
        let mut always: HashSet<ToolName> = HashSet::new();
        while let Some(question) = questions.recv().await {
            let approved = if always.contains(&question.tool_name) {
                true
            } else {
                println!();
                println!(
                    "approve `{}`: {}",
                    question.tool_name,
                    question.arguments.as_str()
                );
                print!("  [y] once  [n] reject  [a] always this session: ");
                std::io::stdout().flush().ok();
                let answer = input.ask().await.unwrap_or_default();
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
    });
}

fn agent_config(goal: Content, context: Context, max_steps: u32) -> anyhow::Result<AgentConfig> {
    let budget: StepBudget = max_steps
        .to_string()
        .parse()
        .context("invalid --max-steps")?;
    Ok(AgentConfig {
        context,
        goal,
        budget,
    })
}

struct EventPrinter {
    model: ModelName,
}

impl EventPrinter {
    fn new(model: ModelName) -> Self {
        Self { model }
    }

    /// Renders one event from the context log.
    fn recorded(&self, event: &CoreEvent) {
        match event {
            CoreEvent::TurnStart => println!("─ turn ─"),
            CoreEvent::MessageStart { .. } => println!("→ model call"),
            CoreEvent::MessageEnd { stop, .. } => {
                if matches!(stop, StopReason::ToolUse { .. }) {
                    println!("← tool calls requested");
                }
            }
            CoreEvent::ToolExecutionStart { call, .. } => {
                println!("  ⚒ {} {}", call.name, call.arguments.as_str());
            }
            CoreEvent::ToolExecutionEnd { is_error, .. } => {
                if *is_error {
                    println!("  ✗ failed");
                }
            }
            // messages are rendered by the summary; the injected marker is
            // handled above; step/turn closes carry no extra news
            CoreEvent::Message(_)
            | CoreEvent::UserMessagesInjected { .. }
            | CoreEvent::StepStart { .. }
            | CoreEvent::StepEnd { .. }
            | CoreEvent::TurnEnd { .. } => {}
        }
    }
}

impl Sink for EventPrinter {
    fn event(&mut self, event: &Event) {
        match event {
            Event::Started { tools, .. } => {
                let names: Vec<&str> = tools.iter().map(|t| t.as_str()).collect();
                println!("* model {}, tools: [{}]", self.model, names.join(", "));
            }
            Event::ToolGated { denied, .. } => {
                println!("  ⛔ denied by {}: {}", denied.by, denied.reason);
            }
            // 交互式审批 UI 还是后续工作；此处先可见问询与落定
            Event::ApprovalRequested { call, .. } => {
                println!(
                    "  ⏳ waiting approval: {} {}",
                    call.name,
                    call.arguments.as_str()
                );
            }
            Event::ApprovalResolved { outcome, .. } => {
                if !outcome.allows() {
                    println!("  ✗ not approved ({outcome:?})");
                }
            }
            Event::Recorded { event, .. } => match event {
                CoreEvent::UserMessagesInjected { messages, .. } => {
                    println!("  ▸ injected {} queued message(s)", messages.len());
                }
                other => self.recorded(other),
            },
            Event::Finished { .. } => {}
        }
    }
}

fn print_summary(summary: &agnes_agent::RunSummary) {
    match &summary.outcome {
        Outcome::Answered { reply } => {
            println!();
            println!("{reply}");
        }
        Outcome::Interrupted => {
            println!();
            println!("(stopped: interrupted by the user — session kept)");
        }
        Outcome::BudgetExhausted => {
            println!();
            println!("(stopped: step budget exhausted — raise --max-steps)");
        }
        Outcome::LengthExceeded => {
            println!();
            println!("(stopped: model output hit the token limit)");
        }
    }
    let seconds = summary.duration().as_secs();
    println!(
        "[{seconds}s, {} step(s), {}]",
        summary.steps_used, summary.usage
    );
    // 单次 run：没生效的插话已经无处可去，提示一下别静默丢失
    for message in &summary.pending_user_messages {
        eprintln!("note: unused queued message: {message}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str) -> Rule {
        Rule::Name(ToolName::from_str(name).unwrap())
    }

    #[test]
    fn deny_rules_parse_exact_names_and_prefixes() {
        assert_eq!(parse_rule("run_shell").unwrap(), tool("run_shell"));
        assert_eq!(parse_rule("run_*").unwrap(), Rule::Prefix("run_".into()));
        assert!(parse_rule("*").is_err());
        assert!(parse_rule("a b").is_err());
    }

    #[test]
    fn rotation_picks_the_next_free_sibling() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        std::fs::write(&path, "").unwrap();
        assert_eq!(rotate(&path), dir.path().join("session.1.jsonl"));

        std::fs::write(dir.path().join("session.1.jsonl"), "").unwrap();
        assert_eq!(rotate(&path), dir.path().join("session.2.jsonl"));
    }

    #[test]
    fn ask_rules_follow_shell_and_yes() {
        let cli = Cli::parse_from(["agnes", "chat"]);
        assert!(ask_rules(&cli).is_empty());

        let cli = Cli::parse_from(["agnes", "chat", "--allow-shell"]);
        assert_eq!(ask_rules(&cli), vec![tool("run_shell")]);

        let cli = Cli::parse_from(["agnes", "chat", "--allow-shell", "--yes"]);
        assert!(ask_rules(&cli).is_empty());
    }
}
