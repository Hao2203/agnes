//! Shell tool: runs `sh -c <command>` in the sandbox root. Opt-in — hosts
//! construct it only when the user asked for it. Async via `tokio::process`,
//! with a hard timeout; killing on drop means an interrupted run leaves no
//! stray process behind.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::Duration;

use agnes_core::message::{ToolArguments, ToolOutput};
use agnes_core::tool::{ToolError, ToolSpec};
use agnes_io::tool::Tool;
use async_trait::async_trait;

use super::text::{MAX_OUTPUT_CHARS, truncate};

/// Hard cap on one command; a hung shell cannot hang the agent loop.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(120);

/// Runs a shell command with `sh -c`, capturing combined stdout and stderr.
pub struct Shell {
    root: PathBuf,
    spec: ToolSpec,
}

impl Shell {
    /// # Panics
    ///
    /// Only if the built-in tool name or JSON schema is invalid — a
    /// programming error in this crate, not user input.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            spec: ToolSpec::new(
                "run_shell".parse().expect("valid tool name"),
                "Run a shell command (`sh -c`) in the working directory and return combined stdout/stderr plus the exit code.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "The shell command to run" }
                    },
                    "required": ["command"]
                })
                .try_into()
                .expect("schema is an object"),
            ),
        }
    }
}

#[derive(serde::Deserialize)]
struct ShellArgs {
    command: String,
}

#[async_trait]
impl Tool for Shell {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, arguments: &ToolArguments) -> Result<ToolOutput, ToolError> {
        let args: ShellArgs = arguments
            .parse()
            .map_err(|source| ToolError::BadArguments { source })?;
        let child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&args.command)
            .current_dir(&self.root)
            .kill_on_drop(true)
            .output();
        let output = tokio::time::timeout(COMMAND_TIMEOUT, child)
            .await
            .map_err(|_| ToolError::Timeout {
                seconds: COMMAND_TIMEOUT.as_secs(),
            })?
            .map_err(|source| ToolError::Io {
                action: "cannot spawn shell in",
                path: self.root.display().to_string(),
                source,
            })?;

        let mut report = String::with_capacity(MAX_OUTPUT_CHARS.saturating_add(64));
        let _ = write!(report, "{}", output.status);
        append_section(&mut report, "\n--- stdout ---\n", &output.stdout);
        append_section(&mut report, "\n--- stderr ---\n", &output.stderr);
        Ok(ToolOutput::success(truncate(report)))
    }
}

fn append_section(report: &mut String, label: &str, raw: &[u8]) {
    let text = String::from_utf8_lossy(raw);
    let trimmed = text.trim_end();
    if trimmed.trim().is_empty() {
        return;
    }
    report.push_str(label);
    let remaining = MAX_OUTPUT_CHARS.saturating_sub(report.chars().count());
    report.extend(trimmed.chars().take(remaining));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn runs_command_in_root() {
        let dir = tempfile::tempdir().unwrap();
        let shell = Shell::new(dir.path());
        let out = shell
            .run(&ToolArguments::new(r#"{"command": "echo hi"}"#))
            .await
            .unwrap();
        assert!(!out.is_error());
        assert!(out.content().contains("exit status: 0"));
        assert!(out.content().contains("hi"));
    }

    #[tokio::test]
    async fn failing_command_still_succeeds_as_tool_output() {
        let dir = tempfile::tempdir().unwrap();
        let shell = Shell::new(dir.path());
        let out = shell
            .run(&ToolArguments::new(r#"{"command": "exit 3"}"#))
            .await
            .unwrap();
        assert!(!out.is_error());
        assert!(out.content().contains("exit status: 3"));
    }
}
