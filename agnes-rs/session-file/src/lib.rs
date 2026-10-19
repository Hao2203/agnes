//! 会话日志的文件实现：JSONL、单写者、崩溃后补写恢复。
//!
//! 格式 v3（每行一个 JSON 对象；首行是版本标记，之后是两种记录）：
//!
//! ```text
//! {"kind":"meta","format":3}
//! {"kind":"core","seq":1,"time":"2026-10-19T…","event":{"header":{…}}}
//! {"kind":"host","seq":2,"time":"…","event":{"approval_requested":{…}}}
//! ```
//!
//! `core` 行是运行的真源（`Event`），`host` 行是 core 不该知道的审计记录
//! （审批问询/决定、错误中止的原因）。两者共享一个 `seq` 顺序。
//!
//! 系统提示与工具环境不是单独的一行：它们是事件（`Event::Header`），
//! 每个 turn 记一条 —— 所以日志是唯一状态，恢复不需要比对任何旁路配置。
//!
//! 规则：
//!
//! - `seq` 从 1 起严格连续（跨两种行）；不连续 = 损坏，拒绝加载（fail closed）。
//! - 读不懂的 `format` 拒绝加载，不猜；旧格式（v1/v2）同样拒载。
//! - 只有**最后一行**允许是撕裂的半行（崩溃时没写完）：丢弃并截断到物理
//!   有效前缀；其他位置的不完整行是损坏。
//! - 恢复 = 补写收尾事件（形状与 [`Turn::interrupted`] 一致），**绝不重写**
//!   已有记录。补写内容随 `Opened::repairs` 一并返回。
//! - 单写者：打开时取文件锁；已被占用即失败。
//!
//! [`Turn::interrupted`]: agnes_core::turn::Turn::interrupted

use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};

use agnes_core::event::Event;
use agnes_core::log::Log;
use agnes_core::message::{AssistantMessage, Message, ToolCall, ToolCallId, ToolOutput};
use agnes_core::turn::{Outcome, StepNumber, StepOutcome};
use agnes_io::session::{HostEvent, SessionError, SessionLog};
use async_trait::async_trait;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufWriter};

/// 本实现写出的格式版本。
pub const FORMAT: u32 = 3;

/// 未执行的调用补位时用的理由，与 core 的打断路径保持一致。
const NOT_RUN: &str = "not run: the user interrupted the agent";

#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error("session `{}` is already open by another process", path.display())]
    AlreadyOpen { path: PathBuf },
    #[error("unsupported session format {found}; this build understands {supported}")]
    UnsupportedVersion { found: u32, supported: u32 },
    #[error("corrupt session at line {line}: {detail}")]
    Corrupt { line: usize, detail: String },
    #[error("failed to encode a session record: {0}")]
    Encode(String),
    #[error("session i/o error: {0}")]
    Io(#[from] std::io::Error),
}

impl From<SessionError> for OpenError {
    fn from(err: SessionError) -> Self {
        match err {
            SessionError::Io(err) => OpenError::Io(err),
            SessionError::Format(detail) => OpenError::Encode(detail),
        }
    }
}

/// 一条宿主审计记录（审批问询/决定、失败原因），带落盘时间。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub at: Timestamp,
    pub event: HostEvent,
}

/// 打开（或新建）的结果：可写的会话句柄、恢复出的日志与审计记录、以及
/// 本次补写的收尾事件（空 = 日志本来就完整）。
pub struct Opened {
    pub session: SessionFile,
    pub log: Log,
    pub notes: Vec<Note>,
    pub repairs: Vec<Event>,
}

impl fmt::Debug for Opened {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Opened")
            .field("path", &self.session.path)
            .field("events", &self.log.events().len())
            .field("notes", &self.notes.len())
            .field("repairs", &self.repairs.len())
            .finish()
    }
}

/// 一个打开中的会话文件：单写者、追加式、显式 flush。
pub struct SessionFile {
    path: PathBuf,
    writer: BufWriter<tokio::fs::File>,
    next_seq: u64,
}

impl SessionFile {
    /// 打开或新建一个会话文件。
    ///
    /// 已存在时校验版本标记与记录连续性，然后补齐上一次崩溃留下的未收尾
    /// 事件；返回的 `log` 已经包含补写。系统提示与工具环境不再单独校验 ——
    /// 它们在日志里，随每个 turn 的 header 事件入账。
    pub async fn open_or_create(path: impl Into<PathBuf>) -> Result<Opened, OpenError> {
        let path = path.into();
        let std_file = std::fs::OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(&path)?;
        if let Err(err) = std_file.try_lock() {
            return Err(match err {
                std::fs::TryLockError::WouldBlock => OpenError::AlreadyOpen { path },
                std::fs::TryLockError::Error(err) => OpenError::Io(err),
            });
        }

        let mut reader = tokio::fs::File::from_std(std_file.try_clone()?);
        let mut raw = String::new();
        reader.read_to_string(&mut raw).await?;
        drop(reader);

        let parsed = parse(&raw)?;
        let needs_newline = parsed.valid_len > 0 && !raw[..parsed.valid_len].ends_with('\n');
        if parsed.torn {
            // 只截掉从未写过一整行的尾巴；已有记录一个字节都不动。
            std_file.set_len(parsed.valid_len as u64)?;
        }

        let mut writer = BufWriter::new(tokio::fs::File::from_std(std_file));
        if parsed.meta.is_none() {
            write_line(&mut writer, &Line::Meta(Meta { format: FORMAT })).await?;
        }
        if needs_newline {
            writer.write_all(b"\n").await?;
        }

        let mut events = Vec::new();
        let mut notes = Vec::new();
        for entry in parsed.entries {
            match entry {
                Entry::Core(record) => events.push(record.event),
                Entry::Host(record) => notes.push(Note {
                    at: record.time,
                    event: record.event,
                }),
            }
        }
        let mut session = SessionFile {
            path,
            writer,
            next_seq: (events.len() + notes.len()) as u64,
        };
        let repairs = repairs(&events);
        for repair in &repairs {
            session
                .append(Timestamp::now(), repair)
                .await
                .map_err(OpenError::from)?;
        }
        session.flush().await.map_err(OpenError::from)?;

        let mut log_events = events;
        log_events.extend(repairs.iter().cloned());
        Ok(Opened {
            session,
            log: Log::restore(log_events),
            notes,
            repairs,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[async_trait]
impl SessionLog for SessionFile {
    async fn append(&mut self, at: Timestamp, event: &Event) -> Result<(), SessionError> {
        self.next_seq += 1;
        let record = Record {
            seq: self.next_seq,
            time: at,
            event: event.clone(),
        };
        write_line(&mut self.writer, &Line::Core(record)).await
    }

    async fn append_host(&mut self, at: Timestamp, event: &HostEvent) -> Result<(), SessionError> {
        self.next_seq += 1;
        let record = HostRecord {
            seq: self.next_seq,
            time: at,
            event: event.clone(),
        };
        write_line(&mut self.writer, &Line::Host(record)).await
    }

    async fn flush(&mut self) -> Result<(), SessionError> {
        self.writer.flush().await?;
        self.writer.get_ref().sync_all().await?;
        Ok(())
    }
}

async fn write_line(
    writer: &mut BufWriter<tokio::fs::File>,
    line: &Line,
) -> Result<(), SessionError> {
    let mut bytes =
        serde_json::to_vec(line).map_err(|err| SessionError::Format(err.to_string()))?;
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    Ok(())
}

// —— 格式 ——

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Line {
    Meta(Meta),
    Core(Record),
    Host(HostRecord),
}

#[derive(Debug, Serialize, Deserialize)]
struct Meta {
    format: u32,
}

#[derive(Debug, Serialize, Deserialize)]
struct Record {
    seq: u64,
    time: Timestamp,
    event: Event,
}

#[derive(Debug, Serialize, Deserialize)]
struct HostRecord {
    seq: u64,
    time: Timestamp,
    event: HostEvent,
}

/// 一条落盘的记录（两种词汇共享 `seq` 顺序）。
enum Entry {
    Core(Record),
    Host(HostRecord),
}

impl Entry {
    fn seq(&self) -> u64 {
        match self {
            Entry::Core(record) => record.seq,
            Entry::Host(record) => record.seq,
        }
    }
}

struct Parsed {
    meta: Option<Meta>,
    entries: Vec<Entry>,
    /// 物理有效前缀的字节长度（末尾撕裂行之前）。
    valid_len: usize,
    /// 最后一行是否是被丢弃的半行。
    torn: bool,
}

fn parse(raw: &str) -> Result<Parsed, OpenError> {
    let mut meta: Option<Meta> = None;
    let mut entries: Vec<Entry> = Vec::new();
    let mut offset = 0;
    let mut valid_len = 0;
    let mut line_no = 0;

    while offset < raw.len() {
        let end = raw[offset..]
            .find('\n')
            .map(|at| offset + at + 1)
            .unwrap_or(raw.len());
        let text = raw[offset..end].trim();
        line_no += 1;

        if text.is_empty() {
            offset = end;
            valid_len = end;
            continue;
        }

        // 语法都读不通只可能是末尾撕裂的写入；能解析成 JSON 但语义不对
        // （未知 kind、坏字段、旧格式）一律拒载。
        let value: serde_json::Value = match serde_json::from_str(text) {
            Ok(value) => value,
            Err(_) if end == raw.len() => {
                return Ok(Parsed {
                    meta,
                    entries,
                    valid_len,
                    torn: true,
                });
            }
            Err(err) => {
                return Err(OpenError::Corrupt {
                    line: line_no,
                    detail: err.to_string(),
                });
            }
        };
        let line: Line = serde_json::from_value(value).map_err(|err| OpenError::Corrupt {
            line: line_no,
            detail: err.to_string(),
        })?;

        let entry = match line {
            Line::Meta(found) => {
                if meta.is_some() || !entries.is_empty() {
                    return Err(OpenError::Corrupt {
                        line: line_no,
                        detail: "the version marker must be the first line".into(),
                    });
                }
                if found.format != FORMAT {
                    return Err(OpenError::UnsupportedVersion {
                        found: found.format,
                        supported: FORMAT,
                    });
                }
                meta = Some(found);
                valid_len = end;
                offset = end;
                continue;
            }
            Line::Core(record) => Entry::Core(record),
            Line::Host(record) => Entry::Host(record),
        };

        if meta.is_none() {
            return Err(OpenError::Corrupt {
                line: line_no,
                detail: "record before the version marker".into(),
            });
        }
        let expected = entries.len() as u64 + 1;
        if entry.seq() != expected {
            return Err(OpenError::Corrupt {
                line: line_no,
                detail: format!("expected seq {expected}, found {}", entry.seq()),
            });
        }
        entries.push(entry);
        valid_len = end;
        offset = end;
    }

    Ok(Parsed {
        meta,
        entries,
        valid_len,
        torn: false,
    })
}

// —— 崩溃恢复 ——

/// 从日志推导崩溃时要补的收尾事件（与 `Turn::interrupted` 同形）：
///
/// 1. 当前 step 里模型声明过、却没有结果的调用 → `tool_execution_end`
///    + 错误 `tool_result`（按声明顺序）；
/// 2. 开着的 step → `step_end{interrupted}`；
/// 3. 开着的 turn → `turn_end{interrupted}`。
///
/// 完整收场的日志返回空。
pub fn repairs(events: &[Event]) -> Vec<Event> {
    let mut turn_open = false;
    let mut open_step: Option<StepNumber> = None;
    let mut declared: Vec<ToolCall> = Vec::new();
    let mut resulted: HashSet<ToolCallId> = HashSet::new();

    for event in events {
        match event {
            Event::TurnStart => {
                turn_open = true;
                open_step = None;
                declared.clear();
                resulted.clear();
            }
            Event::TurnEnd { .. } => {
                turn_open = false;
                open_step = None;
                declared.clear();
                resulted.clear();
            }
            Event::StepStart { step } => {
                open_step = Some(*step);
                declared.clear();
                resulted.clear();
            }
            Event::StepEnd { .. } => {
                open_step = None;
                declared.clear();
                resulted.clear();
            }
            Event::Message(Message::Assistant(AssistantMessage::ToolCalls { calls, .. })) => {
                declared = calls.to_vec();
                resulted.clear();
            }
            Event::Message(Message::ToolResult { call_id, .. }) => {
                resulted.insert(call_id.clone());
            }
            _ => {}
        }
    }

    let Some(step) = open_step else {
        return if turn_open {
            vec![Event::TurnEnd {
                outcome: Outcome::Interrupted,
            }]
        } else {
            Vec::new()
        };
    };

    let mut closers = Vec::new();
    for call in declared {
        if resulted.contains(&call.id) {
            continue;
        }
        closers.push(Event::ToolExecutionEnd {
            step,
            call_id: call.id.clone(),
            is_error: true,
        });
        closers.push(Event::Message(Message::ToolResult {
            call_id: call.id,
            output: ToolOutput::error(NOT_RUN),
        }));
    }
    closers.push(Event::StepEnd {
        step,
        outcome: StepOutcome::Interrupted,
    });
    closers.push(Event::TurnEnd {
        outcome: Outcome::Interrupted,
    });
    closers
}

#[cfg(test)]
mod tests {
    use super::*;
    use agnes_core::config::{ModelName, StepBudget, SystemPrompt};
    use agnes_core::content::Content;
    use agnes_core::header::{Environment, Header};
    use agnes_core::llm::{LlmResponse, StopReason};
    use agnes_core::log::Log;
    use agnes_core::message::{TokenUsage, ToolArguments};
    use agnes_core::tool::ToolName;
    use agnes_core::turn::{AgentConfig, AwaitingMessage, MessageReplied, ToolsAdvanced, Turn};
    use agnes_util::NonEmpty;
    use std::num::NonZeroU32;
    use std::str::FromStr;

    fn system() -> SystemPrompt {
        SystemPrompt::new("sys")
    }

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: ToolCallId::new(id),
            name: ToolName::from_str("note").unwrap(),
            arguments: ToolArguments::new("{}"),
        }
    }

    fn config() -> AgentConfig {
        AgentConfig {
            log: Log::default(),
            header: Header::new(system(), Environment::default()),
            goal: Content::try_from("take notes").unwrap(),
            budget: StepBudget::new(NonZeroU32::new(4).unwrap()),
        }
    }

    fn tool_use(calls: Vec<ToolCall>) -> LlmResponse {
        LlmResponse {
            model: ModelName::from_str("test-model").unwrap(),
            stop_reason: StopReason::ToolUse {
                content: None,
                calls: NonEmpty::from_vec(calls).unwrap(),
            },
            usage: TokenUsage::default(),
        }
    }

    /// 走到「模型要了两个调用、第一个跑完、第二个挂着」为止。
    fn half_finished_turn() -> (Vec<Event>, Vec<Event>) {
        let turn = Turn::<AwaitingMessage>::start(config());
        let MessageReplied::Tools { mut turn } =
            turn.model_responded(tool_use(vec![call("c1"), call("c2")]))
        else {
            panic!("expected a tool phase")
        };
        turn.tool_started(&ToolCallId::new("c1"));
        let ToolsAdvanced::Ongoing { turn } =
            turn.tool_finished(ToolCallId::new("c1"), ToolOutput::success("noted"))
        else {
            panic!("expected the batch to stay open")
        };

        let prefix = turn.log().events().to_vec();
        let closed = turn.interrupted();
        let closer = closed.into_log().events()[prefix.len()..].to_vec();
        (prefix, closer)
    }

    #[test]
    fn repairs_match_what_the_machine_emits_on_interrupt() {
        let (prefix, expected) = half_finished_turn();
        assert_eq!(repairs(&prefix), expected);
    }

    #[test]
    fn repairs_close_a_step_that_never_saw_a_response() {
        let turn = Turn::<AwaitingMessage>::start(config());
        let prefix = turn.log().events().to_vec();
        let closed = turn.interrupted();
        let expected = closed.into_log().events()[prefix.len()..].to_vec();
        assert_eq!(repairs(&prefix), expected);
    }

    #[test]
    fn a_finished_turn_needs_no_repairs() {
        let events = vec![
            Event::TurnStart,
            Event::TurnEnd {
                outcome: Outcome::Interrupted,
            },
        ];
        assert!(repairs(&events).is_empty());
    }

    #[tokio::test]
    async fn host_notes_interleave_with_core_events_and_survive_resume() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");

        let Opened { mut session, .. } = SessionFile::open_or_create(path.as_path()).await.unwrap();
        session
            .append(Timestamp::now(), &Event::TurnStart)
            .await
            .unwrap();
        session
            .append_host(
                Timestamp::now(),
                &HostEvent::ApprovalRequested {
                    call_id: ToolCallId::new("c1"),
                    tool: ToolName::from_str("run_shell").unwrap(),
                },
            )
            .await
            .unwrap();
        session
            .append_host(
                Timestamp::now(),
                &HostEvent::ApprovalResolved {
                    call_id: ToolCallId::new("c1"),
                    outcome: agnes_io::session::ApprovalOutcome::Rejected,
                },
            )
            .await
            .unwrap();
        // turn 没写完 —— 恢复时要补写收尾，但审计记录必须原样保留
        session.flush().await.unwrap();
        drop(session);

        let Opened {
            log,
            notes,
            repairs,
            ..
        } = SessionFile::open_or_create(path.as_path()).await.unwrap();
        assert_eq!(repairs.len(), 1);
        assert_eq!(log.events().len(), 2);
        assert_eq!(notes.len(), 2);
        assert!(matches!(
            notes[0].event,
            HostEvent::ApprovalRequested { .. }
        ));
        assert!(matches!(
            notes[1].event,
            HostEvent::ApprovalResolved {
                outcome: agnes_io::session::ApprovalOutcome::Rejected,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn a_session_round_trips_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");

        let Opened {
            mut session, log, ..
        } = SessionFile::open_or_create(path.as_path()).await.unwrap();
        assert!(log.events().is_empty());
        session
            .append(Timestamp::now(), &Event::TurnStart)
            .await
            .unwrap();
        session
            .append(
                Timestamp::now(),
                &Event::Message(Message::User {
                    content: Content::try_from("hi").unwrap(),
                }),
            )
            .await
            .unwrap();
        session
            .append(
                Timestamp::now(),
                &Event::TurnEnd {
                    outcome: Outcome::Interrupted,
                },
            )
            .await
            .unwrap();
        session.flush().await.unwrap();
        drop(session);

        let Opened {
            log,
            repairs,
            mut session,
            ..
        } = SessionFile::open_or_create(path.as_path()).await.unwrap();
        assert_eq!(log.events().len(), 3);
        assert!(repairs.is_empty());
        assert!(matches!(log.events()[0], Event::TurnStart));
        session.flush().await.unwrap();
    }

    #[tokio::test]
    async fn resuming_appends_closers_and_keeps_every_existing_byte() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let (prefix, expected) = half_finished_turn();

        let Opened { mut session, .. } = SessionFile::open_or_create(path.as_path()).await.unwrap();
        for event in &prefix {
            session.append(Timestamp::now(), event).await.unwrap();
        }
        session.flush().await.unwrap();
        drop(session);
        let before = std::fs::read_to_string(&path).unwrap();

        let Opened {
            log,
            repairs,
            mut session,
            ..
        } = SessionFile::open_or_create(path.as_path()).await.unwrap();
        assert_eq!(repairs, expected);
        assert_eq!(log.events().len(), prefix.len() + repairs.len());

        // 只追加：原文件是现在文件的前缀
        session.flush().await.unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.starts_with(&before));
    }

    #[tokio::test]
    async fn a_sequence_gap_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let raw = concat!(
            "{\"kind\":\"meta\",\"format\":3}\n",
            "{\"kind\":\"core\",\"seq\":1,\"time\":\"2026-10-19T00:00:00Z\",\"event\":\"turn_start\"}\n",
            "{\"kind\":\"core\",\"seq\":3,\"time\":\"2026-10-19T00:00:01Z\",\"event\":\"turn_start\"}\n",
        );
        std::fs::write(&path, raw).unwrap();

        let err = SessionFile::open_or_create(path.as_path())
            .await
            .unwrap_err();
        assert!(matches!(err, OpenError::Corrupt { line: 3, .. }));
    }

    #[tokio::test]
    async fn a_future_format_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        std::fs::write(&path, "{\"kind\":\"meta\",\"format\":4}\n").unwrap();

        let err = SessionFile::open_or_create(path.as_path())
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            OpenError::UnsupportedVersion {
                found: 4,
                supported: FORMAT
            }
        ));
    }

    #[tokio::test]
    async fn a_v2_session_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        std::fs::write(&path, "{\"kind\":\"meta\",\"format\":2}\n").unwrap();

        let err = SessionFile::open_or_create(path.as_path())
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            OpenError::UnsupportedVersion {
                found: 2,
                supported: FORMAT
            }
        ));
    }

    #[tokio::test]
    async fn a_v1_header_line_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let raw = concat!(
            "{\"kind\":\"header\",\"format\":1,\"system\":\"sys\",\"environment\":{\"tools\":[]}}\n",
            "{\"kind\":\"core\",\"seq\":1,\"time\":\"2026-10-19T00:00:00Z\",\"event\":\"turn_start\"}\n",
        );
        std::fs::write(&path, raw).unwrap();

        let err = SessionFile::open_or_create(path.as_path())
            .await
            .unwrap_err();
        assert!(matches!(err, OpenError::Corrupt { line: 1, .. }));
    }

    #[tokio::test]
    async fn a_new_file_starts_with_the_version_marker() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let Opened { mut session, .. } = SessionFile::open_or_create(path.as_path()).await.unwrap();
        session.flush().await.unwrap();
        drop(session);

        let raw = std::fs::read_to_string(&path).unwrap();
        assert_eq!(raw, "{\"kind\":\"meta\",\"format\":3}\n");
    }

    #[tokio::test]
    async fn a_torn_tail_is_dropped_and_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let raw = concat!(
            "{\"kind\":\"meta\",\"format\":3}\n",
            "{\"kind\":\"core\",\"seq\":1,\"time\":\"2026-10-19T00:00:00Z\",\"event\":\"turn_start\"}\n",
            "{\"kind\":\"core\",\"seq\":2",
        );
        std::fs::write(&path, raw).unwrap();

        let Opened {
            log,
            repairs,
            mut session,
            ..
        } = SessionFile::open_or_create(path.as_path()).await.unwrap();
        // 撕裂行被丢弃；剩下的半个 turn 被补上收尾
        assert_eq!(repairs.len(), 1);
        assert_eq!(log.events().len(), 2);
        assert!(matches!(log.events()[0], Event::TurnStart));
        assert!(matches!(log.events()[1], Event::TurnEnd { .. }));

        session
            .append(
                Timestamp::now(),
                &Event::Message(Message::User {
                    content: Content::try_from("after the crash").unwrap(),
                }),
            )
            .await
            .unwrap();
        session.flush().await.unwrap();
        drop(session);

        // 截断 + 追加之后仍然是一个可解析的日志
        let Opened { log, repairs, .. } =
            SessionFile::open_or_create(path.as_path()).await.unwrap();
        assert_eq!(log.events().len(), 3);
        assert!(repairs.is_empty());
    }

    #[tokio::test]
    async fn a_second_writer_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let _first = SessionFile::open_or_create(path.as_path()).await.unwrap();

        let err = SessionFile::open_or_create(path.as_path())
            .await
            .unwrap_err();
        assert!(matches!(err, OpenError::AlreadyOpen { .. }));
    }
}
