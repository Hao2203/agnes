//! Built-in filesystem tools, sandboxed under a root directory. Async via
//! `tokio::fs`.

use std::path::{Component, Path, PathBuf};
use std::str::FromStr;

use agnes_core::message::{ToolArguments, ToolOutput};
use agnes_core::tool::{ToolError, ToolName, ToolSpec};
use agnes_ports::tool::Tool;
use async_trait::async_trait;

/// Largest tool output handed back to the model, in characters.
const MAX_OUTPUT_CHARS: usize = 100_000;

/// Resolves `relative` under `root`, rejecting absolute paths and any `..`
/// that would climb out of the root. Symlink escapes are not followed up on
/// here — the root is expected to be a trusted directory.
fn resolve_under(root: &Path, relative: &str) -> Result<PathBuf, ToolError> {
    let rel = Path::new(relative);
    if rel.is_absolute() {
        return Err(ToolError::BadArguments {
            detail: format!("path must be relative to the working directory: {relative}"),
        });
    }
    let mut parts: Vec<std::ffi::OsString> = Vec::new();
    for component in rel.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => parts.push(part.to_owned()),
            Component::ParentDir => {
                if parts.pop().is_none() {
                    return Err(ToolError::BadArguments {
                        detail: format!("path escapes the working directory: {relative}"),
                    });
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(ToolError::BadArguments {
                    detail: format!("path must be relative to the working directory: {relative}"),
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

fn truncate(content: String) -> String {
    if content.chars().count() <= MAX_OUTPUT_CHARS {
        return content;
    }
    let mut out: String = content.chars().take(MAX_OUTPUT_CHARS).collect();
    out.push_str("\n... (output truncated)");
    out
}

fn spec(
    name: &str,
    description: &str,
    properties: serde_json::Value,
    required: &[&str],
) -> ToolSpec {
    ToolSpec {
        name: ToolName::from_str(name).expect("built-in tool names are valid"),
        description: description.into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": properties,
            "required": required,
        })
        .try_into()
        .expect("built-in schemas are objects"),
    }
}

/// Reads a text file under the root directory.
pub struct ReadFile {
    root: PathBuf,
    spec: ToolSpec,
}

impl ReadFile {
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
        let args: PathArgs = arguments.parse().map_err(|e| ToolError::BadArguments {
            detail: e.to_string(),
        })?;
        let path = resolve_under(&self.root, &args.path)?;
        let content = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| ToolError::Failed {
                detail: format!("cannot read `{}`: {e}", args.path),
            })?;
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
        let args: WriteArgs = arguments.parse().map_err(|e| ToolError::BadArguments {
            detail: e.to_string(),
        })?;
        let path = resolve_under(&self.root, &args.path)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| ToolError::Failed {
                    detail: format!("cannot create directory for `{}`: {e}", args.path),
                })?;
        }
        tokio::fs::write(&path, &args.content)
            .await
            .map_err(|e| ToolError::Failed {
                detail: format!("cannot write `{}`: {e}", args.path),
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
        let args: ListArgs = arguments.parse().map_err(|e| ToolError::BadArguments {
            detail: e.to_string(),
        })?;
        let display = args.path.as_deref().unwrap_or(".");
        let path = resolve_under(&self.root, display)?;
        let mut reader = tokio::fs::read_dir(&path)
            .await
            .map_err(|e| ToolError::Failed {
                detail: format!("cannot list `{display}`: {e}"),
            })?;
        let mut entries: Vec<String> = Vec::new();
        while let Some(entry) = reader.next_entry().await.map_err(|e| ToolError::Failed {
            detail: format!("cannot list `{display}`: {e}"),
        })? {
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
                entries.push(format!("{name}/"));
            } else {
                entries.push(name);
            }
        }
        entries.sort();
        if entries.is_empty() {
            Ok(ToolOutput::success("(empty directory)"))
        } else {
            Ok(ToolOutput::success(truncate(entries.join("\n"))))
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
        assert!(!out.is_error);

        let read = ReadFile::new(dir.path());
        let out = read
            .run(&ToolArguments::new(r#"{"path": "sub/a.txt"}"#))
            .await
            .unwrap();
        assert_eq!(out.content, "hello");

        let list = ListDir::new(dir.path());
        let out = list.run(&ToolArguments::new("{}")).await.unwrap();
        assert_eq!(out.content, "sub/");
    }

    #[tokio::test]
    async fn missing_file_reports_error_output() {
        let dir = tempfile::tempdir().unwrap();
        let read = ReadFile::new(dir.path());
        let err = read
            .run(&ToolArguments::new(r#"{"path": "nope.txt"}"#))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Failed { .. }));
    }
}
