//! Shell tool: runs `sh -c <command>` in the sandbox root. Opt-in — hosts
//! construct it only when the user asked for it. Async via `tokio::process`,
//! with a hard timeout; killing on drop means an interrupted run leaves no
//! stray process behind.

use std::path::PathBuf;
use std::time::Duration;

use agnes_core::message::{ToolArguments, ToolOutput};
use agnes_core::tool::{ToolError, ToolSpec};
use agnes_io::tool::Tool;
use async_trait::async_trait;

/// Largest combined output handed back to the model, in characters.
const MAX_OUTPUT_CHARS: usize = 100_000;

/// Hard cap on one command; a hung shell cannot hang the agent loop.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(120);

/// Runs a shell command with `sh -c`, capturing combined stdout and stderr.
pub struct Shell {
    root: PathBuf,
    spec: ToolSpec,
}

impl Shell {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            spec: ToolSpec {
                name: "run_shell".parse().expect("valid tool name"),
                description: "Run a shell command (`sh -c`) in the working directory and return combined stdout/stderr plus the exit code."
                    .into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "The shell command to run" }
                    },
                    "required": ["command"]
                })
                .try_into()
                .expect("schema is an object"),
            },
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
        let args: ShellArgs = arguments.parse().map_err(|e| ToolError::BadArguments {
            detail: e.to_string(),
        })?;
        let child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&args.command)
            .current_dir(&self.root)
            .kill_on_drop(true)
            .output();
        let output = tokio::time::timeout(COMMAND_TIMEOUT, child)
            .await
            .map_err(|_| ToolError::Failed {
                detail: format!("command timed out after {}s", COMMAND_TIMEOUT.as_secs()),
            })?
            .map_err(|e| ToolError::Failed {
                detail: format!("cannot spawn shell: {e}"),
            })?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let mut report = format!("exit code: {}", output.status.code().unwrap_or(-1));
        if !stdout.trim().is_empty() {
            report.push_str("\n--- stdout ---\n");
            report.push_str(stdout.trim_end());
        }
        if !stderr.trim().is_empty() {
            report.push_str("\n--- stderr ---\n");
            report.push_str(stderr.trim_end());
        }
        if report.chars().count() > MAX_OUTPUT_CHARS {
            let truncated: String = report.chars().take(MAX_OUTPUT_CHARS).collect();
            return Ok(ToolOutput::success(format!(
                "{truncated}\n... (output truncated)"
            )));
        }
        Ok(ToolOutput::success(report))
    }
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
        assert!(!out.is_error);
        assert!(out.content.contains("exit code: 0"));
        assert!(out.content.contains("hi"));
    }

    #[tokio::test]
    async fn failing_command_still_succeeds_as_tool_output() {
        let dir = tempfile::tempdir().unwrap();
        let shell = Shell::new(dir.path());
        let out = shell
            .run(&ToolArguments::new(r#"{"command": "exit 3"}"#))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("exit code: 3"));
    }
}
