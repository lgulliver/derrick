//! Post-spec clarify step (DESIGN.md §5.3).
//!
//! The reusable clarify core — question parsing, answer selection, markdown
//! rendering, and the interactive/non-interactive loop — lives in
//! `derrick-specify` so the native spec provider can clarify the raw request
//! *before* a spec exists. This module re-exports those pure helpers and
//! delegates [`execute_clarify`] (the post-spec step) to
//! [`derrick_specify::clarify::run_clarify_loop`], so the step's on-disk
//! behaviour (`clarify.md` shape, recommendation acceptance, host call) is
//! unchanged.

use std::path::Path;
use std::sync::Arc;

use derrick_tools::{HostRegistry, HostRequest};

use derrick_assay::io::{relative_to_root, write_log};
use derrick_assay::types::{RunError, StepExecution};

// Re-export the shared clarify core so existing call sites
// (`clarify::parse_clarify_questions`, etc.) keep compiling unchanged. The
// `mod clarify` declaration is private, so a plain `pub use` of symbols only the
// test module references would warn; `#[allow(unused_imports)]` keeps the full
// re-export surface available to tests without churn.
#[allow(unused_imports)]
pub use derrick_specify::clarify::{
    ClarifyQuestion, parse_clarify_questions, render_clarify_markdown, run_clarify_loop,
    select_clarify_answer,
};

fn build_clarify_prompt(spec_rel: &Path) -> String {
    format!(
        "You are helping refine a specification. Read and analyze the specification at \
         `{}` from the working directory. Generate clarifying questions to ensure the \
         requirements are well-understood. Focus on ambiguous areas, trade-offs, and critical \
         decisions that need human input.\n\n\
         For each question, provide:\n\
         - The question\n\
         - Multiple choice options (at least 2)\n\
         - Your recommendation\n\n\
         Format each question as:\n\
         Q: <question>\n\
         Options: <option1>, <option2>, ...\n\
         Recommendation: <recommended option>",
        spec_rel.display()
    )
}

pub async fn execute_clarify(
    hosts: Arc<HostRegistry>,
    repo_root: &std::path::Path,
    working_dir: &Path,
    feature_dir: &Path,
    _state_prompt: &str,
    _run_id: &str,
    log_path: &Path,
) -> Result<StepExecution, RunError> {
    let spec_path = working_dir.join(feature_dir).join("spec.md");
    let spec = std::fs::read_to_string(&spec_path).map_err(|source| RunError::Io {
        path: spec_path,
        source,
    })?;

    let spec_lines = spec.lines().count();
    tracing::info!(
        target: "derrick_flow::clarify",
        lines = spec_lines,
        "specification loaded for review"
    );

    let spec_rel = feature_dir.join("spec.md");
    let prompt = build_clarify_prompt(&spec_rel);

    let host = hosts
        .get("claude")
        .ok_or_else(|| RunError::Config("clarify requires the claude host adapter".to_owned()))?;
    let mut request = HostRequest::new(prompt, working_dir);
    request.headless = true;
    let response = host
        .run(request)
        .await
        .map_err(|source| RunError::StepFailed {
            id: "clarify".to_owned(),
            message: source.to_string(),
        })?;

    let tokens_in = response.tokens_in;
    let tokens_out = response.tokens_out;
    write_log(log_path, &response.stdout, &response.stderr)?;

    let questions = parse_clarify_questions(&response.stdout);
    if questions.is_empty() {
        tracing::info!(
            target: "derrick_flow::clarify",
            "no clarifying questions needed; proceeding"
        );
        return Ok(StepExecution::success(Vec::new()).with_tokens(tokens_in, tokens_out));
    }

    // Interactive: read each answer from stdin (Enter accepts the
    // recommendation). The loop core is shared with the native provider; this
    // CLI-facing caller injects the real stdin/stderr so the library crate
    // stays free of direct stream access.
    let answers = run_clarify_loop(
        &questions,
        std::io::stdin().lock(),
        std::io::stderr().lock(),
    )
    .map_err(|source| RunError::Io {
        path: std::path::PathBuf::from("<stdin>"),
        source,
    })?;

    let clarify_path = working_dir.join(feature_dir).join("clarify.md");
    let content = render_clarify_markdown(&questions, &answers);
    std::fs::write(&clarify_path, &content).map_err(|source| RunError::Io {
        path: clarify_path.clone(),
        source,
    })?;

    tracing::info!(
        target: "derrick_flow::clarify",
        "clarification complete; answers saved"
    );
    Ok(
        StepExecution::success(vec![relative_to_root(repo_root, clarify_path)?])
            .with_tokens(tokens_in, tokens_out),
    )
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    #[test]
    fn clarify_prompt_references_spec_path() {
        let prompt = super::build_clarify_prompt(Path::new("specs/001-test/spec.md"));
        assert!(prompt.contains("specs/001-test/spec.md"));
        assert!(!prompt.contains("Specification:\n"));
    }
}
