use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let path = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("usage: agnes <file.agnes>"))?;
    let src = tokio::fs::read_to_string(std::path::PathBuf::from(&path)).await?;
    let mut reg = agnes_registry::Registry::new();
    agnes_builtins::register_builtins(&mut reg)?;
    let program = agnes_parser::parse(&src).map_err(|e| anyhow::anyhow!("{e}"))?;
    reg.load(&program).map_err(|e| anyhow::anyhow!("{e}"))?;
    agnes_checker::check(&program, &reg).map_err(|e| anyhow::anyhow!("{e}"))?;
    let dag = agnes_compiler::compile(&program, &reg).map_err(|e| anyhow::anyhow!("{e}"))?;
    // Temporary MockProvider so the workspace stays green through this task.
    // Task 10 replaces this with real provider resolution.
    let mock: Arc<dyn agnes_llm::Provider> = Arc::new(agnes_llm::MockProvider::new(vec![
        "[LLM output placeholder for CLI shim]".into();
        16
    ]));
    let dispatch = agnes_builtins::native_dispatch(mock);
    let result = agnes_runtime::execute(&dag, &reg, &dispatch)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    println!("{}", result.data);
    Ok(())
}
