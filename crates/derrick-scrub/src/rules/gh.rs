//! Rules for GitHub CLI output.

use crate::{Action, Replacement, RuleSet, add_regex_rule};

/// Return the default `gh` rules.
pub fn rules() -> RuleSet {
    let mut rules = RuleSet::new();
    add_regex_rule(
        &mut rules,
        "gh spinner",
        r"^[⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏]\s+.+$",
        Action::Drop,
    );
    add_regex_rule(
        &mut rules,
        "gh success prefix",
        r"^✓\s+(.+)$",
        Action::Replace(Replacement("$1".to_owned())),
    );
    add_regex_rule(
        &mut rules,
        "gh ansi controls",
        r"\x1b\[[0-9;?]*[A-Za-z]",
        Action::Replace(Replacement(String::new())),
    );
    rules
}
