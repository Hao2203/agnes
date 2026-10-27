//! 会话文件的 CLI 包装：打开/恢复、`/new` 轮转、保存活动日志。

use std::path::{Path, PathBuf};

use agnes_agent::record::Event as RecordEvent;
use agnes_agent::{Log, SessionLog};
use agnes_session_file::{Note, Opened, SessionFile};
use anyhow::{Context, Result, bail};

pub struct ChatSession {
    path: PathBuf,
    file: SessionFile,
    log: Log,
}

pub struct LoadedSession {
    pub session: ChatSession,
    pub notes: Vec<Note>,
    pub repairs: Vec<RecordEvent>,
}

impl ChatSession {
    pub async fn open(path: PathBuf) -> Result<LoadedSession> {
        let Opened {
            session,
            events,
            notes,
            repairs,
        } = SessionFile::open_or_create(&path)
            .await
            .with_context(|| format!("opening session `{}`", path.display()))?;
        let log = restore_log(&path, events)?;
        Ok(LoadedSession {
            session: Self {
                path,
                file: session,
                log,
            },
            notes,
            repairs,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn log(&self) -> &Log {
        &self.log
    }

    pub fn set_log(&mut self, log: Log) {
        self.log = log;
    }

    pub fn file_mut(&mut self) -> &mut SessionFile {
        &mut self.file
    }

    pub async fn flush(&mut self) -> Result<()> {
        self.file.flush().await.context("flushing the session")
    }

    /// `/new`: flush the old file and atomically claim the next sibling name.
    pub async fn rotate(mut self) -> Result<LoadedSession> {
        self.flush().await?;
        let next = next_free_sibling(&self.path).await?;
        match Self::open(next.clone()).await {
            Ok(loaded) => Ok(loaded),
            Err(err) => {
                let _ = tokio::fs::remove_file(&next).await;
                Err(err)
            }
        }
    }
}

fn restore_log(path: &Path, events: Vec<RecordEvent>) -> Result<Log> {
    if events
        .iter()
        .any(|event| matches!(event, RecordEvent::Header(_)))
    {
        return Ok(Log::restore(events));
    }
    if events.is_empty() {
        return Ok(Log::default());
    }
    bail!("session `{}` has records but no header", path.display())
}

async fn next_free_sibling(path: &Path) -> Result<PathBuf> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("session");
    let extension = path.extension().and_then(|extension| extension.to_str());
    for n in 1..=u32::MAX {
        let name = match extension {
            Some(extension) => format!("{stem}.{n}.{extension}"),
            None => format!("{stem}.{n}"),
        };
        let candidate = parent.join(name);
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
            .await
        {
            Ok(file) => {
                drop(file);
                return Ok(candidate);
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("cannot create `{}`", candidate.display()));
            }
        }
    }
    bail!("no free session name for `{}`", path.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rotation_creates_the_next_free_sibling() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        std::fs::write(&path, "").unwrap();

        let first = next_free_sibling(&path).await.unwrap();
        assert_eq!(first, dir.path().join("session.1.jsonl"));
        assert!(first.exists());

        let second = next_free_sibling(&path).await.unwrap();
        assert_eq!(second, dir.path().join("session.2.jsonl"));
        assert!(second.exists());
    }

    #[tokio::test]
    async fn a_new_session_starts_with_an_empty_log() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let loaded = ChatSession::open(path.clone()).await.unwrap();
        assert!(loaded.session.log().events().is_empty());
        assert!(loaded.session.path().exists());
        assert!(loaded.repairs.is_empty());
        assert!(loaded.notes.is_empty());
    }
}
