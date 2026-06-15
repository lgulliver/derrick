//! Rules for Claude Code output.

use crate::{Action, Replacement, RuleSet, add_regex_rule};

/// Return the default Claude Code rules.
pub fn rules() -> RuleSet {
    let mut rules = RuleSet::new();
    add_regex_rule(
        &mut rules,
        "claude info",
        r"^\[(INFO|DEBUG)\]\s+.+$",
        Action::Drop,
    );
    add_regex_rule(
        &mut rules,
        "claude tool use",
        r"^Tool use:\s+([A-Za-z]+)\((.*)\)$",
        Action::Replace(Replacement("$1: $2".to_owned())),
    );
    add_regex_rule(
        &mut rules,
        "claude thinking marker",
        r"^Thinking…$",
        Action::Drop,
    );
    rules
}
