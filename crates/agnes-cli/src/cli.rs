use agnes_llm::LlmCliOpts;
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "agnes", version, about = "agnes DSL runtime")]
pub struct Args {
    #[command(flatten)]
    pub llm: LlmFlags,

    #[command(subcommand)]
    pub cmd: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Interactive REPL: NL turns are planned into DSL by an LLM.
    Chat,
    /// Non-interactive: parse, compile, and execute a .agnes file.
    Run { file: String },
}

#[derive(Debug, clap::Args)]
pub struct LlmFlags {
    #[arg(long)]
    pub llm_provider: Option<String>,
    #[arg(long)]
    pub llm_model: Option<String>,
    #[arg(long)]
    pub llm_base_url: Option<String>,
}

impl LlmFlags {
    pub fn to_opts(&self) -> LlmCliOpts {
        LlmCliOpts {
            provider: self.llm_provider.clone(),
            model: self.llm_model.clone(),
            base_url: self.llm_base_url.clone(),
        }
    }
}
