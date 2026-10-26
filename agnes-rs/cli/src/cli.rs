//! 命令行参数：解析即得到领域类型，不在 `main` 里重复校验。

use std::path::PathBuf;

use agnes_agent::permission::Rule;
use agnes_core::config::{ApiKey, BaseUrl, ModelName, StepBudget};
use clap::{Parser, Subcommand};

pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
pub const DEFAULT_MODEL: &str = "gpt-4o-mini";
pub const DEFAULT_MAX_STEPS: &str = "32";

/// A terminal AI agent (OpenAI-compatible APIs).
#[derive(Debug, Parser)]
#[command(
    name = "agnes",
    version,
    about = "A terminal AI agent (OpenAI-compatible APIs)"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Model name (e.g. gpt-4o-mini)
    #[arg(long, global = true, default_value = DEFAULT_MODEL)]
    pub model: ModelName,

    /// OpenAI-compatible API base URL
    #[arg(long, global = true, default_value = DEFAULT_BASE_URL)]
    pub base_url: BaseUrl,

    /// API key (defaults to $AGNES_API_KEY, then $OPENAI_API_KEY)
    #[arg(long, global = true, env = "AGNES_API_KEY", hide_env_values = true)]
    pub api_key: Option<ApiKey>,

    /// Maximum number of model round trips (steps) per task
    #[arg(long, global = true, default_value = DEFAULT_MAX_STEPS)]
    pub max_steps: StepBudget,

    /// Offer the run_shell tool to the model
    #[arg(long, global = true)]
    pub allow_shell: bool,

    /// Keep the session in this append-only file; resumes when it exists
    #[arg(long, global = true)]
    pub session: Option<PathBuf>,

    /// Auto-approve tools with ask rules (the audit trail is kept)
    #[arg(long, global = true)]
    pub yes: bool,

    /// Deny a tool by exact name, or by prefix with a trailing `*` (repeatable)
    #[arg(long = "deny", global = true, value_name = "TOOL")]
    pub deny: Vec<Rule>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run a single task and exit
    Run {
        /// The task; read from piped stdin when omitted
        task: Option<String>,
    },
    /// Interactive session: one agent task per line, full session memory,
    /// `/new` resets the context, `exit` leaves
    Chat,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_parses_typed_values_and_defaults() {
        let cli = Cli::parse_from(["agnes", "chat"]);
        assert_eq!(cli.model.as_str(), DEFAULT_MODEL);
        assert_eq!(cli.base_url.as_str(), "https://api.openai.com/v1/");
        assert_eq!(cli.max_steps.to_string(), DEFAULT_MAX_STEPS);
        assert!(matches!(cli.command, Some(Command::Chat)));
    }

    #[test]
    fn deny_rules_parse_exact_names_and_prefixes() {
        let cli = Cli::parse_from(["agnes", "chat", "--deny", "run_shell", "--deny", "run_*"]);
        assert_eq!(
            cli.deny,
            vec![
                Rule::Name("run_shell".parse().unwrap()),
                Rule::Prefix("run_".into())
            ]
        );
        assert!(Cli::try_parse_from(["agnes", "chat", "--deny", "*"]).is_err());
        assert!(Cli::try_parse_from(["agnes", "chat", "--deny", "a b"]).is_err());
    }
}
