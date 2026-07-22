use crate::sink_stderr::StderrEventSink;
use agnes_llm::Provider;
use agnes_session::{Session, TurnInput};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

pub async fn run_file(file: &str, provider: Arc<dyn Provider>) -> anyhow::Result<()> {
    let src = tokio::fs::read_to_string(PathBuf::from(file)).await?;
    let mut session = Session::new(provider)?;
    let sink = StderrEventSink::new();
    let wrapped = Arc::new(Mutex::new(sink));
    let out = session.run_turn(TurnInput::RawDsl(src), wrapped).await?;
    println!("{}", out.data);
    Ok(())
}
