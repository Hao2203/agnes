//! 会话日志的文件实现：JSONL、单写者、崩溃后补写恢复。
//!
//! 格式 v1（每行一个 JSON 对象）：
//!
//! ```text
//! {"kind":"header","format":1,"system":"…","environment":{"tools":[…]}}
//! {"kind":"record","seq":1,"time":"2026-10-19T…","event":{…}}
//! ```
//!
//! 规则：
//!
//! - `seq` 从 1 起严格连续；不连续 = 损坏，拒绝加载（fail closed）。
//! - 读不懂的 `format` 拒绝加载，不猜。
//! - 只有**最后一行**允许是撕裂的半行（崩溃时没写完）：丢弃并截断到物理
//!   有效前缀；其他位置的不完整行是损坏。
//! - 恢复 = 补写收尾事件（形状与 [`Turn::interrupted`] 一致），**绝不重写**
//!   已有记录。补写内容随 `Opened::repairs` 一并返回。
//! - 单写者：打开时取排他文件锁；已被占用即失败。
//!
//! [`Turn::interrupted`]: agnes_core::turn::Turn::interrupted

use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};

use agnes_core::config::SystemPrompt;
use agnes_core::context::Environment;
use agnes_core::event::Event;
use agnes_core::message::{AssistantMessage, Message, ToolCall, ToolCallId, ToolOutput};
use agnes_core::turn::{Outcome, StepNumber, StepOutcome};
use agnes_io::session::{SessionError, SessionLog};
use async_trait::async_trait;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufWriter};

/// 本实现写出的格式版本。
pub const FORMAT: u32 = 1;

/// 未执行的调用补位时用的理由，与 core 的打断路径保持一致。
const NOT_RUN: &str = "not run: the user interrupted the agent";

#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error("session `{}` is already open by another process", path.display())]
    AlreadyOpen { path: PathBuf },
    #[error("unsupported session format {found}; this build understands {supported}")]
    UnsupportedVersion { found: u32, supported: u32 },
    #[error("session does not match this run: {detail}")]
    HeaderMismatch { detail: String },
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

/// 打开（或新建）的结果：可写的会话句柄、恢复出的日志、以及本次补写的
/// 收尾事件（空 = 日志本来就完整）。
pub struct Opened {
    pub session: SessionFile,
    pub events: Vec<Event>,
    pub repairs: Vec<Event>,
}

impl fmt::Debug for Opened {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Opened")
            .field("path", &self.session.path)
            .field("events", &self.events.len())
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
    /// 已存在时校验头（系统提示与工具集必须与本次运行一致）与记录连续性，
    /// 然后补齐上一次崩溃留下的未收尾事件；返回的 `events` 已经包含补写。
    pub async fn open_or_create(
        path: impl Into<PathBuf>,
        system: SystemPrompt,
        environment: Environment,
    ) -> Result<Opened, OpenError> {
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
        let mut events: Vec<Event> = parsed.records.into_iter().map(|r| r.event).collect();

        match parsed.header {
            Some(header) => check_header(&header, &system, &environment)?,
            None => {
                let header = Header {
                    format: FORMAT,
                    system,
                    environment,
                };
                write_line(&mut writer, &Line::Header(header)).await?;
            }
        }
        if needs_newline {
            writer.write_all(b"\n").await?;
        }

        let mut session = SessionFile {
            path,
            writer,
            next_seq: events.len() as u64,
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
        write_line(&mut self.writer, &Line::Record(record)).await?;
        Ok(())
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
    Header(Header),
    Record(Record),
}

#[derive(Debug, Serialize, Deserialize)]
struct Header {
    format: u32,
    system: SystemPrompt,
    environment: Environment,
}

#[derive(Debug, Serialize, Deserialize)]
struct Record {
    seq: u64,
    time: Timestamp,
    event: Event,
}

struct Parsed {
    header: Option<Header>,
    records: Vec<Record>,
    /// 物理有效前缀的字节长度（末尾撕裂行之前）。
    valid_len: usize,
    /// 最后一行是否是被丢弃的半行。
    torn: bool,
}

fn parse(raw: &str) -> Result<Parsed, OpenError> {
    let mut header: Option<Header> = None;
    let mut records: Vec<Record> = Vec::new();
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

        match serde_json::from_str::<Line>(text) {
            Ok(Line::Header(parsed_header)) => {
                if header.is_some() || !records.is_empty() {
                    return Err(OpenError::Corrupt {
                        line: line_no,
                        detail: "the header must be the first line".into(),
                    });
                }
                header = Some(parsed_header);
                valid_len = end;
            }
            Ok(Line::Record(record)) => {
                if header.is_none() {
                    return Err(OpenError::Corrupt {
                        line: line_no,
                        detail: "record before the header".into(),
                    });
                }
                let expected = records.len() as u64 + 1;
                if record.seq != expected {
                    return Err(OpenError::Corrupt {
                        line: line_no,
                        detail: format!("expected seq {expected}, found {}", record.seq),
                    });
                }
                records.push(record);
                valid_len = end;
            }
            Err(err) => {
                // 只有最后一行可能是撕裂的写入；中间的不完整行是损坏。
                if end == raw.len() {
                    return Ok(Parsed {
                        header,
                        records,
                        valid_len,
                        torn: true,
                    });
                }
                return Err(OpenError::Corrupt {
                    line: line_no,
                    detail: err.to_string(),
                });
            }
        }
        offset = end;
    }

    Ok(Parsed {
        header,
        records,
        valid_len,
        torn: false,
    })
}

fn check_header(
    header: &Header,
    system: &SystemPrompt,
    environment: &Environment,
) -> Result<(), OpenError> {
    if header.format != FORMAT {
        return Err(OpenError::UnsupportedVersion {
            found: header.format,
            supported: FORMAT,
        });
    }
    if header.system.as_str() != system.as_str() {
        return Err(OpenError::HeaderMismatch {
            detail: "the system prompt changed since the session was created".into(),
        });
    }
    let stored = tool_names(&header.environment);
    let live = tool_names(environment);
    if stored != live {
        return Err(OpenError::HeaderMismatch {
            detail: format!("the tool set changed: stored {stored:?}, live {live:?}"),
        });
    }
    Ok(())
}

fn tool_names(environment: &Environment) -> Vec<&str> {
    environment
        .tools()
        .iter()
        .map(|spec| spec.name.as_str())
        .collect()
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
    use agnes_core::config::{StepBudget, SystemPrompt};
    use agnes_core::content::Content;
    use agnes_core::context::Context;
    use agnes_core::llm::{LlmResponse, StopReason};
    use agnes_core::message::{TokenUsage, ToolArguments};
    use agnes_core::tool::{JsonSchema, ToolName, ToolSpec};
    use agnes_core::turn::{AgentConfig, AwaitingMessage, MessageReplied, ToolsAdvanced, Turn};
    use agnes_util::NonEmpty;
    use std::num::NonZeroU32;
    use std::str::FromStr;

    fn system() -> SystemPrompt {
        SystemPrompt::new("sys")
    }

    fn spec(name: &str) -> ToolSpec {
        ToolSpec {
            name: ToolName::from_str(name).unwrap(),
            description: "".into(),
            parameters: JsonSchema::try_from(serde_json::json!({"type": "object"})).unwrap(),
        }
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
            context: Context::new(system(), Environment::default()),
            goal: Content::try_from("take notes").unwrap(),
            budget: StepBudget::new(NonZeroU32::new(4).unwrap()),
        }
    }

    fn tool_use(calls: Vec<ToolCall>) -> LlmResponse {
        LlmResponse {
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

        let prefix = turn.context().events().to_vec();
        let closed = turn.interrupted();
        let closer = closed.context().events()[prefix.len()..].to_vec();
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
        let prefix = turn.context().events().to_vec();
        let closed = turn.interrupted();
        let expected = closed.context().events()[prefix.len()..].to_vec();
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
    async fn a_session_round_trips_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");

        let Opened {
            mut session,
            events,
            ..
        } = SessionFile::open_or_create(path.as_path(), system(), Environment::default())
            .await
            .unwrap();
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
        } = SessionFile::open_or_create(path.as_path(), system(), Environment::default())
            .await
            .unwrap();
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

        let Opened { mut session, .. } =
            SessionFile::open_or_create(path.as_path(), system(), Environment::default())
                .await
                .unwrap();
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
        } = SessionFile::open_or_create(path.as_path(), system(), Environment::default())
            .await
            .unwrap();
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
            "{\"kind\":\"header\",\"format\":1,\"system\":\"sys\",\"environment\":{\"tools\":[]}}\n",
            "{\"kind\":\"record\",\"seq\":1,\"time\":\"2026-10-19T00:00:00Z\",\"event\":\"turn_start\"}\n",
            "{\"kind\":\"record\",\"seq\":3,\"time\":\"2026-10-19T00:00:01Z\",\"event\":\"turn_start\"}\n",
        );
        std::fs::write(&path, raw).unwrap();

        let err = SessionFile::open_or_create(path.as_path(), system(), Environment::default())
            .await
            .unwrap_err();
        assert!(matches!(err, OpenError::Corrupt { line: 3, .. }));
    }

    #[tokio::test]
    async fn a_future_format_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        std::fs::write(
            &path,
            "{\"kind\":\"header\",\"format\":2,\"system\":\"sys\",\"environment\":{\"tools\":[]}}\n",
        )
        .unwrap();

        let err = SessionFile::open_or_create(path.as_path(), system(), Environment::default())
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
    async fn a_torn_tail_is_dropped_and_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let raw = concat!(
            "{\"kind\":\"header\",\"format\":1,\"system\":\"sys\",\"environment\":{\"tools\":[]}}\n",
            "{\"kind\":\"record\",\"seq\":1,\"time\":\"2026-10-19T00:00:00Z\",\"event\":\"turn_start\"}\n",
            "{\"kind\":\"record\",\"seq\":2",
        );
        std::fs::write(&path, raw).unwrap();

        let Opened {
            events,
            repairs,
            mut session,
        } = SessionFile::open_or_create(path.as_path(), system(), Environment::default())
            .await
            .unwrap();
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
        } = SessionFile::open_or_create(path.as_path(), system(), Environment::default())
            .await
            .unwrap();
        assert_eq!(events.len(), 3);
        assert!(repairs.is_empty());
    }

    #[tokio::test]
    async fn a_second_writer_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let _first = SessionFile::open_or_create(path.as_path(), system(), Environment::default())
            .await
            .unwrap();

        let err = SessionFile::open_or_create(path.as_path(), system(), Environment::default())
            .await
            .unwrap_err();
        assert!(matches!(err, OpenError::AlreadyOpen { .. }));
    }

    #[tokio::test]
    async fn a_changed_header_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        drop(
            SessionFile::open_or_create(path.as_path(), system(), Environment::default())
                .await
                .unwrap(),
        );

        let err = SessionFile::open_or_create(
            path.as_path(),
            SystemPrompt::new("another system prompt"),
            Environment::default(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, OpenError::HeaderMismatch { .. }));

        let err = SessionFile::open_or_create(
            path.as_path(),
            system(),
            Environment::new(vec![spec("note")]),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, OpenError::HeaderMismatch { .. }));
    }
}
