//! Generalised clarify loop (shared core).
//!
//! The reusable clarify primitives originally lived in
//! `crates/derrick-flow/src/clarify.rs`. They are hoisted here so the native
//! spec provider can clarify the *raw feature request* before any spec exists,
//! while `derrick-flow::clarify` re-exports the pure helpers and delegates its
//! post-spec clarify step to [`run_clarify_loop`] — keeping that step's
//! behaviour and tests byte-for-byte unchanged.
//!
//! The pure helpers ([`parse_clarify_questions`], [`select_clarify_answer`],
//! [`render_clarify_markdown`]) carry no I/O. [`run_clarify_loop`] drives the
//! interactive (stdin) and non-interactive (auto-accept the recommendation)
//! paths and renders `clarify.md`.

use std::io::Write as _;

/// One clarifying question with its options and recommended answer.
pub struct ClarifyQuestion {
    /// The question text.
    pub question: String,
    /// Multiple-choice options (may be empty for a free-form question).
    pub options: Vec<String>,
    /// The model's recommended answer, if any.
    pub recommendation: Option<String>,
}

/// Parses a model's clarify output into structured questions.
///
/// Recognises `Q:` / `Options:` / `Recommendation:` line prefixes; everything
/// else is ignored. Identical parsing to the original flow implementation.
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
            options = split_options_smart(stripped)
                .into_iter()
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

/// Split options text on commas, but only commas at the top level
/// (not inside matching parentheses, brackets, or backtick pairs).
fn split_options_smart(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut depth = 0i32;
    for (i, ch) in text.char_indices() {
        match ch {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            '`' => {
                // Toggle backtick depth
                if depth == 0 {
                    depth = -1
                } else if depth == -1 {
                    depth = 0
                }
            }
            ',' if depth == 0 => {
                parts.push(text[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    if start < text.len() {
        parts.push(text[start..].trim());
    }
    parts
}

/// Resolves a user's raw input for a question into the chosen answer.
///
/// Empty input accepts the recommendation; a 1-based numeric input selects the
/// matching option; anything else is taken verbatim.
pub fn select_clarify_answer(question: &ClarifyQuestion, user_input: &str) -> String {
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

/// Renders the clarify Q&A markdown written to `clarify.md`.
pub fn render_clarify_markdown(questions: &[ClarifyQuestion], answers: &[String]) -> String {
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

/// Builds the prompt that asks a model to clarify the *raw feature request*
/// (before any spec exists), grounded in the survey pre-pass.
///
/// This is the clarify-first variant the native provider uses; the post-spec
/// variant lives in `derrick-flow::clarify` and is unchanged.
pub fn build_raw_prompt_questions(raw_prompt: &str, grounding_block: &str) -> String {
    format!(
        "You are helping clarify a feature request BEFORE any specification is written. \
         The raw request is:\n\n{raw_prompt}\n\n\
         Relevant grounding from the codebase index:\n{grounding_block}\n\n\
         Generate clarifying questions that resolve ambiguity, trade-offs, and critical \
         decisions needed before drafting a specification. Do not propose implementation; \
         surface decisions.\n\n\
         For each question, provide:\n\
         - The question\n\
         - Multiple choice options (at least 2)\n\
         - Your recommendation\n\n\
         Format each question as:\n\
         Q: <question>\n\
         Options: <option1>, <option2>, ...\n\
         Recommendation: <recommended option>"
    )
}

/// Drives the clarify loop over `questions`, returning the chosen answers.
///
/// When `interactive` is true, each question is presented on stderr and the
/// answer read from stdin (empty input accepts the recommendation). When false
/// (CI / headless), every question auto-accepts its [`recommendation`], which is
/// the same outcome a developer pressing Enter would get.
///
/// [`recommendation`]: ClarifyQuestion::recommendation
pub fn run_clarify_loop(
    questions: &[ClarifyQuestion],
    interactive: bool,
) -> std::io::Result<Vec<String>> {
    let mut answers: Vec<String> = Vec::with_capacity(questions.len());
    for (i, q) in questions.iter().enumerate() {
        if !interactive {
            answers.push(select_clarify_answer(q, ""));
            continue;
        }
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
        std::io::stderr().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        let trimmed = answer.trim().to_owned();
        answers.push(select_clarify_answer(q, &trimmed));
    }
    Ok(answers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_options_respects_parens() {
        let input = "`0.0.0-nightly.20260520` (pre-release, sorts below stable), `0.0.0+nightly.20260520` (build metadata, unordered)";
        let result = split_options_smart(input);
        assert_eq!(result.len(), 2);
        assert!(result[0].contains("nightly"));
        assert!(result[1].contains("build metadata"));
    }

    #[test]
    fn split_options_simple() {
        let input = "REST, GraphQL, gRPC";
        let result = split_options_smart(input);
        assert_eq!(result, vec!["REST", "GraphQL", "gRPC"]);
    }

    #[test]
    fn parses_question_blocks() {
        let text = "Q: Which format?\nOptions: JSON, YAML\nRecommendation: JSON\n";
        let questions = parse_clarify_questions(text);
        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0].options, vec!["JSON", "YAML"]);
        assert_eq!(questions[0].recommendation.as_deref(), Some("JSON"));
    }

    #[test]
    fn non_interactive_auto_accepts_recommendation() {
        let questions = parse_clarify_questions(
            "Q: Which format?\nOptions: JSON, YAML\nRecommendation: JSON\n",
        );
        let answers = run_clarify_loop(&questions, false).expect("loop");
        assert_eq!(answers, vec!["JSON".to_owned()]);
    }

    #[test]
    fn raw_prompt_question_builder_embeds_prompt_and_grounding() {
        let prompt = build_raw_prompt_questions("add an export command", "src/lib.rs:1 export");
        assert!(prompt.contains("add an export command"));
        assert!(prompt.contains("src/lib.rs:1 export"));
        assert!(prompt.contains("BEFORE any specification"));
    }
}
