//! Rules for `gt` output.

use crate::{add_regex_rule, Action, Replacement, RuleSet};

/// Return the default `gt` rules.
pub fn rules() -> RuleSet {
    let mut rules = RuleSet::new();
    add_regex_rule(
        &mut rules,
        "gt ansi controls",
        r"\x1b\[[0-9;?]*[A-Za-z]",
        Action::Replace(Replacement(String::new())),
    );
    add_regex_rule(
        &mut rules,
        "gt spinner",
        r"^[⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏]\s+.+$",
        Action::Drop,
    );
    add_regex_rule(
        &mut rules,
        "gt repeated header",
        r"^=+ derrick activity =+$",
        Action::KeepFirstDropRest { key: None },
    );
    rules
}
