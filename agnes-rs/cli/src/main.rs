mod input;

use std::io::{IsTerminal, Write};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, bail};
use clap::{Parser, Subcommand};
use jiff::Timestamp;
use tokio::io::AsyncReadExt;

use agnes_agent::tools::{ListDir, ReadFile, Shell, WriteFile};
use agnes_agent::{Event, Sink};
use agnes_core::config::{ApiKey, BaseUrl, ModelName, SystemPrompt, TurnBudget};
use agnes_core::content::Content;
use agnes_core::context::Context;
use agnes_core::turn::Outcome;
use agnes_io::input::Input;
use agnes_io::llm::Llm;
use agnes_io::tool::{Tool, ToolRegistry};
use agnes_llm_openai::OpenAiClient;
use agnes_transport_reqwest::ReqwestTransport;

use crate::input::{Line, UserInput};

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const DEFAULT_MODEL: &str = "gpt-4o-mini";
const DEFAULT_MAX_TURNS: u32 = 32;
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

    /// Maximum number of model turns per task
    #[arg(long, global = true, default_value_t = DEFAULT_MAX_TURNS)]
    max_turns: u32,

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
            let host = Host {
                llm: &client,
                registry: &registry,
                model,
                max_turns: cli.max_turns,
            };
            let summary = host
                .run_task(
                    goal,
                    Context::new(SystemPrompt::new(SYSTEM_PROMPT)),
                    &*input,
                    EventPrinter::default(),
                )
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
                max_turns: cli.max_turns,
            };
            let mut context = Context::new(SystemPrompt::new(SYSTEM_PROMPT));
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
                            .run_task(goal, context.clone(), &*input, EventPrinter::default())
                            .await;
                        input.end_run();
                        match result {
                            Ok(summary) => {
                                print_summary(&summary);
                                context = summary.context;
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
/// registry, the model name and the turn budget.
struct Host<'a> {
    llm: &'a dyn Llm,
    registry: &'a Arc<ToolRegistry>,
    model: ModelName,
    max_turns: u32,
}

impl Host<'_> {
    async fn run_task(
        &self,
        goal: Content,
        context: Context,
        input: &dyn Input,
        printer: EventPrinter,
    ) -> anyhow::Result<agnes_agent::RunSummary> {
        let budget: TurnBudget = self
            .max_turns
            .to_string()
            .parse()
            .context("invalid --max-turns")?;
        let config = agnes_core::turn::AgentConfig {
            context,
            goal,
            tools: self.registry.specs(),
            budget,
        };
        let mut printer = printer;
        agnes_agent::run(
            config,
            self.model.clone(),
            Arc::clone(self.registry),
            self.llm,
            input,
            &mut printer,
        )
        .await
        .map_err(anyhow::Error::new)
    }
}

#[derive(Default)]
struct EventPrinter {
    started_at: Option<Timestamp>,
}

impl Sink for EventPrinter {
    fn event(&mut self, event: &Event) {
        match event {
            Event::Started { model, tools, .. } => {
                self.started_at = Some(Timestamp::now());
                let names: Vec<&str> = tools.iter().map(|t| t.as_str()).collect();
                println!("* model {model}, tools: [{}]", names.join(", "));
            }
            Event::ModelRequested { turn, .. } => {
                println!("→ model call (turn {turn})");
            }
            Event::ModelResponded { stop, .. } => {
                if matches!(stop, agnes_core::llm::StopReason::ToolUse { .. }) {
                    println!("← tool calls requested");
                }
            }
            Event::ToolStarted { call, .. } => {
                println!("  ⚒ {} {}", call.name, call.arguments.as_str());
            }
            Event::ToolFinished { is_error, .. } => {
                if *is_error {
                    println!("  ✗ failed");
                }
            }
            Event::UserMessagesInjected { messages, .. } => {
                println!("  ▸ injected {} queued message(s)", messages.len());
            }
            Event::Finished { .. } => {}
        }
    }
}

fn print_summary(summary: &agnes_agent::RunSummary) {
    match &summary.outcome {
        Outcome::Completed { reply } => {
            println!();
            println!("{reply}");
        }
        Outcome::Interrupted => {
            println!();
            println!("(stopped: interrupted by the user — session kept)");
        }
        Outcome::BudgetExhausted => {
            println!();
            println!("(stopped: turn budget exhausted — raise --max-turns)");
        }
        Outcome::LengthExceeded => {
            println!();
            println!("(stopped: model output hit the token limit)");
        }
    }
    let seconds = summary.duration().as_secs();
    println!(
        "[{seconds}s, turn {}, {}]",
        summary.turns_used, summary.usage
    );
}
