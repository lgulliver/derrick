//! Integration tests for the native stacking backend.
//!
//! Per the crate's no-mock rule these run against a real temporary git repo
//! created with the real `git` CLI. They exercise the rebase/conflict machinery
//! that derrick owns directly (D72): `git rebase --onto`, the D19 abort-and-bail
//! conflict policy, and the `--force-with-lease` push gate. No remote and no
//! `gh` are required — `restack` operates purely on local refs (its `git fetch
//! origin` failure is logged and tolerated), and `force_push` against an absent
//! remote is asserted only for the policy-off short-circuit.

use std::path::Path;
use std::process::Command;

use derrick_config::ForcePush;
use derrick_stack::{NativeStackBackend, RestackOutcome, RestackParams, StackBackend};

fn git(repo_root: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(repo_root)
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed");
}

fn git_stdout(repo_root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo_root)
        .output()
        .expect("run git");
    assert!(output.status.success(), "git {args:?} failed");
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn write(repo_root: &Path, name: &str, contents: &str) {
    std::fs::write(repo_root.join(name), contents).expect("write file");
}

/// Initialise a repo with a `main` commit and return its path. The tempdir is
/// returned alongside so the caller keeps it alive for the test's duration.
fn init_repo() -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_root = tmp.path().join("repo");
    std::fs::create_dir_all(&repo_root).expect("mkdir repo");
    git(&repo_root, &["init", "-q", "-b", "main"]);
    git(&repo_root, &["config", "user.email", "t@example.com"]);
    git(&repo_root, &["config", "user.name", "Test"]);
    git(&repo_root, &["config", "commit.gpgsign", "false"]);
    git(&repo_root, &["config", "tag.gpgsign", "false"]);
    write(&repo_root, "base.txt", "base\n");
    git(&repo_root, &["add", "."]);
    git(&repo_root, &["commit", "-q", "--no-gpg-sign", "-m", "init"]);
    (tmp, repo_root)
}

/// A clean restack: `feature` was cut from `main@v1`; `main` advances to a new
/// parent branch with a non-conflicting change; rebasing `feature` --onto the
/// new parent replays its commit on top, and the file from the new parent is
/// now present on `feature`.
#[tokio::test]
async fn native_restack_clean_rebase_moves_branch_onto_new_parent() {
    let (_tmp, repo_root) = init_repo();

    // old_parent: the commit `feature` was originally based on.
    git(&repo_root, &["branch", "old-parent"]);

    // feature branch adds its own file on top of old-parent.
    git(&repo_root, &["checkout", "-q", "-b", "feature"]);
    write(&repo_root, "feature.txt", "feature work\n");
    git(&repo_root, &["add", "."]);
    git(
        &repo_root,
        &["commit", "-q", "--no-gpg-sign", "-m", "feature"],
    );

    // new-parent advances main with a disjoint file.
    git(&repo_root, &["checkout", "-q", "-b", "new-parent", "main"]);
    write(&repo_root, "parent.txt", "parent work\n");
    git(&repo_root, &["add", "."]);
    git(
        &repo_root,
        &["commit", "-q", "--no-gpg-sign", "-m", "parent"],
    );

    let backend = NativeStackBackend::new(repo_root.clone(), ForcePush::WithLease);
    let outcome = backend
        .restack(RestackParams {
            branch: "feature".to_owned(),
            old_parent: "old-parent".to_owned(),
            new_parent: "new-parent".to_owned(),
            repo_root: repo_root.clone(),
        })
        .await
        .expect("restack");
    assert!(matches!(outcome, RestackOutcome::Restacked), "{outcome:?}");

    // feature now contains the new parent's file and its own work, and sits on
    // top of new-parent.
    git(&repo_root, &["checkout", "-q", "feature"]);
    assert!(
        repo_root.join("parent.txt").exists(),
        "feature should carry new-parent's file after rebase",
    );
    assert!(repo_root.join("feature.txt").exists());
    let merge_base = git_stdout(&repo_root, &["merge-base", "feature", "new-parent"]);
    let new_parent_tip = git_stdout(&repo_root, &["rev-parse", "new-parent"]);
    assert_eq!(
        merge_base, new_parent_tip,
        "feature must be based on new-parent's tip",
    );
}

/// A conflicting restack must bail per D19: the backend aborts the in-progress
/// rebase (leaving a clean working tree on the original branch) and returns the
/// exact `git rebase --onto` recipe — it never force-resolves.
#[tokio::test]
async fn native_restack_conflict_aborts_and_returns_recipe() {
    let (_tmp, repo_root) = init_repo();

    git(&repo_root, &["branch", "old-parent"]);

    // feature edits conflict.txt.
    git(&repo_root, &["checkout", "-q", "-b", "feature"]);
    write(&repo_root, "conflict.txt", "feature version\n");
    git(&repo_root, &["add", "."]);
    git(
        &repo_root,
        &["commit", "-q", "--no-gpg-sign", "-m", "feature edit"],
    );

    // new-parent edits the SAME file differently → rebase conflict.
    git(&repo_root, &["checkout", "-q", "-b", "new-parent", "main"]);
    write(&repo_root, "conflict.txt", "parent version\n");
    git(&repo_root, &["add", "."]);
    git(
        &repo_root,
        &["commit", "-q", "--no-gpg-sign", "-m", "parent edit"],
    );

    git(&repo_root, &["checkout", "-q", "feature"]);

    let backend = NativeStackBackend::new(repo_root.clone(), ForcePush::WithLease);
    let outcome = backend
        .restack(RestackParams {
            branch: "feature".to_owned(),
            old_parent: "old-parent".to_owned(),
            new_parent: "new-parent".to_owned(),
            repo_root: repo_root.clone(),
        })
        .await
        .expect("restack call should not error; it returns Conflict");

    match outcome {
        RestackOutcome::Conflict { recipe } => {
            assert!(recipe.contains("git rebase --onto"), "got: {recipe}");
            assert!(recipe.contains("new-parent"), "got: {recipe}");
            assert!(recipe.contains("old-parent"), "got: {recipe}");
            assert!(recipe.contains("feature"), "got: {recipe}");
        }
        other => panic!("expected Conflict, got {other:?}"),
    }

    // The abort must have left no rebase in progress and a clean tree.
    let status = git_stdout(&repo_root, &["status", "--porcelain"]);
    assert!(
        status.is_empty(),
        "working tree should be clean: {status:?}"
    );
    assert!(
        !repo_root.join(".git/rebase-merge").exists()
            && !repo_root.join(".git/rebase-apply").exists(),
        "no rebase should be in progress after abort",
    );
}

/// `force_push` honours the policy gate: when force-push is off it short-circuits
/// with `NotSupported` and never shells out to git.
#[tokio::test]
async fn native_force_push_off_is_not_supported() {
    let (_tmp, repo_root) = init_repo();
    let backend = NativeStackBackend::new(repo_root.clone(), ForcePush::Off);
    let err = backend
        .force_push("feature", &repo_root)
        .await
        .expect_err("force_push off must be rejected");
    assert!(
        matches!(err, derrick_stack::StackError::NotSupported { .. }),
        "{err:?}",
    );
}
