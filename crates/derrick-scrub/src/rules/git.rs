//! Rules for `git` output.

use crate::{add_regex_rule, Action, RuleSet};

/// Return the default `git` rules.
pub fn rules() -> RuleSet {
    let mut rules = RuleSet::new();
    add_regex_rule(
        &mut rules,
        "git remote progress",
        r"^remote: ((Counting|Compressing) objects: .+|Total .+)$",
        Action::Drop,
    );
    add_regex_rule(
        &mut rules,
        "git transfer progress",
        r"^(Receiving objects|Resolving deltas|Writing objects):\s+\d+%.*$",
        Action::Drop,
    );
    add_regex_rule(
        &mut rules,
        "git repeated warning",
        r"^warning: .+$",
        Action::KeepFirstDropRest { key: None },
    );
    rules
}
