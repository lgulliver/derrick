//! Rules for `git` output.

use crate::{Action, Replacement, RuleSet, add_regex_rule};

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
    add_regex_rule(
        &mut rules,
        "git enumerate progress",
        r"^remote: Enumerating objects: .+$",
        Action::Drop,
    );
    add_regex_rule(
        &mut rules,
        "git unpack progress",
        r"^Unpacking objects: .+$",
        Action::Drop,
    );
    add_regex_rule(&mut rules, "git remote url", r"^From \S+$", Action::Drop);
    add_regex_rule(
        &mut rules,
        "git delta compression header",
        r"^Delta compression using up to \d+ threads$",
        Action::Drop,
    );
    add_regex_rule(
        &mut rules,
        "git ansi controls",
        r"\x1b\[[0-9;?]*[A-Za-z]",
        Action::Replace(Replacement(String::new())),
    );
    add_regex_rule(
        &mut rules,
        "git tracking branch setup",
        r"^Branch '.+' set up to track .+$",
        Action::Drop,
    );
    rules
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Scrubber;

    fn make() -> Scrubber {
        let mut s = Scrubber::empty();
        s.register("git", rules());
        s
    }

    #[test]
    fn git_fetch_output_hits_80_pct() {
        let input = concat!(
            "remote: Enumerating objects: 42, done.\n",
            "remote: Counting objects: 100% (42/42), done.\n",
            "remote: Compressing objects: 100% (23/23), done.\n",
            "remote: Total 28 (delta 12), reused 4 (delta 2), pack-reused 0 (from 0)\n",
            "Unpacking objects: 100% (28/28), 45.89 KiB | 1.26 MiB/s, done.\n",
            "From https://github.com/owner/repo\n",
            "   abc1234..def5678  main -> origin/main\n",
        )
        .as_bytes();
        let scrubber = make();
        let (_, stats) = scrubber.scrub("git", input);
        assert!(
            stats.savings_pct() >= 80.0,
            "expected >=80% savings, got {:.1}%",
            stats.savings_pct()
        );
    }
}
