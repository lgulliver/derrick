//! Rules for OpenCode CLI output (`opencode run`).
//!
//! OpenCode emits several categories of noise before the final response:
//! - A multi-line ASCII-art banner on startup.
//! - Tool-use progress lines (reads, writes, shell calls).
//! - Thinking block markers when `--thinking` is set.
//! - Session/cost footers.

use crate::{add_regex_rule, Action, Replacement, RuleSet};

/// Return the default OpenCode rules.
pub fn rules() -> RuleSet {
    let mut rules = RuleSet::new();

    // ASCII banner lines — box-drawing / block characters only, no useful content.
    add_regex_rule(
        &mut rules,
        "opencode banner",
        r"^[█▀▄▐▌⠀\s]*$",
        Action::Drop,
    );

    // Tool-use progress: "Reading file.rs…", "Writing src/lib.rs…", etc.
    add_regex_rule(
        &mut rules,
        "opencode tool read",
        r"^Reading\s+.+$",
        Action::Drop,
    );
    add_regex_rule(
        &mut rules,
        "opencode tool write",
        r"^Writing\s+.+$",
        Action::Drop,
    );
    add_regex_rule(
        &mut rules,
        "opencode tool run",
        r"^Running\s+.+$",
        Action::Drop,
    );

    // Spinner / progress dots emitted while the model is thinking.
    add_regex_rule(
        &mut rules,
        "opencode spinner",
        r"^[⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏]\s+.*$",
        Action::Drop,
    );

    // Thinking block markers (emitted with --thinking).
    add_regex_rule(
        &mut rules,
        "opencode thinking marker",
        r"^(▶|◀)\s*(Thinking|thinking).*$",
        Action::Drop,
    );

    // Session cost / token footer lines.
    add_regex_rule(
        &mut rules,
        "opencode cost footer",
        r"^\$[\d.,]+\s+\([\d,]+\s+tokens\).*$",
        Action::Collapse {
            render: Replacement("Cost: $1 ($count lines)".to_owned()),
            key: None,
        },
    );

    rules
}
