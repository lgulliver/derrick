//! End-to-end build + query test over a small mixed-language fixture repo.

use std::fs;

use derrick_survey::{BuildOptions, Survey, SurveyConfig};

async fn open_index(repo: &std::path::Path) -> Survey {
    let derrick_dir = repo.join(".derrick");
    fs::create_dir_all(&derrick_dir).unwrap();
    Survey::open(SurveyConfig {
        db_path: derrick_dir.join("index.db"),
        repo_root: repo.to_path_buf(),
        reader_pool: SurveyConfig::DEFAULT_READER_POOL,
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn builds_and_queries_a_mixed_repo() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();

    fs::write(
        repo.join("lib.rs"),
        "pub fn helper() {}\npub fn caller() {\n    helper();\n}\n",
    )
    .unwrap();
    fs::write(
        repo.join("app.py"),
        "def worker():\n    helper()\n\ndef helper():\n    pass\n",
    )
    .unwrap();
    fs::write(
        repo.join("svc.go"),
        "package main\ntype Server struct{}\nfunc serve() {\n\tlisten()\n}\nfunc listen() {}\n",
    )
    .unwrap();
    fs::write(
        repo.join("widget.ts"),
        "interface Props { id: number }\nclass Widget {\n  render() { draw(); }\n}\nfunction draw() {}\n",
    )
    .unwrap();
    fs::write(
        repo.join("util.js"),
        "const compute = (a, b) => a + b;\nfunction main() {\n  compute(1, 2);\n}\n",
    )
    .unwrap();
    // A directory that must be pruned.
    fs::create_dir_all(repo.join("node_modules")).unwrap();
    fs::write(repo.join("node_modules/skip.js"), "function ignored() {}\n").unwrap();

    let survey = open_index(repo).await;

    let report = survey.build(BuildOptions::default()).await.unwrap();
    assert_eq!(
        report.files_indexed, 5,
        "five source files, node_modules pruned"
    );
    assert!(report.symbols >= 4);

    // Search finds the Rust helper.
    let hits = survey.search("helper", 10).await.unwrap();
    assert!(hits.iter().any(|h| h.name == "helper"));

    // Every target language reaches the index through the build pipeline.
    for (query, symbol) in [
        ("serve", "serve"),     // Go
        ("Widget", "Widget"),   // TypeScript
        ("compute", "compute"), // JavaScript
    ] {
        let hits = survey.search(query, 10).await.unwrap();
        assert!(
            hits.iter().any(|h| h.name == symbol),
            "expected to find {symbol} from the {query} fixture"
        );
    }

    // Cross-language impact within a single language still resolves (Go).
    let go_impact = survey
        .impact("listen")
        .await
        .unwrap()
        .expect("listen exists");
    assert!(go_impact.callers.iter().any(|h| h.name == "serve"));

    // Impact: helper is called by caller (Rust) and worker (Python) by name.
    let impact = survey
        .impact("helper")
        .await
        .unwrap()
        .expect("helper exists");
    let caller_names: Vec<&str> = impact.callers.iter().map(|h| h.name.as_str()).collect();
    assert!(caller_names.contains(&"caller"));
    assert!(caller_names.contains(&"worker"));

    // Context for caller includes helper among related symbols.
    let ctx = survey.context("caller", 5).await.unwrap();
    assert!(ctx.entry_points.iter().any(|h| h.name == "caller"));
    assert!(ctx.related.iter().any(|h| h.name == "helper"));

    // Status: clean immediately after a full build.
    let status = survey.status().await.unwrap();
    assert_eq!(status.files, 5);
    assert!(status.pending.is_empty(), "no pending files after build");

    // Incremental: edit a file, status reports it modified, rebuild picks it up.
    fs::write(
        repo.join("app.py"),
        "def worker():\n    helper()\n\ndef helper():\n    pass\n\ndef extra():\n    pass\n",
    )
    .unwrap();
    let status = survey.status().await.unwrap();
    assert!(
        status
            .pending
            .iter()
            .any(|p| p.path == "app.py" && p.reason == "modified")
    );

    let report = survey.build(BuildOptions::default()).await.unwrap();
    assert_eq!(report.files_indexed, 1, "only the changed file reparsed");
    assert_eq!(report.files_unchanged, 4);
    let hits = survey.search("extra", 10).await.unwrap();
    assert!(hits.iter().any(|h| h.name == "extra"));
}

/// `stats()` reports the index's own counts without diffing the working tree:
/// it stays clean even when the tree on disk diverges, where `status()` would
/// (correctly) report the divergence. This is the pushed-index contract —
/// there is no tree to diff against, so freshness must not read "stale".
#[tokio::test]
async fn stats_ignores_working_tree_divergence() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();

    fs::write(repo.join("a.rs"), "pub fn alpha() {}\n").unwrap();
    fs::write(repo.join("b.rs"), "pub fn beta() {}\n").unwrap();

    let survey = open_index(repo).await;
    survey.build(BuildOptions::default()).await.unwrap();

    // Baseline: stats() matches status() right after a clean build.
    let stats = survey.stats().await.unwrap();
    assert_eq!(stats.files, 2, "two source files indexed");
    assert!(stats.symbols >= 2);
    assert!(stats.pending.is_empty(), "fresh build has no pending files");
    assert_eq!(stats.freshness, "fresh");

    // Now diverge the working tree: delete one indexed file, add a new one.
    fs::remove_file(repo.join("b.rs")).unwrap();
    fs::write(repo.join("c.rs"), "pub fn gamma() {}\n").unwrap();

    // status() walks repo_root and reports the divergence (unchanged behaviour).
    let status = survey.status().await.unwrap();
    assert!(
        status.pending.iter().any(|p| p.path == "b.rs"),
        "status() should report the deleted file: {status:?}"
    );
    assert!(
        status.pending.iter().any(|p| p.path == "c.rs"),
        "status() should report the new file: {status:?}"
    );
    assert!(
        status.freshness.starts_with("stale"),
        "status() should read stale after divergence: {status:?}"
    );

    // stats() ignores the tree entirely: same counts as the build, empty
    // pending, and a non-stale freshness label.
    let stats = survey.stats().await.unwrap();
    assert_eq!(
        stats.files, 2,
        "stats() reports the indexed count, not on-disk"
    );
    assert!(
        stats.pending.is_empty(),
        "stats() must never diff the tree: {stats:?}"
    );
    assert!(
        !stats.freshness.starts_with("stale"),
        "stats() must not read stale: {stats:?}"
    );
    assert_eq!(stats.freshness, "fresh");
}
