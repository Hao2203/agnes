//! Interactive input: a background stdin reader, the shared queue the chat
//! loop and the agent driver both consume, and Ctrl-C handling.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use agnes_core::content::Content;
use agnes_ports::input::Input;
use async_trait::async_trait;
use tokio::io::AsyncBufReadExt;
use tokio::sync::{Notify, watch};

/// One classified line the user typed.
pub enum Line {
    /// A task/message to run.
    Task(Content),
    /// `/new` — drop the session's prior turns.
    Reset,
    /// `exit`/`quit`, EOF, or Ctrl-C at the prompt.
    Exit,
}

/// The queue between the stdin reader task, the signal handler, the chat
/// loop and — mid-run — the agent driver.
pub struct UserInput {
    queue: Mutex<VecDeque<Line>>,
    wake: Notify,
    stop: watch::Sender<bool>,
    running: AtomicBool,
}

impl UserInput {
    pub fn new() -> Arc<Self> {
        let (stop, _) = watch::channel(false);
        Arc::new(Self {
            queue: Mutex::new(VecDeque::new()),
            wake: Notify::new(),
            stop,
            running: AtomicBool::new(false),
        })
    }

    pub fn push(&self, line: Line) {
        self.queue.lock().unwrap().push_back(line);
        self.wake.notify_one();
    }

    /// Next line for the chat loop; waits until one is available.
    pub async fn next(&self) -> Line {
        loop {
            {
                let mut queue = self.queue.lock().unwrap();
                if let Some(line) = queue.pop_front() {
                    return line;
                }
            }
            self.wake.notified().await;
        }
    }

    /// Marks a run as in flight and clears a stop left over from a previous
    /// run, so one Ctrl-C never leaks into the next task.
    pub fn begin_run(&self) {
        self.stop.send_replace(false);
        self.running.store(true, Ordering::SeqCst);
    }

    pub fn end_run(&self) {
        self.running.store(false, Ordering::SeqCst);
    }

    fn request_stop(&self) {
        self.stop.send_replace(true);
    }

    fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Input for UserInput {
    fn drain(&self) -> Vec<Content> {
        // Only messages ahead of any command line are injected; a command is
        // a barrier, so `/new` and whatever follows it stay queued for the
        // loop to handle in order.
        let mut queue = self.queue.lock().unwrap();
        let mut messages = Vec::new();
        while matches!(queue.front(), Some(Line::Task(_))) {
            if let Some(Line::Task(content)) = queue.pop_front() {
                messages.push(content);
            }
        }
        messages
    }

    async fn interrupted(&self) {
        let mut stop = self.stop.subscribe();
        while !*stop.borrow_and_update() {
            if stop.changed().await.is_err() {
                return;
            }
        }
    }
}

/// Reads stdin line by line in the background, classifying every line, until
/// EOF — which queues `Line::Exit` like `exit`/`quit` do.
pub fn spawn_stdin_reader(input: &Arc<UserInput>) {
    let input = Arc::clone(input);
    tokio::spawn(async move {
        let mut reader = tokio::io::BufReader::new(tokio::io::stdin());
        loop {
            match read_line_lossy(&mut reader).await {
                Ok(Some(line)) => {
                    if let Some(line) = classify(&line) {
                        input.push(line);
                    }
                }
                Ok(None) | Err(_) => {
                    input.push(Line::Exit);
                    return;
                }
            }
        }
    });
}

/// Ctrl-C: interrupts a run in flight; at the prompt it leaves the session.
pub fn spawn_ctrl_c(input: &Arc<UserInput>) {
    let input = Arc::clone(input);
    tokio::spawn(async move {
        loop {
            if tokio::signal::ctrl_c().await.is_err() {
                return;
            }
            if input.is_running() {
                input.request_stop();
            } else {
                input.push(Line::Exit);
                return;
            }
        }
    });
}

/// Maps one raw line to a [`Line`], or `None` for blank lines.
fn classify(line: &str) -> Option<Line> {
    let trimmed = line.trim();
    match trimmed {
        "" => None,
        "exit" | "quit" => Some(Line::Exit),
        "/new" => Some(Line::Reset),
        _ => Content::try_from(trimmed).ok().map(Line::Task),
    }
}

/// Reads one line as raw bytes and decodes lossily. Input methods and
/// backspace editing can leave torn multi-byte sequences in a terminal's
/// input buffer; decoding those must degrade to replacement characters, not
/// abort the session.
pub(crate) async fn read_line_lossy<R>(reader: &mut R) -> anyhow::Result<Option<String>>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut bytes = Vec::new();
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

    fn content(text: &str) -> Content {
        Content::try_from(text).unwrap()
    }

    #[test]
    fn drain_takes_leading_messages_and_stops_at_a_command() {
        let input = UserInput::new();
        input.push(Line::Task(content("first")));
        input.push(Line::Task(content("second")));
        input.push(Line::Reset);
        input.push(Line::Task(content("after reset")));

        let drained = input.drain();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].as_str(), "first");
        assert_eq!(drained[1].as_str(), "second");

        // the command barrier and whatever follows it stay queued
        assert!(input.drain().is_empty());
    }

    #[test]
    fn classify_recognizes_commands_and_skips_blanks() {
        assert!(classify("  ").is_none());
        assert!(matches!(classify("exit"), Some(Line::Exit)));
        assert!(matches!(classify("quit"), Some(Line::Exit)));
        assert!(matches!(classify("/new"), Some(Line::Reset)));
        assert!(matches!(classify("do the thing"), Some(Line::Task(_))));
    }
}
