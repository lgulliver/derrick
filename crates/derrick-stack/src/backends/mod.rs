//! Backend implementations for [`crate::StackBackend`].

mod cli;

pub mod git_spice;
pub mod graphite;
pub mod native;
pub mod none;

use std::path::Path;
use std::process::Stdio;

use tokio::process::Command;
use tracing::debug;

use crate::StackError;

/// Check out `branch` using plain `git` before handing the working tree to an
/// external stacking CLI (`gt`/`gs`), which operate on the checked-out branch.
///
/// `backend` and the CLI's binary name are used only to make the error message
/// actionable; the checkout itself is always plain `git`.
pub(crate) async fn checkout_branch(
    _cli_binary: &str,
    backend: &'static str,
    branch: &str,
    repo_root: &Path,
) -> Result<(), StackError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .arg("checkout")
        .arg(branch)
        .stdin(Stdio::null())
        .output()
        .await?;
    if !output.status.success() {
        return Err(StackError::Git {
            message: format!(
                "git checkout {branch} failed before {backend} operation: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        });
    }
    debug!(branch, backend, "checked out branch for stacking cli");
    Ok(())
}

/// Resolve the tip SHA of `branch` via `git rev-parse`.
pub(crate) async fn git_rev_parse(repo_root: &Path, branch: &str) -> Result<String, StackError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .arg("rev-parse")
        .arg(branch)
        .stdin(Stdio::null())
        .output()
        .await?;
    if !output.status.success() {
        return Err(StackError::Git {
            message: format!(
                "git rev-parse {branch} failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Scan `text` for the first GitHub PR URL and parse its trailing number.
///
/// Used to recover the PR identity from a stacking CLI's human-readable output
/// (`gt submit` / `gs branch submit` both print the PR URL on success).
pub(crate) fn find_pr_url_and_number(text: &str) -> Option<(String, u64)> {
    for token in text.split_whitespace() {
        let candidate = token.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '/');
        if let Some(idx) = candidate.find("/pull/") {
            let number_part = &candidate[idx + "/pull/".len()..];
            let digits: String = number_part
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if let Ok(number) = digits.parse::<u64>() {
                let url_end = idx + "/pull/".len() + digits.len();
                return Some((candidate[..url_end].to_owned(), number));
            }
        }
    }
    None
}

/// Fall back to `gh pr view <branch>` to recover the PR URL and number when the
/// stacking CLI's output did not contain a parseable URL.
pub(crate) async fn lookup_pr_via_gh(
    branch: &str,
    repo_root: &Path,
) -> Result<(String, u64), StackError> {
    let output = Command::new("gh")
        .arg("pr")
        .arg("view")
        .arg(branch)
        .arg("--json")
        .arg("number,url")
        .current_dir(repo_root)
        .stdin(Stdio::null())
        .output()
        .await?;
    if !output.status.success() {
        return Err(StackError::Gh {
            message: format!(
                "could not resolve PR for {branch} via gh pr view: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        });
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value =
        serde_json::from_str(stdout.trim()).map_err(|error| StackError::Gh {
            message: format!("could not parse gh pr view JSON for {branch}: {error}"),
        })?;
    let url = value
        .get("url")
        .and_then(|v| v.as_str())
        .ok_or_else(|| StackError::Gh {
            message: format!("gh pr view returned no url for {branch}"),
        })?
        .to_owned();
    let number = value
        .get("number")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| StackError::Gh {
            message: format!("gh pr view returned no number for {branch}"),
        })?;
    Ok((url, number))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_pr_url_and_number_extracts_from_gt_output() {
        let text = "Submitted! View it at https://github.com/foo/bar/pull/123";
        assert_eq!(
            find_pr_url_and_number(text),
            Some(("https://github.com/foo/bar/pull/123".to_owned(), 123)),
        );
    }

    #[test]
    fn find_pr_url_and_number_handles_trailing_punctuation() {
        let text = "Created PR (https://github.com/foo/bar/pull/7).";
        assert_eq!(
            find_pr_url_and_number(text),
            Some(("https://github.com/foo/bar/pull/7".to_owned(), 7)),
        );
    }

    #[test]
    fn find_pr_url_and_number_none_when_absent() {
        assert_eq!(find_pr_url_and_number("nothing to see here"), None);
    }
}
