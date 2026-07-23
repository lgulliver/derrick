//! Shared test helpers for the corpus-driven test binaries
//! (`tests/caveman.rs`, `tests/skill_parity.rs`).
//!
//! This lives under `tests/support/mod.rs` (not `tests/support.rs`) so
//! Cargo does not treat it as its own integration-test binary — it is
//! a plain module pulled in via `mod support;`.

use std::fs;
use std::path::{Path, PathBuf};

use derrick_caveman::Intensity;

/// The three corpus sub-directories, paired with their `Intensity`.
pub fn intensity_dirs() -> [(Intensity, &'static str); 3] {
    [
        (Intensity::Lite, "lite"),
        (Intensity::Full, "full"),
        (Intensity::Ultra, "ultra"),
    ]
}

/// All `.in` fixture paths under `tests/corpus/<dir>`, sorted for
/// deterministic iteration order.
pub fn corpus_inputs(dir: &str) -> Result<Vec<PathBuf>, Box<dyn std::error::Error>> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("corpus")
        .join(dir);
    let mut inputs = Vec::new();

    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "in") {
            inputs.push(path);
        }
    }

    inputs.sort();
    Ok(inputs)
}

/// D93: invented prose abbreviations the installed caveman skill bans
/// outright — "never invent new abbreviations (cfg/impl/req/res/fn)"
/// (Rules), sharpened at Ultra to "NO prose abbreviations
/// (cfg/impl/req/res/fn/auth) ... measured zero token saving under
/// tokenizer, cost decode clarity" (Intensity table). `rewrite_word` in
/// `src/lib.rs` must never map any word onto one of these. Kept as a
/// single shared list so both the fixture-level tests
/// (`tests/caveman.rs`) and the D91 parity harness
/// (`tests/skill_parity.rs`) enforce the same class instead of each
/// hand-maintaining a copy that could drift out of sync.
pub const BANNED_INVENTED_ABBREVIATIONS: [&str; 6] = ["cfg", "impl", "req", "res", "fn", "auth"];

/// True if `output` contains `banned` as a standalone, punctuation-trimmed
/// word (case-insensitive) — i.e. some shaping rule emitted the banned
/// abbreviation as its own token, not merely as a substring of a longer,
/// legitimate word (e.g. this must not flag `configuration`).
pub fn contains_banned_word(output: &str, banned: &str) -> bool {
    output.split_whitespace().any(|word| {
        word.trim_matches(|ch: char| !ch.is_alphanumeric())
            .eq_ignore_ascii_case(banned)
    })
}
