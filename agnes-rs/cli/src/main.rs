mod app;
mod cli;
mod input;
mod render;

use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;

use crate::cli::Cli;

fn main() -> ExitCode {
    let cli = Cli::parse();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("building the tokio runtime");
    let result = runtime.block_on(app::run(cli));
    // 被中止的 stdin 任务可能还留有一个阻塞中的读线程；限时关闭，
    // 而不是陪它等到 EOF。
    runtime.shutdown_timeout(Duration::from_millis(500));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}
