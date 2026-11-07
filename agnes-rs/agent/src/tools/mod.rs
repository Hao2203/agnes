//! 内置工具：沙箱化的文件工具与可选的 shell。

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

/// 文件工具包：以 `root` 为沙箱边界，`shell` 决定是否提供 `run_shell`。
///
/// 交给 [`ToolsPlugin`](crate::runtime::ToolsPlugin) 逐把注册。
pub fn builtin_tools(root: impl Into<PathBuf>, shell: bool) -> Vec<Arc<dyn Tool>> {
    let root = root.into();
    let mut tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(ReadFile::new(root.clone())),
        Arc::new(WriteFile::new(root.clone())),
        Arc::new(ListDir::new(root.clone())),
    ];
    if shell {
        tools.push(Arc::new(Shell::new(root)));
    }
    tools
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

    #[test]
    fn builtin_tools_optionally_include_the_shell() {
        assert_eq!(builtin_tools("/tmp", false).len(), 3);
        assert_eq!(builtin_tools("/tmp", true).len(), 4);
    }
}
