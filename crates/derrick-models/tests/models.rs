use std::collections::HashMap;
use std::error::Error;
use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use derrick_config::Config;
use derrick_models::{
    resolve_role, AuthStore, CompletionEvent, CompletionRequest, CompletionResponse,
    CompletionStream, FinishReason, Model, ModelError, ProviderRegistry, Secret,
};
use futures::{stream, StreamExt};
use tempfile::{tempdir, TempDir};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

#[derive(Clone, Debug)]
struct StaticModel;

#[async_trait]
impl Model for StaticModel {
    fn name(&self) -> &str {
        "static"
    }

    fn provider(&self) -> &str {
        "static"
    }

    fn cost_hint(&self) -> Option<&derrick_models::CostHint> {
        None
    }

    async fn stream(&self, _request: CompletionRequest) -> Result<CompletionStream, ModelError> {
        let events = vec![
            Ok(CompletionEvent::Content {
                text: "hello ".to_owned(),
            }),
            Ok(CompletionEvent::Content {
                text: "world".to_owned(),
            }),
            Ok(CompletionEvent::End {
                tokens_in: 1,
                tokens_out: 2,
                finish_reason: FinishReason::Stop,
            }),
        ];
        Ok(Box::pin(stream::iter(events)))
    }
}

fn request(prompt: &str, timeout: Duration) -> CompletionRequest {
    CompletionRequest {
        cached_prefix: Some("cache me".to_owned()),
        prompt: prompt.to_owned(),
        system: Some("system".to_owned()),
        max_tokens: Some(128),
        temperature: Some(0.2),
        timeout,
    }
}

fn fixture(name: &str) -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
        .to_string_lossy()
        .into_owned()
}

fn write_config(cli: &str, role_model: &str) -> TestResult<(TempDir, Config)> {
    let dir = tempdir()?;
    let path = dir.path().join("derrick.yaml");
    let cli = serde_json::to_string(cli)?;
    write_config_file(&path, &format!("    cli: {cli}"), role_model)?;
    let config = Config::load_from_path(&path)?;
    Ok((dir, config))
}

fn write_config_without_cli(role_model: &str) -> TestResult<(TempDir, Config)> {
    let dir = tempdir()?;
    let path = dir.path().join("derrick.yaml");
    write_config_file(&path, "", role_model)?;
    let config = Config::load_from_path(&path)?;
    Ok((dir, config))
}

fn write_config_file(path: &Path, shell_extra: &str, role_model: &str) -> TestResult {
    fs::write(
        path,
        format!(
            r#"
version: 1
site:
  name: derrick
  prefix: drk
models:
  shell-model:
    provider: shell
    model: shell-test
{shell_extra}
  other-model:
    provider: shell
    model: other-test
{shell_extra}
roles:
  drafter: {role_model}
tools:
  speckit:
    enabled: true
    version: ">=0.4.0"
  assay:
    enabled: false
    role: drafter
    reviewers: [drafter]
  substrate:
    backend: native
    mode: solo
  copilot:
    agent_identity: derrick-hand
pipeline: []
guardrails:
  constitution_path: .specify/memory/constitution.md
parallelism:
  batch_max: 8
  step_max: 4
  assay_max: 2
state:
  dir: .derrick
  log_runs: true
  worktree_root: .derrick/worktrees
"#
        ),
    )?;
    Ok(())
}

#[test]
fn auth_store_reads_env_vars() -> TestResult {
    let key = unique_env_key("AUTH");
    // SAFETY: unique key; no other thread reads this env var.
    unsafe { std::env::set_var(&key, "test-secret") };
    let auth = AuthStore::from_env();

    let secret = auth.get("anthropic", &key).ok_or("secret should exist")?;

    assert_eq!(secret.expose(), "test-secret");
    Ok(())
}

#[test]
fn auth_store_env_map_exposes_process_env() -> TestResult {
    let key = unique_env_key("ENVMAP");
    // SAFETY: unique key; no other thread reads this env var.
    unsafe { std::env::set_var(&key, "passthrough-value") };
    let auth = AuthStore::from_env();

    let map = auth.env_map();

    assert_eq!(map.get(&key).map(String::as_str), Some("passthrough-value"));
    Ok(())
}

#[test]
fn secret_debug_does_not_leak() {
    let secret = Secret::new("never-print-me");
    let debug = format!("{secret:?}");

    assert!(debug.contains("***"));
    assert!(!debug.contains("never-print-me"));
}

#[test]
fn secret_expose_returns_inner() {
    let secret = Secret::new("inner");

    assert_eq!(secret.expose(), "inner");
}

#[test]
fn auth_store_for_testing_returns_override() -> TestResult {
    let mut map = HashMap::new();
    map.insert(
        ("provider".to_owned(), "API_KEY".to_owned()),
        Secret::new("override-secret"),
    );
    let auth = AuthStore::for_testing(map);

    let secret = auth
        .get("provider", "API_KEY")
        .ok_or("override should exist")?;

    assert_eq!(secret.expose(), "override-secret");
    Ok(())
}

#[test]
fn model_error_retryability_matches_error_kind() {
    assert!(ModelError::Timeout {
        provider: "shell".to_owned(),
        seconds: 1,
    }
    .is_retryable());
    assert!(ModelError::Provider {
        provider: "shell".to_owned(),
        message: "retry".to_owned(),
        retryable: true,
    }
    .is_retryable());
    assert!(!ModelError::UnknownProvider("nope".to_owned()).is_retryable());
    assert!(!ModelError::InvalidConfig {
        model: "bad".to_owned(),
        message: "invalid".to_owned(),
    }
    .is_retryable());
}

#[tokio::test]
async fn model_complete_drains_stream() -> TestResult {
    let response = StaticModel
        .complete(request("ignored", Duration::from_secs(1)))
        .await?;

    assert_eq!(
        response,
        CompletionResponse {
            text: "hello world".to_owned(),
            tokens_in: 1,
            tokens_out: 2,
            finish_reason: FinishReason::Stop,
        }
    );
    Ok(())
}

#[test]
fn provider_registry_resolves_known_provider() -> TestResult {
    let (_dir, config) = write_config(&fixture("echo_prompt.sh"), "shell-model")?;
    let model_def = config
        .models()
        .get("shell-model")
        .ok_or("shell model should exist")?;
    let model = ProviderRegistry::with_defaults().build(model_def, &AuthStore::default())?;

    assert_eq!(model.provider(), "shell");
    assert_eq!(model.name(), "shell-test");
    assert_eq!(model.cost_hint(), None);
    assert!(!model.host_delegated_auth());
    Ok(())
}

#[test]
fn provider_registry_unknown_provider_returns_typed_error() -> TestResult {
    // `azure-openai` is not a host and is not aliased to one — it is no longer
    // a registered provider post-D65, so building it is a typed error.
    let (_dir, config) = write_minimal_config(
        "  legacy:\n    provider: azure-openai\n    model: gpt-5\n",
        "legacy",
    )?;
    let model_def = config.models().get("legacy").ok_or("legacy model")?;
    let error = ProviderRegistry::with_defaults()
        .build(model_def, &AuthStore::default())
        .err()
        .ok_or("unknown provider should error")?;

    assert!(matches!(error, ModelError::UnknownProvider(provider) if provider == "azure-openai"));
    Ok(())
}

#[tokio::test]
async fn resolve_role_walks_role_to_model_to_provider() -> TestResult {
    let (_dir, config) = write_config(&fixture("echo_prompt.sh"), "shell-model")?;
    let model = resolve_role(
        "drafter",
        config.roles(),
        config.models(),
        &AuthStore::default(),
    )
    .await?;

    assert_eq!(model.provider(), "shell");
    Ok(())
}

#[tokio::test]
async fn resolve_role_unknown_role_returns_typed_error() -> TestResult {
    let (_dir, config) = write_config(&fixture("echo_prompt.sh"), "shell-model")?;
    let error = resolve_role(
        "missing",
        config.roles(),
        config.models(),
        &AuthStore::default(),
    )
    .await
    .err()
    .ok_or("unknown role should error")?;

    assert!(matches!(error, ModelError::UnknownRole(role) if role == "missing"));
    Ok(())
}

#[tokio::test]
async fn resolve_role_unknown_model_returns_typed_error() -> TestResult {
    let (_dir_a, roles_config) = write_config(&fixture("echo_prompt.sh"), "shell-model")?;
    let models_config = Config::defaults();
    let error = resolve_role(
        "drafter",
        roles_config.roles(),
        models_config.models(),
        &AuthStore::default(),
    )
    .await
    .err()
    .ok_or("unknown model should error")?;

    assert!(matches!(error, ModelError::UnknownModel(model) if model == "shell-model"));
    Ok(())
}

#[tokio::test]
async fn shell_provider_completes_simple_prompt() -> TestResult {
    let (_dir, config) = write_config(&fixture("echo_prompt.sh"), "shell-model")?;
    let model = resolve_role(
        "drafter",
        config.roles(),
        config.models(),
        &AuthStore::default(),
    )
    .await?;

    let response = model
        .complete(request("hello shell", Duration::from_secs(10)))
        .await?;

    assert_eq!(response.text, "hello shell\n");
    assert_eq!(response.tokens_in, 7);
    assert_eq!(response.tokens_out, 11);
    assert_eq!(response.finish_reason, FinishReason::Stop);
    Ok(())
}

#[tokio::test]
async fn shell_provider_respects_timeout() -> TestResult {
    let (_dir, config) = write_config(&fixture("sleep.sh"), "shell-model")?;
    let model = resolve_role(
        "drafter",
        config.roles(),
        config.models(),
        &AuthStore::default(),
    )
    .await?;
    let started = std::time::Instant::now();
    let error = model
        .complete(request("slow", Duration::from_millis(100)))
        .await
        .err()
        .ok_or("timeout should error")?;

    assert!(matches!(error, ModelError::Timeout { provider, .. } if provider == "shell"));
    assert!(started.elapsed() < Duration::from_secs(1));
    Ok(())
}

#[tokio::test]
async fn shell_provider_nonzero_exit_surfaces_stderr() -> TestResult {
    let (_dir, config) = write_config(&fixture("fail.sh"), "shell-model")?;
    let model = resolve_role(
        "drafter",
        config.roles(),
        config.models(),
        &AuthStore::default(),
    )
    .await?;
    let error = model
        .complete(request("fail", Duration::from_secs(10)))
        .await
        .err()
        .ok_or("nonzero exit should error")?;

    assert!(
        matches!(error, ModelError::Provider { provider, message, retryable: false }
            if provider == "shell" && message.contains("fixture failed"))
    );
    Ok(())
}

#[tokio::test]
async fn shell_provider_spawn_failure_returns_provider_error() -> TestResult {
    let (_dir, config) = write_config("/definitely/not/a/derrick-model", "shell-model")?;
    let model = resolve_role(
        "drafter",
        config.roles(),
        config.models(),
        &AuthStore::default(),
    )
    .await?;
    let error = model
        .complete(request("spawn", Duration::from_secs(10)))
        .await
        .err()
        .ok_or("spawn failure should error")?;

    assert!(
        matches!(error, ModelError::Provider { provider, message, retryable: false }
            if provider == "shell" && message.contains("failed to spawn"))
    );
    Ok(())
}

#[test]
fn shell_provider_missing_cli_returns_invalid_config() -> TestResult {
    let (_dir, config) = write_config_without_cli("shell-model")?;
    let model_def = config
        .models()
        .get("shell-model")
        .ok_or("shell model should exist")?;
    let error = ProviderRegistry::with_defaults()
        .build(model_def, &AuthStore::default())
        .err()
        .ok_or("missing cli should error")?;

    assert!(matches!(error, ModelError::InvalidConfig { model, message }
            if model == "shell-test" && message.contains("requires cli")));
    Ok(())
}

#[test]
fn shell_provider_empty_cli_returns_invalid_config() -> TestResult {
    let (_dir, config) = write_config("", "shell-model")?;
    let model_def = config
        .models()
        .get("shell-model")
        .ok_or("shell model should exist")?;
    let error = ProviderRegistry::with_defaults()
        .build(model_def, &AuthStore::default())
        .err()
        .ok_or("empty cli should error")?;

    assert!(matches!(error, ModelError::InvalidConfig { model, message }
            if model == "shell-test" && message.contains("must not be empty")));
    Ok(())
}

#[test]
fn shell_provider_invalid_cli_returns_invalid_config() -> TestResult {
    let (_dir, config) = write_config("'unterminated", "shell-model")?;
    let model_def = config
        .models()
        .get("shell-model")
        .ok_or("shell model should exist")?;
    let error = ProviderRegistry::with_defaults()
        .build(model_def, &AuthStore::default())
        .err()
        .ok_or("invalid cli should error")?;

    assert!(matches!(error, ModelError::InvalidConfig { model, message }
            if model == "shell-test" && message.contains("invalid cli command")));
    Ok(())
}

#[tokio::test]
async fn shell_provider_missing_trailing_json_falls_back() -> TestResult {
    let (_dir, config) = write_config(&fixture("no_meta.sh"), "shell-model")?;
    let model = resolve_role(
        "drafter",
        config.roles(),
        config.models(),
        &AuthStore::default(),
    )
    .await?;

    let response = model
        .complete(request("plain", Duration::from_secs(10)))
        .await?;

    assert_eq!(response.text, "plain output\n");
    assert_eq!(response.tokens_in, 0);
    assert_eq!(response.tokens_out, 0);
    assert_eq!(response.finish_reason, FinishReason::Error);
    Ok(())
}

#[tokio::test]
async fn shell_provider_streaming_emits_content_then_end() -> TestResult {
    let (_dir, config) = write_config(&fixture("stream.sh"), "shell-model")?;
    let model = resolve_role(
        "drafter",
        config.roles(),
        config.models(),
        &AuthStore::default(),
    )
    .await?;
    let events = model
        .stream(request("stream", Duration::from_secs(10)))
        .await?
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?;

    assert_eq!(
        events,
        vec![
            CompletionEvent::Content {
                text: "one\n".to_owned(),
            },
            CompletionEvent::Content {
                text: "two\n".to_owned(),
            },
            CompletionEvent::End {
                tokens_in: 2,
                tokens_out: 3,
                finish_reason: FinishReason::Length,
            },
        ]
    );
    Ok(())
}

#[tokio::test]
async fn shell_provider_handles_crlf_in_stdout() -> TestResult {
    let (_dir, config) = write_config(&fixture("crlf.sh"), "shell-model")?;
    let model = resolve_role(
        "drafter",
        config.roles(),
        config.models(),
        &AuthStore::default(),
    )
    .await?;

    let response = model
        .complete(request("crlf", Duration::from_secs(10)))
        .await?;

    assert_eq!(response.text, "crlf one\r\n");
    assert_eq!(response.tokens_in, 3);
    assert_eq!(response.tokens_out, 4);
    assert_eq!(response.finish_reason, FinishReason::Stop);
    Ok(())
}

#[tokio::test]
async fn shell_provider_invalid_metadata_returns_provider_error() -> TestResult {
    let (_dir, config) = write_config(&fixture("invalid_meta.sh"), "shell-model")?;
    let model = resolve_role(
        "drafter",
        config.roles(),
        config.models(),
        &AuthStore::default(),
    )
    .await?;
    let error = model
        .complete(request("bad meta", Duration::from_secs(10)))
        .await
        .err()
        .ok_or("invalid metadata should error")?;

    assert!(
        matches!(error, ModelError::Provider { provider, message, retryable: false }
            if provider == "shell" && message.contains("invalid shell metadata"))
    );
    Ok(())
}

#[tokio::test]
async fn shell_provider_error_finish_reason_round_trips() -> TestResult {
    let (_dir, config) = write_config(&fixture("error_finish.sh"), "shell-model")?;
    let model = resolve_role(
        "drafter",
        config.roles(),
        config.models(),
        &AuthStore::default(),
    )
    .await?;

    let response = model
        .complete(request("error finish", Duration::from_secs(10)))
        .await?;

    assert_eq!(response.finish_reason, FinishReason::Error);
    Ok(())
}

#[tokio::test]
async fn shell_provider_invalid_finish_reason_returns_provider_error() -> TestResult {
    let (_dir, config) = write_config(&fixture("invalid_reason.sh"), "shell-model")?;
    let model = resolve_role(
        "drafter",
        config.roles(),
        config.models(),
        &AuthStore::default(),
    )
    .await?;
    let error = model
        .complete(request("bad reason", Duration::from_secs(10)))
        .await
        .err()
        .ok_or("invalid finish reason should error")?;

    assert!(
        matches!(error, ModelError::Provider { provider, message, retryable: false }
            if provider == "shell" && message.contains("invalid shell finish reason"))
    );
    Ok(())
}

#[test]
fn default_models_build_host_delegated_for_each_host() -> TestResult {
    // Every default-config model now resolves to a host-delegated provider
    // whose name equals the host CLI; none requires an API key.
    let config = Config::defaults();
    let registry = ProviderRegistry::with_defaults();
    let expectations = [
        ("claude-opus", "claude", "claude-opus-4-8"),
        ("claude-sonnet", "claude", "claude-sonnet-4-6"),
        ("claude-haiku", "claude", "claude-haiku-4-5"),
        ("codex-gpt5", "codex", "gpt-5.5"),
        // `auto` (D67): the executor model is foreman-selected per ticket.
        ("copilot", "copilot", "auto"),
    ];
    for (model_key, host, model_id) in expectations {
        let model_def = config
            .models()
            .get(model_key)
            .ok_or("default model should exist")?;
        let model = registry.build(model_def, &AuthStore::default())?;
        assert_eq!(model.provider(), host, "{model_key} provider");
        assert_eq!(model.name(), model_id, "{model_key} model id");
        assert!(
            model.host_delegated_auth(),
            "{model_key} should be host-delegated"
        );
    }
    Ok(())
}

#[tokio::test]
async fn host_delegated_complete_returns_host_text_and_tokens() -> TestResult {
    // Build a claude host-delegated model whose adapter points at a fake CLI
    // that emits the claude JSON envelope, and assert complete() returns the
    // host text plus the reported token counts.
    let dir = tempdir()?;
    let bin_dir = dir.path().join("bin");
    fs::create_dir_all(&bin_dir)?;
    let fake = bin_dir.join("claude");
    fs::write(
        &fake,
        "#!/bin/sh\nprintf '%s' '{\"result\":\"host says hi\",\"usage\":{\"input_tokens\":11,\"output_tokens\":4}}'\n",
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&fake)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&fake, perms)?;
    }
    let old_path = std::env::var_os("PATH");
    let joined = std::env::join_paths(
        std::iter::once(bin_dir.clone())
            .chain(std::env::split_paths(&old_path.clone().unwrap_or_default())),
    )?;
    // SAFETY: tokio::test defaults to a current-thread runtime; no other
    // threads run concurrently inside this test.
    unsafe { std::env::set_var("PATH", &joined) };

    let (_cfg_dir, config) = write_minimal_config(
        "  c:\n    provider: claude\n    model: claude-opus-4-8\n",
        "c",
    )?;
    let model_def = config.models().get("c").ok_or("c model")?;
    let model = ProviderRegistry::with_defaults().build(model_def, &AuthStore::default())?;

    let response = model
        .complete(request("hello", Duration::from_secs(10)))
        .await;

    // SAFETY: tokio::test defaults to a current-thread runtime; no other
    // threads run concurrently inside this test.
    if let Some(path) = old_path {
        unsafe { std::env::set_var("PATH", path) };
    } else {
        unsafe { std::env::remove_var("PATH") };
    }

    let response = response?;
    assert_eq!(response.text, "host says hi");
    assert_eq!(response.tokens_in, 11);
    assert_eq!(response.tokens_out, 4);
    assert_eq!(response.finish_reason, FinishReason::Stop);
    Ok(())
}

fn write_minimal_config(extra_models: &str, role_model: &str) -> TestResult<(TempDir, Config)> {
    let dir = tempdir()?;
    let path = dir.path().join("derrick.yaml");
    fs::write(
        &path,
        format!(
            r#"
version: 1
site:
  name: derrick
  prefix: drk
models:
{extra_models}
roles:
  drafter: {role_model}
tools:
  speckit:
    enabled: true
    version: ">=0.4.0"
  assay:
    enabled: false
    role: drafter
    reviewers: [drafter]
  substrate:
    backend: native
    mode: solo
  copilot:
    agent_identity: derrick-hand
pipeline: []
guardrails:
  constitution_path: .specify/memory/constitution.md
parallelism:
  batch_max: 8
  step_max: 4
  assay_max: 2
state:
  dir: .derrick
  log_runs: true
  worktree_root: .derrick/worktrees
"#
        ),
    )?;
    let config = Config::load_from_path(&path)?;
    Ok((dir, config))
}

#[test]
fn opencode_builds_host_delegated() -> TestResult {
    let (_dir, config) = write_minimal_config(
        "  oc:\n    provider: opencode\n    model: anthropic/claude-sonnet-4-6\n",
        "oc",
    )?;
    let model_def = config.models().get("oc").ok_or("oc model")?;
    let model = ProviderRegistry::with_defaults().build(model_def, &AuthStore::default())?;
    assert_eq!(model.provider(), "opencode");
    assert!(model.host_delegated_auth());
    Ok(())
}

#[test]
fn aider_builds_host_delegated() -> TestResult {
    let (_dir, config) = write_minimal_config(
        "  ai:\n    provider: aider\n    model: openai/gpt-5.5\n",
        "ai",
    )?;
    let model_def = config.models().get("ai").ok_or("ai model")?;
    let model = ProviderRegistry::with_defaults().build(model_def, &AuthStore::default())?;
    assert_eq!(model.provider(), "aider");
    assert!(model.host_delegated_auth());
    Ok(())
}

#[test]
fn legacy_openai_cli_alias_resolves_to_codex_host() -> TestResult {
    // A pinned config naming the pre-D65 `openai-cli` provider is remapped to
    // the `codex` host at config finalize (one-release compatibility shim).
    let (_dir, config) = write_minimal_config(
        "  gpt5:\n    provider: openai-cli\n    model: gpt-5.5\n",
        "gpt5",
    )?;
    let model_def = config.models().get("gpt5").ok_or("gpt5 model")?;
    let model = ProviderRegistry::with_defaults().build(model_def, &AuthStore::default())?;
    assert_eq!(model.provider(), "codex");
    assert!(model.host_delegated_auth());
    Ok(())
}

fn unique_env_key(label: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    format!("DERRICK_MODELS_TEST_{label}_{nanos}")
}

#[allow(dead_code)]
fn _assert_send_sync(model: &dyn Model) {
    fn assert_send_sync<T: Send + Sync + ?Sized>(_value: &T) {}

    assert_send_sync(model);
}
