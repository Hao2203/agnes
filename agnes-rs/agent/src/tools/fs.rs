//! Built-in filesystem tools, sandboxed under a root directory. Async via
//! `tokio::fs`.

use std::path::{Component, Path, PathBuf};
use std::str::FromStr;

use agnes_core::message::{ToolArguments, ToolOutput};
use agnes_core::tool::{ToolError, ToolName, ToolSpec};
use agnes_io::tool::Tool;
use async_trait::async_trait;
use tokio::io::AsyncReadExt as _;

use super::text::{MAX_OUTPUT_BYTES, MAX_OUTPUT_CHARS, MAX_READ_BYTES, truncate};

/// Resolves `relative` under `root`, rejecting absolute paths and any `..`
/// that would climb out of the root. Symlink escapes are not followed up on
/// here — the root is expected to be a trusted directory.
fn resolve_under(root: &Path, relative: &str) -> Result<PathBuf, ToolError> {
    let rel = Path::new(relative);
    if rel.is_absolute() {
        return Err(ToolError::AbsolutePath {
            path: relative.to_owned(),
        });
    }
    let mut parts: Vec<std::ffi::OsString> = Vec::with_capacity(rel.components().count());
    for component in rel.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => parts.push(part.to_owned()),
            Component::ParentDir => {
                if parts.pop().is_none() {
                    return Err(ToolError::PathEscape {
                        path: relative.to_owned(),
                    });
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(ToolError::AbsolutePath {
                    path: relative.to_owned(),
                });
            }
        }
    }
    let mut resolved = root.to_path_buf();
    for part in parts {
        resolved.push(part);
    }
    Ok(resolved)
}

fn spec(
    name: &str,
    description: &str,
    properties: serde_json::Value,
    required: &[&str],
) -> ToolSpec {
    ToolSpec::new(
        ToolName::from_str(name).expect("built-in tool names are valid"),
        description,
        serde_json::json!({
            "type": "object",
            "properties": properties,
            "required": required,
        })
        .try_into()
        .expect("built-in schemas are objects"),
    )
}

/// Reads a text file under the root directory.
pub struct ReadFile {
    root: PathBuf,
    spec: ToolSpec,
}

impl ReadFile {
    /// # Panics
    ///
    /// Panics only if the built-in tool name or schema is invalid (a
    /// programming error in this crate).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            spec: spec(
                "read_file",
                "Read a text file relative to the working directory and return its contents.",
                serde_json::json!({
                    "path": { "type": "string", "description": "Relative file path" }
                }),
                &["path"],
            ),
        }
    }
}

#[derive(serde::Deserialize)]
struct PathArgs {
    path: String,
}

#[async_trait]
impl Tool for ReadFile {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, arguments: &ToolArguments) -> Result<ToolOutput, ToolError> {
        let args: PathArgs = arguments
            .parse()
            .map_err(|source| ToolError::BadArguments { source })?;
        let path = resolve_under(&self.root, &args.path)?;
        let file = tokio::fs::File::open(&path)
            .await
            .map_err(|source| ToolError::Io {
                action: "cannot open file",
                path: args.path.clone(),
                source,
            })?;
        let metadata = file.metadata().await.map_err(|source| ToolError::Io {
            action: "cannot stat file",
            path: args.path.clone(),
            source,
        })?;
        // UTF-8 needs at most four bytes per character; reading this prefix is
        // enough to fill the character budget without slurping a huge file.
        let limit = metadata.len().min(MAX_READ_BYTES);
        let mut bytes =
            Vec::with_capacity(usize::try_from(limit).expect("the read cap fits into usize"));
        let mut reader = file.take(limit);
        reader
            .read_to_end(&mut bytes)
            .await
            .map_err(|source| ToolError::Io {
                action: "cannot read file",
                path: args.path.clone(),
                source,
            })?;
        let content = match String::from_utf8(bytes) {
            Ok(content) => content,
            Err(err) => String::from_utf8_lossy(err.as_bytes()).into_owned(),
        };
        Ok(ToolOutput::success(truncate(content)))
    }
}

/// Creates or overwrites a text file under the root directory (parent
/// directories are created as needed).
pub struct WriteFile {
    root: PathBuf,
    spec: ToolSpec,
}

impl WriteFile {
    /// # Panics
    ///
    /// Panics only if the built-in tool name or schema is invalid (a
    /// programming error in this crate).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            spec: spec(
                "write_file",
                "Create or overwrite a text file relative to the working directory. Parent directories are created as needed.",
                serde_json::json!({
                    "path": { "type": "string", "description": "Relative file path" },
                    "content": { "type": "string", "description": "Full file contents" }
                }),
                &["path", "content"],
            ),
        }
    }
}

#[derive(serde::Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
}

#[async_trait]
impl Tool for WriteFile {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, arguments: &ToolArguments) -> Result<ToolOutput, ToolError> {
        let args: WriteArgs = arguments
            .parse()
            .map_err(|source| ToolError::BadArguments { source })?;
        let path = resolve_under(&self.root, &args.path)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|source| ToolError::Io {
                    action: "cannot create directory for",
                    path: args.path.clone(),
                    source,
                })?;
        }
        tokio::fs::write(&path, &args.content)
            .await
            .map_err(|source| ToolError::Io {
                action: "cannot write",
                path: args.path.clone(),
                source,
            })?;
        Ok(ToolOutput::success(format!(
            "wrote {} bytes to `{}`",
            args.content.len(),
            args.path
        )))
    }
}

/// Lists a directory under the root (defaults to the root itself).
pub struct ListDir {
    root: PathBuf,
    spec: ToolSpec,
}

impl ListDir {
    /// # Panics
    ///
    /// Panics only if the built-in tool name or schema is invalid (a
    /// programming error in this crate).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            spec: spec(
                "list_dir",
                "List the entries of a directory relative to the working directory. Directories are suffixed with `/`.",
                serde_json::json!({
                    "path": { "type": "string", "description": "Relative directory path (default: `.`)" }
                }),
                &[],
            ),
        }
    }
}

#[derive(serde::Deserialize, Default)]
struct ListArgs {
    path: Option<String>,
}

#[async_trait]
impl Tool for ListDir {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, arguments: &ToolArguments) -> Result<ToolOutput, ToolError> {
        const MAX_ENTRIES: usize = 10_000;

        let args: ListArgs = arguments
            .parse()
            .map_err(|source| ToolError::BadArguments { source })?;
        let display = args.path.as_deref().unwrap_or(".");
        let path = resolve_under(&self.root, display)?;
        let mut reader = tokio::fs::read_dir(&path)
            .await
            .map_err(|source| ToolError::Io {
                action: "cannot list",
                path: display.to_owned(),
                source,
            })?;
        let mut entries: Vec<String> = Vec::with_capacity(16);
        let mut capped = false;
        while let Some(entry) = reader.next_entry().await.map_err(|source| ToolError::Io {
            action: "cannot list",
            path: display.to_owned(),
            source,
        })? {
            if entries.len() >= MAX_ENTRIES {
                capped = true;
                break;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            // A failed stat degrades to "not a directory"; the entry is still
            // listed. Only read_dir failures abort the listing.
            if entry
                .file_type()
                .await
                .map(|kind| kind.is_dir())
                .unwrap_or(false)
            {
                entries.push(format!("{name}/"));
            } else {
                entries.push(name);
            }
        }
        entries.sort();
        if entries.is_empty() {
            Ok(ToolOutput::success("(empty directory)"))
        } else {
            let mut output =
                String::with_capacity(MAX_OUTPUT_CHARS.min(entries.len().saturating_mul(32)));
            for entry in entries {
                let needed = output.len().saturating_add(entry.len()).saturating_add(1);
                if needed > MAX_OUTPUT_BYTES {
                    capped = true;
                    break;
                }
                if !output.is_empty() {
                    output.push('\n');
                }
                output.push_str(&entry);
            }
            if capped {
                output.push_str("\n... (output truncated)");
            }
            Ok(ToolOutput::success(truncate(output)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_under_rejects_escapes() {
        let root = Path::new("/tmp/project");
        assert_eq!(
            resolve_under(root, "src/main.rs").unwrap(),
            PathBuf::from("/tmp/project/src/main.rs")
        );
        assert_eq!(
            resolve_under(root, "./src/../lib.rs").unwrap(),
            PathBuf::from("/tmp/project/lib.rs")
        );
        assert!(resolve_under(root, "../outside").is_err());
        assert!(resolve_under(root, "/etc/passwd").is_err());
        assert!(resolve_under(root, "a/../../escape").is_err());
    }

    #[tokio::test]
    async fn read_write_list_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let write = WriteFile::new(dir.path());
        let out = write
            .run(&ToolArguments::new(
                r#"{"path": "sub/a.txt", "content": "hello"}"#,
            ))
            .await
            .unwrap();
        assert!(!out.is_error());

        let read = ReadFile::new(dir.path());
        let out = read
            .run(&ToolArguments::new(r#"{"path": "sub/a.txt"}"#))
            .await
            .unwrap();
        assert_eq!(out.content(), "hello");

        let list = ListDir::new(dir.path());
        let out = list.run(&ToolArguments::new("{}")).await.unwrap();
        assert_eq!(out.content(), "sub/");
    }

    #[tokio::test]
    async fn read_file_truncates_large_output() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("large.txt"), "a".repeat(150_000)).unwrap();
        let read = ReadFile::new(dir.path());
        let out = read
            .run(&ToolArguments::new(r#"{"path": "large.txt"}"#))
            .await
            .unwrap();
        assert!(out.content().ends_with("\n... (output truncated)"));
        assert_eq!(
            out.content().chars().count(),
            100_000 + "\n... (output truncated)".chars().count()
        );
    }

    #[tokio::test]
    async fn missing_file_reports_error_output() {
        let dir = tempfile::tempdir().unwrap();
        let read = ReadFile::new(dir.path());
        let err = read
            .run(&ToolArguments::new(r#"{"path": "nope.txt"}"#))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Io { .. }));
    }
}
