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
