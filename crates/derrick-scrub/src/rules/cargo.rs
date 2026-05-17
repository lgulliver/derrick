//! Rules for Cargo output.

use crate::{add_regex_rule, Action, Replacement, RuleSet};

/// Return the default Cargo rules.
pub fn rules() -> RuleSet {
    let mut rules = RuleSet::new();
    add_regex_rule(
        &mut rules,
        "cargo compile progress",
        r"^\s+(Compiling|Checking)\s+.+$",
        Action::KeepFirstDropRest {
            key: Some(Replacement("$1".to_owned())),
        },
    );
    add_regex_rule(
        &mut rules,
        "cargo fresh progress",
        r"^\s+Fresh\s+.+$",
        Action::Drop,
    );
    add_regex_rule(
        &mut rules,
        "cargo finished footer",
        r"^\s+Finished\s+.+ target\(s\) in .+$",
        Action::Drop,
    );
    rules
}
