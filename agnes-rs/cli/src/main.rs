mod input;

use std::io::{IsTerminal, Write};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, bail};
use clap::{Parser, Subcommand};
use tokio::io::AsyncReadExt;

use agnes_agent::tools::{ListDir, ReadFile, Shell, WriteFile};
use agnes_agent::{Event, Sink, StandardRequestBuilder};
use agnes_core::config::{ApiKey, BaseUrl, ModelName, StepBudget, SystemPrompt};
use agnes_core::content::Content;
use agnes_core::context::{Context, Environment};
use agnes_core::event::Event as CoreEvent;
use agnes_core::llm::StopReason;
use agnes_core::message::MessageDelta;
use agnes_core::turn::Outcome;
use agnes_io::input::Input;
use agnes_io::llm::Llm;
use agnes_io::tool::{Tool, ToolRegistry};
use agnes_llm_openai::OpenAiClient;
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
    let mut tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(ReadFile::new(root.clone())),
        Arc::new(WriteFile::new(root.clone())),
        Arc::new(ListDir::new(root.clone())),
    ];
    if cli.allow_shell {
        tools.push(Arc::new(Shell::new(root)));
    }
    let registry = Arc::new(ToolRegistry::build(tools).context("building tool registry")?);

    let transport = ReqwestTransport::new(REQUEST_TIMEOUT);
    let client = OpenAiClient::new(transport, base_url, api_key);

    match cli.command {
        Some(Command::Run { task }) => {
            let goal = match task {
                Some(task) => task,
                None => read_goal_from_stdin().await?,
            };
            let goal = Content::try_from(goal.trim()).context(
                "empty task: pass a task argument (`agnes run \"do X\"`) or pipe one in",
            )?;
            let input = UserInput::new();
            input::spawn_ctrl_c(&input);
            input.begin_run();
            let printer = EventPrinter::new(model.clone());
            let host = Host {
                llm: &client,
                registry: &registry,
                model,
                max_steps: cli.max_steps,
            };
            let summary = host
                .run_task(goal, session_context(&registry), &*input, printer)
                .await?;
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
            let host = Host {
                llm: &client,
                registry: &registry,
                model,
                max_steps: cli.max_steps,
            };
            let mut context = session_context(&registry);
            loop {
                print!("agnes> ");
                std::io::stdout().flush().ok();
                match input.next().await {
                    Line::Exit => break,
                    Line::Reset => {
                        context.reset();
                        println!("(session cleared)");
                    }
                    Line::Task(goal) => {
                        input.begin_run();
                        let result = host
                            .run_task(
                                goal,
                                context.clone(),
                                &*input,
                                EventPrinter::new(host.model.clone()),
                            )
                            .await;
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

/// The fixed wiring every task run needs: the model client, the tool
/// registry, the model name and the step budget.
struct Host<'a> {
    llm: &'a dyn Llm,
    registry: &'a Arc<ToolRegistry>,
    model: ModelName,
    max_steps: u32,
}

impl Host<'_> {
    async fn run_task(
        &self,
        goal: Content,
        context: Context,
        input: &dyn Input,
        printer: EventPrinter,
    ) -> anyhow::Result<agnes_agent::RunSummary> {
        let budget: StepBudget = self
            .max_steps
            .to_string()
            .parse()
            .context("invalid --max-turns")?;
        let config = agnes_core::turn::AgentConfig {
            context,
            goal,
            budget,
        };
        let requests = StandardRequestBuilder::new(self.model.clone());
        let mut printer = printer;
        agnes_agent::run(
            config,
            Arc::clone(self.registry),
            self.llm,
            input,
            &requests,
            &mut printer,
        )
        .await
        .map_err(anyhow::Error::new)
    }
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
            CoreEvent::MessageUpdate { delta, .. } => print_delta(delta),
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

/// Streamed text is written through as it arrives; other fragments wait for
/// the assembled message. Streaming adapters are not wired up yet, so this
/// fires only once they are.
fn print_delta(delta: &MessageDelta) {
    if let MessageDelta::Text(text) = delta {
        print!("{text}");
        std::io::stdout().flush().ok();
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
            println!("(stopped: step budget exhausted — raise --max-turns)");
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
