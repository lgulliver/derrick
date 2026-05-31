use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::thread;

use chrono::{DateTime, TimeZone, Utc};
use derrick_config::{Config, Site};
use derrick_memory::{
    Lesson, MemoryError, MemoryLayer, MemoryPaths, MemoryStore, Seeds, extract_query_tags,
};
use derrick_substrate::ticket_id_pattern;
use serde_json::{Value, json};
use tempfile::{TempDir, tempdir};

fn temp_dir() -> TempDir {
    tempdir().unwrap_or_else(|error| panic!("tempdir should be created: {error}"))
}

fn site_from_yaml(name: &str, prefix: &str) -> Site {
    let dir = temp_dir();
    let path = dir.path().join("derrick.yaml");
    fs::write(
        &path,
        format!(
            r#"
version: 1
site:
  name: {name}
  prefix: {prefix}
models:
  claude-sonnet:
    provider: anthropic
    model: claude-sonnet-4-6
  codex-gpt5:
    provider: openai-cli
    model: gpt-5
roles:
  drafter: claude-sonnet
  reviewer: codex-gpt5
tools:
  speckit:
    enabled: true
    version: ">=0.4.0"
  assay:
    enabled: false
    role: reviewer
    reviewers: [reviewer]
  substrate:
    backend: native
    mode: solo
  copilot:
    agent_identity: derrick-hand
pipeline: []
guardrails:
  constitution_path: .specify/memory/constitution.md
parallelism:
  batch_max: 8
  step_max: 4
  assay_max: 2
state:
  dir: .derrick
  log_runs: true
  worktree_root: .derrick/worktrees
"#
        ),
    )
    .unwrap_or_else(|error| panic!("site fixture should be written: {error}"));
    Config::load_from_path(&path)
        .unwrap_or_else(|error| panic!("site fixture should load: {error}"))
        .site()
        .clone()
}

fn default_site() -> Site {
    Config::defaults().site().clone()
}

fn store_with_host() -> (TempDir, MemoryStore) {
    let dir = temp_dir();
    let paths = MemoryPaths {
        host_memory_root: Some(dir.path().join("host-memory")),
        repo_state: dir.path().join(".derrick"),
    };
    let store = MemoryStore::open(paths, &default_site())
        .unwrap_or_else(|error| panic!("memory store should open: {error}"));
    (dir, store)
}

fn seeds() -> Seeds {
    Seeds {
        project: vec![
            ("site".to_owned(), "derrick".to_owned()),
            ("prefix".to_owned(), "drk".to_owned()),
        ],
        reference: vec![("tasks".to_owned(), "tickets live under tickets/".to_owned())],
        feedback: vec![(
            "guardrails".to_owned(),
            "assay verdicts are binding".to_owned(),
        )],
    }
}

fn read_to_string(path: &Path) -> String {
    fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("{} should be readable: {error}", path.display()))
}

fn utc(year: i32, month: u32, day: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(year, month, day, 0, 0, 0)
        .single()
        .unwrap_or_else(|| panic!("test timestamp should be valid"))
}

fn lesson(at: DateTime<Utc>, body: &str) -> Lesson {
    Lesson {
        at,
        batch: Some("batch-1".to_owned()),
        body: body.to_owned(),
        tags: extract_query_tags(body),
    }
}

#[test]
fn seed_list_and_unmemoize_cover_host_memory_domain() {
    let (dir, store) = store_with_host();
    let first = store
        .seed(&seeds())
        .unwrap_or_else(|error| panic!("seed should write: {error}"));
    let second = store
        .seed(&seeds())
        .unwrap_or_else(|error| panic!("seed should be idempotent: {error}"));
    assert_eq!(first.len(), 5);
    assert!(second.is_empty());

    let site_dir = dir.path().join("host-memory/derrick/derrick");
    assert_eq!(
        read_to_string(&site_dir.join("MEMORY.md")),
        "- feedback/guardrails.md\n- project/prefix.md\n- project/site.md\n- reference/tasks.md\n"
    );
    assert!(
        store
            .list()
            .unwrap_or_else(|error| panic!("list should work: {error}"))
            .iter()
            .any(|entry| entry.layer == MemoryLayer::Project)
    );

    let outside = dir.path().join("host-memory/outside.md");
    fs::write(&outside, "keep")
        .unwrap_or_else(|error| panic!("outside fixture should write: {error}"));
    store
        .unmemoize()
        .unwrap_or_else(|error| panic!("unmemoize should succeed: {error}"));
    assert!(outside.exists());
    assert!(!site_dir.exists());
}

#[test]
fn repo_state_domain_round_trips_and_prunes() {
    let (_dir, store) = store_with_host();
    store
        .append_run_digest("20260518", "plan accepted")
        .unwrap_or_else(|error| panic!("digest should append: {error}"));
    store
        .set_feature_state("feature-1", &json!({"ticket": "drk-1"}))
        .unwrap_or_else(|error| panic!("state should write: {error}"));
    assert_eq!(
        store
            .get_feature_state::<Value>("feature-1")
            .unwrap_or_else(|error| panic!("state should read: {error}")),
        Some(json!({"ticket": "drk-1"}))
    );
    store
        .prune_feature_state("feature-1")
        .unwrap_or_else(|error| panic!("state should prune: {error}"));
    assert_eq!(
        store
            .get_feature_state::<Value>("feature-1")
            .unwrap_or_else(|error| panic!("state should read: {error}")),
        None
    );

    store
        .append_lesson(&lesson(utc(2026, 1, 1), "drk-1 old lesson"))
        .unwrap_or_else(|error| panic!("lesson should append: {error}"));
    store
        .append_lesson(&lesson(utc(2026, 3, 1), "per #9.B.7 new lesson"))
        .unwrap_or_else(|error| panic!("lesson should append: {error}"));
    assert_eq!(
        store
            .lessons(Some(utc(2026, 2, 1)))
            .unwrap_or_else(|error| panic!("lessons should filter: {error}"))
            .len(),
        1
    );
    assert_eq!(
        store
            .prune_lessons(Some(utc(2026, 2, 1)))
            .unwrap_or_else(|error| panic!("lessons should prune: {error}")),
        1
    );
}

#[test]
fn quality_gate_and_validation_errors_are_explicit() {
    let (dir, store) = store_with_host();
    assert_eq!(ticket_id_pattern(), "^[a-z]{1,6}-\\d+$");
    assert!(matches!(
        store.append_lesson(&lesson(utc(2026, 5, 18), "be careful with concurrency")),
        Err(MemoryError::Rejected { .. })
    ));
    assert!(matches!(
        store.append_run_digest("../bad", "bad"),
        Err(MemoryError::Invalid { .. })
    ));
    assert!(matches!(
        store.seed(&Seeds {
            project: vec![("bad/name".to_owned(), "body".to_owned())],
            ..Seeds::default()
        }),
        Err(MemoryError::Invalid { .. })
    ));
    assert!(matches!(
        MemoryStore::open(
            MemoryPaths {
                host_memory_root: None,
                repo_state: dir.path().join("other-state"),
            },
            &site_from_yaml("../bad", "bad")
        ),
        Err(MemoryError::Invalid { .. })
    ));
}

#[test]
fn append_digest_is_safe_under_parallel_writers() {
    let (_dir, store) = store_with_host();
    let store = Arc::new(store);
    let mut handles = Vec::new();
    for index in 0..16 {
        let store = Arc::clone(&store);
        handles.push(thread::spawn(move || {
            store
                .append_run_digest("20260518", &format!("digest-{index}"))
                .unwrap_or_else(|error| panic!("digest should append: {error}"));
        }));
    }
    for handle in handles {
        handle
            .join()
            .unwrap_or_else(|error| panic!("thread should join: {error:?}"));
    }
    let entries = store
        .list()
        .unwrap_or_else(|error| panic!("list should work: {error}"));
    assert!(
        entries
            .iter()
            .any(|entry| entry.layer == MemoryLayer::RunDigest)
    );
}

#[test]
fn multiple_sites_have_separate_host_namespaces() {
    let dir = temp_dir();
    let root = dir.path().join("host-memory");
    let repo_state = dir.path().join(".derrick");
    let first = MemoryStore::open(
        MemoryPaths {
            host_memory_root: Some(root.clone()),
            repo_state: repo_state.clone(),
        },
        &site_from_yaml("alpha", "alp"),
    )
    .unwrap_or_else(|error| panic!("first store should open: {error}"));
    let second = MemoryStore::open(
        MemoryPaths {
            host_memory_root: Some(root.clone()),
            repo_state,
        },
        &site_from_yaml("beta", "bet"),
    )
    .unwrap_or_else(|error| panic!("second store should open: {error}"));

    first
        .seed(&Seeds {
            project: vec![("site".to_owned(), "alpha".to_owned())],
            ..Seeds::default()
        })
        .unwrap_or_else(|error| panic!("first seed should write: {error}"));
    second
        .seed(&Seeds {
            project: vec![("site".to_owned(), "beta".to_owned())],
            ..Seeds::default()
        })
        .unwrap_or_else(|error| panic!("second seed should write: {error}"));

    assert_eq!(
        read_to_string(&root.join("derrick/alpha/project/site.md")),
        "alpha"
    );
    assert_eq!(
        read_to_string(&root.join("derrick/beta/project/site.md")),
        "beta"
    );
}

#[test]
fn list_surfaces_all_repo_layers() {
    let (_dir, store) = store_with_host();
    store
        .seed(&seeds())
        .unwrap_or_else(|error| panic!("seed should write: {error}"));
    store
        .append_run_digest("20260518", "plan accepted")
        .unwrap_or_else(|error| panic!("digest should append: {error}"));
    store
        .set_feature_state("feature-1", &json!({"status": "open"}))
        .unwrap_or_else(|error| panic!("state should write: {error}"));
    store
        .append_lesson(&lesson(utc(2026, 5, 18), "drk-7 clarified #9.A"))
        .unwrap_or_else(|error| panic!("lesson should append: {error}"));

    let layers = store
        .list()
        .unwrap_or_else(|error| panic!("list should work: {error}"))
        .into_iter()
        .map(|entry| entry.layer)
        .collect::<BTreeSet<_>>();

    assert_eq!(
        layers,
        BTreeSet::from([
            MemoryLayer::Project,
            MemoryLayer::Reference,
            MemoryLayer::Feedback,
            MemoryLayer::RunDigest,
            MemoryLayer::FeatureState,
            MemoryLayer::Lessons,
        ])
    );
}
