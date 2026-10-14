use std::io::{IsTerminal, Write};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, bail};
use clap::{Parser, Subcommand};
use jiff::Timestamp;
use tokio::io::{AsyncBufReadExt, AsyncReadExt};

use agnes_agent::tools::{ListDir, ReadFile, Shell, WriteFile};
use agnes_agent::{Event, Sink};
use agnes_core::config::{ApiKey, BaseUrl, ModelName, SystemPrompt, TurnBudget};
use agnes_core::content::Content;
use agnes_core::context::Context;
use agnes_core::turn::Outcome;
use agnes_llm_openai::OpenAiClient;
use agnes_ports::llm::Llm;
use agnes_ports::tool::{Tool, ToolRegistry};
use agnes_transport_reqwest::ReqwestTransport;

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
            let summary = run_task(
                &client,
                &registry,
                model,
                cli.max_turns,
                goal,
                Context::new(SystemPrompt::new(SYSTEM_PROMPT)),
                EventPrinter::default(),
            )
            .await?;
            print_summary(&summary);
        }
        Some(Command::Chat) | None => {
            println!(
                "interactive session — each line is a task with full session memory; \
                 /new resets the context, exit leaves"
            );
            let mut context = Context::new(SystemPrompt::new(SYSTEM_PROMPT));
            let mut reader = tokio::io::BufReader::new(tokio::io::stdin());
            loop {
                print!("agnes> ");
                std::io::stdout().flush().ok();
                let line = match read_line_lossy(&mut reader).await {
                    Ok(Some(line)) => line,
                    Ok(None) => break, // EOF (Ctrl-D)
                    Err(err) => {
                        eprintln!("error reading input: {err}");
                        break;
                    }
                };
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if trimmed == "exit" || trimmed == "quit" {
                    break;
                }
                if trimmed == "/new" {
                    context.reset();
                    println!("(session cleared)");
                    continue;
                }
                let Ok(goal) = Content::try_from(trimmed) else {
                    continue;
                };
                match run_task(
                    &client,
                    &registry,
                    model.clone(),
                    cli.max_turns,
                    goal,
                    context.clone(),
                    EventPrinter::default(),
                )
                .await
                {
                    Ok(summary) => {
                        print_summary(&summary);
                        context = summary.context;
                    }
                    Err(err) => eprintln!("error: {err:#}"),
                }
            }
        }
    }
    Ok(())
}

/// Reads one line as raw bytes and decodes lossily. Input methods and
/// backspace editing can leave torn multi-byte sequences in a terminal's
/// input buffer; decoding those must degrade to replacement characters, not
/// abort the session.
async fn read_line_lossy<R>(reader: &mut R) -> anyhow::Result<Option<String>>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut bytes = Vec::new();
    let n = reader.read_until(b'\n', &mut bytes).await?;
    if n == 0 {
        return Ok(None); // EOF
    }
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    if bytes.last() == Some(&b'\r') {
        bytes.pop();
    }
    Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
}

async fn read_goal_from_stdin() -> anyhow::Result<String> {
    if std::io::stdin().is_terminal() {
        print!("enter task (finish with Enter): ");
        std::io::stdout().flush().ok();
        let mut reader = tokio::io::BufReader::new(tokio::io::stdin());
        Ok(read_line_lossy(&mut reader).await?.unwrap_or_default())
    } else {
        let mut buf = String::new();
        tokio::io::stdin()
            .read_to_string(&mut buf)
            .await
            .context("reading task from stdin")?;
        Ok(buf.trim().to_owned())
    }
}

async fn run_task(
    llm: &dyn Llm,
    registry: &Arc<ToolRegistry>,
    model: ModelName,
    max_turns: u32,
    goal: Content,
    context: Context,
    printer: EventPrinter,
) -> anyhow::Result<agnes_agent::RunSummary> {
    let budget: TurnBudget = max_turns
        .to_string()
        .parse()
        .context("invalid --max-turns")?;
    let config = agnes_core::turn::AgentConfig {
        context,
        goal,
        model,
        tools: registry.specs(),
        budget,
    };
    let mut printer = printer;
    agnes_agent::run(config, Arc::clone(registry), llm, &mut printer)
        .await
        .map_err(anyhow::Error::new)
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
