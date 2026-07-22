use agnes_cli::{
    cli::{Args, Command},
    run_cmd,
};
use agnes_llm::resolve_provider;
use clap::Parser;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let _ = dotenvy::dotenv();
    let args = Args::parse();
    let provider = resolve_provider(&args.llm.to_opts()).map_err(|e| anyhow::anyhow!("{e}"))?;
    match args.cmd.unwrap_or(Command::Chat) {
        Command::Chat => agnes_cli::chat::run(provider, args.max_turns, args.allow_root, args.allow_shell).await,
        Command::Run { file } => run_cmd::run_file(&file, provider).await,
    }
}
