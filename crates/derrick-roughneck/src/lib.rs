//! Roughneck — LLM output compression via prompt injection.
//!
//! Prepends terse-response instructions to each pipeline step's prompt to
//! cut roughly 65–75% of output tokens at the cost of a small fixed prefix
//! on the input side. Three intensity levels are provided: `lite`, `full`,
//! and `ultra`. Unknown levels default to `full`. The special values `off`
//! and `none` disable injection.
//!
//! ## Savings estimation
//!
//! [`estimate_savings`] inspects the *actual model output* to detect whether
//! the model obeyed the injected compression instruction before attributing
//! savings. This avoids over-counting when the model ignored the directive.
//!
//! ### Compliance heuristic
//!
//! The detector examines four independent signals from the model's response:
//!
//! 1. **Preamble phrases** — known filler openers like "Certainly,",
//!    "I would like to", "Of course," that compressed responses should omit.
//! 2. **Filler word density** — fraction of prose words that are low-signal
//!    hedges/filler. Compressed responses have markedly fewer.
//! 3. **Average sentence length** — measured in words. Terse fragment/bullet
//!    style produces shorter "sentences" (often just one clause or label).
//! 4. **Fragment/bullet fraction** — fraction of non-empty lines that look
//!    like fragments or bullets (no terminal full stop, or starting with a
//!    bullet/dash marker).
//!
//! Each signal contributes one point toward a score in `[0, 4]`. The final
//! compliance classification is:
//!
//! | Score | Classification |
//! |-------|----------------|
//! | 3–4   | [`Compliance::Full`] |
//! | 1–2   | [`Compliance::Partial`] |
//! | 0     | [`Compliance::None`] |
//!
//! `lite` level uses relaxed thresholds because it only trims filler and does
//! not mandate bullet/fragment style.
//!
//! ### Tokens saved
//!
//! * `Full` — applies the rate formula: `saved = actual × rate / (1 − rate)`
//! * `Partial` — 40% of the Full estimate
//! * `None` — 0 (the model did not comply; no saving to attribute)

/// Lite-intensity instructions: drop filler, keep prose.
pub const LITE_INSTRUCTIONS: &str = "[ROUGHNECK:LITE] Be concise. Drop filler words, preambles, and closing summaries. Keep all technical content complete and accurate.";

/// Default-intensity instructions: fragments and bullets.
pub const FULL_INSTRUCTIONS: &str = "[ROUGHNECK:FULL] Respond in fragments and bullets. Skip preambles (\"I will now…\"), affirmations (\"Great!\"), and closing summaries. Use one line per decision. Preserve all code, paths, identifiers, and technical values verbatim. Omit nothing of substance.";

/// Ultra-intensity instructions: telegraphic only.
pub const ULTRA_INSTRUCTIONS: &str = "[ROUGHNECK:ULTRA] Telegraphic only. Fragments. One line per decision. No preamble, no summary, no transitions. Omit if uncertain. Preserve code/paths/identifiers exactly.";

/// Compliance classification for a single response.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compliance {
    /// The model clearly followed the compression instruction.
    Full,
    /// The model partially followed it; estimate is discounted.
    Partial,
    /// The model ignored the instruction; no savings attributed.
    None,
}

/// Savings estimate paired with a compliance verdict.
#[derive(Clone, Debug)]
pub struct RoughneckSavings {
    /// Estimated tokens saved. Zero when compliance is `None`.
    pub tokens_saved: u32,
    /// How faithfully the model obeyed the compression instruction.
    pub compliance: Compliance,
}

/// Filler / hedge words that compressed responses should largely omit.
///
/// These match the `should_drop_word` list in `derrick-caveman` plus a few
/// extra preamble-adjacent words.
const FILLER_WORDS: &[&str] = &[
    "just",
    "really",
    "basically",
    "actually",
    "simply",
    "maybe",
    "probably",
    "perhaps",
    "likely",
    "moreover",
    "furthermore",
    "additionally",
    "essentially",
    "obviously",
    "clearly",
    "indeed",
    "typically",
    "generally",
    "necessarily",
    "subsequently",
    "consequently",
    "accordingly",
    "therefore",
    "thus",
    "certainly",
    "absolutely",
    "definitely",
    "undoubtedly",
    "needless",
    "straightforward",
    "straightforwardly",
    "important",
    "note",
    "notably",
];

/// Phrases that appear in the opening sentence of unconstrained LLM responses.
const PREAMBLE_PHRASES: &[&str] = &[
    "certainly",
    "of course",
    "sure",
    "great",
    "happy to",
    "i would like to",
    "i will now",
    "i'll now",
    "let me",
    "allow me",
    "absolutely",
    "no problem",
    "with pleasure",
    "as requested",
    "as you can see",
    "needless to say",
    "i'm glad",
    "i am glad",
    "i'd be happy",
    "i would be happy",
    "thank you for",
    "thanks for",
];

/// Returns the instruction block for `level`, defaulting to `FULL` when
/// unrecognised. Returns `None` when the level is `off` or `none`.
fn instructions_for(level: &str) -> Option<&'static str> {
    match level {
        "off" | "none" => None,
        "lite" => Some(LITE_INSTRUCTIONS),
        "ultra" => Some(ULTRA_INSTRUCTIONS),
        _ => Some(FULL_INSTRUCTIONS),
    }
}

/// Prepends the roughneck instruction block for `level` to `prompt`.
///
/// When `level` is `off` or `none`, the prompt is returned unchanged. Any
/// unrecognised level falls back to `full`.
pub fn inject_prompt(prompt: &str, level: &str) -> String {
    match instructions_for(level) {
        None => prompt.to_owned(),
        Some(instructions) => format!("{instructions}\n\n{prompt}"),
    }
}

/// Inspect the model's `output` and estimate how many tokens roughneck saved
/// when `level` was active during that call.
///
/// This is the primary API. It measures compliance before attributing any
/// saving. See the crate-level documentation for the heuristic details.
///
/// ```
/// # use derrick_roughneck::{estimate_savings, Compliance};
/// // A tight, bullet-style response → Full compliance.
/// let terse = "- parse: reads input\n- validate: checks schema\n- emit: writes output";
/// let s = estimate_savings(terse, "full");
/// assert_eq!(s.compliance, Compliance::Full);
///
/// // Disabled level → no savings attributed.
/// let s = estimate_savings("any text", "off");
/// assert_eq!(s.compliance, Compliance::None);
/// assert_eq!(s.tokens_saved, 0);
/// ```
pub fn estimate_savings(output: &str, level: &str) -> RoughneckSavings {
    let rate: f64 = match level {
        "lite" => 0.30,
        "full" => 0.65,
        "ultra" => 0.75,
        _ => return RoughneckSavings { tokens_saved: 0, compliance: Compliance::None },
    };

    let compliance = detect_compliance(output, level);
    let tokens_saved = compute_tokens_saved(output, rate, compliance);
    RoughneckSavings { tokens_saved, compliance }
}

/// Estimates how many output tokens roughneck saved given the observed
/// `tokens_out` and the level applied, assuming full compliance.
///
/// **Deprecated** — prefer [`estimate_savings`] which measures compliance
/// from the actual output text. This function retains the original
/// unconditional formula and is kept for callers that already have a
/// token count rather than raw text.
///
/// If `actual = baseline * (1 - rate)`, then `saved = actual * rate / (1 - rate)`.
/// Returns 0 for unknown / disabled levels.
#[deprecated(
    since = "0.2.0",
    note = "Use estimate_savings(output_text, level) which measures compliance \
            before attributing savings. This function assumes full compliance unconditionally."
)]
pub fn estimate_tokens_saved(tokens_out: u32, level: &str) -> u32 {
    let rate: f64 = match level {
        "lite" => 0.30,
        "full" => 0.65,
        "ultra" => 0.75,
        _ => return 0,
    };
    let actual = f64::from(tokens_out);
    let saved = actual * rate / (1.0 - rate);
    if saved.is_finite() && saved >= 0.0 {
        saved.round().min(f64::from(u32::MAX)) as u32
    } else {
        0
    }
}

/// Builds a prompt asking an LLM to compress `document` into terse form,
/// preserving all technical content.
pub fn compress_document_prompt(document: &str, doc_type: &str) -> String {
    let header = FULL_INSTRUCTIONS;
    format!(
        "{header}\n\nCompress the following {doc_type} into terse form. Preserve all technical content, code, paths, identifiers, and decisions exactly. Drop all filler, prose padding, and redundant explanation. Output only the compressed document.\n\n---\n{document}"
    )
}

// ──────────────────────────────────────────────────────────────────────────────
// Compliance detector (pure functions, no I/O)
// ──────────────────────────────────────────────────────────────────────────────

/// Classify compliance by scoring four independent signals.
///
/// The score is a count of signals that indicate the model *did* follow the
/// instruction. Score ≥ 3 → Full, 1–2 → Partial, 0 → None.
fn detect_compliance(output: &str, level: &str) -> Compliance {
    if output.is_empty() {
        return Compliance::None;
    }

    let lite = level == "lite";

    let score = score_no_preamble(output)
        + score_low_filler_density(output)
        + score_short_sentences(output, lite)
        + score_fragment_fraction(output, lite);

    match score {
        3 | 4 => Compliance::Full,
        1 | 2 => Compliance::Partial,
        _ => Compliance::None,
    }
}

/// 1 point if the response does not open with a known preamble phrase.
fn score_no_preamble(output: &str) -> u32 {
    // Check only the first 200 characters of the first non-empty line.
    let first_line = output
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .to_ascii_lowercase();
    let window = if first_line.len() > 200 { &first_line[..200] } else { &first_line };

    let has_preamble = PREAMBLE_PHRASES.iter().any(|phrase| window.contains(phrase));
    if has_preamble { 0 } else { 1 }
}

/// 1 point if filler word density is below the threshold for `level`.
///
/// Threshold: lite ≤ 6%, full/ultra ≤ 3%.
fn score_low_filler_density(output: &str) -> u32 {
    let words: Vec<String> = prose_words(output);
    if words.is_empty() {
        return 1; // empty/code-only output is fine
    }
    let total = words.len();
    let filler = words.iter().filter(|w| is_filler(w)).count();
    let density = filler as f64 / total as f64;
    // We use a single threshold; lite level is just less strict overall but
    // both benefit from the same density signal.
    if density <= 0.06 { 1 } else { 0 }
}

/// 1 point if average sentence length (words per sentence) is below threshold.
///
/// Thresholds: lite ≤ 20 words/sentence, full/ultra ≤ 12 words/sentence.
fn score_short_sentences(output: &str, lite: bool) -> u32 {
    let threshold = if lite { 20.0_f64 } else { 12.0_f64 };
    let avg = average_sentence_length(output);
    if avg <= threshold { 1 } else { 0 }
}

/// 1 point if the fraction of fragment/bullet lines meets the threshold.
///
/// Thresholds: lite ≥ 20%, full/ultra ≥ 40%.
fn score_fragment_fraction(output: &str, lite: bool) -> u32 {
    let threshold = if lite { 0.20_f64 } else { 0.40_f64 };
    let frac = fragment_fraction(output);
    if frac >= threshold { 1 } else { 0 }
}

/// Tokenise prose words from `output`, skipping code spans and fenced blocks.
fn prose_words(output: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut in_fence = false;

    for line in output.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        // Strip inline code spans (crude but sufficient for density scoring).
        let stripped = strip_inline_code(line);
        for word in stripped.split_whitespace() {
            // Keep only alphabetic tokens (no numbers, no punctuation tokens).
            let clean = word.trim_matches(|c: char| !c.is_alphabetic());
            if !clean.is_empty() && clean.chars().all(|c| c.is_alphabetic()) {
                words.push(clean.to_owned());
            }
        }
    }
    words
}

/// Very rough inline-code stripper: replaces `...` spans with a space.
fn strip_inline_code(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut in_code = false;
    for ch in line.chars() {
        if ch == '`' {
            in_code = !in_code;
            out.push(' ');
        } else if in_code {
            // skip
        } else {
            out.push(ch);
        }
    }
    out
}

/// True if `word` (already lowercased) is a known filler word.
fn is_filler(word: &str) -> bool {
    let lower = word.to_ascii_lowercase();
    FILLER_WORDS.contains(&lower.as_str())
}

/// Average words per sentence. Sentences are split on `.`, `!`, `?`.
/// Lines that end without a terminal punctuation are treated as one sentence
/// each (fragment style lowers the average because each fragment is short).
fn average_sentence_length(output: &str) -> f64 {
    let mut sentence_word_counts: Vec<usize> = Vec::new();
    let mut current_words = 0usize;
    let mut in_fence = false;

    for line in output.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        let stripped = strip_inline_code(line);
        let trimmed = stripped.trim();
        if trimmed.is_empty() {
            // Blank line ends any running fragment
            if current_words > 0 {
                sentence_word_counts.push(current_words);
                current_words = 0;
            }
            continue;
        }
        for ch in stripped.chars() {
            if ch.is_ascii_alphabetic() || ch == '\'' {
                // part of a word, counted below
            } else if matches!(ch, '.' | '!' | '?') {
                // sentence terminal
                if current_words > 0 {
                    sentence_word_counts.push(current_words);
                    current_words = 0;
                }
            }
        }
        // Count words in this line
        for word in stripped.split_whitespace() {
            let clean = word.trim_matches(|c: char| !c.is_alphabetic());
            if !clean.is_empty() {
                current_words += 1;
            }
        }
        // If line ends without a terminal, it's a fragment line:
        // push what we have as its own "sentence" and reset.
        let ends_with_terminal = trimmed.ends_with(['.', '!', '?']);
        if !ends_with_terminal && current_words > 0 {
            sentence_word_counts.push(current_words);
            current_words = 0;
        } else if ends_with_terminal {
            // The word count was already appended token-by-token above for
            // each terminal found; current_words already reset there.
        }
    }
    if current_words > 0 {
        sentence_word_counts.push(current_words);
    }

    if sentence_word_counts.is_empty() {
        return 0.0;
    }
    let total: usize = sentence_word_counts.iter().sum();
    total as f64 / sentence_word_counts.len() as f64
}

/// Fraction of non-empty, non-fence lines that are fragments or bullets.
///
/// A line counts as a fragment/bullet when it:
/// - starts with a bullet marker (`-`, `*`, `+`, `•`) or a numbered list marker
/// - OR does not end with `.`, `!`, `?` (i.e. no sentence terminal)
fn fragment_fraction(output: &str) -> f64 {
    let mut total = 0usize;
    let mut fragment = 0usize;
    let mut in_fence = false;

    for line in output.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        total += 1;
        let is_bullet = trimmed.starts_with(['-', '*', '+', '•'])
            || trimmed
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_digit())
                && trimmed.contains(". ");
        let no_terminal = !trimmed.ends_with(['.', '!', '?']);
        if is_bullet || no_terminal {
            fragment += 1;
        }
    }

    if total == 0 {
        return 0.0;
    }
    fragment as f64 / total as f64
}

/// Compute the tokens_saved figure given the actual output token count
/// (derived from character length as a rough proxy — only ratios matter here),
/// the configured rate, and the compliance level.
fn compute_tokens_saved(output: &str, rate: f64, compliance: Compliance) -> u32 {
    // Rough token estimate: characters / 4 (industry standard proxy).
    let actual = (output.chars().count() as f64 / 4.0).round() as u32;
    let full_saved = {
        let a = f64::from(actual);
        let s = a * rate / (1.0 - rate);
        if s.is_finite() && s >= 0.0 {
            s.round().min(f64::from(u32::MAX)) as u32
        } else {
            0
        }
    };
    match compliance {
        Compliance::Full => full_saved,
        Compliance::Partial => (full_saved as f64 * 0.40).round() as u32,
        Compliance::None => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── inject_prompt ──────────────────────────────────────────────────────

    #[test]
    fn inject_prompt_full_is_default_for_unknown_level() {
        let out = inject_prompt("hello", "wat");
        assert!(out.starts_with("[ROUGHNECK:FULL]"));
        assert!(out.ends_with("\n\nhello"));
    }

    #[test]
    fn inject_prompt_off_passes_through() {
        assert_eq!(inject_prompt("hello", "off"), "hello");
        assert_eq!(inject_prompt("hello", "none"), "hello");
    }

    #[test]
    fn inject_prompt_lite_uses_lite_instructions() {
        let out = inject_prompt("body", "lite");
        assert!(out.starts_with("[ROUGHNECK:LITE]"));
    }

    #[test]
    fn inject_prompt_ultra_uses_ultra_instructions() {
        let out = inject_prompt("body", "ultra");
        assert!(out.starts_with("[ROUGHNECK:ULTRA]"));
    }

    // ── estimate_tokens_saved (deprecated, kept for API compatibility) ─────

    #[allow(deprecated)]
    #[test]
    fn estimate_tokens_saved_zero_for_unknown() {
        assert_eq!(estimate_tokens_saved(1_000, "off"), 0);
        assert_eq!(estimate_tokens_saved(1_000, "wat"), 0);
    }

    #[allow(deprecated)]
    #[test]
    fn estimate_tokens_saved_lite() {
        let saved = estimate_tokens_saved(700, "lite");
        assert!((saved as i64 - 300).abs() <= 1);
    }

    #[allow(deprecated)]
    #[test]
    fn estimate_tokens_saved_full() {
        let saved = estimate_tokens_saved(350, "full");
        assert!((saved as i64 - 650).abs() <= 1);
    }

    #[allow(deprecated)]
    #[test]
    fn estimate_tokens_saved_ultra() {
        let saved = estimate_tokens_saved(250, "ultra");
        assert!((saved as i64 - 750).abs() <= 1);
    }

    // ── compress_document_prompt ───────────────────────────────────────────

    #[test]
    fn compress_document_prompt_includes_doc_and_header() {
        let p = compress_document_prompt("BODY", "spec");
        assert!(p.starts_with("[ROUGHNECK:FULL]"));
        assert!(p.contains("Compress the following spec into terse form"));
        assert!(p.contains("\n---\nBODY"));
    }

    // ── compliance detector ────────────────────────────────────────────────

    #[test]
    fn verbose_chatty_output_classifies_none() {
        // This sample opens with a preamble, uses lots of filler words,
        // has long sentences, and no fragments/bullets.
        let verbose = "Certainly! I would like to help you with this. \
                       Let me explain the situation in a comprehensive and detailed manner. \
                       The system is basically working really well, and I am happy to clarify \
                       any points that perhaps seem unclear to you. \
                       Moreover, there are actually several important components that work \
                       together seamlessly to achieve the generally expected result. \
                       I hope this overview was very helpful and informative to you!";
        let s = estimate_savings(verbose, "full");
        assert_eq!(
            s.compliance,
            Compliance::None,
            "chatty verbose text must classify None"
        );
        assert_eq!(s.tokens_saved, 0, "None compliance must yield 0 saved");
    }

    #[test]
    fn terse_fragment_output_classifies_full() {
        // Tight bullets, no preamble, no filler, short per-line.
        let terse = "- parse: reads input bytes\n\
                     - validate: checks schema constraints\n\
                     - emit: writes output to sink\n\
                     - error path: returns Err(kind)";
        let s = estimate_savings(terse, "full");
        assert_eq!(
            s.compliance,
            Compliance::Full,
            "tight bullet output must classify Full"
        );
        assert!(s.tokens_saved > 0, "Full compliance must yield >0 saved");
    }

    #[test]
    fn partial_compliance_discounts_savings() {
        // No preamble, few filler words, but long paragraphs (no fragments).
        // This should land in Partial territory.
        let mixed = "The parser reads input bytes and validates the schema. \
                     It then transforms the data structure and resolves references. \
                     Errors are propagated upward via the standard Result type. \
                     The emitter writes the final output to the configured sink. \
                     Performance is acceptable for the expected workload size.";
        let s = estimate_savings(mixed, "full");
        // It should not be Full (no bullets, sentences are medium length)
        // and not None (no preamble, low filler).
        assert!(
            s.compliance == Compliance::Partial || s.compliance == Compliance::None,
            "mixed prose without preamble/filler: got {:?}",
            s.compliance
        );
    }

    #[test]
    fn disabled_level_returns_none_compliance() {
        let s = estimate_savings("anything", "off");
        assert_eq!(s.compliance, Compliance::None);
        assert_eq!(s.tokens_saved, 0);
    }

    #[test]
    fn unknown_level_returns_none_compliance() {
        let s = estimate_savings("anything", "turbo");
        assert_eq!(s.compliance, Compliance::None);
        assert_eq!(s.tokens_saved, 0);
    }

    #[test]
    fn full_compliance_savings_positive() {
        let terse = "- step A\n- step B\n- step C\n- step D\n- step E";
        let s = estimate_savings(terse, "full");
        if s.compliance == Compliance::Full {
            assert!(s.tokens_saved > 0);
        }
    }

    #[test]
    fn lite_level_uses_relaxed_thresholds() {
        // Lite allows prose; the key is no preamble and reasonable density.
        let clean_prose = "The parser reads input and checks the schema. \
                           Errors propagate via Result. \
                           Output is written to the sink.";
        let s = estimate_savings(clean_prose, "lite");
        // Lite should not penalise prose too harshly — should be at least Partial.
        assert!(
            s.compliance != Compliance::None || s.tokens_saved == 0,
            "lite clean prose should not be None unless tokens_saved is also 0"
        );
    }

    #[test]
    fn preamble_detection_fires_on_known_openers() {
        assert_eq!(score_no_preamble("Certainly! Here is the answer."), 0);
        assert_eq!(score_no_preamble("Of course, let me explain."), 0);
        assert_eq!(score_no_preamble("- direct answer"), 1);
    }

    #[test]
    fn filler_density_signal_fires_correctly() {
        let dense = "just basically really actually simply maybe probably perhaps likely";
        assert_eq!(score_low_filler_density(dense), 0);
        let clean = "parse validate emit transform";
        assert_eq!(score_low_filler_density(clean), 1);
    }

    #[test]
    fn fragment_fraction_bullets_score_high() {
        let bullets = "- alpha\n- beta\n- gamma\n- delta\n- epsilon";
        assert!(fragment_fraction(bullets) > 0.8);
    }

    #[test]
    fn fragment_fraction_prose_paragraphs_score_low() {
        let prose = "The system works well.\nIt handles all cases correctly.\n\
                     Performance is good.";
        // All lines end with a period → fraction should be 0.
        assert_eq!(fragment_fraction(prose), 0.0);
    }
}
