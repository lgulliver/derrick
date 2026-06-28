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
//! [`render_clarify_markdown`]) carry no I/O, as does
//! [`auto_accept_recommendations`] (the path the native provider uses). The
//! interactive loop [`run_clarify_loop`] takes an **injected reader + writer** so
//! this library crate never touches `stdin`/`stderr` directly — the CLI-facing
//! caller in `derrick-flow` supplies the real streams.

use std::io::{BufRead, Write};

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
///
/// Bracket nesting (`bracket_depth`) and backtick state (`in_backticks`) are
/// tracked separately so a backticked option such as `` `foo(bar, baz)` `` does
/// not split on the inner comma. Bracket depth is only adjusted outside
/// backticks; a comma separates only when `!in_backticks && bracket_depth == 0`.
fn split_options_smart(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut bracket_depth = 0i32;
    let mut in_backticks = false;
    for (i, ch) in text.char_indices() {
        match ch {
            '`' => in_backticks = !in_backticks,
            '(' | '[' | '{' if !in_backticks => bracket_depth += 1,
            ')' | ']' | '}' if !in_backticks => bracket_depth -= 1,
            ',' if !in_backticks && bracket_depth == 0 => {
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
    // Options are presented 1-based; only a positive index selects an option.
    // Input `0` (and any non-positive value) is treated as free-form text.
    if let Ok(index) = user_input.parse::<usize>() {
        if index > 0 {
            if let Some(selected) = question.options.get(index - 1) {
                return selected.clone();
            }
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

/// Auto-accepts every question's recommendation, returning the chosen answers.
///
/// This is the non-interactive (CI / headless) path the native provider uses:
/// it is pure (no I/O) and yields the same answer a developer pressing Enter
/// would get. Equivalent to `select_clarify_answer(q, "")` for each question.
pub fn auto_accept_recommendations(questions: &[ClarifyQuestion]) -> Vec<String> {
    questions
        .iter()
        .map(|q| select_clarify_answer(q, ""))
        .collect()
}

/// Drives the interactive clarify loop over `questions` using **injected**
/// streams, returning the chosen answers.
///
/// Each question is presented on `writer` and the answer read from `reader`
/// (empty input accepts the recommendation). Taking the reader/writer as
/// parameters keeps this library crate free of any direct `stdin`/`stderr`
/// access — the CLI-facing caller (`derrick-flow`'s clarify step) passes the
/// real `std::io::stdin().lock()` and `std::io::stderr()`; tests pass in-memory
/// buffers.
pub fn run_clarify_loop<R: BufRead, W: Write>(
    questions: &[ClarifyQuestion],
    mut reader: R,
    mut writer: W,
) -> std::io::Result<Vec<String>> {
    let mut answers: Vec<String> = Vec::with_capacity(questions.len());
    for (i, q) in questions.iter().enumerate() {
        writeln!(
            writer,
            "\n--- Question {} of {} ---",
            i + 1,
            questions.len()
        )?;
        writeln!(writer, "{}", q.question)?;
        if !q.options.is_empty() {
            for (j, opt) in q.options.iter().enumerate() {
                let is_rec = q.recommendation.as_deref() == Some(opt.as_str());
                if is_rec {
                    writeln!(writer, "  {}. {} [recommended]", j + 1, opt)?;
                } else {
                    writeln!(writer, "  {}. {}", j + 1, opt)?;
                }
            }
        }
        write!(
            writer,
            "Your answer (or press Enter to accept recommendation): "
        )?;
        writer.flush()?;
        let mut answer = String::new();
        // `read_line` returns Ok(0) at end-of-stream. A closed/exhausted reader
        // must NOT be treated as an empty line (which would silently auto-accept
        // the recommendation) — abort instead. A real empty line ("\n") reads as
        // Ok(1) and is still accepted as "press Enter to take the recommendation".
        if reader.read_line(&mut answer)? == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "input closed while reading clarify answer",
            ));
        }
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
        let answers = auto_accept_recommendations(&questions);
        assert_eq!(answers, vec!["JSON".to_owned()]);
    }

    #[test]
    fn interactive_loop_reads_from_injected_reader() {
        let questions = parse_clarify_questions(
            "Q: Which format?\nOptions: JSON, YAML\nRecommendation: JSON\n",
        );
        // First answer selects option 2 (YAML); injected streams, no real I/O.
        let reader = std::io::Cursor::new(b"2\n".to_vec());
        let mut writer: Vec<u8> = Vec::new();
        let answers = run_clarify_loop(&questions, reader, &mut writer).expect("loop");
        assert_eq!(answers, vec!["YAML".to_owned()]);
        // The prompt was written to the injected writer, not a real stream.
        assert!(String::from_utf8_lossy(&writer).contains("Which format?"));
    }

    #[test]
    fn interactive_loop_empty_line_accepts_recommendation() {
        let questions = parse_clarify_questions(
            "Q: Which format?\nOptions: JSON, YAML\nRecommendation: JSON\n",
        );
        // A real empty line (user pressed Enter) is Ok(1), not EOF, and accepts
        // the recommendation.
        let reader = std::io::Cursor::new(b"\n".to_vec());
        let mut writer: Vec<u8> = Vec::new();
        let answers = run_clarify_loop(&questions, reader, &mut writer).expect("loop");
        assert_eq!(answers, vec!["JSON".to_owned()]);
    }

    #[test]
    fn interactive_loop_eof_errors_instead_of_auto_accepting() {
        let questions = parse_clarify_questions(
            "Q: Which format?\nOptions: JSON, YAML\nRecommendation: JSON\n",
        );
        // An exhausted/closed reader (no input at all) must error, not silently
        // choose the recommendation.
        let reader = std::io::Cursor::new(Vec::new());
        let mut writer: Vec<u8> = Vec::new();
        let err =
            run_clarify_loop(&questions, reader, &mut writer).expect_err("EOF must abort the loop");
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn interactive_loop_one_line_per_question() {
        let questions = parse_clarify_questions(
            "Q: First?\nOptions: A, B\nRecommendation: A\n\
             Q: Second?\nOptions: C, D\nRecommendation: C\n",
        );
        // One input line per question; the second question's blank line accepts
        // its recommendation.
        let reader = std::io::Cursor::new(b"2\n\n".to_vec());
        let mut writer: Vec<u8> = Vec::new();
        let answers = run_clarify_loop(&questions, reader, &mut writer).expect("loop");
        assert_eq!(answers, vec!["B".to_owned(), "C".to_owned()]);
    }

    #[test]
    fn select_answer_zero_index_is_freeform_not_option_one() {
        let questions = parse_clarify_questions(
            "Q: Which format?\nOptions: JSON, YAML\nRecommendation: JSON\n",
        );
        // `0` must NOT map to option 1; it is taken as free-form text.
        assert_eq!(select_clarify_answer(&questions[0], "0"), "0");
    }

    #[test]
    fn split_options_keeps_backticked_commas_together() {
        let result = split_options_smart("`foo(bar, baz)`, plain");
        assert_eq!(result.len(), 2);
        assert!(result[0].contains("bar, baz"));
        assert_eq!(result[1], "plain");
    }

    #[test]
    fn raw_prompt_question_builder_embeds_prompt_and_grounding() {
        let prompt = build_raw_prompt_questions("add an export command", "src/lib.rs:1 export");
        assert!(prompt.contains("add an export command"));
        assert!(prompt.contains("src/lib.rs:1 export"));
        assert!(prompt.contains("BEFORE any specification"));
    }
}
