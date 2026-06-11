//! Integration tests for the native backend's `gh`-facing methods using a
//! fake `gh` on PATH.
//!
//! Per the crate's no-mock rule for git we still use real `git`, but `gh`
//! talks to GitHub, so we install a recording shell stub named `gh` in a temp
//! dir prepended to PATH. The stub appends its full argv to a log file and
//! emits canned stdout. Tests assert on the recorded invocations
//! (retarget_pr, set_pr_body) and on parsed stdout (pr_body).

use std::path::PathBuf;

use derrick_config::ForcePush;
use derrick_stack::{NativeStackBackend, StackBackend};

/// A fake `gh` binary on PATH that records every invocation.
struct FakeGh {
    _tmp: tempfile::TempDir,
    bin_dir: PathBuf,
    log_path: PathBuf,
    old_path: Option<String>,
}

impl FakeGh {
    /// Install a `gh` stub. `body_stdout` is what `gh pr view ... -q .body`
    /// should print; all other invocations just succeed silently.
    fn install(body_stdout: &str) -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let bin_dir = tmp.path().join("bin");
        std::fs::create_dir_all(&bin_dir).expect("mkdir bin");
        let log_path = tmp.path().join("gh-invocations.log");
        let gh_path = bin_dir.join("gh");

        // The stub logs all args (one per line, NUL-free) then, when asked for
        // a body via `pr view`, prints the canned body. Everything exits 0.
        let script = format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> '{log}'\n\
             case \"$1 $2\" in\n\
             'pr view') cat <<'EOF'\n{body}\nEOF\n;;\n\
             esac\n\
             exit 0\n",
            log = log_path.display(),
            body = body_stdout,
        );
        std::fs::write(&gh_path, script).expect("write gh stub");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&gh_path).expect("stat").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&gh_path, perms).expect("chmod");
        }

        let old_path = std::env::var("PATH").ok();
        let new_path = match &old_path {
            Some(existing) => format!("{}:{}", bin_dir.display(), existing),
            None => bin_dir.display().to_string(),
        };
        // SAFETY: tests in this file run single-threaded per process via the
        // serial guard below; PATH mutation is scoped to the FakeGh lifetime.
        unsafe {
            std::env::set_var("PATH", &new_path);
        }

        Self {
            _tmp: tmp,
            bin_dir,
            log_path,
            old_path,
        }
    }

    fn invocations(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log_path)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

impl Drop for FakeGh {
    fn drop(&mut self) {
        unsafe {
            match &self.old_path {
                Some(p) => std::env::set_var("PATH", p),
                None => std::env::remove_var("PATH"),
            }
        }
        let _ = &self.bin_dir;
    }
}

fn repo_root() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("repo");
    std::fs::create_dir_all(&root).expect("mkdir");
    (tmp, root)
}

// Mutating the process PATH is not thread-safe; serialise these tests. The
// tests are sync `#[test]`s that build their own current-thread runtime and
// `block_on`, so the guard never crosses an `.await` (clippy
// await_holding_lock) — the synchronous `block_on` returns before the guard
// drops.
static GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn run_serial<F: std::future::Future<Output = ()>>(body: F) {
    let _g = GUARD.lock().unwrap_or_else(|e| e.into_inner());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(body);
}

#[test]
fn retarget_pr_invokes_gh_pr_edit_base() {
    run_serial(async {
        let fake = FakeGh::install("");
        let (_tmp, root) = repo_root();
        let backend = NativeStackBackend::new(root.clone(), ForcePush::WithLease);

        backend
            .retarget_pr("derrick/b/drk-2", "main", &root)
            .await
            .expect("retarget");

        let calls = fake.invocations();
        assert!(
            calls
                .iter()
                .any(|c| c.contains("pr edit derrick/b/drk-2 --base main")),
            "expected gh pr edit --base, got: {calls:?}",
        );
    });
}

#[test]
fn set_pr_body_invokes_gh_pr_edit_body() {
    run_serial(async {
        let fake = FakeGh::install("");
        let (_tmp, root) = repo_root();
        let backend = NativeStackBackend::new(root.clone(), ForcePush::WithLease);

        backend
            .set_pr_body("derrick/b/drk-1", "new body text", &root)
            .await
            .expect("set body");

        let calls = fake.invocations();
        assert!(
            calls
                .iter()
                .any(|c| c.contains("pr edit derrick/b/drk-1 --body")),
            "expected gh pr edit --body, got: {calls:?}",
        );
    });
}

#[test]
fn pr_body_reads_existing_body_via_gh() {
    run_serial(async {
        let _fake = FakeGh::install("hello stack body");
        let (_tmp, root) = repo_root();
        let backend = NativeStackBackend::new(root.clone(), ForcePush::WithLease);

        let body = backend
            .pr_body("derrick/b/drk-1", &root)
            .await
            .expect("read body");
        assert_eq!(body.as_deref(), Some("hello stack body"));
    });
}
