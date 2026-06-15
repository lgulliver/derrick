//! Rules for Copilot CLI output.

use crate::{Action, Replacement, RuleSet, add_regex_rule};

/// Return the default Copilot CLI rules.
pub fn rules() -> RuleSet {
    let mut rules = RuleSet::new();
    add_regex_rule(
        &mut rules,
        "copilot file decoration",
        r"^●\s+Read\s+.+$",
        Action::Drop,
    );
    add_regex_rule(
        &mut rules,
        "copilot premium telemetry",
        r"^Premium request .+$",
        Action::Collapse {
            render: Replacement("Premium request telemetry ($count lines)".to_owned()),
            key: None,
        },
    );
    add_regex_rule(
        &mut rules,
        "copilot thinking",
        r"^Thinking\.\.\.$",
        Action::Drop,
    );
    rules
}
