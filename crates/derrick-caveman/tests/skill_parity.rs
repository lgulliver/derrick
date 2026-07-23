//! D91: automated skill-parity harness enforcing D7 ("caveman
//! byte-identical to the installed skill at matched intensities").
//!
//! D7 previously had no automated enforcement — this file used to be a
//! 9-line `#[ignore]`d placeholder — which is exactly how the D90 drift
//! (Ultra converting `because`/`therefore` into a forbidden arrow) shipped
//! unnoticed. This harness has three parts:
//!
//! 1. [`vendored_corpus_matches_compress_output`] — always runs, in CI and
//!    locally, with no external dependency. It re-asserts `compress()`
//!    against the crate's own checked-in corpus (`tests/corpus/**`), which
//!    was hand-audited against the skill during the D7/D90 work. This is
//!    the genuine, CI-enforced regression gate: any future change to the
//!    shaping rules that silently diverges from the vendored, skill-derived
//!    expected output fails the build.
//!
//! 2. [`installed_skill_matches_vendored_snapshot`] — a drift check against
//!    the *actual* installed skill file, wherever it lives on the machine
//!    running the test (`CAVEMAN_SKILL_PATH` env override, else the default
//!    plugin-marketplace path). The installed skill is a user-local plugin
//!    file, not part of this repository, so it will typically be **absent
//!    in CI** — when absent, this test prints a clear skip message and
//!    passes trivially rather than failing the build. When present (e.g. a
//!    maintainer's dev machine with the caveman plugin installed), it does
//!    a byte-for-byte diff against the vendored snapshot at
//!    `tests/skill_fixtures/SKILL.snapshot.md` and fails loudly if the two
//!    have diverged, so a human re-audits the shaping rules and re-vendors
//!    the snapshot before the drift ships unnoticed (the exact failure mode
//!    D90 closed manually).
//!
//! 3. [`vendored_snapshot_still_bans_arrows`] — a narrow, always-on content
//!    anchor tying the vendored snapshot to the specific D90/D7 rule this
//!    crate depends on (Ultra strips causal conjunctions and never emits an
//!    arrow). If the skill's own wording ever changes, this fails and tells
//!    a human exactly which rule needs re-review, independent of whether
//!    the live skill file is reachable.
//!
//! Together these make "byte-identical to the skill" a build-time
//! guarantee for the part that's deterministically checkable (the vendored
//! corpus/snapshot), rather than a manual, easy-to-forget verification
//! step.

use std::env;
use std::fs;
use std::path::PathBuf;

use derrick_caveman::compress;

mod support;
use support::{corpus_inputs, intensity_dirs};

/// Part 1: always-on, CI-enforced regression gate. See module docs.
#[test]
fn vendored_corpus_matches_compress_output() -> Result<(), Box<dyn std::error::Error>> {
    let mut checked = 0usize;

    for (intensity, dir) in intensity_dirs() {
        for input_path in corpus_inputs(dir)? {
            let input = fs::read_to_string(&input_path)?;
            let expected = fs::read_to_string(input_path.with_extension("out"))?;
            let output = compress(&input, intensity);
            assert_eq!(
                output.text,
                expected,
                "D7/D91 parity break: compress() diverged from the vendored \
                 corpus fixture {}",
                input_path.display(),
            );
            checked += 1;
        }
    }

    assert!(
        checked > 0,
        "no corpus fixtures were found — parity harness would be enforcing nothing"
    );

    Ok(())
}

/// Part 3: content anchor. Always runs, no external dependency.
#[test]
fn vendored_snapshot_still_bans_arrows() -> Result<(), Box<dyn std::error::Error>> {
    let snapshot = fs::read_to_string(vendored_snapshot_path())?;

    assert!(
        snapshot.contains("NO arrows"),
        "vendored SKILL.snapshot.md no longer states the arrow prohibition \
         that D90/D7 depend on — re-read the installed skill, update \
         causal_regex in src/lib.rs accordingly, and re-vendor the snapshot"
    );
    assert!(
        snapshot.contains("Strip conjunctions when cause-then-effect stay unambiguous"),
        "vendored SKILL.snapshot.md no longer states the Ultra \
         conjunction-stripping rule that causal_regex mirrors — re-audit \
         D90 before changing this fixture"
    );

    Ok(())
}

/// Part 2: live-skill drift guard. Skips gracefully when the installed
/// skill isn't reachable (the expected case in CI); enforces byte parity
/// when it is.
#[test]
fn installed_skill_matches_vendored_snapshot() -> Result<(), Box<dyn std::error::Error>> {
    let Some(installed_path) = locate_installed_skill() else {
        eprintln!(
            "skill_parity: installed caveman SKILL.md not found locally \
             (checked $CAVEMAN_SKILL_PATH and the default plugin-marketplace \
             path) — skipping live-skill diff. This is expected in CI; the \
             vendored-corpus assertion in \
             vendored_corpus_matches_compress_output still enforces D7 \
             there. Run this test on a machine with the caveman plugin \
             installed to also check for upstream skill drift."
        );
        return Ok(());
    };

    let installed = fs::read_to_string(&installed_path)?;
    let vendored = fs::read_to_string(vendored_snapshot_path())?;

    assert_eq!(
        installed,
        vendored,
        "installed caveman SKILL.md ({}) has diverged from the vendored \
         snapshot at tests/skill_fixtures/SKILL.snapshot.md. Re-read the \
         installed skill, re-audit derrick-caveman's shaping rules for D7 \
         conformance (this is exactly how the D90 arrow-vs-strip drift \
         shipped unnoticed), then update the vendored snapshot to match.",
        installed_path.display(),
    );

    Ok(())
}

fn vendored_snapshot_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("skill_fixtures")
        .join("SKILL.snapshot.md")
}

/// Locate the installed caveman skill file, if reachable from this
/// machine. Checked in priority order:
/// 1. `CAVEMAN_SKILL_PATH` — explicit override (e.g. a CI job that vendors
///    the plugin marketplace on purpose).
/// 2. The default Claude Code plugin-marketplace install path under
///    `$HOME`.
///
/// Returns `None` (never panics) when neither resolves to a readable
/// file — the caller is responsible for treating that as "skip", not
/// "fail".
fn locate_installed_skill() -> Option<PathBuf> {
    if let Ok(override_path) = env::var("CAVEMAN_SKILL_PATH") {
        let path = PathBuf::from(override_path);
        if path.is_file() {
            return Some(path);
        }
        return None;
    }

    let home = env::var("HOME").ok()?;
    let default_path = PathBuf::from(home)
        .join(".claude")
        .join("plugins")
        .join("marketplaces")
        .join("caveman")
        .join("skills")
        .join("caveman")
        .join("SKILL.md");

    default_path.is_file().then_some(default_path)
}
