//! Interactive input: a background stdin reader that classifies lines and
//! injects them as ECS messages, answer mode for approval questions, and
//! Ctrl-C handling.
//!
//! The reader never blocks the ECS: everything it does is sending messages
//! on the [`RuntimeSender`] or cancelling the [`CancelToken`].

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use agnes_core::content::Content;
use tokio::io::AsyncBufReadExt;
use tokio::sync::{Notify, watch};
use tokio::task::JoinHandle;

use crate::app::HostChannels;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One classified line the user typed.
pub(crate) enum Line {
    /// A task/message to run.
    Task(Content),
    /// `/new` — drop the session's prior turns.
    Reset,
    /// `exit`/`quit`, EOF, or Ctrl-C at the prompt.
    Exit,
}

/// Shared input state: answer mode for approval questions plus the run flag
/// the signal handler and the stdin reader watch.
pub struct UserInput {
    /// 审批问询期间：读到的行不分类，原样进答覆队列。
    answering: AtomicBool,
    answers: Mutex<VecDeque<String>>,
    answer_wake: Notify,
    running: watch::Sender<bool>,
}

impl UserInput {
    pub fn new() -> Arc<Self> {
        let (running, _) = watch::channel(false);
        Arc::new(Self {
            answering: AtomicBool::new(false),
            answers: Mutex::new(VecDeque::new()),
            answer_wake: Notify::new(),
            running,
        })
    }

    /// Whether a turn is in flight; the stdin reader routes Task lines by it.
    pub fn running_handle(&self) -> watch::Receiver<bool> {
        self.running.subscribe()
    }

    /// Marks a turn as in flight (or finished); called by the host systems.
    pub fn set_running(&self, running: bool) {
        let _ = self.running.send_replace(running);
    }

    /// Enters answer mode **before** the question is printed, closing the
    /// window where a fast answer would be classified as a task.
    pub fn begin_answer(&self) -> AnswerMode<'_> {
        self.answering.store(true, Ordering::Release);
        AnswerMode { input: self }
    }

    /// Waits for the next raw line supplied by the stdin reader.
    pub async fn answer(&self) -> String {
        loop {
            {
                let mut answers = lock(&self.answers);
                if let Some(line) = answers.pop_front() {
                    return line;
                }
            }
            self.answer_wake.notified().await;
        }
    }

    pub(crate) fn is_answering(&self) -> bool {
        self.answering.load(Ordering::Acquire)
    }

    pub(crate) fn push_answer(&self, line: String) {
        lock(&self.answers).push_back(line);
        self.answer_wake.notify_one();
    }
}

/// Answer mode guard: dropping it (including on cancellation) restores normal
/// line classification.
pub struct AnswerMode<'a> {
    input: &'a UserInput,
}

impl Drop for AnswerMode<'_> {
    fn drop(&mut self) {
        self.input.answering.store(false, Ordering::Release);
    }
}

/// Reads stdin line by line in the background, turning every line into an
/// ECS message, until EOF — which shuts the runtime down like `exit` does.
pub fn spawn_stdin_reader(input: Arc<UserInput>, channels: Arc<HostChannels>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut running = input.running_handle();
        let mut pending_reset = false;
        let mut reader = tokio::io::BufReader::new(tokio::io::stdin());
        loop {
            tokio::select! {
                biased;
                // 运行结束后补发推迟的 `/new`。
                _ = running.changed(), if pending_reset => {
                    if !*running.borrow_and_update() {
                        pending_reset = false;
                        let _ = channels.sender.send_message(agnes_plugin_session::RotateSession);
                    }
                }
                line = read_line_lossy(&mut reader) => {
                    match line {
                        Ok(Some(raw)) => {
                            if input.is_answering() {
                                input.push_answer(raw);
                                continue;
                            }
                            match classify(&raw) {
                                None => {}
                                Some(Line::Exit) => {
                                    // EOF 也要给挂起的审批一个答覆，否则问询会一直悬着。
                                    if input.is_answering() {
                                        input.push_answer(String::new());
                                    }
                                    channels.shutdown.cancel();
                                    return;
                                }
                                Some(Line::Reset) => {
                                    if *running.borrow_and_update() {
                                        // 命令是屏障：等当前 turn 收场后再轮转。
                                        pending_reset = true;
                                    } else {
                                        let _ = channels
                                            .sender
                                            .send_message(agnes_plugin_session::RotateSession);
                                    }
                                }
                                Some(Line::Task(content)) => {
                                    let _ = channels.sender.send_message(TaskLine { content });
                                }
                            }
                        }
                        Ok(None) | Err(_) => {
                            if input.is_answering() {
                                input.push_answer(String::new());
                            }
                            channels.shutdown.cancel();
                            return;
                        }
                    }
                }
            }
        }
    })
}

/// Ctrl-C: interrupts a turn in flight; at the prompt it shuts the runtime
/// down.
pub fn spawn_ctrl_c(input: Arc<UserInput>, channels: Arc<HostChannels>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut running = input.running_handle();
        loop {
            if tokio::signal::ctrl_c().await.is_err() {
                return;
            }
            if *running.borrow_and_update() {
                let _ = channels
                    .sender
                    .send_message(agnes_agent::runtime::InterruptTurn);
            } else {
                if input.is_answering() {
                    input.push_answer(String::new());
                }
                channels.shutdown.cancel();
                return;
            }
        }
    })
}

/// Maps one raw line to a [`Line`], or `None` for blank lines.
pub(crate) fn classify(line: &str) -> Option<Line> {
    let trimmed = line.trim();
    match trimmed {
        "" => None,
        "exit" | "quit" => Some(Line::Exit),
        "/new" => Some(Line::Reset),
        _ => Content::try_from(trimmed).ok().map(Line::Task),
    }
}

/// A task line from the host: routed to a new turn or, while a turn is
/// running, to the injection pipeline.
#[derive(Debug)]
pub struct TaskLine {
    pub content: Content,
}

/// Reads one line as raw bytes and decodes lossily. Input methods and
/// backspace editing can leave torn multi-byte sequences in a terminal's
/// input buffer; decoding those must degrade to replacement characters, not
/// abort the session.
pub(crate) async fn read_line_lossy<R>(reader: &mut R) -> anyhow::Result<Option<String>>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut bytes = Vec::with_capacity(128);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn answering_mode_takes_raw_lines_and_restores_on_drop() {
        let input = UserInput::new();
        let mode = input.begin_answer();
        input.push_answer("y".into());
        assert_eq!(input.answer().await, "y");
        drop(mode);
        // 答覆模式已退出
        assert!(!input.is_answering());
    }

    #[test]
    fn classify_recognizes_commands_and_skips_blanks() {
        assert!(classify("  ").is_none());
        assert!(matches!(classify("exit"), Some(Line::Exit)));
        assert!(matches!(classify("quit"), Some(Line::Exit)));
        assert!(matches!(classify("/new"), Some(Line::Reset)));
        assert!(matches!(classify("do the thing"), Some(Line::Task(_))));
    }

    #[test]
    fn the_running_flag_starts_false_and_is_settable() {
        let input = UserInput::new();
        let mut handle = input.running_handle();
        assert!(!*handle.borrow_and_update());
        input.set_running(true);
        assert!(*handle.borrow_and_update());
    }
}
