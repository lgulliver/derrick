//! Rules for `bd` output.

use crate::{Action, Replacement, RuleSet, add_regex_rule};

/// Return the default `bd` rules.
pub fn rules() -> RuleSet {
    let mut rules = RuleSet::new();
    add_regex_rule(
        &mut rules,
        "bd header",
        r"^ID\s+Title\s+Status$",
        Action::Drop,
    );
    add_regex_rule(&mut rules, "bd separator", r"^-{3,}.*$", Action::Drop);
    add_regex_rule(
        &mut rules,
        "bd list row",
        r"^([A-Za-z]+-[0-9]+)\s{2,}(.+?)\s{2,}([A-Za-z_-]+)$",
        Action::Replace(Replacement("$1: $2".to_owned())),
    );
    rules
}
