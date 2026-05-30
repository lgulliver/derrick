//! Shared terminal theme for the CLI.
//!
//! One place decides whether output is styled (stdout is a TTY and `NO_COLOR`
//! is unset) and exposes the colour/weight primitives, glyphs, and semantic
//! line builders every command uses, so the whole surface looks consistent
//! instead of each command hand-rolling its own ANSI escapes.
//!
//! All helpers return plain text when [`styled`] is false, so piped/CI output
//! stays clean. The `indicatif`-based run reporter has its own stderr-scoped
//! styling in [`crate::progress`]; this module governs everything else.

use std::io::IsTerminal;

use owo_colors::OwoColorize;

/// Whether stdout should carry ANSI styling.
pub(crate) fn styled() -> bool {
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

macro_rules! paint {
    ($name:ident, $method:ident) => {
        /// Apply the named style, or return the text unchanged when unstyled.
        pub(crate) fn $name(s: &str) -> String {
            if styled() {
                s.$method().to_string()
            } else {
                s.to_owned()
            }
        }
    };
}

paint!(bold, bold);
paint!(dim, dimmed);
paint!(cyan, cyan);
paint!(green, green);
paint!(red, red);
paint!(yellow, yellow);

// ─── coloured glyphs ───────────────────────────────────────────────────────

/// Cyan prompt/hint arrow `›`.
pub(crate) fn arrow() -> String {
    cyan("\u{203a}")
}

/// Green success check `✓`.
pub(crate) fn tick() -> String {
    green("\u{2713}")
}

/// Red failure cross `✗`.
pub(crate) fn cross() -> String {
    red("\u{2717}")
}

/// Yellow warning sign `⚠`.
pub(crate) fn warn_glyph() -> String {
    yellow("\u{26a0}")
}

// ─── semantic lines (include the leading two-space indent) ───────────────────

/// `  ✓  <name>  ready` — the post-init success banner line.
pub(crate) fn ready(name: &str) -> String {
    format!("  {}  {}  ready", tick(), bold(name))
}

/// `  ·  <path>` — a file that was written, green bullet.
pub(crate) fn written(path: &str) -> String {
    format!("  {}  {path}", green("\u{b7}"))
}

/// `  ·  <path>  (skipped, already exists)` — fully dimmed.
pub(crate) fn skipped(path: &str) -> String {
    dim(&format!("  \u{b7}  {path}  (skipped, already exists)"))
}

/// `  ·  <text>` — a green bullet status line (e.g. "git repository initialised").
pub(crate) fn done(text: &str) -> String {
    format!("  {}  {text}", green("\u{b7}"))
}

/// `  ›  <text>` — a cyan next-step hint.
pub(crate) fn hint(text: &str) -> String {
    format!("  {}  {text}", arrow())
}

/// `  ⚠  <text>` — a yellow warning line.
pub(crate) fn warn(text: &str) -> String {
    format!("  {}  {text}", warn_glyph())
}

/// A dimmed full-width horizontal rule.
pub(crate) fn rule() -> String {
    dim(&"\u{2500}".repeat(62))
}

/// `  ─── <title> ───…` — a left-aligned section header rule.
pub(crate) fn section(title: &str) -> String {
    let fill = 62usize.saturating_sub(title.len() + 5);
    format!(
        "  \u{2500}\u{2500}\u{2500} {title} {}",
        "\u{2500}".repeat(fill)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // Tests run with stdout piped (not a TTY), so `styled()` is false and every
    // helper must return clean, escape-free text.
    #[test]
    fn unstyled_helpers_emit_no_escapes() {
        for s in [
            bold("x"),
            dim("x"),
            cyan("x"),
            green("x"),
            red("x"),
            yellow("x"),
            arrow(),
            tick(),
            cross(),
            warn_glyph(),
            ready("site"),
            written("a.txt"),
            skipped("a.txt"),
            done("init"),
            hint("go"),
            warn("careful"),
            rule(),
            section("config"),
        ] {
            assert!(!s.contains('\u{1b}'), "unexpected ANSI escape in {s:?}");
        }
    }

    #[test]
    fn semantic_lines_carry_expected_text() {
        assert_eq!(ready("widget"), "  \u{2713}  widget  ready");
        assert_eq!(written("derrick.yaml"), "  \u{b7}  derrick.yaml");
        assert!(skipped("x").contains("(skipped, already exists)"));
        assert!(hint("run doctor").starts_with("  \u{203a}  "));
        assert!(section("AI tools").starts_with("  \u{2500}\u{2500}\u{2500} AI tools "));
    }
}
