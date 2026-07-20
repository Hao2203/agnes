use agnes_llm::{LlmCliOpts, LlmError, ResolvedKind, resolve_decision, resolve_provider};

// Serialize these tests — they mutate process env.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn clear_env() {
    for k in [
        "AGNES_LLM_PROVIDER",
        "AGNES_LLM_MODEL",
        "AGNES_LLM_BASE_URL",
        "ANTHROPIC_API_KEY",
        "OPENAI_API_KEY",
    ] {
        // SAFETY: process-serialized via ENV_LOCK.
        unsafe {
            std::env::remove_var(k);
        }
    }
}

/// `Arc<dyn Provider>` is not `Debug`, so `Result::unwrap_err` won't compile.
/// This helper pulls out the error and panics with the type name on Ok.
fn expect_err(r: Result<std::sync::Arc<dyn agnes_llm::Provider>, LlmError>) -> LlmError {
    match r {
        Ok(_) => panic!("expected Err, got Ok(Arc<dyn Provider>)"),
        Err(e) => e,
    }
}

#[test]
fn missing_provider_selection_errors() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_env();
    let cli = LlmCliOpts {
        provider: None,
        model: None,
        base_url: None,
    };
    let err = expect_err(resolve_provider(&cli));
    assert!(
        matches!(
            err,
            LlmError::MissingConfig {
                flag: "--llm-provider",
                ..
            }
        ),
        "got: {err}"
    );
}

#[test]
fn anthropic_needs_key() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_env();
    let cli = LlmCliOpts {
        provider: Some("anthropic".into()),
        model: Some("m".into()),
        base_url: None,
    };
    let err = expect_err(resolve_provider(&cli));
    assert!(
        matches!(
            err,
            LlmError::MissingApiKey {
                env_var: "ANTHROPIC_API_KEY"
            }
        ),
        "got: {err}"
    );
}

#[test]
fn anthropic_resolves_with_key_and_default_model() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_env();
    unsafe {
        std::env::set_var("ANTHROPIC_API_KEY", "sk-test");
    }
    let cli = LlmCliOpts {
        provider: Some("anthropic".into()),
        model: None,
        base_url: None,
    };
    let (kind, model) = resolve_decision(&cli).expect("should resolve");
    assert_eq!(kind, ResolvedKind::Anthropic);
    assert_eq!(model, "claude-haiku-4-5"); // brief-mandated default
    // sanity: provider construction succeeds too
    resolve_provider(&cli).expect("should build");
}

#[test]
fn default_model_when_neither_cli_nor_env_supplies_one() {
    // Locks the default-model contract: no CLI model, no AGNES_LLM_MODEL,
    // anthropic selected -> ("Anthropic", "claude-haiku-4-5").
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_env();
    unsafe {
        std::env::set_var("ANTHROPIC_API_KEY", "sk-test");
        // AGNES_LLM_MODEL deliberately unset.
    }
    let cli = LlmCliOpts {
        provider: Some("anthropic".into()),
        model: None,
        base_url: None,
    };
    let (kind, model) = resolve_decision(&cli).expect("should resolve");
    assert_eq!(kind, ResolvedKind::Anthropic);
    assert_eq!(model, "claude-haiku-4-5");
}

#[test]
fn openai_needs_key_and_base_url() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_env();
    let cli = LlmCliOpts {
        provider: Some("openai".into()),
        model: Some("m".into()),
        base_url: None,
    };
    let err = expect_err(resolve_provider(&cli));
    // First missing thing surfaced: key.
    assert!(matches!(
        err,
        LlmError::MissingApiKey {
            env_var: "OPENAI_API_KEY"
        }
    ));

    unsafe {
        std::env::set_var("OPENAI_API_KEY", "sk-test");
    }
    let err2 = expect_err(resolve_provider(&cli));
    assert!(matches!(
        err2,
        LlmError::MissingConfig {
            what: "base_url",
            flag: "--llm-base-url",
            ..
        }
    ));
}

#[test]
fn cli_flag_beats_env() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_env();
    unsafe {
        std::env::set_var("AGNES_LLM_PROVIDER", "openai");
        std::env::set_var("AGNES_LLM_MODEL", "env-model");
        std::env::set_var("ANTHROPIC_API_KEY", "sk-test");
    }
    // CLI says anthropic — must win over env's "openai".
    let cli = LlmCliOpts {
        provider: Some("anthropic".into()),
        model: Some("cli-model".into()),
        base_url: None,
    };
    let (kind, model) = resolve_decision(&cli).expect("should resolve as anthropic");
    assert_eq!(kind, ResolvedKind::Anthropic); // proves CLI beat env's "openai"
    assert_eq!(model, "cli-model"); // proves CLI model beat env's "env-model"
}
