//! plain 行输出后端：管道、非 TTY 与 `agnes run` 使用的经典输出。
//!
//! 流式文本直接写到 stdout（不等待换行）；步骤统计按行打印。

use std::io::{BufWriter, IsTerminal, Write};
use std::sync::mpsc;

use super::UiCommand;

/// Runs the line-based writer until the channel closes or shutdown arrives.
pub(crate) fn run(rx: mpsc::Receiver<UiCommand>) {
    let stdout = std::io::stdout();
    let mut out = BufWriter::new(stdout.lock());
    // 思考流式文本以灰字展示；仅在 stdout 是终端时上色，管道输出保持纯净。
    let color = stdout.is_terminal();
    let mut streaming = false;
    let mut reasoning_open = false;
    while let Ok(command) = rx.recv() {
        match command {
            UiCommand::Line(line) => {
                close_reasoning(&mut out, &mut reasoning_open, color);
                if streaming {
                    let _ = out.write_all(b"\n");
                    streaming = false;
                }
                let _ = out.write_all(line.as_bytes());
                let _ = out.write_all(b"\n");
                let _ = out.flush();
            }
            UiCommand::Prompt(prompt) => {
                close_reasoning(&mut out, &mut reasoning_open, color);
                if streaming {
                    let _ = out.write_all(b"\n");
                    streaming = false;
                }
                let _ = out.write_all(prompt.as_bytes());
                let _ = out.flush();
            }
            UiCommand::StreamStart => {
                close_reasoning(&mut out, &mut reasoning_open, color);
                if streaming {
                    let _ = out.write_all(b"\n");
                }
                streaming = false;
            }
            UiCommand::StreamDelta(text) => {
                close_reasoning(&mut out, &mut reasoning_open, color);
                let _ = out.write_all(text.as_bytes());
                let _ = out.flush();
                streaming = true;
            }
            UiCommand::StreamEnd => {
                close_reasoning(&mut out, &mut reasoning_open, color);
                if streaming {
                    let _ = out.write_all(b"\n");
                    streaming = false;
                }
                let _ = out.flush();
            }
            UiCommand::ReasoningDelta(text) => {
                if !reasoning_open {
                    if color {
                        let _ = out.write_all(b"\n\x1b[90m(thinking) ");
                    } else {
                        let _ = out.write_all(b"\n(thinking) ");
                    }
                    reasoning_open = true;
                }
                let _ = out.write_all(text.as_bytes());
                let _ = out.flush();
            }
            UiCommand::CommitAssistant(text) => {
                close_reasoning(&mut out, &mut reasoning_open, color);
                if !streaming {
                    let _ = out.write_all(text.as_bytes());
                    let _ = out.write_all(b"\n");
                    let _ = out.flush();
                }
            }
            UiCommand::StepStats { step, usage } => {
                close_reasoning(&mut out, &mut reasoning_open, color);
                if streaming {
                    let _ = out.write_all(b"\n");
                    streaming = false;
                }
                let _ = writeln!(out, "  [step {step}: {usage}]");
                let _ = out.flush();
            }
            UiCommand::Models { current, models } => {
                close_reasoning(&mut out, &mut reasoning_open, color);
                let _ = writeln!(out, "models:");
                for model in models {
                    if model == current {
                        let _ = writeln!(out, "  {model}  (current)");
                    } else {
                        let _ = writeln!(out, "  {model}");
                    }
                }
                let _ = writeln!(out, "  use /model <name> to switch");
                let _ = out.flush();
            }
            UiCommand::ModelsFailed(error) => {
                close_reasoning(&mut out, &mut reasoning_open, color);
                let _ = writeln!(out, "error: cannot list models: {error}");
                let _ = out.flush();
            }
            UiCommand::Status(_) | UiCommand::Approval(_) => {}
            UiCommand::Flush => {
                let _ = out.flush();
            }
            UiCommand::Shutdown => break,
        }
    }
}

fn close_reasoning<W: Write>(out: &mut W, reasoning_open: &mut bool, color: bool) {
    if *reasoning_open {
        if color {
            let _ = out.write_all(b"\x1b[0m");
        }
        let _ = out.write_all(b"\n");
        let _ = out.flush();
        *reasoning_open = false;
    }
}
