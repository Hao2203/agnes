//! 会话日志的文件实现：JSONL、单写者、崩溃后补写恢复。
//!
//! 格式 v6（每行一个 JSON 对象；首行是版本标记，之后是两种记录）：
//!
//! ```text
//! {"kind":"meta","format":6}
//! {"kind":"core","seq":1,"time":"2026-10-19T…","event":{"header":{…}}}
//! {"kind":"host","seq":2,"time":"…","event":{"approval_requested":{…}}}
//! ```
//!
//! `core` 行是运行的真源（`Event`），`host` 行是 core 不该知道的审计记录
//! （审批问询/决定、插件中止的原因、错误中止的原因）。两者共享一个 `seq` 顺序。
//!
//! 系统提示与工具环境不是单独的一行：它们是事件（`Event::Header`），
//! 每个 turn 开局与工具集变化时各记一条 —— 所以日志是唯一状态，恢复不需要
//! 比对任何旁路配置。
//!
//! 规则：
//!
//! - `seq` 从 1 起严格连续（跨两种行）；不连续 = 损坏，拒绝加载（fail closed）。
//! - 读不懂的 `format` 拒绝加载，不猜；旧格式（v1–v5）同样拒载。
//! - `event` 载荷的具体形状由私有的 `wire` 模块定义（领域类型不实现序列化）：
//!   未知变体与非法载荷一律拒载，不猜。
//! - 只有**最后一行**允许是撕裂的半行（崩溃时没写完）：丢弃并截断到物理
//!   有效前缀；其他位置的不完整行是损坏。
//! - 恢复 = 补写收尾事件（形状与 [`Agent::interrupted`] 一致），**绝不重写**
//!   已有记录。补写内容随 `Opened::repairs` 一并返回。
//! - 单写者：打开时取文件锁；已被占用即失败。
//!
//! [`Agent::interrupted`]: agnes_core::agent::Agent::interrupted

mod wire;

pub use wire::WireError;

use std::fmt;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use agnes_core::message::{AssistantMessage, Message, ToolCall, ToolOutput};
use agnes_core::turn::{Outcome, StepNumber};
use agnes_record::{Event, HostEvent, SessionError, SessionLog, StepOutcome};
use async_trait::async_trait;
use jiff::Timestamp;
use tokio::io::{AsyncWriteExt, BufWriter};

/// 本实现写出的格式版本。
pub const FORMAT: u32 = 6;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OpenError {
    #[error("session `{}` is already open by another process", path.display())]
    AlreadyOpen { path: PathBuf },
    #[error("unsupported session format {found}; this build understands {supported}")]
    UnsupportedVersion { found: u32, supported: u32 },
    #[error("corrupt session at line {line}: {reason}")]
    Corrupt {
        line: usize,
        #[source]
        reason: CorruptReason,
    },
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error("session i/o error: {0}")]
    Io(#[from] std::io::Error),
}

/// Why a session line could not be accepted.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CorruptReason {
    #[error("invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Shape(#[from] WireError),
    #[error("the version marker must be the first line")]
    MarkerNotFirst,
    #[error("record before the version marker")]
    RecordBeforeMarker,
    #[error("expected seq {expected}, found {found}")]
    Sequence { expected: u64, found: u64 },
}

/// 一条宿主审计记录（审批问询/决定、失败原因），带落盘时间。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub at: Timestamp,
    pub event: HostEvent,
}

/// 打开（或新建）的结果：可写的会话句柄、恢复出的事件序列与审计记录、
/// 以及本次补写的收尾事件（空 = 日志本来就完整）。
pub struct Opened {
    pub session: SessionFile,
    pub events: Vec<Event>,
    pub notes: Vec<Note>,
    pub repairs: Vec<Event>,
}

impl fmt::Debug for Opened {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Opened")
            .field("path", &self.session.path)
            .field("events", &self.events.len())
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
    /// 一次写失败之后，缓冲区里可能留有半行；拒绝继续写，避免把损坏
    /// 留在文件中间。
    poisoned: bool,
}

impl SessionFile {
    /// 打开或新建一个会话文件。
    ///
    /// 已存在时校验版本标记与记录连续性，然后补齐上一次崩溃留下的未收尾
    /// 事件；返回的 `events` 已经包含补写。系统提示与工具环境不再单独校验
    /// —— 它们在事件序列里，随每个 turn 的 header 事件入账。
    ///
    /// # Errors
    ///
    /// 文件被其他进程占用、格式过旧/未知、记录损坏，或底层 I/O 失败。
    pub async fn open_or_create(path: impl Into<PathBuf>) -> Result<Opened, OpenError> {
        let path = path.into();
        let open_path = path.clone();
        // 打开、加锁、整份读取、解析与撕裂尾截断都是阻塞工作；不要占着
        // executor 线程做。
        let (std_file, parsed) = tokio::task::spawn_blocking(move || {
            let std_file = std::fs::OpenOptions::new()
                .read(true)
                .append(true)
                .create(true)
                .open(&open_path)?;
            if let Err(err) = std_file.try_lock() {
                return Err(match err {
                    std::fs::TryLockError::WouldBlock => OpenError::AlreadyOpen { path: open_path },
                    std::fs::TryLockError::Error(err) => OpenError::Io(err),
                });
            }

            let mut reader = std_file.try_clone()?;
            let mut raw = String::new();
            reader.read_to_string(&mut raw)?;
            drop(reader);

            let parsed = parse(&raw)?;
            if parsed.torn {
                // 只截掉从未写过一整行的尾巴；已有记录一个字节都不动。
                let valid_len = u64::try_from(parsed.valid_len).map_err(|_| {
                    OpenError::Io(std::io::Error::other(
                        "the session file offset does not fit into u64",
                    ))
                })?;
                std_file.set_len(valid_len)?;
            }
            Ok((std_file, parsed))
        })
        .await
        .map_err(|err| OpenError::Io(std::io::Error::other(err)))??;

        let Parsed {
            meta,
            entries,
            needs_newline,
            ..
        } = parsed;
        let next_seq = entries.last().map_or(0, Entry::seq);

        let mut writer = BufWriter::new(tokio::fs::File::from_std(std_file));
        if meta.is_none() {
            write_line(
                &mut writer,
                &wire::Line::Meta(wire::Meta { format: FORMAT }),
            )
            .await?;
        }
        if needs_newline {
            writer.write_all(b"\n").await?;
        }

        let note_count = entries
            .iter()
            .filter(|entry| matches!(entry, Entry::Host { .. }))
            .count();
        let mut events = Vec::with_capacity(entries.len().saturating_sub(note_count));
        let mut notes = Vec::with_capacity(note_count);
        for entry in entries {
            match entry {
                Entry::Core { event, .. } => events.push(event),
                Entry::Host { note, .. } => notes.push(note),
            }
        }
        let mut session = SessionFile {
            path,
            writer,
            next_seq,
            poisoned: false,
        };
        let repairs = repairs(&events);
        for repair in &repairs {
            session
                .append(Timestamp::now(), repair)
                .await
                .map_err(OpenError::from)?;
        }
        session.flush().await.map_err(OpenError::from)?;

        events.extend(repairs.iter().cloned());
        Ok(Opened {
            session,
            events,
            notes,
            repairs,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Serializes and appends one record. Only a successful write advances the
    /// sequence; a failed write poisons the handle.
    async fn write_record(&mut self, line: &wire::Line) -> Result<(), SessionError> {
        if self.poisoned {
            return Err(SessionError::Poisoned);
        }
        let bytes = encode_line(line)?;
        match self.writer.write_all(&bytes).await {
            Ok(()) => Ok(()),
            Err(err) => {
                self.poisoned = true;
                Err(SessionError::Io(err))
            }
        }
    }
}

#[async_trait]
impl SessionLog for SessionFile {
    async fn append(&mut self, at: Timestamp, event: &Event) -> Result<(), SessionError> {
        let seq = self
            .next_seq
            .checked_add(1)
            .ok_or(SessionError::SequenceExhausted)?;
        let record = wire::CoreRecord {
            seq,
            time: at,
            event: event.into(),
        };
        self.write_record(&wire::Line::Core(record)).await?;
        self.next_seq = seq;
        Ok(())
    }

    async fn append_host(&mut self, at: Timestamp, event: &HostEvent) -> Result<(), SessionError> {
        let seq = self
            .next_seq
            .checked_add(1)
            .ok_or(SessionError::SequenceExhausted)?;
        let record = wire::HostRecord {
            seq,
            time: at,
            event: event.into(),
        };
        self.write_record(&wire::Line::Host(record)).await?;
        self.next_seq = seq;
        Ok(())
    }

    async fn flush(&mut self) -> Result<(), SessionError> {
        if self.poisoned {
            return Err(SessionError::Poisoned);
        }
        if let Err(err) = self.writer.flush().await {
            self.poisoned = true;
            return Err(SessionError::Io(err));
        }
        if let Err(err) = self.writer.get_ref().sync_all().await {
            self.poisoned = true;
            return Err(SessionError::Io(err));
        }
        Ok(())
    }
}

fn encode_line(line: &wire::Line) -> Result<Vec<u8>, SessionError> {
    let mut bytes = serde_json::to_vec(line).map_err(|err| SessionError::Encode(Box::new(err)))?;
    bytes.push(b'\n');
    Ok(bytes)
}

async fn write_line(
    writer: &mut BufWriter<tokio::fs::File>,
    line: &wire::Line,
) -> Result<(), SessionError> {
    let bytes = encode_line(line)?;
    writer.write_all(&bytes).await.map_err(SessionError::Io)
}

// —— 格式 ——

/// 一条落盘的记录（两种词汇共享 `seq` 顺序）—— 线形状在 [`wire`] 换回
/// 领域类型之后的样子。
enum Entry {
    Core { seq: u64, event: Event },
    Host { seq: u64, note: Note },
}

impl Entry {
    fn seq(&self) -> u64 {
        match self {
            Entry::Core { seq, .. } | Entry::Host { seq, .. } => *seq,
        }
    }
}

struct Parsed {
    meta: Option<u32>,
    entries: Vec<Entry>,
    /// 物理有效前缀的字节长度（末尾撕裂行之前）。
    valid_len: usize,
    /// 最后一行是否是被丢弃的半行。
    torn: bool,
    /// 有效前缀末尾是否缺少换行（恢复时要补一个）。
    needs_newline: bool,
}

fn account_line(line: &str, valid_len: &mut usize, last_had_newline: &mut bool) {
    *valid_len += line.len();
    *last_had_newline = line.ends_with('\n');
}

fn needs_newline(valid_len: usize, last_had_newline: bool) -> bool {
    valid_len > 0 && !last_had_newline
}

fn parse(raw: &str) -> Result<Parsed, OpenError> {
    let mut meta: Option<u32> = None;
    let mut entries: Vec<Entry> = Vec::new();
    let mut valid_len = 0;
    let mut last_had_newline = false;

    for (index, line) in raw.split_inclusive('\n').enumerate() {
        let line_no = index + 1;
        let text = line.trim();

        if text.is_empty() {
            account_line(line, &mut valid_len, &mut last_had_newline);
            continue;
        }

        // 语法都读不通只可能是末尾撕裂的写入；能解析成 JSON 但语义不对
        // （未知 kind、坏字段、旧格式）一律拒载。
        let value: serde_json::Value = match serde_json::from_str(text) {
            Ok(value) => value,
            Err(_) if !line.ends_with('\n') => {
                return Ok(Parsed {
                    meta,
                    entries,
                    valid_len,
                    torn: true,
                    needs_newline: needs_newline(valid_len, last_had_newline),
                });
            }
            Err(err) => {
                return Err(OpenError::Corrupt {
                    line: line_no,
                    reason: CorruptReason::Json(err),
                });
            }
        };
        let wire_line: wire::Line =
            serde_json::from_value(value).map_err(|err| OpenError::Corrupt {
                line: line_no,
                reason: CorruptReason::Json(err),
            })?;

        let entry = match wire_line {
            wire::Line::Meta(found) => {
                if meta.is_some() || !entries.is_empty() {
                    return Err(OpenError::Corrupt {
                        line: line_no,
                        reason: CorruptReason::MarkerNotFirst,
                    });
                }
                if found.format != FORMAT {
                    return Err(OpenError::UnsupportedVersion {
                        found: found.format,
                        supported: FORMAT,
                    });
                }
                meta = Some(found.format);
                account_line(line, &mut valid_len, &mut last_had_newline);
                continue;
            }
            wire::Line::Core(record) => Entry::Core {
                seq: record.seq,
                event: record.event.try_into().map_err(|err| OpenError::Corrupt {
                    line: line_no,
                    reason: CorruptReason::Shape(err),
                })?,
            },
            wire::Line::Host(record) => Entry::Host {
                seq: record.seq,
                note: Note {
                    at: record.time,
                    event: record.event.try_into().map_err(|err| OpenError::Corrupt {
                        line: line_no,
                        reason: CorruptReason::Shape(err),
                    })?,
                },
            },
        };

        if meta.is_none() {
            return Err(OpenError::Corrupt {
                line: line_no,
                reason: CorruptReason::RecordBeforeMarker,
            });
        }
        let expected = u64::try_from(entries.len())
            .map_err(|_| {
                OpenError::Io(std::io::Error::other(
                    "the session entry count does not fit into u64",
                ))
            })?
            .saturating_add(1);
        if entry.seq() != expected {
            return Err(OpenError::Corrupt {
                line: line_no,
                reason: CorruptReason::Sequence {
                    expected,
                    found: entry.seq(),
                },
            });
        }
        entries.push(entry);
        account_line(line, &mut valid_len, &mut last_had_newline);
    }

    Ok(Parsed {
        meta,
        entries,
        valid_len,
        torn: false,
        needs_newline: needs_newline(valid_len, last_had_newline),
    })
}

// —— 崩溃恢复 ——

/// 从日志推导崩溃时要补的收尾事件（与 `Agent::interrupted` 同形）：
///
/// 1. 当前 step 里模型声明过、却没有结果的调用 → `tool_execution_end`
///    + 错误 `tool_result`（按声明顺序）；
/// 2. 开着的 step → `step_end{interrupted}`；
/// 3. 开着的 turn → `turn_end{interrupted}`。
///
/// 完整收场的日志返回空。
pub fn repairs(events: &[Event]) -> Vec<Event> {
    struct Slot {
        call: ToolCall,
        finished: bool,
    }

    let mut turn_open = false;
    let mut open_step: Option<StepNumber> = None;
    let mut declared: Vec<Slot> = Vec::new();

    for event in events {
        match event {
            Event::TurnStart => {
                turn_open = true;
                open_step = None;
            }
            Event::TurnEnd { .. } => {
                turn_open = false;
                open_step = None;
            }
            Event::StepStart { step } => open_step = Some(*step),
            Event::StepEnd { .. } => open_step = None,
            Event::Message(Message::Assistant(AssistantMessage::ToolCalls { calls, .. })) => {
                declared = calls
                    .iter()
                    .map(|call| Slot {
                        call: call.clone(),
                        finished: false,
                    })
                    .collect();
                continue;
            }
            Event::Message(Message::ToolResult { call_id, .. }) => {
                if let Some(slot) = declared
                    .iter_mut()
                    .find(|slot| slot.call.id == *call_id && !slot.finished)
                {
                    slot.finished = true;
                }
                continue;
            }
            _ => continue,
        }
        declared.clear();
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

    let mut closers = Vec::with_capacity(declared.len().saturating_mul(2).saturating_add(2));
    for slot in declared {
        if slot.finished {
            continue;
        }
        closers.push(Event::ToolExecutionEnd {
            step,
            call_id: slot.call.id.clone(),
        });
        closers.push(Event::Message(Message::ToolResult {
            call_id: slot.call.id,
            output: ToolOutput::not_run(),
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
    use agnes_core::config::{ModelName, SystemPrompt};
    use agnes_core::content::Content;
    use agnes_core::header::{Environment, Header};
    use agnes_core::llm::StopKind;
    use agnes_core::message::{TokenUsage, ToolArguments, ToolCallId};
    use agnes_core::tool::ToolName;
    use agnes_record::ApprovalOutcome;
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

    fn step(number: u32) -> StepNumber {
        StepNumber::new(NonZeroU32::new(number).unwrap())
    }

    fn goal() -> Message {
        Message::User {
            content: Content::try_from("take notes").unwrap(),
        }
    }

    /// 驱动在 turn 开局 + 第一个 step 开始时记录的事件。
    fn started(events: &mut Vec<Event>) {
        events.push(Event::Header(Header::new(system(), Environment::default())));
        events.push(Event::Message(goal()));
        events.push(Event::TurnStart);
        events.push(Event::StepStart { step: step(1) });
        events.push(Event::MessageStart { step: step(1) });
    }

    /// 写到「模型要了两个调用、第一个跑完、第二个挂着」为止。
    fn half_finished_turn() -> (Vec<Event>, Vec<Event>) {
        let mut prefix = Vec::new();
        started(&mut prefix);
        prefix.push(Event::Message(Message::Assistant(
            AssistantMessage::ToolCalls {
                content: None,
                calls: NonEmpty::from_vec(vec![call("c1"), call("c2")]).unwrap(),
            },
        )));
        prefix.push(Event::MessageEnd {
            step: step(1),
            model: ModelName::from_str("test-model").unwrap(),
            stop: StopKind::ToolUse,
            usage: TokenUsage::default(),
        });
        prefix.push(Event::ToolExecutionStart {
            step: step(1),
            call: call("c1"),
        });
        prefix.push(Event::ToolExecutionEnd {
            step: step(1),
            call_id: ToolCallId::new("c1"),
        });
        prefix.push(Event::Message(Message::ToolResult {
            call_id: ToolCallId::new("c1"),
            output: ToolOutput::success("noted"),
        }));

        let expected = vec![
            Event::ToolExecutionEnd {
                step: step(1),
                call_id: ToolCallId::new("c2"),
            },
            Event::Message(Message::ToolResult {
                call_id: ToolCallId::new("c2"),
                output: ToolOutput::not_run(),
            }),
            Event::StepEnd {
                step: step(1),
                outcome: StepOutcome::Interrupted,
            },
            Event::TurnEnd {
                outcome: Outcome::Interrupted,
            },
        ];
        (prefix, expected)
    }

    #[test]
    fn repairs_match_what_the_driver_emits_on_interrupt() {
        let (prefix, expected) = half_finished_turn();
        assert_eq!(repairs(&prefix), expected);
    }

    #[test]
    fn repairs_close_a_step_that_never_saw_a_response() {
        let mut prefix = Vec::new();
        started(&mut prefix);
        let expected = vec![
            Event::StepEnd {
                step: step(1),
                outcome: StepOutcome::Interrupted,
            },
            Event::TurnEnd {
                outcome: Outcome::Interrupted,
            },
        ];
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
                    outcome: ApprovalOutcome::Rejected,
                },
            )
            .await
            .unwrap();
        session
            .append_host(
                Timestamp::now(),
                &HostEvent::Aborted {
                    by: "policy".into(),
                    reason: "stop here".into(),
                },
            )
            .await
            .unwrap();
        // turn 没写完 —— 恢复时要补写收尾，但审计记录必须原样保留
        session.flush().await.unwrap();
        drop(session);

        let Opened {
            events,
            notes,
            repairs,
            ..
        } = SessionFile::open_or_create(path.as_path()).await.unwrap();
        assert_eq!(repairs.len(), 1);
        assert_eq!(events.len(), 2);
        assert_eq!(notes.len(), 3);
        assert!(matches!(
            notes[0].event,
            HostEvent::ApprovalRequested { .. }
        ));
        assert!(matches!(
            notes[1].event,
            HostEvent::ApprovalResolved {
                outcome: ApprovalOutcome::Rejected,
                ..
            }
        ));
        assert!(matches!(
            &notes[2].event,
            HostEvent::Aborted { by, reason } if by.as_str() == "policy" && reason == "stop here"
        ));
    }

    #[tokio::test]
    async fn a_session_round_trips_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");

        let Opened {
            mut session,
            events,
            ..
        } = SessionFile::open_or_create(path.as_path()).await.unwrap();
        assert!(events.is_empty());
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
            events,
            repairs,
            mut session,
            ..
        } = SessionFile::open_or_create(path.as_path()).await.unwrap();
        assert_eq!(events.len(), 3);
        assert!(repairs.is_empty());
        assert!(matches!(events[0], Event::TurnStart));
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
            events,
            repairs,
            mut session,
            ..
        } = SessionFile::open_or_create(path.as_path()).await.unwrap();
        assert_eq!(repairs, expected);
        assert_eq!(events.len(), prefix.len() + repairs.len());

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
            "{\"kind\":\"meta\",\"format\":6}\n",
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
    async fn invalid_payloads_are_refused() {
        // 线载荷换回领域类型时 fail closed：非法载荷与未知变体都拒载，
        // 并且报出准确的行号。
        let cases = [
            // 空内容
            r#"{"kind":"core","seq":1,"time":"2026-10-19T00:00:00Z","event":{"message":{"user":{"content":""}}}}"#,
            // 空工具调用集合 —— NonEmpty 的构造器不可绕过
            r#"{"kind":"core","seq":1,"time":"2026-10-19T00:00:00Z","event":{"message":{"assistant":{"tool_calls":{"content":null,"calls":[]}}}}}"#,
            // step 编号不能为 0
            r#"{"kind":"core","seq":1,"time":"2026-10-19T00:00:00Z","event":{"step_start":{"step":0}}}"#,
            // 工具名不能带空白
            r#"{"kind":"core","seq":1,"time":"2026-10-19T00:00:00Z","event":{"tool_execution_start":{"step":1,"call":{"id":"c1","name":"a b","arguments":"{}"}}}}"#,
            // 参数模式必须是对象
            r#"{"kind":"core","seq":1,"time":"2026-10-19T00:00:00Z","event":{"header":{"system":"sys","environment":{"tools":[{"name":"note","description":"","parameters":[1,2]}]}}}}"#,
            // 未知事件变体：读不懂就拒载，不静默丢弃
            r#"{"kind":"core","seq":1,"time":"2026-10-19T00:00:00Z","event":{"future_event":{}}}"#,
        ];
        for case in cases {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("s.jsonl");
            std::fs::write(
                &path,
                format!("{{\"kind\":\"meta\",\"format\":6}}\n{case}\n"),
            )
            .unwrap();

            let err = SessionFile::open_or_create(path.as_path())
                .await
                .unwrap_err();
            assert!(
                matches!(&err, OpenError::Corrupt { line: 2, .. }),
                "expected a corrupt line 2 for {case}, got {err:?}"
            );
        }
    }

    #[tokio::test]
    async fn corrupt_reasons_are_typed() {
        let dir = tempfile::tempdir().unwrap();
        let bad_json = dir.path().join("bad-json.jsonl");
        std::fs::write(&bad_json, "{\"kind\":\"meta\",\"format\":6}\nnot json\n").unwrap();
        let err = SessionFile::open_or_create(bad_json.as_path())
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            OpenError::Corrupt {
                reason: CorruptReason::Json(_),
                ..
            }
        ));

        let bad_shape = dir.path().join("bad-shape.jsonl");
        std::fs::write(
            &bad_shape,
            "{\"kind\":\"meta\",\"format\":6}\n\
             {\"kind\":\"core\",\"seq\":1,\"time\":\"2026-10-19T00:00:00Z\",\
             \"event\":{\"message\":{\"assistant\":{\"tool_calls\":{\"content\":null,\"calls\":[]}}}}}\n",
        )
        .unwrap();
        let err = SessionFile::open_or_create(bad_shape.as_path())
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            OpenError::Corrupt {
                reason: CorruptReason::Shape(WireError::EmptyToolCalls),
                ..
            }
        ));
    }

    #[tokio::test]
    async fn a_poisoned_session_refuses_to_advance_the_sequence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let Opened { mut session, .. } = SessionFile::open_or_create(&path).await.unwrap();
        let before = session.next_seq;
        session.poisoned = true;

        let err = session
            .append(Timestamp::now(), &Event::TurnStart)
            .await
            .unwrap_err();
        assert!(matches!(err, SessionError::Poisoned));
        assert_eq!(session.next_seq, before);
        assert!(matches!(
            session.flush().await.unwrap_err(),
            SessionError::Poisoned
        ));
    }

    #[tokio::test]
    async fn a_future_format_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        std::fs::write(&path, "{\"kind\":\"meta\",\"format\":7}\n").unwrap();

        let err = SessionFile::open_or_create(path.as_path())
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            OpenError::UnsupportedVersion {
                found: 7,
                supported: FORMAT
            }
        ));
    }

    #[tokio::test]
    async fn a_v5_session_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        std::fs::write(&path, "{\"kind\":\"meta\",\"format\":5}\n").unwrap();

        let err = SessionFile::open_or_create(path.as_path())
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            OpenError::UnsupportedVersion {
                found: 5,
                supported: FORMAT
            }
        ));
    }

    #[tokio::test]
    async fn a_v4_session_is_refused() {
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
    async fn a_v3_session_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        std::fs::write(&path, "{\"kind\":\"meta\",\"format\":3}\n").unwrap();

        let err = SessionFile::open_or_create(path.as_path())
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            OpenError::UnsupportedVersion {
                found: 3,
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
        assert_eq!(raw, "{\"kind\":\"meta\",\"format\":6}\n");
    }

    #[tokio::test]
    async fn a_torn_tail_is_dropped_and_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let raw = concat!(
            "{\"kind\":\"meta\",\"format\":6}\n",
            "{\"kind\":\"core\",\"seq\":1,\"time\":\"2026-10-19T00:00:00Z\",\"event\":\"turn_start\"}\n",
            "{\"kind\":\"core\",\"seq\":2",
        );
        std::fs::write(&path, raw).unwrap();

        let Opened {
            events,
            repairs,
            mut session,
            ..
        } = SessionFile::open_or_create(path.as_path()).await.unwrap();
        // 撕裂行被丢弃；剩下的半个 turn 被补上收尾
        assert_eq!(repairs.len(), 1);
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], Event::TurnStart));
        assert!(matches!(events[1], Event::TurnEnd { .. }));

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
        let Opened {
            events, repairs, ..
        } = SessionFile::open_or_create(path.as_path()).await.unwrap();
        assert_eq!(events.len(), 3);
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
