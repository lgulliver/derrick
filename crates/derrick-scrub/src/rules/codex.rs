//! Rules for Codex output.

use crate::{add_regex_rule, Action, Replacement, RuleSet};

/// Return the default Codex rules.
pub fn rules() -> RuleSet {
    let mut rules = RuleSet::new();
    add_regex_rule(
        &mut rules,
        "codex tokens used",
        r"^tokens used: .+$",
        Action::Drop,
    );
    add_regex_rule(&mut rules, "codex exec recap", r"^exec: .+$", Action::Drop);
    add_regex_rule(
        &mut rules,
        "codex succeeded footer",
        r"^succeeded in \d+ms$",
        Action::Collapse {
            render: Replacement("succeeded ($count steps)".to_owned()),
            key: None,
        },
    );
    rules
}
