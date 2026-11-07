//! Interactive input: a background stdin reader that classifies lines and
//! injects them as ECS messages, answer mode for approval questions, and
//! Ctrl-C handling.
//!
//! The reader never blocks the ECS: everything it does is sending messages
//! on the [`RuntimeSender`] or cancelling the [`CancelToken`].

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use agnes_core::config::{ModelName, Thinking};
use agnes_core::content::Content;
use agnes_ecs::Entity;
use tokio::io::AsyncBufReadExt;
use tokio::sync::{Notify, watch};
use tokio::task::JoinHandle;

use crate::app::HostChannels;
use crate::render::Renderer;

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
    /// `/model` — ask the provider for its model list.
    ListModels,
    /// `/model <name>` — switch to a named model.
    SetModel(ModelName),
    /// `/think` — open the thinking picker (TUI); plain prints usage.
    OpenThinking,
    /// `/think <off|low|medium|high>` — set the thinking level.
    SetThinking(Thinking),
    /// A recognized command with an invalid argument; echoed instead of
    /// being sent to the model.
    BadArguments(String),
}

/// Shared input state: answer mode for approval questions, the run flag
/// the signal handler and the stdin reader watch, and the agent the
/// host tasks address their messages to.
pub struct UserInput {
    /// 审批问询期间：读到的行不分类，原样进答覆队列。
    answering: AtomicBool,
    answers: Mutex<VecDeque<String>>,
    answer_wake: Notify,
    running: watch::Sender<bool>,
    /// 目标 agent 实体：装配完成后由 `run()` 绑定；宿主任务发送
    /// SetModel / SetThinking / InterruptTurn 前从这里取。
    agent: Mutex<Option<Entity>>,
}

impl UserInput {
    pub fn new() -> Arc<Self> {
        let (running, _) = watch::channel(false);
        Arc::new(Self {
            answering: AtomicBool::new(false),
            answers: Mutex::new(VecDeque::new()),
            answer_wake: Notify::new(),
            running,
            agent: Mutex::new(None),
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

    /// Binds the agent the host tasks address; called once after the
    /// app is assembled.
    pub fn set_agent(&self, agent: Entity) {
        *lock(&self.agent) = Some(agent);
    }

    /// The bound agent, if assembly has finished.
    pub(crate) fn agent(&self) -> Option<Entity> {
        *lock(&self.agent)
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
pub fn spawn_stdin_reader(
    input: Arc<UserInput>,
    channels: Arc<HostChannels>,
    renderer: Renderer,
) -> JoinHandle<()> {
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
                        let _ = channels.sender.send_message(agnes_plugin_session::RotateSession {
                            agent: input.agent().expect("the host bound the agent"),
                        });
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
                                            .send_message(agnes_plugin_session::RotateSession {
                                                agent: input
                                                    .agent()
                                                    .expect("the host bound the agent"),
                                            });
                                    }
                                }
                                Some(Line::Task(content)) => {
                                    let _ = channels.sender.send_message(TaskLine { content });
                                }
                                Some(Line::ListModels) => {
                                    renderer.line("(fetching models…)");
                                    let _ =
                                        channels.sender.send_message(agnes_io::llm::ListModels);
                                }
                                Some(Line::SetModel(model)) => {
                                    if let Some(agent) = input.agent() {
                                        let _ = channels
                                            .sender
                                            .send_message(agnes_agent::model::SetModel {
                                                agent,
                                                model,
                                            });
                                    }
                                }
                                Some(Line::OpenThinking) => {
                                    renderer.line("thinking: /think off|low|medium|high");
                                }
                                Some(Line::SetThinking(thinking)) => {
                                    if let Some(agent) = input.agent() {
                                        let _ = channels
                                            .sender
                                            .send_message(agnes_agent::model::SetThinking {
                                                agent,
                                                thinking,
                                            });
                                    }
                                }
                                Some(Line::BadArguments(command)) => {
                                    renderer.line(format!("invalid arguments: {command}"));
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
                if let Some(agent) = input.agent() {
                    let _ = channels
                        .sender
                        .send_message(agnes_agent::runtime::InterruptTurn { agent });
                }
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
    if trimmed.is_empty() {
        return None;
    }
    if matches!(trimmed, "exit" | "quit") {
        return Some(Line::Exit);
    }
    if trimmed == "/new" {
        return Some(Line::Reset);
    }
    if let Some(argument) = command_argument(trimmed, "/model") {
        return Some(match argument {
            None => Line::ListModels,
            Some(name) => match name.parse::<ModelName>() {
                Ok(model) => Line::SetModel(model),
                Err(_) => Line::BadArguments(trimmed.to_owned()),
            },
        });
    }
    if let Some(argument) = command_argument(trimmed, "/think") {
        return Some(match argument {
            None => Line::OpenThinking,
            Some(level) => match level.parse::<Thinking>() {
                Ok(thinking) => Line::SetThinking(thinking),
                Err(_) => Line::BadArguments(trimmed.to_owned()),
            },
        });
    }
    // 其余行（含拼错的 `/xxx`）都是任务：会话中每一行都是一个任务。
    Content::try_from(trimmed).ok().map(Line::Task)
}

/// Splits `line` into the argument of `command`, if the command matches.
/// `Some(None)` means the command had no argument; `None` means the line is
/// a different command (or a longer word starting with the same prefix).
fn command_argument<'a>(line: &'a str, command: &str) -> Option<Option<&'a str>> {
    let rest = line.strip_prefix(command)?;
    if rest.is_empty() {
        return Some(None);
    }
    let argument = rest.strip_prefix(char::is_whitespace)?.trim();
    Some((!argument.is_empty()).then_some(argument))
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
        assert!(matches!(classify("/model"), Some(Line::ListModels)));
        assert!(
            matches!(classify("  /model  gpt-4o  "), Some(Line::SetModel(model)) if model.as_str() == "gpt-4o")
        );
        assert!(matches!(classify("/think"), Some(Line::OpenThinking)));
        assert!(matches!(
            classify("/think high"),
            Some(Line::SetThinking(agnes_core::config::Thinking::On(
                agnes_core::config::ReasoningEffort::High
            )))
        ));
        assert!(matches!(
            classify("/think off"),
            Some(Line::SetThinking(agnes_core::config::Thinking::Off))
        ));
        assert!(matches!(
            classify("/think loud"),
            Some(Line::BadArguments(_))
        ));
        assert!(matches!(
            classify("/think HIGH"),
            Some(Line::BadArguments(_))
        ));
        // 拼错的命令不是命令：会话中每一行都是一个任务。
        assert!(matches!(classify("/modelfoo"), Some(Line::Task(_))));
        assert!(matches!(classify("/nope"), Some(Line::Task(_))));
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
