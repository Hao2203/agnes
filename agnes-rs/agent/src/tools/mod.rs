//! 内置工具与它们的装载单元。

mod fs;
mod shell;
mod text;

pub use fs::{ListDir, ReadFile, WriteFile};
pub use shell::Shell;

use std::path::PathBuf;
use std::sync::Arc;

use agnes_core::message::ToolArguments;
use agnes_core::tool::ToolError;
use agnes_io::tool::Tool;
use serde::de::DeserializeOwned;

use crate::ext::Bundle;
use crate::host::{Host, LoadError};

/// 在工具边界把原始 JSON 参数解析成具体类型。
///
/// # Errors
///
/// 当参数不是 `T` 对应的合法 JSON 时返回 [`ToolError::BadArguments`]。
pub(crate) fn parse_arguments<T: DeserializeOwned>(
    arguments: &ToolArguments,
) -> Result<T, ToolError> {
    serde_json::from_str(arguments.as_str()).map_err(|source| ToolError::BadArguments { source })
}

/// 内置工具包：沙箱化的文件工具，加上可选的 shell。
pub struct FsBundle {
    root: PathBuf,
    shell: bool,
}

impl FsBundle {
    /// `root` 是全部文件工具的沙箱边界；`shell` 决定是否提供 `run_shell`。
    pub fn new(root: impl Into<PathBuf>, shell: bool) -> Self {
        Self {
            root: root.into(),
            shell,
        }
    }
}

impl Bundle for FsBundle {
    fn name(&self) -> &'static str {
        "fs-tools"
    }

    fn install(&self, host: &mut Host<'_>) -> Result<(), LoadError> {
        let mut tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(ReadFile::new(self.root.clone())),
            Arc::new(WriteFile::new(self.root.clone())),
            Arc::new(ListDir::new(self.root.clone())),
        ];
        if self.shell {
            tools.push(Arc::new(Shell::new(self.root.clone())));
        }
        for tool in tools {
            host.tools_mut().register(tool)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct Args {
        path: String,
    }

    #[test]
    fn parses_typed_arguments_at_the_boundary() {
        let args: Args = parse_arguments(&ToolArguments::new(r#"{"path": "a.txt"}"#)).unwrap();
        assert_eq!(args.path, "a.txt");
        assert!(matches!(
            parse_arguments::<Args>(&ToolArguments::new("not json")),
            Err(ToolError::BadArguments { .. })
        ));
    }
}
