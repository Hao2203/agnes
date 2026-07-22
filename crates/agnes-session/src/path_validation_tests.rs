use super::session::Session;
use agnes_llm::{Provider, CompletionRequest, LlmError};
use std::sync::Arc;

fn create_test_session() -> Session {
    // Create a mock provider that doesn't actually do anything
    struct MockProvider;

    #[async_trait::async_trait]
    impl Provider for MockProvider {
        async fn complete(&self, _req: CompletionRequest) -> Result<String, LlmError> {
            Ok(String::new())
        }
    }

    let provider: Arc<dyn Provider> = Arc::new(MockProvider);
    Session::new(provider).unwrap()
}

#[tokio::test]
async fn test_resolve_path_inside_root_allowed() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_owned();

    let session = create_test_session()
        .with_allow_root(root.clone());

    // We need to create the directory and file for canonicalize to succeed
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src").join("main.rs"), "test").unwrap();
    let input = format!("{}/src/main.rs", root.display());
    let result = session.resolve_path(&input).await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_resolve_path_outside_root_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir(&root).unwrap();
    // Create the outside file
    let outside = temp.path().join("outside.txt");
    std::fs::write(&outside, "test").unwrap();

    let session = create_test_session()
        .with_allow_root(root.clone());

    // Use absolute path that is definitely outside the root
    let input = outside.to_str().unwrap();
    let result = session.resolve_path(input).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.contains("outside allowed root"));
}

#[tokio::test]
async fn test_resolve_path_symlink_outside_rejected() {
    // Skip this test if symlinks are not available
    if cfg!(windows) {
        return;
    }

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir(&root).unwrap();

    let outside = temp.path().join("outside.txt");
    std::fs::write(&outside, "test").unwrap();

    // Create a symlink from inside to outside
    let symlink = root.join("link.txt");
    std::os::unix::fs::symlink(&outside, &symlink).unwrap();

    let session = create_test_session()
        .with_allow_root(root);

    let result = session.resolve_path("link.txt").await;
    assert!(result.is_err());
}
