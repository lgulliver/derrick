//! Roughneck — LLM output compression via prompt injection.
//!
//! Prepends terse-response instructions to each pipeline step's prompt to
//! cut roughly 65-75% of output tokens at the cost of a small fixed prefix
//! on the input side. Three intensity levels are provided: `lite`, `full`,
//! and `ultra`. Unknown levels default to `full`. The special values `off`
//! and `none` disable injection.

/// Lite-intensity instructions: drop filler, keep prose.
pub const LITE_INSTRUCTIONS: &str = "[ROUGHNECK:LITE] Be concise. Drop filler words, preambles, and closing summaries. Keep all technical content complete and accurate.";

/// Default-intensity instructions: fragments and bullets.
pub const FULL_INSTRUCTIONS: &str = "[ROUGHNECK:FULL] Respond in fragments and bullets. Skip preambles (\"I will now…\"), affirmations (\"Great!\"), and closing summaries. Use one line per decision. Preserve all code, paths, identifiers, and technical values verbatim. Omit nothing of substance.";

/// Ultra-intensity instructions: telegraphic only.
pub const ULTRA_INSTRUCTIONS: &str = "[ROUGHNECK:ULTRA] Telegraphic only. Fragments. One line per decision. No preamble, no summary, no transitions. Omit if uncertain. Preserve code/paths/identifiers exactly.";

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

/// Estimates how many output tokens roughneck saved given the observed
/// `tokens_out` and the level applied.
///
/// If `actual = baseline * (1 - rate)`, then `saved = actual * rate / (1 - rate)`.
/// Returns 0 for unknown / disabled levels.
pub fn estimate_tokens_saved(tokens_out: u32, level: &str) -> u32 {
    let rate: f64 = match level {
        "lite" => 0.30,
        "full" => 0.65,
        "ultra" => 0.75,
        _ => return 0,
    };
    let actual = f64::from(tokens_out);
    let saved = actual * rate / (1.0 - rate);
    // Clamp to u32 range; negative is unreachable here.
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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn estimate_tokens_saved_zero_for_unknown() {
        assert_eq!(estimate_tokens_saved(1_000, "off"), 0);
        assert_eq!(estimate_tokens_saved(1_000, "wat"), 0);
    }

    #[test]
    fn estimate_tokens_saved_lite() {
        // actual=700, rate=0.3 → baseline=1000, saved=300
        let saved = estimate_tokens_saved(700, "lite");
        assert!((saved as i64 - 300).abs() <= 1);
    }

    #[test]
    fn estimate_tokens_saved_full() {
        // actual=350, rate=0.65 → baseline=1000, saved=650
        let saved = estimate_tokens_saved(350, "full");
        assert!((saved as i64 - 650).abs() <= 1);
    }

    #[test]
    fn estimate_tokens_saved_ultra() {
        // actual=250, rate=0.75 → baseline=1000, saved=750
        let saved = estimate_tokens_saved(250, "ultra");
        assert!((saved as i64 - 750).abs() <= 1);
    }

    #[test]
    fn compress_document_prompt_includes_doc_and_header() {
        let p = compress_document_prompt("BODY", "spec");
        assert!(p.starts_with("[ROUGHNECK:FULL]"));
        assert!(p.contains("Compress the following spec into terse form"));
        assert!(p.contains("\n---\nBODY"));
    }
}
