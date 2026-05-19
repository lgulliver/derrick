use std::io::Write as _;
use std::path::{Path, PathBuf};

use derrick_models::AuthStore;

use crate::io::{relative_to_root, write_log};
use crate::types::{RunError, StepExecution};

pub struct ClarifyQuestion {
    pub question: String,
    pub options: Vec<String>,
    pub recommendation: Option<String>,
}

pub fn parse_clarify_questions(text: &str) -> Vec<ClarifyQuestion> {
    let mut questions: Vec<ClarifyQuestion> = Vec::new();
    let mut question: Option<String> = None;
    let mut options: Vec<String> = Vec::new();
    let mut recommendation: Option<String> = None;
    for line in text.lines() {
        let t = line.trim();
        if let Some(stripped) = t.strip_prefix("Q:") {
            if let Some(q) = question.take() {
                questions.push(ClarifyQuestion {
                    question: q,
                    options: std::mem::take(&mut options),
                    recommendation: recommendation.take(),
                });
            }
            question = Some(stripped.trim().to_owned());
        } else if let Some(stripped) = t.strip_prefix("Options:") {
            options = stripped
                .split(',')
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect();
        } else if let Some(stripped) = t.strip_prefix("Recommendation:") {
            recommendation = Some(stripped.trim().to_owned());
        }
    }
    if let Some(q) = question {
        questions.push(ClarifyQuestion {
            question: q,
            options,
            recommendation,
        });
    }
    questions
}

pub(crate) fn select_clarify_answer(question: &ClarifyQuestion, user_input: &str) -> String {
    if user_input.is_empty() {
        return question.recommendation.clone().unwrap_or_default();
    }
    if let Ok(index) = user_input.parse::<usize>() {
        if let Some(selected) = question.options.get(index.saturating_sub(1)) {
            return selected.clone();
        }
    }
    user_input.to_owned()
}

pub(crate) fn render_clarify_markdown(questions: &[ClarifyQuestion], answers: &[String]) -> String {
    let mut content = String::from("# Clarification Q&A\n\n");
    for (q, a) in questions.iter().zip(answers.iter()) {
        content.push_str("## Question\n");
        content.push_str(&q.question);
        content.push_str("\n\nOptions: ");
        content.push_str(&q.options.join(", "));
        content.push_str("\n\nRecommendation: ");
        content.push_str(q.recommendation.as_deref().unwrap_or("none"));
        content.push_str("\n\nAnswer: ");
        content.push_str(a);
        content.push_str("\n\n");
    }
    content
}

pub async fn execute_clarify(
    config: &derrick_config::Config,
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

    eprintln!("\n--- Specification (review before clarification) ---\n{spec}---\n");

    let prompt = format!(
        "You are helping refine a specification. Based on the specification below, generate \
         clarifying questions to ensure the requirements are well-understood. Focus on ambiguous \
         areas, trade-offs, and critical decisions that need human input.\n\n\
         For each question, provide:\n\
         - The question\n\
         - Multiple choice options (at least 2)\n\
         - Your recommendation\n\n\
         Format each question as:\n\
         Q: <question>\n\
         Options: <option1>, <option2>, ...\n\
         Recommendation: <recommended option>\n\n\
         Specification:\n{spec}"
    );

    let model = derrick_models::resolve_role(
        "drafter",
        config.roles(),
        config.models(),
        &AuthStore::from_env(),
    )
    .await?;

    let response = model
        .complete(completion_request(prompt, None, None))
        .await?;

    write_log(log_path, &response.text, "")?;

    let questions = parse_clarify_questions(&response.text);
    if questions.is_empty() {
        eprintln!("No clarifying questions needed. Proceeding.");
        return Ok(StepExecution::success(Vec::new()));
    }

    let mut answers: Vec<String> = Vec::new();
    for (i, q) in questions.iter().enumerate() {
        eprintln!("\n--- Question {} of {} ---", i + 1, questions.len());
        eprintln!("{}", q.question);
        if !q.options.is_empty() {
            for (j, opt) in q.options.iter().enumerate() {
                let is_rec = q.recommendation.as_deref() == Some(opt.as_str());
                if is_rec {
                    eprintln!("  {}. {} [recommended]", j + 1, opt);
                } else {
                    eprintln!("  {}. {}", j + 1, opt);
                }
            }
        }
        eprint!("Your answer (or press Enter to accept recommendation): ");
        std::io::stdout().flush().map_err(|source| RunError::Io {
            path: PathBuf::from("<stdout>"),
            source,
        })?;
        let mut answer = String::new();
        std::io::stdin()
            .read_line(&mut answer)
            .map_err(|source| RunError::Io {
                path: PathBuf::from("<stdin>"),
                source,
            })?;
        let trimmed = answer.trim().to_owned();
        answers.push(select_clarify_answer(q, &trimmed));
    }

    let clarify_path = working_dir.join(feature_dir).join("clarify.md");
    let content = render_clarify_markdown(&questions, &answers);
    std::fs::write(&clarify_path, &content).map_err(|source| RunError::Io {
        path: clarify_path.clone(),
        source,
    })?;

    eprintln!("\nClarification complete. Answers saved.");
    Ok(
        StepExecution::success(vec![relative_to_root(repo_root, clarify_path)?])
            .with_tokens(response.tokens_in, response.tokens_out),
    )
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
