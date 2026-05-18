use std::collections::HashMap;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use derrick_tools::{
    ClaudeHost, CodexHost, CopilotHost, CopilotToolPermission, HostAdapter, HostError,
    HostRegistry, HostRequest,
};
use tempfile::{tempdir, TempDir};
use tokio::sync::{Mutex, MutexGuard};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

static PROCESS_LOCK: Mutex<()> = Mutex::const_new(());

struct StaticHost {
    name: &'static str,
}

#[async_trait::async_trait]
impl HostAdapter for StaticHost {
    fn name(&self) -> &str {
        self.name
    }

    fn is_available(&self) -> bool {
        true
    }

    async fn run(&self, _request: HostRequest) -> Result<derrick_tools::HostResponse, HostError> {
        Err(HostError::NotFound {
            host: self.name.to_owned(),
        })
    }
}

#[derive(Clone, Copy)]
enum HostKind {
    Claude,
    Codex,
    Copilot,
}

impl HostKind {
    fn name(&self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Copilot => "copilot",
        }
    }

    fn adapter(&self, binary: impl Into<PathBuf>) -> Box<dyn HostAdapter> {
        match self {
            Self::Claude => Box::new(ClaudeHost::with_binary(binary)),
            Self::Codex => Box::new(CodexHost::with_binary(binary)),
            Self::Copilot => Box::new(CopilotHost::with_binary(binary)),
        }
    }

    fn default_adapter(&self) -> Box<dyn HostAdapter> {
        match self {
            Self::Claude => Box::new(ClaudeHost::new()),
            Self::Codex => Box::new(CodexHost::new()),
            Self::Copilot => Box::new(CopilotHost::new()),
        }
    }
}

fn request(cwd: &Path) -> HostRequest {
    HostRequest {
        prompt: "/speckit.specify hello world\nsecond line".to_owned(),
        cwd: cwd.to_path_buf(),
        timeout: Duration::from_secs(2),
        env: HashMap::new(),
        copilot_tools: CopilotToolPermission::Default,
        model: None,
        headless: false,
    }
}

#[cfg(unix)]
fn mock_host(name: &str, body: &str) -> TestResult<TempDir> {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempdir()?;
    let path = dir.path().join(name);
    fs::write(&path, body)?;
    let mut permissions = fs::metadata(&path)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions)?;
    Ok(dir)
}

#[cfg(windows)]
fn mock_host(_name: &str, _body: &str) -> TestResult<TempDir> {
    // TODO(T009 follow-up): define Windows process and script semantics.
    Err("derrick-tools host tests are supported on macOS and Linux only".into())
}

async fn process_lock() -> MutexGuard<'static, ()> {
    PROCESS_LOCK.lock().await
}

fn path_with(dir: &Path) -> TestResult<String> {
    let current = std::env::var_os("PATH").unwrap_or_default();
    let paths = std::iter::once(dir.to_path_buf()).chain(std::env::split_paths(&current));
    Ok(std::env::join_paths(paths)?.to_string_lossy().into_owned())
}

async fn run_with_script(kind: HostKind, script: &str) -> TestResult<derrick_tools::HostResponse> {
    let _guard = process_lock().await;
    let host = mock_host(kind.name(), script)?;
    let cwd = tempdir()?;
    let adapter = kind.adapter(host.path().join(kind.name()));
    Ok(adapter.run(request(cwd.path())).await?)
}

async fn assert_correct_args(kind: HostKind) -> TestResult {
    let _guard = process_lock().await;
    let host = mock_host(
        kind.name(),
        r#"#!/bin/sh
for arg in "$@"; do
  printf '%s\037' "$arg" >&2
done
printf 'ok'
"#,
    )?;
    let cwd = tempdir()?;
    let adapter = kind.adapter(host.path().join(kind.name()));
    let req = request(cwd.path());
    let expected = match kind {
        HostKind::Claude => vec!["--print".to_owned(), req.prompt.clone()],
        HostKind::Codex => vec![
            "exec".to_owned(),
            "--skip-git-repo-check".to_owned(),
            req.prompt.clone(),
        ],
        HostKind::Copilot => vec![
            "-p".to_owned(),
            req.prompt.clone(),
            "--add-dir".to_owned(),
            cwd.path().to_string_lossy().into_owned(),
        ],
    };

    let response = adapter.run(req).await?;

    let stderr = response
        .stderr
        .trim_end_matches('\u{1f}')
        .split('\u{1f}')
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    assert_eq!(stderr, expected);
    assert!(
        !stderr.iter().any(|arg| arg == "--model"),
        "--model must never be passed: {stderr:?}"
    );
    Ok(())
}

async fn assert_prompt_single_arg(kind: HostKind) -> TestResult {
    let response = run_with_script(
        kind,
        r#"#!/bin/sh
printf 'argc=%s\n' "$#" >&2
case "$1:$2:$3" in
  --print:/speckit.specify\ hello\ world* | exec:--skip-git-repo-check:/speckit.specify\ hello\ world* | -p:/speckit.specify\ hello\ world* )
    printf 'prompt-ok'
    ;;
  *)
    printf 'bad prompt args: %s|%s|%s\n' "$1" "$2" "$3" >&2
    exit 7
    ;;
esac
"#,
    )
    .await?;

    assert_eq!(response.stdout, "prompt-ok");
    Ok(())
}

async fn assert_passes_cwd(kind: HostKind) -> TestResult {
    let _guard = process_lock().await;
    let host = mock_host(
        kind.name(),
        r#"#!/bin/sh
pwd
"#,
    )?;
    let cwd = tempdir()?;
    let adapter = kind.adapter(host.path().join(kind.name()));

    let response = adapter.run(request(cwd.path())).await?;
    let actual = fs::canonicalize(Path::new(response.stdout.trim_end()))?;
    let expected = fs::canonicalize(cwd.path())?;

    assert_eq!(actual, expected);
    Ok(())
}

async fn assert_stdout(kind: HostKind) -> TestResult {
    let response = run_with_script(
        kind,
        r#"#!/bin/sh
printf 'assistant response'
"#,
    )
    .await?;

    assert_eq!(response.stdout, "assistant response");
    assert_eq!(response.exit_code, 0);
    Ok(())
}

async fn assert_nonzero(kind: HostKind) -> TestResult {
    let _guard = process_lock().await;
    let host = mock_host(
        kind.name(),
        r#"#!/bin/sh
printf 'typed failure' >&2
exit 7
"#,
    )?;
    let cwd = tempdir()?;
    let adapter = kind.adapter(host.path().join(kind.name()));

    let error = adapter.run(request(cwd.path())).await;

    match error {
        Err(HostError::NonZeroExit {
            host,
            exit_code: 7,
            stderr,
        }) => {
            assert_eq!(host, kind.name());
            assert_eq!(stderr, "typed failure");
        }
        other => return Err(format!("unexpected error: {other:?}").into()),
    }
    Ok(())
}

async fn assert_timeout(kind: HostKind) -> TestResult {
    let _guard = process_lock().await;
    let host = mock_host(
        kind.name(),
        r#"#!/bin/sh
sleep 5
"#,
    )?;
    let cwd = tempdir()?;
    let adapter = kind.adapter(host.path().join(kind.name()));
    let mut req = request(cwd.path());
    req.timeout = Duration::from_millis(50);
    let started = Instant::now();

    let error = adapter.run(req).await;

    assert!(started.elapsed() < Duration::from_secs(2));
    match error {
        Err(HostError::Timeout { host, .. }) => assert_eq!(host, kind.name()),
        other => return Err(format!("unexpected timeout result: {other:?}").into()),
    }
    Ok(())
}

async fn assert_available_when_on_path(kind: HostKind) -> TestResult {
    let _guard = process_lock().await;
    let host = mock_host(
        kind.name(),
        r#"#!/bin/sh
exit 0
"#,
    )?;
    let old_path = std::env::var_os("PATH");
    std::env::set_var("PATH", path_with(host.path())?);
    let adapter = kind.default_adapter();

    let available = adapter.is_available();

    restore_path(old_path);
    assert!(available);
    Ok(())
}

async fn assert_unavailable_when_absent(kind: HostKind) -> TestResult {
    let _guard = process_lock().await;
    let empty = tempdir()?;
    let old_path = std::env::var_os("PATH");
    std::env::set_var("PATH", empty.path());
    let adapter = kind.default_adapter();

    let available = adapter.is_available();

    restore_path(old_path);
    assert!(!available);
    Ok(())
}

fn restore_path(path: Option<std::ffi::OsString>) {
    if let Some(path) = path {
        std::env::set_var("PATH", path);
    } else {
        std::env::remove_var("PATH");
    }
}

async fn assert_no_model_override(kind: HostKind) -> TestResult {
    let response = run_with_script(
        kind,
        r#"#!/bin/sh
for arg in "$@"; do
  if [ "$arg" = "--model" ]; then
    printf 'unexpected --model' >&2
    exit 7
  fi
done
printf 'no-model'
"#,
    )
    .await?;

    assert_eq!(response.stdout, "no-model");
    Ok(())
}

#[test]
fn registry_with_defaults_has_four_hosts() {
    let registry = HostRegistry::with_defaults();

    assert_eq!(
        registry.names(),
        vec!["claude", "codex", "copilot", "opencode"]
    );
}

#[test]
fn registry_get_unknown_returns_none() {
    let registry = HostRegistry::empty();

    assert!(registry.get("missing").is_none());
}

#[test]
fn registry_register_replaces_existing() {
    let mut registry = HostRegistry::empty();
    registry.register("claude", Box::new(StaticHost { name: "first" }));
    registry.register("claude", Box::new(StaticHost { name: "second" }));

    let adapter = registry.get("claude").map(HostAdapter::name);

    assert_eq!(adapter, Some("second"));
}

#[test]
fn registry_names_lists_registered_hosts() {
    let mut registry = HostRegistry::empty();
    registry.register("copilot", Box::new(StaticHost { name: "copilot" }));
    registry.register("claude", Box::new(StaticHost { name: "claude" }));

    assert_eq!(registry.names(), vec!["claude", "copilot"]);
}

#[test]
fn host_default_constructors_have_names() {
    assert_eq!(ClaudeHost::default().name(), "claude");
    assert_eq!(CodexHost::default().name(), "codex");
    assert_eq!(CopilotHost::default().name(), "copilot");
}

#[test]
fn host_request_new_sets_safe_defaults() -> TestResult {
    let cwd = tempdir()?;

    let request = HostRequest::new("prompt", cwd.path());

    assert_eq!(request.prompt, "prompt");
    assert_eq!(request.cwd, cwd.path());
    assert_eq!(request.timeout, Duration::from_secs(600));
    assert!(request.env.is_empty());
    assert_eq!(request.copilot_tools, CopilotToolPermission::Default);
    Ok(())
}

#[tokio::test]
async fn claude_invokes_with_correct_args() -> TestResult {
    assert_correct_args(HostKind::Claude).await
}

#[tokio::test]
async fn codex_invokes_with_correct_args() -> TestResult {
    assert_correct_args(HostKind::Codex).await
}

#[tokio::test]
async fn copilot_invokes_with_correct_args() -> TestResult {
    assert_correct_args(HostKind::Copilot).await
}

#[tokio::test]
async fn claude_passes_prompt_as_single_argv_item() -> TestResult {
    assert_prompt_single_arg(HostKind::Claude).await
}

#[tokio::test]
async fn codex_passes_prompt_as_single_argv_item() -> TestResult {
    assert_prompt_single_arg(HostKind::Codex).await
}

#[tokio::test]
async fn copilot_passes_prompt_as_single_argv_item() -> TestResult {
    assert_prompt_single_arg(HostKind::Copilot).await
}

#[tokio::test]
async fn claude_passes_cwd() -> TestResult {
    assert_passes_cwd(HostKind::Claude).await
}

#[tokio::test]
async fn codex_passes_cwd() -> TestResult {
    assert_passes_cwd(HostKind::Codex).await
}

#[tokio::test]
async fn copilot_passes_cwd() -> TestResult {
    assert_passes_cwd(HostKind::Copilot).await
}

#[tokio::test]
async fn claude_returns_stdout_as_response_text() -> TestResult {
    assert_stdout(HostKind::Claude).await
}

#[tokio::test]
async fn codex_returns_stdout_as_response_text() -> TestResult {
    assert_stdout(HostKind::Codex).await
}

#[tokio::test]
async fn copilot_returns_stdout_as_response_text() -> TestResult {
    assert_stdout(HostKind::Copilot).await
}

#[tokio::test]
async fn claude_surfaces_nonzero_exit_as_typed_error() -> TestResult {
    assert_nonzero(HostKind::Claude).await
}

#[tokio::test]
async fn codex_surfaces_nonzero_exit_as_typed_error() -> TestResult {
    assert_nonzero(HostKind::Codex).await
}

#[tokio::test]
async fn copilot_surfaces_nonzero_exit_as_typed_error() -> TestResult {
    assert_nonzero(HostKind::Copilot).await
}

#[tokio::test]
async fn claude_respects_timeout() -> TestResult {
    assert_timeout(HostKind::Claude).await
}

#[tokio::test]
async fn codex_respects_timeout() -> TestResult {
    assert_timeout(HostKind::Codex).await
}

#[tokio::test]
async fn copilot_respects_timeout() -> TestResult {
    assert_timeout(HostKind::Copilot).await
}

#[tokio::test]
async fn claude_is_available_returns_true_when_on_path() -> TestResult {
    assert_available_when_on_path(HostKind::Claude).await
}

#[tokio::test]
async fn codex_is_available_returns_true_when_on_path() -> TestResult {
    assert_available_when_on_path(HostKind::Codex).await
}

#[tokio::test]
async fn copilot_is_available_returns_true_when_on_path() -> TestResult {
    assert_available_when_on_path(HostKind::Copilot).await
}

#[tokio::test]
async fn claude_is_available_returns_false_when_absent() -> TestResult {
    assert_unavailable_when_absent(HostKind::Claude).await
}

#[tokio::test]
async fn codex_is_available_returns_false_when_absent() -> TestResult {
    assert_unavailable_when_absent(HostKind::Codex).await
}

#[tokio::test]
async fn copilot_is_available_returns_false_when_absent() -> TestResult {
    assert_unavailable_when_absent(HostKind::Copilot).await
}

#[tokio::test]
async fn claude_does_not_pass_model_override() -> TestResult {
    assert_no_model_override(HostKind::Claude).await
}

#[tokio::test]
async fn codex_does_not_pass_model_override() -> TestResult {
    assert_no_model_override(HostKind::Codex).await
}

#[tokio::test]
async fn copilot_does_not_pass_model_override() -> TestResult {
    assert_no_model_override(HostKind::Copilot).await
}

#[tokio::test]
async fn copilot_default_omits_allow_all_tools() -> TestResult {
    let response = run_with_script(
        HostKind::Copilot,
        r#"#!/bin/sh
for arg in "$@"; do
  if [ "$arg" = "--allow-all-tools" ]; then
    printf 'unexpected --allow-all-tools' >&2
    exit 7
  fi
done
printf 'default'
"#,
    )
    .await?;

    assert_eq!(response.stdout, "default");
    Ok(())
}

#[tokio::test]
async fn copilot_allow_all_appends_flag() -> TestResult {
    let _guard = process_lock().await;
    let host = mock_host(
        "copilot",
        r#"#!/bin/sh
for arg in "$@"; do
  if [ "$arg" = "--allow-all-tools" ]; then
    printf 'allow-all'
    exit 0
  fi
done
printf 'missing --allow-all-tools' >&2
exit 7
"#,
    )?;
    let cwd = tempdir()?;
    let adapter = CopilotHost::with_binary(host.path().join("copilot"));
    let mut req = request(cwd.path());
    req.copilot_tools = CopilotToolPermission::AllowAll;

    let response = adapter.run(req).await?;

    assert_eq!(response.stdout, "allow-all");
    Ok(())
}

#[tokio::test]
async fn claude_and_codex_ignore_copilot_tools_field() -> TestResult {
    let _guard = process_lock().await;
    for kind in [HostKind::Claude, HostKind::Codex] {
        let host = mock_host(
            kind.name(),
            r#"#!/bin/sh
for arg in "$@"; do
  if [ "$arg" = "--allow-all-tools" ]; then
    printf 'unexpected --allow-all-tools' >&2
    exit 7
  fi
done
printf 'ignored'
"#,
        )?;
        let cwd = tempdir()?;
        let adapter = kind.adapter(host.path().join(kind.name()));
        let mut req = request(cwd.path());
        req.copilot_tools = CopilotToolPermission::AllowAll;

        let response = adapter.run(req).await?;

        assert_eq!(response.stdout, "ignored");
    }
    Ok(())
}

#[tokio::test]
async fn not_found_returns_typed_error_with_host_name() -> TestResult {
    let _guard = process_lock().await;
    let cwd = tempdir()?;
    let adapter = ClaudeHost::with_binary(cwd.path().join("missing-claude"));

    let error = adapter.run(request(cwd.path())).await;

    match error {
        Err(HostError::NotFound { host }) => assert_eq!(host, "claude"),
        other => return Err(format!("unexpected error: {other:?}").into()),
    }
    Ok(())
}

#[tokio::test]
async fn spawn_io_error_surfaces_typed_error() -> TestResult {
    // Triggers an Io-classified spawn failure cross-platform by giving the
    // binary mode 0 (no execute, no read). Linux + macOS both return
    // PermissionDenied here, distinct from NotFound. An earlier attempt
    // used a regular file as cwd to provoke NotADirectory but macOS's
    // posix_spawn classifies that as NotFound instead, so the test was
    // non-portable.
    let _guard = process_lock().await;
    let dir = tempdir()?;
    let path = dir.path().join("claude");
    fs::write(&path, "#!/bin/sh\nprintf never\n")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = fs::metadata(&path)?.permissions();
        permissions.set_mode(0o000);
        fs::set_permissions(&path, permissions)?;
    }
    let adapter = ClaudeHost::with_binary(path);
    let req = request(dir.path());

    let error = adapter.run(req).await;

    match error {
        Err(HostError::Io { host, source }) => {
            assert_eq!(host, "claude");
            assert_eq!(source.kind(), std::io::ErrorKind::PermissionDenied);
        }
        other => return Err(format!("unexpected error: {other:?}").into()),
    }
    Ok(())
}

#[tokio::test]
async fn env_overrides_apply() -> TestResult {
    let _guard = process_lock().await;
    let host = mock_host(
        "claude",
        r#"#!/bin/sh
printf '%s' "$DERRICK_TOOLS_ENV_TEST"
"#,
    )?;
    let cwd = tempdir()?;
    let adapter = ClaudeHost::with_binary(host.path().join("claude"));
    let mut req = request(cwd.path());
    req.env
        .insert("DERRICK_TOOLS_ENV_TEST".to_owned(), "request".to_owned());

    let response = adapter.run(req).await?;

    assert_eq!(response.stdout, "request");
    Ok(())
}

#[tokio::test]
async fn env_overrides_take_precedence_over_inherited_env() -> TestResult {
    let _guard = process_lock().await;
    let host = mock_host(
        "claude",
        r#"#!/bin/sh
printf '%s' "$PATH"
"#,
    )?;
    let cwd = tempdir()?;
    let adapter = ClaudeHost::with_binary(host.path().join("claude"));
    let mut req = request(cwd.path());
    req.env
        .insert("PATH".to_owned(), "/request/path".to_owned());

    let response = adapter.run(req).await?;

    assert_eq!(response.stdout, "/request/path");
    Ok(())
}

#[tokio::test]
async fn env_omitted_when_request_empty() -> TestResult {
    let _guard = process_lock().await;
    let host = mock_host(
        "claude",
        r#"#!/bin/sh
if [ -n "$PATH" ]; then
  printf 'inherited'
fi
"#,
    )?;
    let cwd = tempdir()?;
    let adapter = ClaudeHost::with_binary(host.path().join("claude"));

    let response = adapter.run(request(cwd.path())).await?;

    assert_eq!(response.stdout, "inherited");
    Ok(())
}
