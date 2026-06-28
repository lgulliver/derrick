//! Integration tests for the native spec provider.
//!
//! House rules: a real SQLite survey index via `tempfile` (no mocks), the
//! deterministic stub host is registered at the real `HostRegistry` boundary
//! (the actual seam, not a logic mock), and there is no `println!` — assertions
//! carry their own messages.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use derrick_config::Config;
use derrick_specify::schema::{has_reject, validate_plan, validate_spec, validate_tasks};
use derrick_specify::{NativeRequest, NativeSpecProvider};
use derrick_survey::{BuildOptions, Survey, SurveyConfig};
use derrick_tools::{HostAdapter, HostError, HostRegistry, HostRequest, HostResponse};

// --- shared fixtures --------------------------------------------------------

fn full_config(dir: &Path) -> Config {
    // A self-contained config: a claude-backed drafter/proposer so the native
    // provider resolves the registered stub host. No CLI is shelled — the
    // `claude` host name maps to the registered StubHost.
    let yaml = "version: 1\n\
        site:\n  name: t\n  prefix: tst\n\
        models:\n  m:\n    provider: claude\n    model: claude-sonnet-4-6\n\
        roles:\n  drafter: m\n  proposer: m\n  reviewer: m\n\
        tools:\n  speckit:\n    enabled: true\n    version: \">=0.4.0\"\n  \
        assay:\n    enabled: false\n    role: reviewer\n    reviewers: [reviewer]\n    rounds: 1\n  \
        substrate:\n    backend: native\n    mode: solo\n  copilot:\n    enabled: false\n    agent_identity: derrick-hand\n  \
        roughneck:\n    enabled: true\n    level: full\n\
        guardrails:\n  constitution_path: .specify/memory/constitution.md\n  forbid_paths: []\n  required_labels: []\n\
        parallelism:\n  batch_max: 8\n  step_max: 4\n  assay_max: 2\n\
        state:\n  dir: .derrick\n  log_runs: true\n  worktree_root: .derrick/worktrees\n";
    let path = dir.join("derrick.yaml");
    std::fs::write(&path, yaml).expect("write config");
    Config::load_from_path(&path).expect("load config")
}

/// Pre-scaffolds the canonical feature dir the way the seam's specify phase
/// does, so the provider has a target directory and `.specify/feature.json`.
fn prescaffold(working_dir: &Path, prompt: &str) -> PathBuf {
    derrick_assay::io::prescaffold_feature_dir(working_dir, prompt).expect("prescaffold")
}

// --- deterministic stub host ------------------------------------------------

/// Emits canned valid artifacts based on which phase prompt it receives. A
/// `fail_first` flag makes the *first* specify call emit an invalid draft (empty
/// acceptance) to force exactly one repair pass; the second call is valid.
struct StubHost {
    calls: Arc<Mutex<Vec<String>>>,
    fail_first_specify: bool,
}

impl StubHost {
    fn new(fail_first_specify: bool) -> (Self, Arc<Mutex<Vec<String>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                calls: calls.clone(),
                fail_first_specify,
            },
            calls,
        )
    }

    fn specify_count(&self) -> usize {
        self.calls
            .lock()
            .expect("lock")
            .iter()
            .filter(|p| p.contains("Write a specification"))
            .count()
    }
}

fn valid_spec_doc() -> &'static str {
    // The model is told NOT to write `grounding:` — derrick injects it. Neutral
    // placeholder content; no forbidden vocabulary.
    "---\n\
     schema: derrick.spec/v1\n\
     slug: widget-export\n\
     intent: Export widgets to a file.\n\
     requirements:\n\
     \x20\x20- id: R1\n\
     \x20\x20\x20\x20must: The system exports widgets as JSON.\n\
     acceptance:\n\
     \x20\x20- id: A1\n\
     \x20\x20\x20\x20check: A JSON file is produced.\n\
     non_goals: []\n\
     open_questions: []\n\
     ---\n\
     # Widget Export\n\n## Context\nWe need exports.\n\n## Requirements\n- R1\n\n## Acceptance Criteria\n- A1\n\n## Out of Scope\nNothing.\n"
}

fn invalid_spec_doc() -> &'static str {
    // Empty acceptance -> spec.no_acceptance reject.
    "---\n\
     schema: derrick.spec/v1\n\
     slug: widget-export\n\
     intent: Export widgets to a file.\n\
     requirements:\n\
     \x20\x20- id: R1\n\
     \x20\x20\x20\x20must: The system exports widgets as JSON.\n\
     acceptance: []\n\
     non_goals: []\n\
     open_questions: []\n\
     ---\n\
     # Widget Export\n\n## Context\nx\n\n## Requirements\nR1\n\n## Acceptance Criteria\nA1\n\n## Out of Scope\nNothing.\n"
}

#[async_trait::async_trait]
impl HostAdapter for StubHost {
    fn name(&self) -> &str {
        "claude"
    }
    fn is_available(&self) -> bool {
        true
    }
    async fn run(&self, request: HostRequest) -> Result<HostResponse, HostError> {
        let prompt = request.prompt.clone();
        self.calls.lock().expect("lock").push(prompt.clone());
        let stdout = if prompt.contains("clarify a feature request BEFORE") {
            // Clarify-first: one question with a recommendation.
            "Q: Which serialization format?\nOptions: JSON, YAML\nRecommendation: JSON\n".to_owned()
        } else if prompt.contains("Your previous draft was rejected") {
            // Repair pass for the spec: always emit the valid doc.
            valid_spec_doc().to_owned()
        } else if prompt.contains("Write a specification") {
            let first = self.specify_count() == 1; // this call already pushed
            if self.fail_first_specify && first {
                invalid_spec_doc().to_owned()
            } else {
                valid_spec_doc().to_owned()
            }
        } else if prompt.contains("implementation plan") {
            "---\nschema: derrick.plan/v1\ncovers: [R1]\ntouches:\n  - src/lib.rs\n---\n# Plan\nDo it.\n"
                .to_owned()
        } else if prompt.contains("Break this plan into tickets") {
            "## First ticket\n<!-- complexity: standard -->\nImplements R1.\n\n## Second ticket\nFollow-up for R1.\n"
                .to_owned()
        } else {
            String::new()
        };
        Ok(HostResponse {
            stdout,
            stderr: String::new(),
            exit_code: 0,
            elapsed: Duration::from_millis(1),
            tokens_in: 10,
            tokens_out: 20,
            pid: None,
        })
    }
}

fn registry(host: StubHost) -> Arc<HostRegistry> {
    let mut hosts = HostRegistry::empty();
    hosts.register("claude", Box::new(host));
    Arc::new(hosts)
}

// --- end-to-end -------------------------------------------------------------

#[tokio::test]
async fn native_produces_three_valid_artifacts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let wd = dir.path();
    let config = full_config(wd);
    std::fs::create_dir_all(wd.join(".specify/memory")).expect("memory dir");
    std::fs::write(wd.join(".specify/memory/constitution.md"), "constitution")
        .expect("constitution");

    let prompt = "Add a widget export command";
    let feature_dir = prescaffold(wd, prompt);
    let (host, _calls) = StubHost::new(false);
    let hosts = registry(host);
    let provider = NativeSpecProvider::new();

    let req = NativeRequest {
        raw_prompt: prompt,
        repo_root: wd,
        working_dir: wd,
        hosts: &hosts,
        config: &config,
        interactive: false,
        feature_dir: &feature_dir,
    };

    let spec = provider.specify(&req).await.expect("specify");
    assert!(spec.tokens_in > 0, "specify should account tokens");
    let plan = provider.plan(&req, None).await.expect("plan");
    let tasks = provider.tasks(&req).await.expect("tasks");

    // The three artifacts exist on disk.
    let spec_md = std::fs::read_to_string(wd.join(&feature_dir).join("spec.md")).expect("spec.md");
    let plan_md = std::fs::read_to_string(wd.join(&feature_dir).join("plan.md")).expect("plan.md");
    let tasks_md =
        std::fs::read_to_string(wd.join(&feature_dir).join("tasks.md")).expect("tasks.md");
    assert!(
        wd.join(&feature_dir).join("clarify.md").exists(),
        "clarify.md written"
    );

    // They parse + validate clean.
    assert!(!has_reject(&validate_spec(&spec_md)), "spec validates");
    assert!(
        !has_reject(&validate_plan(
            &plan_md,
            &["R1".to_owned()],
            &["src/lib.rs".to_owned()]
        )),
        "plan validates"
    );
    assert!(!has_reject(&validate_tasks(&tasks_md)), "tasks validate");

    // Derrick-injected grounding front-matter is present (index degraded here).
    assert!(spec_md.contains("grounding:"), "derrick injects grounding");

    // No repair pass on a clean run.
    assert!(
        !spec.repaired && !plan.repaired && !tasks.repaired,
        "no repair on clean run"
    );
}

#[tokio::test]
async fn invalid_first_draft_triggers_exactly_one_repair_pass() {
    let dir = tempfile::tempdir().expect("tempdir");
    let wd = dir.path();
    let config = full_config(wd);
    let prompt = "Add a widget export command";
    let feature_dir = prescaffold(wd, prompt);
    let (host, calls) = StubHost::new(true);
    let hosts = registry(host);
    let provider = NativeSpecProvider::new();

    let req = NativeRequest {
        raw_prompt: prompt,
        repo_root: wd,
        working_dir: wd,
        hosts: &hosts,
        config: &config,
        interactive: false,
        feature_dir: &feature_dir,
    };

    let outcome = provider.specify(&req).await.expect("specify with repair");
    assert!(
        outcome.repaired,
        "an invalid first draft must trigger a repair pass"
    );

    // Exactly one repair: one clarify call, one initial spec call, one repair call.
    let repair_calls = calls
        .lock()
        .expect("lock")
        .iter()
        .filter(|p| p.contains("Your previous draft was rejected"))
        .count();
    assert_eq!(repair_calls, 1, "exactly one bounded repair pass");

    let spec_md = std::fs::read_to_string(wd.join(&feature_dir).join("spec.md")).expect("spec.md");
    assert!(
        !has_reject(&validate_spec(&spec_md)),
        "repaired spec validates"
    );
}

// --- survey grounding -------------------------------------------------------

#[tokio::test]
async fn grounding_uses_real_index_and_excludes_unindexed_names() {
    let dir = tempfile::tempdir().expect("tempdir");
    let wd = dir.path();
    // A tiny real source tree with a distinctively-named symbol.
    std::fs::create_dir_all(wd.join("src")).expect("src dir");
    std::fs::write(
        wd.join("src/lib.rs"),
        "pub fn export_widget_payload() -> u32 { 7 }\n",
    )
    .expect("source");

    // Build a real SQLite survey index under .derrick/index.db.
    std::fs::create_dir_all(wd.join(".derrick")).expect("derrick dir");
    let survey = Survey::open(SurveyConfig {
        db_path: wd.join(".derrick").join("index.db"),
        repo_root: wd.to_path_buf(),
        reader_pool: SurveyConfig::DEFAULT_READER_POOL,
    })
    .await
    .expect("open survey");
    survey
        .build(BuildOptions { full: true })
        .await
        .expect("build index");

    let result = derrick_specify::grounding::gather(wd, "export widget payload").await;
    assert!(result.front_matter.index_fresh, "index present => fresh");
    let joined = result.front_matter.symbols.join("\n");
    assert!(
        joined.contains("src/lib.rs:1") && joined.contains("export_widget_payload"),
        "real path:line + identifier present, got: {joined}"
    );
    assert!(
        !joined.contains("definitely_not_indexed_symbol"),
        "names that are not in the index must not appear"
    );
}

#[tokio::test]
async fn grounding_degrades_without_index() {
    let dir = tempfile::tempdir().expect("tempdir");
    let result = derrick_specify::grounding::gather(dir.path(), "anything").await;
    assert!(!result.front_matter.index_fresh);
    assert!(result.front_matter.symbols.is_empty());
    assert!(result.context_block.contains("Do not invent"));
}

// --- caveman handoff savings ------------------------------------------------

#[tokio::test]
async fn plan_handoff_caveman_saves_and_protects_tokens() {
    let dir = tempfile::tempdir().expect("tempdir");
    let wd = dir.path();
    let config = full_config(wd);
    let prompt = "Add a widget export command";
    let feature_dir = prescaffold(wd, prompt);

    // Write a spec rich in prose (so caveman has filler to strip) but carrying a
    // protected path:line token that must survive compression byte-for-byte.
    let spec_md = format!(
        "---\nschema: derrick.spec/v1\nslug: x\nintent: i\nrequirements:\n  - id: R1\n    must: m\nacceptance:\n  - id: A1\n    check: c\nnon_goals: []\nopen_questions: []\n---\n# Title\n\n## Context\n{}\nThe relevant code is at src/lib.rs:42 export_widget.\n\n## Requirements\nR1\n\n## Acceptance Criteria\nA1\n\n## Out of Scope\nNothing.\n",
        "This is basically a really verbose and obviously redundant paragraph that clearly contains filler. ".repeat(8)
    );
    std::fs::write(wd.join(&feature_dir).join("spec.md"), &spec_md).expect("spec");

    let (host, calls) = StubHost::new(false);
    let hosts = registry(host);
    let provider = NativeSpecProvider::new();
    let req = NativeRequest {
        raw_prompt: prompt,
        repo_root: wd,
        working_dir: wd,
        hosts: &hosts,
        config: &config,
        interactive: false,
        feature_dir: &feature_dir,
    };
    let outcome = provider.plan(&req, None).await.expect("plan");
    assert!(outcome.bytes_raw > 0, "raw bytes recorded");
    assert!(
        outcome.bytes_saved > 0,
        "caveman saved bytes on the handoff context"
    );

    // Compression must NOT clobber protected tokens: the `path:line identifier`
    // span must survive byte-for-byte in the ACTUAL plan prompt the host saw.
    // Capturing from the call log (not just the byte counters) means a future
    // regression that drops protected tokens fails this test.
    let plan_prompt = calls
        .lock()
        .expect("lock")
        .iter()
        .find(|p| p.contains("implementation plan"))
        .cloned()
        .expect("a plan prompt was sent to the host");
    assert!(
        plan_prompt.contains("src/lib.rs:42 export_widget"),
        "protected path:line token must survive caveman compression in the handoff prompt"
    );
}
