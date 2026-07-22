//! Interactive `agnes chat` REPL.
//!
//! Line-based rustyline loop. A line beginning with `(` or `[` is treated
//! as raw DSL; anything else goes through the LLM planner. Slash commands
//! (`/run <dsl>`, `/history`, `/reset`, `/quit`) provide out-of-band
//! control. Multi-line entry activates when a line opens with `(` and is
//! terminated by a matching close-paren (string literals excluded via
//! [`crate::input::is_balanced`]). Ctrl-D exits cleanly.

use crate::input::is_balanced;
use crate::sink_stderr::StderrEventSink;
use agnes_llm::Provider;
use agnes_session::{Session, SessionError, TurnInput};
use rustyline::error::ReadlineError;
use rustyline::{DefaultEditor, Result as RlResult};
use std::sync::Arc;
use tokio::sync::Mutex;

/// Prints the banner and enters the REPL. Ctrl-D exits cleanly.
pub async fn run(provider: Arc<dyn Provider>, max_turns: Option<u32>) -> anyhow::Result<()> {
    banner();
    let mut session = match max_turns {
        Some(n) => Session::new_with_max_turns(provider, n)?,
        None => Session::new(provider)?,
    };
    let mut rl: DefaultEditor = DefaultEditor::new()?;
    loop {
        match read_line_or_block(&mut rl) {
            Ok(Some(line)) => {
                if let Some(cmd) = line.strip_prefix('/') {
                    if !dispatch_slash(cmd, &mut session).await? {
                        break;
                    }
                    continue;
                }
                if line.trim().is_empty() {
                    continue;
                }
                let sink = StderrEventSink::new();
                let sink = Arc::new(Mutex::new(sink));
                let trimmed = line.trim_start();
                let input = if trimmed.starts_with('(') || trimmed.starts_with('[') {
                    // Direct DSL injection when the user types raw code.
                    TurnInput::RawDsl(line)
                } else {
                    TurnInput::NaturalLanguage(line)
                };
                let cancel = std::sync::Arc::new(tokio::sync::Notify::new());
                let cancel_for_signal = cancel.clone();
                // Set up a one-shot Ctrl-C handler for the duration of
                // this turn only. rustyline is not active while we're
                // awaiting run_turn, so a stray SIGINT would kill the
                // process; the handler swaps that behavior for a soft
                // cancel that lets the loop return SessionError::Cancelled.
                let ctrlc_task = tokio::spawn(async move {
                    if let Err(e) = tokio::signal::ctrl_c().await {
                        eprintln!(
                            "warning: could not set up Ctrl-C handler: {e}; cancellation will not work for this turn"
                        );
                    }
                    cancel_for_signal.notify_one();
                });
                let result = session.run_turn_cancellable(input, sink, cancel).await;
                ctrlc_task.abort();
                match result {
                    Ok(v) => println!("{}", v.data),
                    Err(SessionError::Cancelled { after_iterations }) => {
                        eprintln!("(cancelled after {after_iterations} iteration(s))");
                    }
                    Err(e) => eprintln!("error: {e}"),
                }
            }
            Ok(None) => break, // EOF
            Err(e) => {
                eprintln!("readline: {e}");
                break;
            }
        }
    }
    Ok(())
}

/// Reads one logical entry: either a single line ending on Enter, or a
/// multi-line entry when `(` opens; keeps reading with the continuation
/// prompt `... ` until the paren balance is zero.
fn read_line_or_block(rl: &mut DefaultEditor) -> RlResult<Option<String>> {
    let first = match rl.readline("agnes> ") {
        Ok(s) => s,
        Err(ReadlineError::Eof) => return Ok(None),
        Err(ReadlineError::Interrupted) => return Ok(Some(String::new())),
        Err(e) => return Err(e),
    };
    let _ = rl.add_history_entry(first.as_str());
    if !first.trim_start().starts_with('(') {
        return Ok(Some(first));
    }
    let mut buf = first;
    while !is_balanced(&buf) {
        match rl.readline("     ...> ") {
            Ok(next) => {
                buf.push('\n');
                buf.push_str(&next);
                let _ = rl.add_history_entry(next.as_str());
            }
            Err(ReadlineError::Eof) | Err(ReadlineError::Interrupted) => return Ok(Some(buf)),
            Err(e) => return Err(e),
        }
    }
    Ok(Some(buf))
}

async fn dispatch_slash(cmd: &str, session: &mut Session) -> anyhow::Result<bool> {
    let cmd = cmd.trim();
    if cmd == "quit" || cmd == "exit" {
        return Ok(false);
    }
    if cmd == "reset" {
        session.reset_history();
        println!("(history cleared)");
        return Ok(true);
    }
    if cmd == "history" {
        let mut stdout = std::io::stdout().lock();
        crate::history_view::render_history(session.history(), &mut stdout).ok();
        return Ok(true);
    }
    if let Some(dsl) = cmd.strip_prefix("run ") {
        let sink = StderrEventSink::new();
        let sink = Arc::new(Mutex::new(sink));
        match session
            .run_turn(TurnInput::RawDsl(dsl.into()), sink)
            .await
        {
            Ok(v) => println!("{}", v.data),
            Err(e) => eprintln!("error: {e}"),
        }
        return Ok(true);
    }
    eprintln!("unknown command: /{cmd}. Try: /run <dsl>, /history, /reset, /quit");
    Ok(true)
}

fn banner() {
    eprintln!("━━━ agnes chat ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    eprintln!("type your goal, or /run <dsl>, /history, /reset, /quit");
    eprintln!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
}
