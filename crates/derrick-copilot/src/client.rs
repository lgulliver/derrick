//! Copilot dispatch client.
//!
//! `CopilotDispatchClient` is the trait the dispatcher uses to talk to
//! GitHub. The production implementation shells to the `gh` CLI; tests use
//! an in-memory fake. We trigger the Copilot coding agent the documented
//! way — `gh issue create` followed by `gh issue edit --add-assignee @copilot`
//! — which is the stable surface for a `gh`-based client. (The
//! `create_pull_request_with_copilot` MCP endpoint wraps the same
//! issue-assign flow but is a different transport derrick doesn't use at
//! runtime; it also 401'd during T013 development.)

use std::path::{Path, PathBuf};
use std::process::Stdio;

use async_trait::async_trait;
use serde_json::Value;
use thiserror::Error;
use tokio::process::Command;
use tracing::{debug, warn};

/// GitHub issue number used to identify a dispatched Copilot task.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskId {
    /// Issue number returned by `gh issue create`.
    pub issue_number: u64,
    /// URL of the issue, when reported by gh.
    pub issue_url: Option<String>,
}

/// PR metadata reported by `gh pr list --head <branch>`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrInfo {
    /// PR number.
    pub number: u64,
    /// PR URL.
    pub url: String,
    /// Head commit SHA.
    pub head_sha: String,
}

/// Errors returned by [`CopilotDispatchClient`] implementations.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum CopilotDispatchError {
    /// I/O error spawning or waiting on the `gh` subprocess.
    #[error("gh io error in {cwd}: {source}")]
    Io {
        /// Working directory used for the gh command.
        cwd: PathBuf,
        /// Source I/O error.
        source: std::io::Error,
    },
    /// `gh` exited non-zero.
    #[error("gh {operation} failed (exit {exit_code}): {stderr}")]
    NonZeroExit {
        /// Operation we attempted (e.g. `"issue create"`).
        operation: String,
        /// Exit code reported by gh, or `-1` when no code was reported.
        exit_code: i32,
        /// Captured stderr (lossy UTF-8).
        stderr: String,
    },
    /// `gh` returned output we could not parse (e.g. missing issue number).
    #[error("gh {operation} returned unparsable output: {message}")]
    Parse {
        /// Operation we attempted.
        operation: String,
        /// Description of what was missing or malformed.
        message: String,
    },
}

/// Client used by the dispatcher to file a Copilot task and observe PR
/// creation.
#[async_trait]
pub trait CopilotDispatchClient: Send + Sync {
    /// Create an issue, label it `copilot`, and assign Copilot.
    async fn create_task(
        &self,
        branch: &str,
        title: &str,
        body: &str,
    ) -> Result<TaskId, CopilotDispatchError>;

    /// Look up a PR whose head branch is `branch`. Returns `None` when
    /// Copilot has not opened one yet.
    async fn poll_pr(&self, branch: &str) -> Result<Option<PrInfo>, CopilotDispatchError>;
}

/// Production [`CopilotDispatchClient`] backed by the `gh` CLI.
pub struct GhCopilotClient {
    repo_root: PathBuf,
    label: String,
    assignee: String,
}

impl GhCopilotClient {
    /// Construct a client rooted at `repo_root`, assigning Copilot via the
    /// `@copilot` magic assignee and labelling new issues `copilot`.
    pub fn new(repo_root: PathBuf) -> Self {
        Self {
            repo_root,
            label: "copilot".to_owned(),
            assignee: "@copilot".to_owned(),
        }
    }

    /// Override the issue label (defaults to `copilot`).
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = label.into();
        self
    }

    /// Override the assignee (defaults to `@copilot`).
    pub fn with_assignee(mut self, assignee: impl Into<String>) -> Self {
        self.assignee = assignee.into();
        self
    }

    /// Path to the repo this client operates against.
    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    async fn run_gh(
        &self,
        operation: &str,
        args: &[&str],
    ) -> Result<std::process::Output, CopilotDispatchError> {
        debug!(operation, cwd = %self.repo_root.display(), args = ?args, "running gh");
        let output = Command::new("gh")
            .args(args)
            .current_dir(&self.repo_root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|source| CopilotDispatchError::Io {
                cwd: self.repo_root.clone(),
                source,
            })?;
        if !output.status.success() {
            return Err(CopilotDispatchError::NonZeroExit {
                operation: operation.to_owned(),
                exit_code: output.status.code().unwrap_or(-1),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }
        Ok(output)
    }
}

#[async_trait]
impl CopilotDispatchClient for GhCopilotClient {
    async fn create_task(
        &self,
        branch: &str,
        title: &str,
        body: &str,
    ) -> Result<TaskId, CopilotDispatchError> {
        // The body explicitly tells Copilot which branch to target so the
        // resulting PR can be matched back to this dispatch.
        let augmented_body =
            format!("{body}\n\n---\nDispatched by derrick. Target branch: `{branch}`.\n");
        let create_output = self
            .run_gh(
                "issue create",
                &[
                    "issue",
                    "create",
                    "--title",
                    title,
                    "--body",
                    &augmented_body,
                    "--label",
                    &self.label,
                ],
            )
            .await?;
        let stdout = String::from_utf8_lossy(&create_output.stdout)
            .trim()
            .to_owned();
        let (issue_number, issue_url) =
            parse_issue_create_output(&stdout).ok_or_else(|| CopilotDispatchError::Parse {
                operation: "issue create".to_owned(),
                message: format!("could not extract issue number from gh output: {stdout:?}"),
            })?;

        // Assign Copilot. If this fails the issue still exists; surface
        // the error so the dispatcher can mark dispatch failed.
        let assignee_arg = format!("--add-assignee={}", self.assignee);
        let issue_number_str = issue_number.to_string();
        if let Err(error) = self
            .run_gh(
                "issue edit",
                &["issue", "edit", &issue_number_str, &assignee_arg],
            )
            .await
        {
            warn!(
                ?error,
                issue_number,
                "failed to assign copilot via `gh issue edit`; the issue exists but copilot may not have picked it up"
            );
            return Err(error);
        }
        Ok(TaskId {
            issue_number,
            issue_url,
        })
    }

    async fn poll_pr(&self, branch: &str) -> Result<Option<PrInfo>, CopilotDispatchError> {
        let output = self
            .run_gh(
                "pr list",
                &[
                    "pr",
                    "list",
                    "--head",
                    branch,
                    "--state",
                    "open",
                    "--json",
                    "number,url,headRefOid",
                ],
            )
            .await?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        parse_pr_list_output(&stdout)
    }
}

fn parse_issue_create_output(stdout: &str) -> Option<(u64, Option<String>)> {
    // `gh issue create` prints the issue URL (e.g.
    // `https://github.com/owner/repo/issues/42`) on stdout. Extract the
    // trailing integer.
    let last_line = stdout.lines().last()?.trim();
    let number = last_line.rsplit('/').next()?.parse::<u64>().ok()?;
    let url = if last_line.starts_with("http") {
        Some(last_line.to_owned())
    } else {
        None
    };
    Some((number, url))
}

fn parse_pr_list_output(stdout: &str) -> Result<Option<PrInfo>, CopilotDispatchError> {
    let value: Value =
        serde_json::from_str(stdout.trim()).map_err(|error| CopilotDispatchError::Parse {
            operation: "pr list".to_owned(),
            message: format!("json decode failed: {error}"),
        })?;
    let array = value
        .as_array()
        .ok_or_else(|| CopilotDispatchError::Parse {
            operation: "pr list".to_owned(),
            message: "expected top-level json array".to_owned(),
        })?;
    let Some(first) = array.first() else {
        return Ok(None);
    };
    let number =
        first
            .get("number")
            .and_then(Value::as_u64)
            .ok_or_else(|| CopilotDispatchError::Parse {
                operation: "pr list".to_owned(),
                message: "missing `number`".to_owned(),
            })?;
    let url = first
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| CopilotDispatchError::Parse {
            operation: "pr list".to_owned(),
            message: "missing `url`".to_owned(),
        })?
        .to_owned();
    let head_sha = first
        .get("headRefOid")
        .and_then(Value::as_str)
        .ok_or_else(|| CopilotDispatchError::Parse {
            operation: "pr list".to_owned(),
            message: "missing `headRefOid`".to_owned(),
        })?
        .to_owned();
    Ok(Some(PrInfo {
        number,
        url,
        head_sha,
    }))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    /// In-memory `CopilotDispatchClient` used by dispatcher tests.
    pub(crate) struct FakeGhClient {
        inner: Arc<Mutex<FakeGhClientInner>>,
    }

    #[derive(Default)]
    pub(crate) struct FakeGhClientInner {
        pub create_calls: Vec<(String, String, String)>,
        pub poll_responses:
            std::collections::HashMap<String, std::collections::VecDeque<Option<PrInfo>>>,
        pub next_issue_number: u64,
        pub poll_call_count: usize,
    }

    impl FakeGhClient {
        pub(crate) fn new() -> Self {
            Self {
                inner: Arc::new(Mutex::new(FakeGhClientInner {
                    next_issue_number: 100,
                    ..Default::default()
                })),
            }
        }

        pub(crate) fn handle(&self) -> Arc<Mutex<FakeGhClientInner>> {
            Arc::clone(&self.inner)
        }

        pub(crate) async fn queue_poll_response(&self, branch: &str, response: Option<PrInfo>) {
            let mut inner = self.inner.lock().await;
            inner
                .poll_responses
                .entry(branch.to_owned())
                .or_default()
                .push_back(response);
        }
    }

    #[async_trait]
    impl CopilotDispatchClient for FakeGhClient {
        async fn create_task(
            &self,
            branch: &str,
            title: &str,
            body: &str,
        ) -> Result<TaskId, CopilotDispatchError> {
            let mut inner = self.inner.lock().await;
            inner
                .create_calls
                .push((branch.to_owned(), title.to_owned(), body.to_owned()));
            let issue_number = inner.next_issue_number;
            inner.next_issue_number += 1;
            Ok(TaskId {
                issue_number,
                issue_url: Some(format!("https://example.test/issues/{issue_number}")),
            })
        }

        async fn poll_pr(&self, branch: &str) -> Result<Option<PrInfo>, CopilotDispatchError> {
            let mut inner = self.inner.lock().await;
            inner.poll_call_count += 1;
            let response = inner
                .poll_responses
                .get_mut(branch)
                .and_then(std::collections::VecDeque::pop_front);
            Ok(response.unwrap_or(None))
        }
    }

    #[test]
    fn parse_issue_create_extracts_number_and_url() {
        let stdout = "Creating issue in owner/repo\nhttps://github.com/owner/repo/issues/42";
        let (number, url) = parse_issue_create_output(stdout).expect("parsed");
        assert_eq!(number, 42);
        assert_eq!(
            url.as_deref(),
            Some("https://github.com/owner/repo/issues/42")
        );
    }

    #[test]
    fn parse_issue_create_handles_no_url() {
        let stdout = "issue-77";
        let result = parse_issue_create_output(stdout);
        assert!(
            result.is_none(),
            "issue-77 is not parseable as a trailing int"
        );
    }

    #[test]
    fn parse_pr_list_empty_returns_none() {
        let pr = parse_pr_list_output("[]").expect("parse ok");
        assert!(pr.is_none());
    }

    #[test]
    fn parse_pr_list_returns_first_pr() {
        let stdout = r#"[{"number":7,"url":"https://example/pr/7","headRefOid":"abc123"}]"#;
        let pr = parse_pr_list_output(stdout)
            .expect("parse ok")
            .expect("first present");
        assert_eq!(pr.number, 7);
        assert_eq!(pr.url, "https://example/pr/7");
        assert_eq!(pr.head_sha, "abc123");
    }

    #[test]
    fn parse_pr_list_invalid_json_errors() {
        let err = parse_pr_list_output("not json").expect_err("should error");
        match err {
            CopilotDispatchError::Parse { operation, .. } => assert_eq!(operation, "pr list"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn fake_client_round_trip() {
        let fake = FakeGhClient::new();
        let pr = PrInfo {
            number: 5,
            url: "https://x/5".to_owned(),
            head_sha: "sha".to_owned(),
        };
        fake.queue_poll_response("branch-x", None).await;
        fake.queue_poll_response("branch-x", Some(pr.clone())).await;

        let task = fake
            .create_task("branch-x", "title", "body")
            .await
            .expect("create");
        assert_eq!(task.issue_number, 100);

        assert!(fake.poll_pr("branch-x").await.expect("first").is_none());
        let found = fake.poll_pr("branch-x").await.expect("second");
        assert_eq!(found, Some(pr));
    }
}
