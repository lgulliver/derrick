use std::path::Path;

use derrick_config::Config;
use derrick_models::AuthStore;

use derrick_assay::types::RunError;

/// Outcome of a single adversarial code review pass.
#[derive(Debug)]
pub struct CodeReviewOutcome {
    /// `"pass"` or `"fail"`.
    pub verdict: String,
    /// Full review text produced by the reviewer model.
    pub review_text: String,
    /// Input tokens consumed by the review call.
    pub tokens_in: u32,
    /// Output tokens produced by the review call.
    pub tokens_out: u32,
}

/// Run one adversarial code review pass.
///
/// Called by `derrick ticket code-review`. Uses the configured reviewer role
/// to assess the diff against the ticket requirements. Verdict is extracted
/// from a `## Verdict` section; if none is found the whole response is
/// returned as-is and treated as a failure.
pub async fn run_code_review(
    diff: &str,
    ticket_title: &str,
    ticket_body: &str,
    role: &str,
    config: &Config,
    _repo_root: &Path,
) -> Result<CodeReviewOutcome, RunError> {
    let prompt = format!(
        "You are an adversarial code reviewer. Review the following diff.\n\n\
         ## Ticket\n\n**{ticket_title}**\n\n{ticket_body}\n\n\
         ## Diff\n\n```diff\n{diff}\n```\n\n\
         Review for: security vulnerabilities, logic errors, missing edge cases, \
         inadequate test coverage, constitution violations, and style issues.\n\
         Be direct and specific — cite line numbers where relevant.\n\
         End your review with `## Verdict` followed by exactly one word on its own \
         line: `pass` or `fail`."
    );

    let model = derrick_models::resolve_role(
        role,
        config.roles(),
        config.models(),
        &AuthStore::from_env(),
    )
    .await?;
    let response = model
        .complete(completion_request(prompt, None, None))
        .await?;

    let verdict = extract_verdict_from_review(&response.text);

    Ok(CodeReviewOutcome {
        verdict,
        review_text: response.text,
        tokens_in: response.tokens_in,
        tokens_out: response.tokens_out,
    })
}

pub fn extract_verdict_from_review(text: &str) -> String {
    let mut after_verdict = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.eq_ignore_ascii_case("## verdict") {
            after_verdict = true;
            continue;
        }
        if after_verdict && !trimmed.is_empty() {
            let word = trimmed
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_lowercase();
            if word == "pass" || word == "fail" {
                return word;
            }
            return "fail".to_owned();
        }
    }
    "fail".to_owned()
}

fn completion_request(
    prompt: String,
    cached_prefix: Option<String>,
    system: Option<String>,
) -> derrick_models::CompletionRequest {
    use std::time::Duration;
    derrick_models::CompletionRequest {
        cached_prefix,
        prompt,
        system,
        max_tokens: Some(4096),
        temperature: Some(0.2),
        timeout: Duration::from_secs(600),
    }
}
