//! plain 行输出后端：管道、非 TTY 与 `agnes run` 使用的经典输出。
//!
//! 流式文本直接写到 stdout（不等待换行）；步骤统计按行打印。

use std::io::{BufWriter, Write};
use std::sync::mpsc;

use super::UiCommand;

/// Runs the line-based writer until the channel closes or shutdown arrives.
pub(crate) fn run(rx: mpsc::Receiver<UiCommand>) {
    let stdout = std::io::stdout();
    let mut out = BufWriter::new(stdout.lock());
    let mut streaming = false;
    while let Ok(command) = rx.recv() {
        match command {
            UiCommand::Line(line) => {
                if streaming {
                    let _ = out.write_all(b"\n");
                    streaming = false;
                }
                let _ = out.write_all(line.as_bytes());
                let _ = out.write_all(b"\n");
                let _ = out.flush();
            }
            UiCommand::Prompt(prompt) => {
                if streaming {
                    let _ = out.write_all(b"\n");
                    streaming = false;
                }
                let _ = out.write_all(prompt.as_bytes());
                let _ = out.flush();
            }
            UiCommand::StreamStart => {
                if streaming {
                    let _ = out.write_all(b"\n");
                }
                streaming = false;
            }
            UiCommand::StreamDelta(text) => {
                let _ = out.write_all(text.as_bytes());
                let _ = out.flush();
                streaming = true;
            }
            UiCommand::StreamEnd => {
                if streaming {
                    let _ = out.write_all(b"\n");
                    streaming = false;
                }
                let _ = out.flush();
            }
            UiCommand::CommitAssistant(text) => {
                if !streaming {
                    let _ = out.write_all(text.as_bytes());
                    let _ = out.write_all(b"\n");
                    let _ = out.flush();
                }
            }
            UiCommand::StepStats { step, usage } => {
                if streaming {
                    let _ = out.write_all(b"\n");
                    streaming = false;
                }
                let _ = writeln!(out, "  [step {step}: {usage}]");
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
