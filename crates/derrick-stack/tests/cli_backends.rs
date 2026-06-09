//! Integration tests for the `gt` (Graphite) and `gs` (git-spice) stacking
//! backends.
//!
//! These tests do not require the real `gt`/`gs` tools. Instead they put a
//! small fake shell script named `gt`/`gs` on `PATH` in a tempdir, configured
//! via an environment file to echo whatever output we want and to exit with
//! whatever status we want. Combined with a real temp git repo (created via the
//! real `git` CLI), this exercises the actual subprocess + error-mapping paths
//! the same way the repo's no-mock-DB rule intends: real processes, real git,
//! a scripted external tool.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

/// Tests in this file mutate the process-global `PATH`, so they must run one at
/// a time. Each test acquires this lock for its whole duration. The guard is
/// held alongside the [`Fixture`] (which restores `PATH` on drop).
fn path_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

use derrick_stack::{
    GitSpiceStackBackend, GraphiteStackBackend, OpenPrParams, RestackOutcome, RestackParams,
    StackBackend, StackError,
};

/// A temp git repo plus a tempdir holding fake `gt`/`gs` binaries on PATH.
struct Fixture {
    _guard: MutexGuard<'static, ()>,
    _tmp: tempfile::TempDir,
    repo_root: PathBuf,
    bin_dir: PathBuf,
    prev_path: Option<String>,
    branch: String,
}

impl Fixture {
    fn new() -> Self {
        let guard = path_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo_root = tmp.path().join("repo");
        let bin_dir = tmp.path().join("bin");
        fs::create_dir_all(&repo_root).expect("mkdir repo");
        fs::create_dir_all(&bin_dir).expect("mkdir bin");

        run_git(&repo_root, &["init", "-q", "-b", "main"]);
        run_git(&repo_root, &["config", "user.email", "t@example.com"]);
        run_git(&repo_root, &["config", "user.name", "Test"]);
        run_git(&repo_root, &["config", "commit.gpgsign", "false"]);
        run_git(&repo_root, &["config", "tag.gpgsign", "false"]);
        fs::write(repo_root.join("README.md"), "hello\n").expect("write file");
        run_git(&repo_root, &["add", "."]);
        run_git(
            &repo_root,
            &["commit", "-q", "--no-gpg-sign", "-m", "init"],
        );
        let branch = "derrick/alpha/drk-1".to_owned();
        run_git(&repo_root, &["checkout", "-q", "-b", &branch]);

        // Prepend our fake-bin dir to PATH so `gt`/`gs` resolve to our scripts.
        let prev_path = std::env::var("PATH").ok();
        let new_path = match &prev_path {
            Some(p) => format!("{}:{}", bin_dir.display(), p),
            None => bin_dir.display().to_string(),
        };
        // SAFETY: tests in this file run serially (see `#[serial]`-style guard
        // below via a single test invoking each). We restore PATH on drop.
        unsafe {
            std::env::set_var("PATH", new_path);
        }

        Self {
            _guard: guard,
            _tmp: tmp,
            repo_root,
            bin_dir,
            prev_path,
            branch,
        }
    }

    /// Install a fake `name` binary that prints `stdout`/`stderr` and exits with
    /// `exit_code`. The script ignores its arguments — that is fine because the
    /// tests here assert on derrick's *handling* of the tool's output, and the
    /// command-construction is unit-tested separately.
    fn install_fake(&self, name: &str, stdout: &str, stderr: &str, exit_code: i32) {
        let script = format!(
            "#!/bin/sh\nprintf '%s' \"{stdout}\"\nprintf '%s' \"{stderr}\" 1>&2\nexit {exit_code}\n",
            stdout = shell_escape(stdout),
            stderr = shell_escape(stderr),
            exit_code = exit_code,
        );
        let path = self.bin_dir.join(name);
        fs::write(&path, script).expect("write fake binary");
        make_executable(&path);
    }

    fn open_pr_params(&self) -> OpenPrParams {
        OpenPrParams {
            branch: self.branch.clone(),
            parent_branch: "main".to_owned(),
            title: "t".to_owned(),
            body: "b".to_owned(),
            draft: false,
            repo_root: self.repo_root.clone(),
        }
    }

    fn restack_params(&self) -> RestackParams {
        RestackParams {
            branch: self.branch.clone(),
            old_parent: "main".to_owned(),
            new_parent: "derrick/alpha/drk-0".to_owned(),
            repo_root: self.repo_root.clone(),
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        unsafe {
            match &self.prev_path {
                Some(p) => std::env::set_var("PATH", p),
                None => std::env::remove_var("PATH"),
            }
        }
    }
}

fn run_git(repo_root: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(repo_root)
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed");
}

fn shell_escape(s: &str) -> String {
    // We embed inside double quotes in the script; escape the chars that are
    // special there. Newlines are passed through literally.
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$")
        .replace('`', "\\`")
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path).expect("metadata").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).expect("chmod");
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) {}

// --- Graphite (gt) -------------------------------------------------------

#[tokio::test]
async fn graphite_open_pr_parses_url_from_gt_output() {
    let fx = Fixture::new();
    fx.install_fake(
        "gt",
        "Submitted branch. View it at https://github.com/foo/bar/pull/42\n",
        "",
        0,
    );
    let backend = GraphiteStackBackend::new().expect("gt present");
    let info = backend.open_pr(fx.open_pr_params()).await.expect("open_pr");
    assert_eq!(info.number, 42);
    assert_eq!(info.url, "https://github.com/foo/bar/pull/42");
    assert!(!info.head_sha.is_empty());
}

#[tokio::test]
async fn graphite_open_pr_maps_failure_to_gh_error() {
    let fx = Fixture::new();
    fx.install_fake("gt", "", "fatal: not authenticated\n", 1);
    let backend = GraphiteStackBackend::new().expect("gt present");
    let err = backend
        .open_pr(fx.open_pr_params())
        .await
        .expect_err("should fail");
    match err {
        StackError::Gh { message } => {
            assert!(message.contains("gt submit failed"), "got: {message}");
            assert!(message.contains("not authenticated"), "got: {message}");
        }
        other => panic!("expected Gh error, got {other:?}"),
    }
}

#[tokio::test]
async fn graphite_restack_success() {
    let fx = Fixture::new();
    fx.install_fake("gt", "restacked\n", "", 0);
    let backend = GraphiteStackBackend::new().expect("gt present");
    let outcome = backend.restack(fx.restack_params()).await.expect("restack");
    assert!(matches!(outcome, RestackOutcome::Restacked));
}

#[tokio::test]
async fn graphite_restack_conflict_bails_with_recipe() {
    let fx = Fixture::new();
    // gt reports a conflict on stderr and exits non-zero.
    fx.install_fake(
        "gt",
        "",
        "CONFLICT (content): Merge conflict in src/main.rs\n",
        1,
    );
    let backend = GraphiteStackBackend::new().expect("gt present");
    let outcome = backend.restack(fx.restack_params()).await.expect("restack");
    match outcome {
        RestackOutcome::Conflict { recipe } => {
            assert!(recipe.contains("git rebase --onto"), "got: {recipe}");
            assert!(recipe.contains("derrick/alpha/drk-0"), "got: {recipe}");
            assert!(recipe.contains("gt restack"), "got: {recipe}");
        }
        other => panic!("expected Conflict, got {other:?}"),
    }
}

// --- git-spice (gs) ------------------------------------------------------

#[tokio::test]
async fn git_spice_kind_is_git_spice() {
    let fx = Fixture::new();
    fx.install_fake("gs", "", "", 0);
    let backend = GitSpiceStackBackend::new().expect("gs present");
    assert_eq!(backend.kind(), "git-spice");
}

#[tokio::test]
async fn git_spice_open_pr_parses_url() {
    let fx = Fixture::new();
    fx.install_fake(
        "gs",
        "Created PR #7: https://github.com/foo/bar/pull/7\n",
        "",
        0,
    );
    let backend = GitSpiceStackBackend::new().expect("gs present");
    let info = backend.open_pr(fx.open_pr_params()).await.expect("open_pr");
    assert_eq!(info.number, 7);
    assert_eq!(info.url, "https://github.com/foo/bar/pull/7");
}

#[tokio::test]
async fn git_spice_restack_conflict_bails_with_recipe() {
    let fx = Fixture::new();
    fx.install_fake("gs", "", "merge conflict; please resolve\n", 1);
    let backend = GitSpiceStackBackend::new().expect("gs present");
    let outcome = backend.restack(fx.restack_params()).await.expect("restack");
    match outcome {
        RestackOutcome::Conflict { recipe } => {
            assert!(recipe.contains("git rebase --onto"), "got: {recipe}");
            assert!(recipe.contains("gs upstack restack"), "got: {recipe}");
        }
        other => panic!("expected Conflict, got {other:?}"),
    }
}

#[tokio::test]
async fn git_spice_restack_non_conflict_failure_is_error() {
    let fx = Fixture::new();
    fx.install_fake("gs", "", "fatal: not a git-spice repo\n", 1);
    let backend = GitSpiceStackBackend::new().expect("gs present");
    let err = backend
        .restack(fx.restack_params())
        .await
        .expect_err("should error");
    match err {
        StackError::Git { message } => {
            assert!(message.contains("gs upstack restack failed"), "got: {message}");
        }
        other => panic!("expected Git error, got {other:?}"),
    }
}
