//! Session file persistence: the [`SessionStore`] adapter for one
//! [`SessionFile`].
//!
//! [`FileStore`] is the port implementation the session plugin's writer task
//! drives; the host wraps it in a [`SessionStoreResource`] and attaches it as
//! a component to its session's root entity (absent means memory-only
//! mode). This crate owns everything file-shaped: the file handle, sibling
//! naming for rotation, and reopening. The session plugin owns the state: the
//! log, durability, ordering, and when to flush.
//!
//! Rotation (`/new`) is a store operation: flush the current file, atomically
//! claim the next free sibling (`session.jsonl` → `session.1.jsonl`, …), and
//! open it.

use std::path::{Path, PathBuf};

use agnes_record::{Event, HostEvent, SessionError, SessionLog as _, SessionStore};
use agnes_session_file::{Opened, SessionFile};
use async_trait::async_trait;
use jiff::Timestamp;

/// One session file behind a lock. The session plugin's writer task calls
/// sequentially, so the lock never contends; it exists because the port takes
/// `&self` while the format adapter needs `&mut`.
pub struct FileStore {
    file: tokio::sync::Mutex<SessionFile>,
}

impl FileStore {
    /// Wraps an opened session file as a session store.
    pub fn new(file: SessionFile) -> Self {
        Self {
            file: tokio::sync::Mutex::new(file),
        }
    }
}

#[async_trait]
impl SessionStore for FileStore {
    async fn append(&self, at: Timestamp, event: &Event) -> Result<(), SessionError> {
        self.file.lock().await.append(at, event).await
    }

    async fn append_host(&self, at: Timestamp, event: &HostEvent) -> Result<(), SessionError> {
        self.file.lock().await.append_host(at, event).await
    }

    async fn flush(&self) -> Result<(), SessionError> {
        self.file.lock().await.flush().await
    }

    async fn rotate(&self) -> Result<PathBuf, SessionError> {
        let mut file = self.file.lock().await;
        file.flush().await?;
        let next = next_free_sibling(file.path()).await?;
        let Opened { session, .. } = SessionFile::open_or_create(&next)
            .await
            .map_err(|error| SessionError::Io(std::io::Error::other(error)))?;
        *file = session;
        Ok(next)
    }
}

/// Claims the next free sibling name of `path` (`session.jsonl` →
/// `session.1.jsonl`, …) by atomically creating it.
///
/// # Errors
///
/// Returns an error when no name can be claimed (creation failed for a
/// reason other than "already exists", or the counter is exhausted).
pub(crate) async fn next_free_sibling(path: &Path) -> std::io::Result<PathBuf> {
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
            Err(err) => return Err(err),
        }
    }
    Err(std::io::Error::other(format!(
        "no free session name for `{}`",
        path.display()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agnes_core::config::SystemPrompt;
    use agnes_core::header::{Environment, Header};

    fn header() -> Event {
        Event::Header(Header::new(
            SystemPrompt::new("prompt"),
            Environment::default(),
        ))
    }

    #[tokio::test]
    async fn rotation_claims_the_next_sibling_and_reopens() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("session.jsonl");
        let opened = SessionFile::open_or_create(&path)
            .await
            .expect("open session");
        let store = FileStore::new(opened.session);
        store
            .append(Timestamp::now(), &header())
            .await
            .expect("append");
        store.flush().await.expect("flush");

        let next = store.rotate().await.expect("rotate");
        assert_eq!(next, dir.path().join("session.1.jsonl"));
        store
            .append(Timestamp::now(), &header())
            .await
            .expect("append after rotate");
        store.flush().await.expect("flush after rotate");

        let first = std::fs::read_to_string(&path).expect("read first file");
        let second = std::fs::read_to_string(&next).expect("read second file");
        assert_eq!(first.lines().count(), 2, "meta line plus one record");
        assert_eq!(second.lines().count(), 2, "meta line plus one record");
    }
}
