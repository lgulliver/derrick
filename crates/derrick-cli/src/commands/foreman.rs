//! `derrick foreman ...` subcommands. See DESIGN.md §8.6 and T012.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use derrick_claude::{ClaudeHandDispatcher, ClaudeHandDispatcherConfig};
use derrick_config::{Config, StackBackendKind, SubstrateBackendKind};
use derrick_copilot::{LocalCopilotHandDispatcher, LocalCopilotHandDispatcherConfig};
use derrick_stack::{GraphiteStackBackend, NativeStackBackend, NoneStackBackend, StackBackend};
use derrick_substrate::Substrate;
#[allow(deprecated)]
use derrick_substrate_native::foreman::CopilotStubDispatcher;
use derrick_substrate_native::foreman::{
    Foreman, ForemanTtls, GhRepoState, HandDispatcher, MultiDispatcher,
};
use derrick_substrate_native::NativeSubstrate;

use crate::commands::{
    ForemanArgs, ForemanCommand, ForemanStartArgs, ForemanStartMode, ForemanStopArgs,
    ForemanTickArgs,
};
use crate::exit_code::CliExitCode;
use crate::{current_repo_root, message, native_paths, read_config};

const PID_FILE: &str = "foreman.pid";
const LOG_FILE: &str = "foreman.log";

pub(crate) async fn execute(args: ForemanArgs) -> Result<CliExitCode, crate::CliError> {
    match args.command {
        ForemanCommand::Start(start) => foreman_start(start).await,
        ForemanCommand::Stop(stop) => foreman_stop(stop),
        ForemanCommand::Tick(tick) => foreman_tick(tick).await,
    }
}

async fn foreman_start(args: ForemanStartArgs) -> Result<CliExitCode, crate::CliError> {
    let repo_root = current_repo_root()?;
    let config = read_config(&repo_root)?;
    require_native(&config)?;

    let internal_daemon_child = args.internal_daemon_child;
    let mode = args.mode().unwrap_or(ForemanStartMode::Detached);
    match mode {
        ForemanStartMode::Attached => {
            if internal_daemon_child {
                // Spawned by the parent detached-start path: the parent
                // already wrote `mode = detached` to the foreman row; the
                // child just runs the loop.
                run_daemon_child(&repo_root, &config).await
            } else {
                start_attached(&repo_root, &config).await
            }
        }
        ForemanStartMode::Detached => start_detached(&repo_root, &config).await,
    }
}

async fn start_attached(repo_root: &Path, config: &Config) -> Result<CliExitCode, crate::CliError> {
    let substrate = open_substrate(repo_root, config).await?;
    let pid = std::process::id();
    substrate.record_foreman_attached(pid).await?;
    let foreman = build_foreman(repo_root, config, Arc::clone(&substrate));
    let result = foreman.run_attached().await;
    let _ignored = substrate.record_foreman_stopped().await;
    drop(foreman);
    drop(substrate);
    match result {
        Ok(()) => Ok(CliExitCode::Success),
        Err(error) => Err(message(format!("foreman exited with error: {error}"))),
    }
}

async fn foreman_tick(_args: ForemanTickArgs) -> Result<CliExitCode, crate::CliError> {
    let repo_root = current_repo_root()?;
    let config = read_config(&repo_root)?;
    require_native(&config)?;
    let substrate = open_substrate(&repo_root, &config).await?;
    let foreman = build_foreman(&repo_root, &config, Arc::clone(&substrate));
    let report = foreman
        .tick()
        .await
        .map_err(|error| message(format!("foreman tick failed: {error}")))?;
    println!(
        "tick: cleanup={} verifier={} unblocked={} dispatched={}",
        report.cleanup_actions.len(),
        report.verifier_actions.len(),
        report.unblocked.len(),
        report.dispatched.len()
    );
    Ok(CliExitCode::Success)
}

fn build_foreman(repo_root: &Path, config: &Config, substrate: Arc<NativeSubstrate>) -> Foreman {
    let ttls = ForemanTtls {
        poll_interval: config.tools().foreman().poll_interval(),
        in_review_ttl: chrono::Duration::from_std(config.tools().foreman().in_review_ttl())
            .unwrap_or_else(|_| chrono::Duration::hours(24)),
        hand_ttl: chrono::Duration::from_std(config.tools().foreman().hand_ttl())
            .unwrap_or_else(|_| chrono::Duration::minutes(30)),
        worktree_ttl: chrono::Duration::from_std(config.tools().foreman().worktree_ttl())
            .unwrap_or_else(|_| chrono::Duration::hours(24)),
    };
    let dispatcher: Box<dyn HandDispatcher> = build_dispatcher(repo_root, config, &substrate);
    let stack_cfg = config.tools().git().stacking().clone();
    let stack_backend: Arc<dyn StackBackend> = match stack_cfg.backend() {
        StackBackendKind::Native => Arc::new(NativeStackBackend::new(
            repo_root.to_path_buf(),
            stack_cfg.force_push(),
        )),
        StackBackendKind::Graphite | StackBackendKind::GitSpice => Arc::new(GraphiteStackBackend),
        StackBackendKind::None => Arc::new(NoneStackBackend),
    };
    Foreman::new(
        substrate,
        config.clone(),
        Box::new(GhRepoState::new(repo_root.to_path_buf())),
        repo_root.to_path_buf(),
        dispatcher,
    )
    .with_ttls(ttls)
    .with_exit_when_idle(config.tools().foreman().exit_when_idle())
    .with_stack_backend(stack_backend, stack_cfg)
}

fn build_dispatcher(
    repo_root: &Path,
    config: &Config,
    substrate: &Arc<NativeSubstrate>,
) -> Box<dyn HandDispatcher> {
    let copilot_enabled = config.tools().copilot().enabled();
    let claude_enabled = config.tools().claude().enabled();
    // Default to copilot when enabled, otherwise claude; falls back to the
    // copilot stub below when neither is on.
    let default_kind = if copilot_enabled {
        "copilot"
    } else if claude_enabled {
        "claude"
    } else {
        "copilot"
    };
    let mut multi = MultiDispatcher::new(default_kind);

    if copilot_enabled {
        // Local CLI dispatcher: spawns `copilot -p <prompt> --add-dir <worktree>`
        // per ticket. The legacy cloud (gh issue + @copilot) dispatcher is
        // intentionally not wired here; it will return when the cloud path
        // is needed again.
        let copilot_config = LocalCopilotHandDispatcherConfig {
            auto_dispatch: true,
            poll_interval: config.tools().copilot().poll_interval(),
            poll_timeout: config.tools().copilot().poll_timeout(),
            agent_identity: config.tools().copilot().agent_identity().to_owned(),
            branch_prefix: config.tools().git().branch_prefix().to_owned(),
            queue_dir: repo_root.join(".derrick/copilot-queue"),
            repo_root: repo_root.to_path_buf(),
            worktree_root: repo_root.join(".derrick/copilot-worktrees"),
            copilot_binary: std::path::PathBuf::from("copilot"),
            allow_all_tools: true,
            roughneck_enabled: config.tools().roughneck().enabled(),
            roughneck_level: config.tools().roughneck().level().to_owned(),
        };
        multi = multi.register(Box::new(LocalCopilotHandDispatcher::new(
            Arc::clone(substrate),
            copilot_config,
        )));
    }

    if claude_enabled {
        let claude_cfg = config.tools().claude();
        let dispatcher_config = ClaudeHandDispatcherConfig {
            auto_dispatch: claude_cfg.auto_dispatch(),
            poll_interval: claude_cfg.poll_interval(),
            poll_timeout: claude_cfg.poll_timeout(),
            agent_identity: claude_cfg.agent_identity().to_owned(),
            branch_prefix: config.tools().git().branch_prefix().to_owned(),
            queue_dir: repo_root.join(claude_cfg.queue_dir()),
            base_branch: "main".to_owned(),
            roughneck_enabled: config.tools().roughneck().enabled(),
            roughneck_level: config.tools().roughneck().level().to_owned(),
        };
        multi = multi.register(Box::new(ClaudeHandDispatcher::new(
            Arc::clone(substrate),
            dispatcher_config,
        )));
    }

    if multi.is_empty() {
        // No dispatchers enabled: keep the stub in place so the foreman can
        // still tick on non-copilot workloads (e.g. human mode). The stub
        // returns NotImplemented, which the foreman surfaces as an event
        // without failing the tick.
        #[allow(deprecated)]
        let stub = CopilotStubDispatcher;
        Box::new(stub)
    } else {
        Box::new(multi)
    }
}

async fn open_substrate(
    repo_root: &Path,
    config: &Config,
) -> Result<Arc<NativeSubstrate>, crate::CliError> {
    let native_config = native_paths(repo_root, config);
    if !native_config.db_path.exists() {
        return Err(message(format!(
            "{} does not exist; run `derrick init --greenfield` first",
            native_config.db_path.display()
        )));
    }
    let substrate = NativeSubstrate::open(native_config, config.site().clone()).await?;
    Ok(Arc::new(substrate))
}

fn require_native(config: &Config) -> Result<(), crate::CliError> {
    if config.tools().substrate().backend() != SubstrateBackendKind::Native {
        return Err(message(
            "derrick foreman requires tools.substrate.backend: native",
        ));
    }
    Ok(())
}

fn state_dir(repo_root: &Path, config: &Config) -> PathBuf {
    repo_root.join(config.state().dir())
}

fn pid_path(repo_root: &Path, config: &Config) -> PathBuf {
    state_dir(repo_root, config).join(PID_FILE)
}

fn log_path(repo_root: &Path, config: &Config) -> PathBuf {
    state_dir(repo_root, config).join(LOG_FILE)
}

// ---------- Detached daemonisation (unix) ---------------------------------

#[cfg(unix)]
async fn start_detached(repo_root: &Path, config: &Config) -> Result<CliExitCode, crate::CliError> {
    use std::os::unix::process::CommandExt;

    let state = state_dir(repo_root, config);
    fs::create_dir_all(&state).map_err(|source| crate::CliError::Io {
        path: state.clone(),
        source,
    })?;
    let pid_file = pid_path(repo_root, config);
    if pid_file.exists() {
        if let Ok(existing) = fs::read_to_string(&pid_file) {
            if let Ok(pid) = existing.trim().parse::<i32>() {
                if process_is_alive(pid) {
                    return Err(message(format!(
                        "foreman already running with pid {pid}; run `derrick foreman stop` first"
                    )));
                }
            }
        }
    }

    let exe = std::env::current_exe().map_err(|source| crate::CliError::Io {
        path: "<self>".into(),
        source,
    })?;

    let log = log_path(repo_root, config);
    let log_handle = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .map_err(|source| crate::CliError::Io {
            path: log.clone(),
            source,
        })?;
    let log_handle_err = log_handle
        .try_clone()
        .map_err(|source| crate::CliError::Io {
            path: log.clone(),
            source,
        })?;

    // Spawn the daemon child via std::process::Command. The child re-execs
    // `derrick foreman start --attached --__internal-daemon-child` which
    // skips the foreman-row write (the parent owns that). pre_exec runs in
    // the child after fork and before exec; we use it to setsid() so the
    // child detaches from the controlling terminal.
    let mut command = std::process::Command::new(exe);
    command
        .arg("foreman")
        .arg("start")
        .arg("--attached")
        .arg("--__internal-daemon-child")
        .current_dir(repo_root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log_handle))
        .stderr(std::process::Stdio::from(log_handle_err));
    // SAFETY: `setsid` is async-signal-safe and the only operation we
    // perform between fork and exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn().map_err(|source| crate::CliError::Io {
        path: "<spawn>".into(),
        source,
    })?;
    let child_pid = child.id();
    // Don't wait on the child — it should outlive us.
    std::mem::forget(child);

    // Record substrate state and write pid file (still inside the parent
    // runtime).
    let substrate = open_substrate(repo_root, config).await?;
    substrate.record_foreman_detached(child_pid).await?;
    drop(substrate);

    fs::write(&pid_file, format!("{child_pid}\n")).map_err(|source| crate::CliError::Io {
        path: pid_file.clone(),
        source,
    })?;
    println!("foreman started (pid {child_pid}); log: {}", log.display());
    Ok(CliExitCode::Success)
}

#[cfg(not(unix))]
async fn start_detached(
    _repo_root: &Path,
    _config: &Config,
) -> Result<CliExitCode, crate::CliError> {
    Err(message(
        "foreman --detached is unix-only; use --attached on this platform",
    ))
}

#[cfg(unix)]
async fn run_daemon_child(
    repo_root: &Path,
    config: &Config,
) -> Result<CliExitCode, crate::CliError> {
    // Child path: do NOT write to the foreman row (parent already did).
    // Just run the loop until SIGTERM/SIGINT.
    let substrate = open_substrate(repo_root, config).await?;
    let foreman = build_foreman(repo_root, config, Arc::clone(&substrate));
    let result = foreman.run_attached().await;
    let _ignored = substrate.record_foreman_stopped().await;
    match result {
        Ok(()) => Ok(CliExitCode::Success),
        Err(error) => Err(message(format!("foreman exited with error: {error}"))),
    }
}

#[cfg(not(unix))]
async fn run_daemon_child(
    _repo_root: &Path,
    _config: &Config,
) -> Result<CliExitCode, crate::CliError> {
    Err(message("daemon child path is unix-only"))
}

// ---------- Stop ----------------------------------------------------------

#[cfg(unix)]
fn foreman_stop(_args: ForemanStopArgs) -> Result<CliExitCode, crate::CliError> {
    let repo_root = current_repo_root()?;
    let config = read_config(&repo_root)?;
    let pid_file = pid_path(&repo_root, &config);
    if !pid_file.exists() {
        return Err(message(format!(
            "no foreman pid file at {}",
            pid_file.display()
        )));
    }
    let contents = fs::read_to_string(&pid_file).map_err(|source| crate::CliError::Io {
        path: pid_file.clone(),
        source,
    })?;
    let pid: i32 = contents
        .trim()
        .parse()
        .map_err(|_| message(format!("malformed pid file {}", pid_file.display())))?;

    // SAFETY: signalling a known pid is safe; the kernel does the check.
    let send_signal = |sig: libc::c_int| -> i32 { unsafe { libc::kill(pid, sig) } };

    if !process_is_alive(pid) {
        let _ignored = fs::remove_file(&pid_file);
        println!("foreman pid {pid} not running; cleaned up pid file");
        return Ok(CliExitCode::Success);
    }

    if send_signal(libc::SIGTERM) != 0 {
        return Err(message(format!("failed to send SIGTERM to pid {pid}")));
    }

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if !process_is_alive(pid) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if process_is_alive(pid) {
        let _ = send_signal(libc::SIGKILL);
        // Wait briefly for kernel to reap.
        let kill_deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < kill_deadline {
            if !process_is_alive(pid) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fs::remove_file(&pid_file).map_err(|source| crate::CliError::Io {
        path: pid_file.clone(),
        source,
    })?;
    println!("foreman pid {pid} stopped");
    Ok(CliExitCode::Success)
}

#[cfg(not(unix))]
fn foreman_stop(_args: ForemanStopArgs) -> Result<CliExitCode, crate::CliError> {
    Err(message("derrick foreman stop is unix-only"))
}

#[cfg(unix)]
fn process_is_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // SAFETY: kill(_, 0) is the documented liveness check.
    unsafe { libc::kill(pid, 0) == 0 }
}

#[cfg(not(unix))]
fn process_is_alive(_pid: i32) -> bool {
    false
}
