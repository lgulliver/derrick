//! Rules for Cargo output.

use crate::{add_regex_rule, Action, Replacement, RuleSet};

/// Return the default Cargo rules.
pub fn rules() -> RuleSet {
    let mut rules = RuleSet::new();
    add_regex_rule(
        &mut rules,
        "cargo compile progress",
        r"^\s+(Compiling|Checking)\s+.+$",
        Action::Collapse {
            render: Replacement("$1: $count crates".to_owned()),
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
    add_regex_rule(
        &mut rules,
        "cargo index update",
        r"^\s*(Updating|Downloading|Downloaded|Locking)\s+.+$",
        Action::Drop,
    );
    add_regex_rule(
        &mut rules,
        "cargo test ok line",
        r"^test .+ \.\.\. ok$",
        Action::Collapse {
            render: Replacement("tests: $count passed".to_owned()),
            key: None,
        },
    );
    add_regex_rule(
        &mut rules,
        "cargo running tests",
        r"^running \d+ tests?$",
        Action::Drop,
    );
    add_regex_rule(
        &mut rules,
        "cargo test runner binary",
        r"^\s+Running .+$",
        Action::Drop,
    );
    add_regex_rule(
        &mut rules,
        "cargo ansi controls",
        r"\x1b\[[0-9;?]*[A-Za-z]",
        Action::Replace(Replacement(String::new())),
    );
    rules
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Scrubber;

    fn make() -> Scrubber {
        let mut s = Scrubber::empty();
        s.register("cargo", rules());
        s
    }

    #[test]
    fn cargo_build_output_hits_80_pct() {
        let input = concat!(
            "   Compiling proc-macro2 v1.0.95\n",
            "   Compiling unicode-ident v1.0.14\n",
            "   Compiling quote v1.0.40\n",
            "   Compiling syn v2.0.101\n",
            "   Compiling serde v1.0.219\n",
            "   Compiling serde_derive v1.0.219\n",
            "   Compiling derive_more v2.0.1\n",
            "   Compiling regex v1.11.1\n",
            "   Compiling tokio v1.45.0\n",
            "   Compiling derrick-models v0.1.0\n",
            "   Compiling derrick-cli v0.1.0\n",
            "    Finished `dev` profile target(s) in 23.45s\n",
        )
        .as_bytes();
        let scrubber = make();
        let (_, stats) = scrubber.scrub("cargo", input);
        assert!(
            stats.savings_pct() >= 80.0,
            "expected >=80% savings, got {:.1}%",
            stats.savings_pct()
        );
    }

    #[test]
    fn cargo_test_ok_lines_collapse() {
        let input = concat!(
            "running 3 tests\n",
            "test foo ... ok\n",
            "test bar ... ok\n",
            "test baz ... ok\n",
            "test result: ok. 3 passed; 0 failed\n",
        )
        .as_bytes();
        let scrubber = make();
        let (output, _) = scrubber.scrub("cargo", input);
        let out = String::from_utf8_lossy(&output);
        assert!(out.contains("3 passed"), "should collapse ok lines: {out}");
        assert!(
            !out.contains("running 3"),
            "should drop running line: {out}"
        );
    }
}
