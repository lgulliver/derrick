//! Brownfield-safe init for existing repositories.
//!
//! The crate implements the T011 adoption phases from DESIGN.md §5.2, §5.6,
//! D29, and D34: detect existing project context, produce a deterministic
//! proposal, then apply the reviewed filesystem writes.

#![allow(clippy::pedantic)]

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::Utc;
use derrick_config::{render_init_template, Config, InitTemplateVars, SubstrateMode};
use derrick_models::{resolve_role, AuthStore, CompletionRequest};
use derrick_substrate_native::{NativeConfig, NativeSubstrate};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use thiserror::Error;
use uuid::Uuid;

const INIT_TEMPLATE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../templates/derrick.yaml.in"
));
const CONSTITUTION_STUB_TEMPLATE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../templates/constitution.md.in"
));
const CODEX_INSTRUCTIONS_TEMPLATE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../templates/codex-instructions.md"
));
/// The `/speckit.constitution` Claude command shim. Written into target repos
/// during `derrick init` so the constitution can be generated interactively.
pub const SPECKIT_CONSTITUTION_SHIM: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../.claude/commands/speckit.constitution.md"
));
const PRE_TOOL_TEMPLATE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../templates/hooks/claude-pre-tool-use.json"
));
const POST_TOOL_TEMPLATE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../templates/hooks/claude-post-tool-use.json"
));

const DERRICK_BLOCK_START: &str = "<!-- derrick:start -->";
const DERRICK_BLOCK_END: &str = "<!-- derrick:end -->";
const DRAFT_BANNER_PREFIX: &str = "<!-- DERRICK-DRAFT:";
const CLAUDE_MATCHERS: [&str; 6] = ["Bash", "Read", "Write", "Edit", "Glob", "Grep"];
const COMMAND_NAMES: [&str; 10] = [
    "add-feature.md",
    "derrick-status.md",
    "derrick-doctor.md",
    "derrick-resume.md",
    "speckit.specify.md",
    "speckit.clarify.md",
    "speckit.plan.md",
    "speckit.analyze.md",
    "speckit.tasks.md",
    "speckit.constitution.md",
];
const AGENT_NAMES: [&str; 2] = ["foreman.md", "hand-copilot.md"];
const CONSTITUTION_CANDIDATES: [&str; 8] = [
    ".specify/memory/constitution.md",
    "CONSTITUTION.md",
    "PRINCIPLES.md",
    "STYLE.md",
    "RULES.md",
    "CONTRIBUTING.md",
    "docs/constitution.md",
    "docs/principles.md",
];

/// Brownfield-safe adopter bound to a repository root.
#[derive(Clone, Debug)]
pub struct Adopter {
    repo_root: PathBuf,
}

impl Adopter {
    /// Creates an adopter rooted at a git repository.
    pub fn new(repo_root: impl Into<PathBuf>) -> Self {
        Self {
            repo_root: repo_root.into(),
        }
    }

    /// Walks the repository and returns existing adoption context.
    ///
    /// This phase performs only local reads and PATH availability checks.
    pub fn detect(&self) -> Result<DetectionReport, AdoptError> {
        let mut report = DetectionReport {
            git_repo: self.repo_root.join(".git").exists(),
            ..DetectionReport::default()
        };

        report.agents_md = self.relative_if_file("AGENTS.md");
        report.claude_md = self.relative_if_file("CLAUDE.md");
        report.claude_dir = self.relative_if_dir(".claude");
        report.claude_settings = self.relative_if_file(".claude/settings.json");
        report.codex_dir = self.relative_if_dir(".codex");
        report.codex_instructions = self.relative_if_file(".codex/instructions.md");
        report.codex_config = self
            .relative_if_file(".codex/config.toml")
            .or_else(|| self.relative_if_file(".codex/settings.json"));
        report.github_copilot_instructions =
            self.relative_if_file(".github/copilot-instructions.md");
        report.codeowners = self
            .relative_if_file("CODEOWNERS")
            .or_else(|| self.relative_if_file(".github/CODEOWNERS"));
        report.specify_dir = self.relative_if_dir(".specify");
        report.specify_extensions_derrick = self.relative_if_dir(".specify/extensions/derrick");
        report.existing_derrick_yaml = self.relative_if_file("derrick.yaml");
        report.existing_derrick_dir = self.relative_if_dir(".derrick");
        report.readme = self
            .relative_if_file("README.md")
            .or_else(|| self.relative_if_file("README"));
        report.contributing = self.relative_if_file("CONTRIBUTING.md");
        report.adrs_dir = self
            .relative_if_dir("docs/adrs")
            .or_else(|| self.relative_if_dir("docs/adr"));

        report.claude_agents = self.find_files(".claude/agents", Some("md"))?;
        report.claude_commands = self.find_files(".claude/commands", Some("md"))?;
        report.claude_skills = self.find_skill_files(".claude/skills")?;

        for candidate in CONSTITUTION_CANDIDATES {
            if let Some(path) = self.relative_if_file(candidate) {
                report.constitution_candidates.push(path);
            }
        }
        report.constitution = report.constitution_candidates.first().cloned();

        report.speckit_cli_available = which::which("specify").is_ok();
        report.claude_cli_available = which::which("claude").is_ok();
        report.codex_cli_available = which::which("codex").is_ok();
        report.gh_cli_available = which::which("gh").is_ok();

        self.capture_known_contents(&mut report)?;
        report.tracker_prefixes = tracker_prefixes(&report.file_contents);
        report.sort();
        Ok(report)
    }

    /// Drafts a banner-protected constitution from detected docs.
    ///
    /// This is the only `Adopter` method that calls a model provider.
    pub async fn draft_constitution(
        &self,
        report: &DetectionReport,
        opts: &AdoptOptions,
    ) -> Result<String, AdoptError> {
        if opts.constitution != ConstitutionMode::FromDocs {
            return Err(AdoptError::InvalidOptions(
                "draft_constitution requires ConstitutionMode::FromDocs".to_owned(),
            ));
        }

        let config = Config::load_from_path(&self.repo_root.join("derrick.yaml"))?;
        let model = resolve_role(
            "proposer",
            config.roles(),
            config.models(),
            &AuthStore::from_env(),
        )
        .await?;
        let prompt = self.constitution_prompt(report)?;
        let response = model
            .complete(CompletionRequest {
                cached_prefix: None,
                prompt,
                system: Some(
                    "Draft a concise speckit constitution from the supplied repository docs."
                        .to_owned(),
                ),
                max_tokens: Some(2_000),
                temperature: Some(0.2),
                timeout: Duration::from_secs(30),
            })
            .await?;

        Ok(format!(
            "{DRAFT_BANNER_PREFIX} review, edit, and remove this banner before running plan. Generated from repository docs on {} by {}. -->\n\n{}",
            Utc::now().date_naive(),
            model.name(),
            response.text.trim()
        ))
    }

    /// Produces a deterministic adoption plan without touching the filesystem.
    pub fn propose(
        &self,
        detection: &DetectionReport,
        opts: &AdoptOptions,
        drafted_constitution: Option<&str>,
    ) -> Result<AdoptionPlan, AdoptError> {
        validate_prefix(&opts.site_prefix)?;

        let mut plan = AdoptionPlan::default();
        let mut blockers = BTreeSet::new();
        let mut warnings = BTreeSet::new();

        if !detection.git_repo {
            blockers.insert("derrick init must run inside a git repo".to_owned());
        }
        if detection.existing_derrick_yaml.is_some() && !opts.force {
            if let Some(path) = &detection.existing_derrick_yaml {
                blockers.insert(format!(
                    "{} already exists; pass --force or use `derrick adopt --merge` (future)",
                    path.display()
                ));
            }
        }
        for command in colliding_commands(&detection.claude_commands) {
            if !opts.force {
                blockers.insert(format!(
                    "existing Claude command {} would be overwritten",
                    command.display()
                ));
            }
        }
        if opts.constitution != ConstitutionMode::Reference {
            if let Some(path) = &detection.constitution {
                blockers.insert(format!(
                    "a constitution-like doc already exists at {}; constitution flags refuse to overwrite",
                    path.display()
                ));
            }
        }

        self.add_references(detection, &mut plan);
        self.add_core_writes(
            detection,
            opts,
            drafted_constitution,
            &mut plan,
            &mut blockers,
        )?;
        self.add_append_writes(detection, opts, &mut plan);
        self.add_commands_and_agents(detection, opts, &mut plan, &mut warnings);
        self.add_codex_instructions(detection, &mut plan);
        if !opts.no_hooks {
            self.add_hook_write(detection, opts, &mut plan, &mut blockers, &mut warnings)?;
        }
        self.add_warnings(detection, opts, &mut warnings);

        plan.blockers = blockers.into_iter().collect();
        plan.warnings = warnings.into_iter().collect();
        plan.sort();
        Ok(plan)
    }

    /// Applies a reviewed adoption plan using a stage-then-commit filesystem flow.
    pub async fn apply(&self, plan: &AdoptionPlan) -> Result<AdoptionOutcome, AdoptError> {
        if !plan.blockers.is_empty() {
            return Err(AdoptError::Blocked(plan.blockers.clone()));
        }
        self.preflight(plan)?;

        let state_dir = self.repo_root.join(".derrick");
        fs::create_dir_all(&state_dir).map_err(|source| AdoptError::Io {
            path: state_dir.clone(),
            source,
        })?;
        let stage_dir = state_dir.join(format!(".adopt-stage-{}", Uuid::new_v4()));
        // TODO(T012): Foreman's D32 cleanup loop must prune stale `.derrick/.adopt-stage-*`.
        fs::create_dir_all(&stage_dir).map_err(|source| AdoptError::Io {
            path: stage_dir.clone(),
            source,
        })?;

        for write in &plan.writes {
            let staged_path = stage_dir.join(&write.path);
            if let Some(parent) = staged_path.parent() {
                fs::create_dir_all(parent).map_err(|source| AdoptError::Io {
                    path: parent.to_path_buf(),
                    source,
                })?;
            }
            fs::write(&staged_path, &write.content).map_err(|source| AdoptError::Io {
                path: staged_path,
                source,
            })?;
        }

        let staged_config = stage_dir.join("derrick.yaml");
        if staged_config.exists() {
            Config::load_from_path(&staged_config)?;
        }

        let mut written = Vec::new();
        for write in &plan.writes {
            let staged_path = stage_dir.join(&write.path);
            let target = self.repo_root.join(&write.path);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|source| AdoptError::Io {
                    path: parent.to_path_buf(),
                    source,
                })?;
            }
            if let Err(_source) = fs::rename(&staged_path, &target) {
                return Ok(AdoptionOutcome {
                    written,
                    bookkeeping: vec![relative_path(&stage_dir, &self.repo_root)],
                    partial_failure: Some(PartialFailure {
                        staged_dir: relative_path(&stage_dir, &self.repo_root),
                        committed_paths: plan
                            .writes
                            .iter()
                            .filter(|candidate| self.repo_root.join(&candidate.path).exists())
                            .map(|candidate| candidate.path.clone())
                            .collect(),
                        recovery: "revert listed paths before retrying: git checkout -- <path>"
                            .to_owned(),
                    }),
                });
            }
            written.push(write.path.clone());
        }

        let config = Config::load_from_path(&self.repo_root.join("derrick.yaml"))?;
        let native_config = NativeConfig {
            db_path: self.repo_root.join(config.state().dir()).join("derrick.db"),
            worktree_root: self.repo_root.join(config.state().worktree_root()),
        };
        let substrate = NativeSubstrate::open(native_config, config.site().clone()).await?;
        substrate.close().await?;

        if plan.install_speckit_integration {
            if let Ok(status) = std::process::Command::new("specify")
                .args(["integration", "install", "claude"])
                .current_dir(&self.repo_root)
                .status()
            {
                if status.success() {
                    let skills_dir = self.repo_root.join(".claude/skills");
                    if let Ok(entries) = fs::read_dir(&skills_dir) {
                        for entry in entries.flatten() {
                            let skill = entry.path().join("SKILL.md");
                            if skill.is_file() {
                                written.push(relative_path(&skill, &self.repo_root));
                            }
                        }
                    }
                }
            }
        }

        let mut bookkeeping = vec![
            PathBuf::from(".derrick/derrick.db"),
            relative_path(&stage_dir, &self.repo_root),
        ];
        let state_path = self.append_history(plan, &written)?;
        bookkeeping.push(state_path);

        Ok(AdoptionOutcome {
            written,
            bookkeeping,
            partial_failure: None,
        })
    }

    fn relative_if_file(&self, relative: impl AsRef<Path>) -> Option<PathBuf> {
        let relative = relative.as_ref();
        self.repo_root
            .join(relative)
            .is_file()
            .then(|| relative.to_path_buf())
    }

    fn relative_if_dir(&self, relative: impl AsRef<Path>) -> Option<PathBuf> {
        let relative = relative.as_ref();
        self.repo_root
            .join(relative)
            .is_dir()
            .then(|| relative.to_path_buf())
    }

    fn find_files(
        &self,
        relative_dir: impl AsRef<Path>,
        extension: Option<&str>,
    ) -> Result<Vec<PathBuf>, AdoptError> {
        let relative_dir = relative_dir.as_ref();
        let absolute = self.repo_root.join(relative_dir);
        if !absolute.is_dir() {
            return Ok(Vec::new());
        }
        let mut files = Vec::new();
        for entry in fs::read_dir(&absolute).map_err(|source| AdoptError::Io {
            path: absolute.clone(),
            source,
        })? {
            let entry = entry.map_err(|source| AdoptError::Io {
                path: absolute.clone(),
                source,
            })?;
            let path = entry.path();
            if path.is_file()
                && extension.map_or(true, |expected| {
                    path.extension() == Some(OsStr::new(expected))
                })
            {
                files.push(relative_path(&path, &self.repo_root));
            }
        }
        files.sort();
        Ok(files)
    }

    fn find_skill_files(&self, relative_dir: impl AsRef<Path>) -> Result<Vec<PathBuf>, AdoptError> {
        let relative_dir = relative_dir.as_ref();
        let absolute = self.repo_root.join(relative_dir);
        if !absolute.is_dir() {
            return Ok(Vec::new());
        }
        let mut files = Vec::new();
        for entry in fs::read_dir(&absolute).map_err(|source| AdoptError::Io {
            path: absolute.clone(),
            source,
        })? {
            let entry = entry.map_err(|source| AdoptError::Io {
                path: absolute.clone(),
                source,
            })?;
            let skill = entry.path().join("SKILL.md");
            if skill.is_file() {
                files.push(relative_path(&skill, &self.repo_root));
            }
        }
        files.sort();
        Ok(files)
    }

    fn capture_known_contents(&self, report: &mut DetectionReport) -> Result<(), AdoptError> {
        let mut paths = BTreeSet::new();
        for path in [
            &report.agents_md,
            &report.claude_md,
            &report.claude_settings,
            &report.codex_instructions,
            &report.readme,
            &report.contributing,
        ]
        .into_iter()
        .flatten()
        {
            paths.insert(path.clone());
        }
        if let Some(adrs_dir) = &report.adrs_dir {
            for path in self.find_files(adrs_dir, Some("md"))? {
                paths.insert(path);
            }
        }

        for path in paths {
            let absolute = self.repo_root.join(&path);
            let contents = fs::read_to_string(&absolute).map_err(|source| AdoptError::Io {
                path: absolute,
                source,
            })?;
            report.file_contents.insert(path, contents);
        }
        Ok(())
    }

    fn constitution_prompt(&self, report: &DetectionReport) -> Result<String, AdoptError> {
        let mut sections = BTreeMap::new();
        for path in [&report.readme, &report.contributing].into_iter().flatten() {
            if let Some(contents) = report.file_contents.get(path) {
                sections.insert(path.clone(), contents.clone());
            }
        }
        if let Some(adrs_dir) = &report.adrs_dir {
            for path in self.find_files(adrs_dir, Some("md"))? {
                if let Some(contents) = report.file_contents.get(&path) {
                    sections.insert(path, contents.clone());
                } else {
                    let absolute = self.repo_root.join(&path);
                    let contents =
                        fs::read_to_string(&absolute).map_err(|source| AdoptError::Io {
                            path: absolute,
                            source,
                        })?;
                    sections.insert(path, contents);
                }
            }
        }
        let mut prompt = String::from("Draft a speckit constitution from these repository docs.\n");
        for (path, contents) in sections {
            prompt.push_str("\n--- ");
            prompt.push_str(&path.display().to_string());
            prompt.push_str(" ---\n");
            prompt.push_str(&contents);
            prompt.push('\n');
        }
        Ok(prompt)
    }

    fn add_references(&self, detection: &DetectionReport, plan: &mut AdoptionPlan) {
        for (path, field, rationale) in [
            (
                &detection.agents_md,
                "guardrails.agents_md",
                "existing agent contract is referenced, not overwritten",
            ),
            (
                &detection.claude_md,
                "guardrails.claude_md",
                "existing Claude instructions are referenced, not overwritten",
            ),
            (
                &detection.constitution,
                "guardrails.constitution_path",
                "existing constitution-like doc is referenced",
            ),
            (
                &detection.codeowners,
                "guardrails.codeowners",
                "existing CODEOWNERS is referenced",
            ),
        ] {
            if let Some(path) = path {
                plan.references.push(PlannedReference {
                    path: path.clone(),
                    as_field: (*field).to_owned(),
                    rationale: (*rationale).to_owned(),
                });
            }
        }
    }

    fn add_core_writes(
        &self,
        detection: &DetectionReport,
        opts: &AdoptOptions,
        drafted_constitution: Option<&str>,
        plan: &mut AdoptionPlan,
        blockers: &mut BTreeSet<String>,
    ) -> Result<(), AdoptError> {
        let constitution_path = detection
            .constitution
            .as_deref()
            .unwrap_or_else(|| Path::new(".specify/memory/constitution.md"));
        plan.writes.push(PlannedWrite {
            path: PathBuf::from("derrick.yaml"),
            content: render_derrick_yaml(opts, constitution_path, detection),
            mode: WriteMode::Create,
            rationale: "site configuration for derrick".to_owned(),
        });
        plan.writes.push(PlannedWrite {
            path: PathBuf::from(".derrick/.gitignore"),
            content: "runs/\nstate.json\nderrick.db*\nworktrees/\n.adopt-stage-*/\n".to_owned(),
            mode: WriteMode::Create,
            rationale: "keep local derrick state out of git".to_owned(),
        });

        match opts.constitution {
            ConstitutionMode::Reference => {}
            ConstitutionMode::Stub => {
                if detection.constitution.is_none() {
                    if detection.speckit_cli_available {
                        blockers.insert(
                            "`--constitution-stub` refused because `specify` is available; run `/speckit.constitution` instead"
                                .to_owned(),
                        );
                    } else {
                        plan.writes.push(PlannedWrite {
                            path: PathBuf::from(".specify/memory/constitution.md"),
                            content: CONSTITUTION_STUB_TEMPLATE.to_owned(),
                            mode: WriteMode::Create,
                            rationale: "opt-in banner-protected constitution stub".to_owned(),
                        });
                    }
                }
            }
            ConstitutionMode::FromDocs => {
                if detection.constitution.is_none() {
                    if detection.speckit_cli_available {
                        blockers.insert(
                            "`--constitution-from-docs` refused because `specify` is available; run `/speckit.constitution` instead"
                                .to_owned(),
                        );
                    } else if let Some(draft) = drafted_constitution {
                        plan.writes.push(PlannedWrite {
                            path: PathBuf::from(".specify/memory/constitution.md"),
                            content: draft.to_owned(),
                            mode: WriteMode::Create,
                            rationale: "opt-in banner-protected constitution draft".to_owned(),
                        });
                    } else {
                        return Err(AdoptError::InvalidOptions(
                            "ConstitutionMode::FromDocs requires drafted_constitution".to_owned(),
                        ));
                    }
                }
            }
        }

        plan.writes.push(PlannedWrite {
            path: PathBuf::from(".specify/extensions/derrick/scripts/tasks-to-tickets.sh"),
            content:
                "#!/usr/bin/env bash\nset -euo pipefail\n\nderrick bridge tasks-to-tickets \"$@\"\n"
                    .to_owned(),
            mode: WriteMode::Create,
            rationale: "speckit bridge from tasks to derrick tickets".to_owned(),
        });
        Ok(())
    }

    fn add_append_writes(
        &self,
        detection: &DetectionReport,
        opts: &AdoptOptions,
        plan: &mut AdoptionPlan,
    ) {
        if !opts.append_agents_md {
            return;
        }
        for path in [&detection.agents_md, &detection.claude_md]
            .into_iter()
            .flatten()
        {
            let existing = detection
                .file_contents
                .get(path)
                .cloned()
                .unwrap_or_default();
            plan.writes.push(PlannedWrite {
                path: path.clone(),
                content: replace_or_append_block(&existing, agent_block()),
                mode: WriteMode::Append,
                rationale: "append derrick usage block while preserving existing instructions"
                    .to_owned(),
            });
        }
    }

    fn add_commands_and_agents(
        &self,
        detection: &DetectionReport,
        opts: &AdoptOptions,
        plan: &mut AdoptionPlan,
        warnings: &mut BTreeSet<String>,
    ) {
        let colliding: BTreeSet<PathBuf> = detection.claude_commands.iter().cloned().collect();
        for command in COMMAND_NAMES {
            // When speckit is installed, its own integration provides better versions of
            // these commands as Claude Code skills. Skip derrick's fallback shims.
            if detection.speckit_cli_available && command.starts_with("speckit.") {
                continue;
            }
            let path = PathBuf::from(".claude/commands").join(command);
            if opts.force || !colliding.contains(&path) {
                plan.writes.push(PlannedWrite {
                    path,
                    content: command_template(command),
                    mode: WriteMode::Create,
                    rationale: "derrick Claude Code command".to_owned(),
                });
            }
        }
        plan.install_speckit_integration = detection.speckit_cli_available;

        let colliding_agents: BTreeSet<String> = detection
            .claude_agents
            .iter()
            .filter_map(|path| path.file_name().and_then(OsStr::to_str).map(str::to_owned))
            .collect();
        for agent in AGENT_NAMES {
            if colliding_agents.contains(agent) {
                warnings.insert(format!(
                    "skipped existing Claude agent .claude/agents/{agent}; user file is authoritative"
                ));
                continue;
            }
            plan.writes.push(PlannedWrite {
                path: PathBuf::from(".claude/agents").join(agent),
                content: agent_template(agent),
                mode: WriteMode::Create,
                rationale: "derrick Claude Code specialist stub".to_owned(),
            });
        }
    }

    fn add_codex_instructions(&self, detection: &DetectionReport, plan: &mut AdoptionPlan) {
        let path = PathBuf::from(".codex/instructions.md");
        let existing = detection
            .file_contents
            .get(&path)
            .cloned()
            .unwrap_or_default();
        let content = if existing.is_empty() {
            CODEX_INSTRUCTIONS_TEMPLATE.to_owned()
        } else {
            replace_or_append_block(&existing, CODEX_INSTRUCTIONS_TEMPLATE)
        };
        plan.writes.push(PlannedWrite {
            path,
            content,
            mode: WriteMode::Append,
            rationale: "Codex host context reference; hooks deferred per D34".to_owned(),
        });
    }

    fn add_hook_write(
        &self,
        detection: &DetectionReport,
        opts: &AdoptOptions,
        plan: &mut AdoptionPlan,
        blockers: &mut BTreeSet<String>,
        warnings: &mut BTreeSet<String>,
    ) -> Result<(), AdoptError> {
        let (content, hook_blockers, hook_warnings) = render_settings_json(detection, opts.force)?;
        blockers.extend(hook_blockers);
        warnings.extend(hook_warnings);
        plan.writes.push(PlannedWrite {
            path: PathBuf::from(".claude/settings.json"),
            content,
            mode: WriteMode::MergeJson,
            rationale: "Claude Code D29 scrub and caveman hooks".to_owned(),
        });
        Ok(())
    }

    fn add_warnings(
        &self,
        detection: &DetectionReport,
        opts: &AdoptOptions,
        warnings: &mut BTreeSet<String>,
    ) {
        if !detection.speckit_cli_available {
            warnings.insert(
                "speckit (`specify`) not found; derrick wrote fallback shims to `.claude/commands/`. \
                 Install speckit for richer skills: `uv tool install specify-cli`, then re-run `derrick init`."
                    .to_owned(),
            );
        }
        if detection.specify_extensions_derrick.is_some() {
            warnings.insert(
                "existing `.specify/extensions/derrick/` will be merged file-by-file; review the diff before committing."
                    .to_owned(),
            );
        }
        if !detection.tracker_prefixes.is_empty() {
            warnings.insert(format!(
                "detected tracker prefixes {}; v1 only ships the native substrate, no external-tracker adoption.",
                detection.tracker_prefixes.join(", ")
            ));
        }
        if opts.constitution == ConstitutionMode::FromDocs {
            warnings.insert(
                "the constitution draft is unreviewed LLM prose; `plan` will refuse to run until you remove the banner."
                    .to_owned(),
            );
        }
        if matches!(opts.mode, SubstrateMode::Copilot | SubstrateMode::Crew) {
            warnings.insert(
                "Codex host hook installation is deferred; Codex tool I/O is not scrubbed in v1. See D34."
                    .to_owned(),
            );
        }
    }

    fn preflight(&self, plan: &AdoptionPlan) -> Result<(), AdoptError> {
        for write in &plan.writes {
            if matches!(write.mode, WriteMode::MergeJson) {
                let _: Value = serde_json::from_str(&write.content)?;
            }
            if write.path == Path::new("derrick.yaml") {
                let _: serde_yaml::Value = serde_yaml::from_str(&write.content)?;
            }
        }
        Ok(())
    }

    fn append_history(
        &self,
        plan: &AdoptionPlan,
        written: &[PathBuf],
    ) -> Result<PathBuf, AdoptError> {
        let path = self.repo_root.join(".derrick/state.json");
        let mut state = if path.exists() {
            let contents = fs::read_to_string(&path).map_err(|source| AdoptError::Io {
                path: path.clone(),
                source,
            })?;
            serde_json::from_str::<AdoptionState>(&contents)?
        } else {
            AdoptionState::default()
        };
        state.adoption_history.push(AdoptionHistoryEntry {
            timestamp: Utc::now().to_rfc3339(),
            written: written.to_vec(),
            references: plan
                .references
                .iter()
                .map(|reference| reference.path.clone())
                .collect(),
        });
        let contents = serde_json::to_string_pretty(&state)?;
        fs::write(&path, contents).map_err(|source| AdoptError::Io {
            path: path.clone(),
            source,
        })?;
        Ok(PathBuf::from(".derrick/state.json"))
    }
}

/// Existing repository context discovered by `Adopter::detect`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DetectionReport {
    /// Whether `.git/` exists at the repository root.
    pub git_repo: bool,
    /// Existing `AGENTS.md`.
    pub agents_md: Option<PathBuf>,
    /// Existing `CLAUDE.md`.
    pub claude_md: Option<PathBuf>,
    /// Existing `.claude/`.
    pub claude_dir: Option<PathBuf>,
    /// Existing `.claude/settings.json`.
    pub claude_settings: Option<PathBuf>,
    /// Existing `.claude/agents/*.md`.
    pub claude_agents: Vec<PathBuf>,
    /// Existing `.claude/commands/*.md`.
    pub claude_commands: Vec<PathBuf>,
    /// Existing `.claude/skills/*/SKILL.md`.
    pub claude_skills: Vec<PathBuf>,
    /// Existing `.codex/`.
    pub codex_dir: Option<PathBuf>,
    /// Existing `.codex/instructions.md`.
    pub codex_instructions: Option<PathBuf>,
    /// Existing Codex config.
    pub codex_config: Option<PathBuf>,
    /// Existing GitHub Copilot instructions.
    pub github_copilot_instructions: Option<PathBuf>,
    /// Existing CODEOWNERS.
    pub codeowners: Option<PathBuf>,
    /// Existing `.specify/`.
    pub specify_dir: Option<PathBuf>,
    /// Existing `.specify/extensions/derrick/`.
    pub specify_extensions_derrick: Option<PathBuf>,
    /// First constitution-like document.
    pub constitution: Option<PathBuf>,
    /// Constitution-like docs in canonical priority order.
    pub constitution_candidates: Vec<PathBuf>,
    /// Existing `derrick.yaml`.
    pub existing_derrick_yaml: Option<PathBuf>,
    /// Existing `.derrick/`.
    pub existing_derrick_dir: Option<PathBuf>,
    /// Whether `specify` is on PATH.
    pub speckit_cli_available: bool,
    /// Whether `claude` is on PATH.
    pub claude_cli_available: bool,
    /// Whether `codex` is on PATH.
    pub codex_cli_available: bool,
    /// Whether `gh` is on PATH.
    pub gh_cli_available: bool,
    /// Existing README.
    pub readme: Option<PathBuf>,
    /// Existing `CONTRIBUTING.md`.
    pub contributing: Option<PathBuf>,
    /// Existing ADR directory.
    pub adrs_dir: Option<PathBuf>,
    /// Tracker prefixes scraped from local instruction files.
    pub tracker_prefixes: Vec<String>,
    /// Contents needed by pure proposal rendering.
    pub file_contents: BTreeMap<PathBuf, String>,
}

impl DetectionReport {
    fn sort(&mut self) {
        self.claude_agents.sort();
        self.claude_commands.sort();
        self.claude_skills.sort();
        self.constitution_candidates.sort();
        self.tracker_prefixes.sort();
        self.tracker_prefixes.dedup();
    }
}

/// User-selected adoption options.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdoptOptions {
    /// Site name for `derrick.yaml`.
    pub site_name: String,
    /// Ticket prefix, matching `^[a-z]{1,6}$`.
    pub site_prefix: String,
    /// Substrate operating mode.
    pub mode: SubstrateMode,
    /// Allow overwriting derrick-owned paths and force-prepending hook conflicts.
    pub force: bool,
    /// Skip D29 Claude Code hooks.
    pub no_hooks: bool,
    /// Append derrick blocks to `AGENTS.md` and `CLAUDE.md`.
    pub append_agents_md: bool,
    /// Constitution handling.
    pub constitution: ConstitutionMode,
}

/// Constitution handling mode.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub enum ConstitutionMode {
    /// Reference an existing constitution-like doc, write none otherwise.
    #[default]
    Reference,
    /// Write a minimal banner stub when speckit is unavailable.
    Stub,
    /// Write a bannered LLM draft supplied to `propose`.
    FromDocs,
}

/// Deterministic plan produced by `Adopter::propose`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AdoptionPlan {
    /// Files derrick will write.
    pub writes: Vec<PlannedWrite>,
    /// Existing files derrick will reference without touching.
    pub references: Vec<PlannedReference>,
    /// Non-fatal warnings.
    pub warnings: Vec<String>,
    /// Fatal blockers.
    pub blockers: Vec<String>,
    /// When true, `apply` runs `specify integration install claude` after writing files.
    pub install_speckit_integration: bool,
}

impl AdoptionPlan {
    fn sort(&mut self) {
        self.references.sort_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then(left.as_field.cmp(&right.as_field))
        });
    }
}

/// A planned filesystem write.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlannedWrite {
    /// Relative target path.
    pub path: PathBuf,
    /// Full content to write.
    pub content: String,
    /// Write strategy.
    pub mode: WriteMode,
    /// Why this write is needed.
    pub rationale: String,
}

/// Write strategy for a planned write.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WriteMode {
    /// Create or replace a derrick-owned file.
    Create,
    /// Append or replace a derrick-marked block.
    Append,
    /// Merge a JSON file.
    MergeJson,
}

/// A read-only reference in the adoption plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlannedReference {
    /// Relative referenced path.
    pub path: PathBuf,
    /// Logical field name.
    pub as_field: String,
    /// Why this reference is used.
    pub rationale: String,
}

/// Outcome from applying an adoption plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdoptionOutcome {
    /// Planned paths successfully written.
    pub written: Vec<PathBuf>,
    /// Derrick bookkeeping paths touched by apply.
    pub bookkeeping: Vec<PathBuf>,
    /// Partial failure details, if promotion stopped mid-commit.
    pub partial_failure: Option<PartialFailure>,
}

/// Details for a partial apply failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PartialFailure {
    /// Staging directory kept for inspection.
    pub staged_dir: PathBuf,
    /// Paths committed before failure.
    pub committed_paths: Vec<PathBuf>,
    /// Human recovery instruction.
    pub recovery: String,
}

/// Errors returned by adoption phases.
#[derive(Debug, Error)]
pub enum AdoptError {
    /// Local filesystem error.
    #[error("IO error at {path}: {source}")]
    Io {
        /// Path being accessed.
        path: PathBuf,
        /// Source error.
        source: io::Error,
    },
    /// Invalid options.
    #[error("{0}")]
    InvalidOptions(String),
    /// Proposal has blockers.
    #[error("adoption blocked: {}", .0.join("; "))]
    Blocked(Vec<String>),
    /// Config parsing or validation failed.
    #[error("{0}")]
    Config(#[from] derrick_config::ConfigError),
    /// Substrate open failed.
    #[error("{0}")]
    Substrate(#[from] derrick_substrate::SubstrateError),
    /// Model call failed.
    #[error("{0}")]
    Model(#[from] derrick_models::ModelError),
    /// JSON parsing failed.
    #[error("{0}")]
    Json(#[from] serde_json::Error),
    /// YAML parsing failed.
    #[error("{0}")]
    Yaml(#[from] serde_yaml::Error),
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct AdoptionState {
    adoption_history: Vec<AdoptionHistoryEntry>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct AdoptionHistoryEntry {
    timestamp: String,
    written: Vec<PathBuf>,
    references: Vec<PathBuf>,
}

fn render_derrick_yaml(
    opts: &AdoptOptions,
    constitution_path: &Path,
    detection: &DetectionReport,
) -> String {
    let mut rendered = render_init_template(
        INIT_TEMPLATE,
        InitTemplateVars {
            site_name: &opts.site_name,
            prefix: &opts.site_prefix,
            mode: substrate_mode_name(opts.mode),
        },
    );
    rendered = rendered.replace(
        "constitution_path: .specify/memory/constitution.md",
        &format!("constitution_path: {}", constitution_path.display()),
    );
    let mut comments = Vec::new();
    if let Some(path) = &detection.agents_md {
        comments.push(format!("# guardrails.agents_md: {}", path.display()));
    }
    if let Some(path) = &detection.claude_md {
        comments.push(format!("# guardrails.claude_md: {}", path.display()));
    }
    if let Some(path) = &detection.codeowners {
        comments.push(format!("# guardrails.codeowners: {}", path.display()));
    }
    if !comments.is_empty() {
        rendered.push_str("\n# derrick-adopt references existing brownfield files:\n");
        rendered.push_str(&comments.join("\n"));
        rendered.push('\n');
    }
    rendered
}

fn substrate_mode_name(mode: SubstrateMode) -> &'static str {
    match mode {
        SubstrateMode::Solo => "solo",
        SubstrateMode::Copilot => "copilot",
        SubstrateMode::Crew => "crew",
    }
}

fn render_settings_json(
    detection: &DetectionReport,
    force: bool,
) -> Result<(String, Vec<String>, Vec<String>), AdoptError> {
    let mut root = if let Some(path) = &detection.claude_settings {
        match detection.file_contents.get(path) {
            Some(contents) => serde_json::from_str::<Value>(contents).map_err(|error| {
                AdoptError::InvalidOptions(format!("{} is corrupt JSON: {error}", path.display()))
            })?,
            None => json!({}),
        }
    } else {
        json!({})
    };
    if !root.is_object() {
        return Err(AdoptError::InvalidOptions(
            ".claude/settings.json must contain a JSON object".to_owned(),
        ));
    }

    let hooks = root
        .as_object_mut()
        .and_then(|object| {
            object
                .entry("hooks")
                .or_insert_with(|| json!({}))
                .as_object_mut()
        })
        .ok_or_else(|| {
            AdoptError::InvalidOptions(".claude/settings.json hooks must be an object".to_owned())
        })?;

    let mut blockers = Vec::new();
    let mut warnings = Vec::new();
    merge_stage_hooks(
        hooks,
        "PreToolUse",
        "derrick:scrub",
        force,
        &mut blockers,
        &mut warnings,
    );
    merge_stage_hooks(
        hooks,
        "PostToolUse",
        "derrick:caveman",
        force,
        &mut blockers,
        &mut warnings,
    );
    let content = serde_json::to_string_pretty(&root)?;
    Ok((format!("{content}\n"), blockers, warnings))
}

fn merge_stage_hooks(
    hooks: &mut serde_json::Map<String, Value>,
    stage: &str,
    marker: &str,
    force: bool,
    blockers: &mut Vec<String>,
    warnings: &mut Vec<String>,
) {
    let existing = hooks.entry(stage).or_insert_with(|| json!([]));
    if !existing.is_array() {
        blockers.push(format!(".claude/settings.json {stage} must be an array"));
        return;
    }
    let Some(array) = existing.as_array_mut() else {
        blockers.push(format!(".claude/settings.json {stage} must be an array"));
        return;
    };

    let old_entries = std::mem::take(array);
    let mut by_matcher: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    let mut passthrough = Vec::new();
    for entry in old_entries {
        let matcher = entry
            .get("matcher")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if let Some(matcher) = matcher {
            by_matcher.entry(matcher).or_default().push(entry);
        } else {
            passthrough.push(entry);
        }
    }

    for matcher in CLAUDE_MATCHERS {
        let desired = hook_entry(stage, matcher);
        let entries = by_matcher.remove(matcher).unwrap_or_default();
        let unmarked: Vec<Value> = entries
            .into_iter()
            .filter(|entry| !entry_has_marker(entry, marker))
            .collect();
        if !unmarked.is_empty() && !force {
            blockers.push(format!(
                "`.claude/settings.json` {stage} already has an entry on matcher `{matcher}`; pass --force to prepend derrick's hook before it, or remove the conflicting entry first."
            ));
            array.push(desired);
            array.extend(unmarked);
            continue;
        }
        if !unmarked.is_empty() {
            warnings.push(format!(
                "force-prepended derrick {stage} hook before existing matcher `{matcher}` entry"
            ));
        }
        array.push(desired);
        array.extend(unmarked);
    }

    for (_matcher, entries) in by_matcher {
        array.extend(entries);
    }
    array.extend(passthrough);
}

fn hook_entry(stage: &str, matcher: &str) -> Value {
    let tool = matcher.to_ascii_lowercase();
    let template = if stage == "PreToolUse" {
        PRE_TOOL_TEMPLATE
    } else {
        POST_TOOL_TEMPLATE
    };
    let rendered = template
        .replace("{{matcher}}", matcher)
        .replace("{{tool}}", &tool);
    serde_json::from_str(&rendered).unwrap_or_else(|_| json!({ "matcher": matcher, "hooks": [] }))
}

fn entry_has_marker(entry: &Value, marker: &str) -> bool {
    entry
        .get("hooks")
        .and_then(Value::as_array)
        .map(|hooks| {
            hooks.iter().any(|hook| {
                hook.get("description")
                    .and_then(Value::as_str)
                    .is_some_and(|description| description == marker)
            })
        })
        .unwrap_or(false)
}

fn colliding_commands(commands: &[PathBuf]) -> Vec<PathBuf> {
    let command_names: BTreeSet<&str> = COMMAND_NAMES.into_iter().collect();
    commands
        .iter()
        .filter(|path| {
            path.file_name()
                .and_then(OsStr::to_str)
                .is_some_and(|name| command_names.contains(name))
        })
        .cloned()
        .collect()
}

fn command_template(name: &str) -> String {
    match name {
        "add-feature.md" => {
            "# /add-feature\n\nRun `derrick run add-feature --prompt \"$ARGUMENTS\"`.\n"
        }
        "derrick-status.md" => "# /derrick-status\n\nRun `derrick status`.\n",
        "derrick-doctor.md" => "# /derrick-doctor\n\nRun `derrick doctor`.\n",
        "derrick-resume.md" => {
            "# /derrick-resume\n\nRun `derrick run add-feature --resume-from \"$ARGUMENTS\"`.\n"
        }
        "speckit.specify.md" => include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../.claude/commands/speckit.specify.md"
        )),
        "speckit.clarify.md" => include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../.claude/commands/speckit.clarify.md"
        )),
        "speckit.plan.md" => include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../.claude/commands/speckit.plan.md"
        )),
        "speckit.analyze.md" => include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../.claude/commands/speckit.analyze.md"
        )),
        "speckit.tasks.md" => include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../.claude/commands/speckit.tasks.md"
        )),
        "speckit.constitution.md" => include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../.claude/commands/speckit.constitution.md"
        )),
        _ => "# Derrick command\n",
    }
    .to_owned()
}

fn agent_template(name: &str) -> String {
    format!("# {name}\n\nRead AGENTS.md and DESIGN.md before doing derrick-managed work.\n")
}

fn agent_block() -> &'static str {
    "<!-- derrick:start -->\n\n## Derrick\n\nThis repository is initialized for derrick. Read `derrick.yaml` and keep existing project instructions authoritative.\n\n<!-- derrick:end -->\n"
}

/// Writes the derrick context block into `<repo_root>/.codex/instructions.md`.
///
/// - Creates `.codex/` if it does not exist.
/// - If the file is absent, writes the full template.
/// - If the file exists and already contains the derrick block, replaces it
///   in place (idempotent).
/// - If the file exists but has no derrick block, appends the block.
///
/// Called by the greenfield init path; the brownfield adopt path drives the
/// same write through [`AdoptionPlan`].
pub fn write_codex_instructions(repo_root: &Path) -> Result<(), AdoptError> {
    let dir = repo_root.join(".codex");
    fs::create_dir_all(&dir).map_err(|source| AdoptError::Io {
        path: dir.clone(),
        source,
    })?;
    let path = dir.join("instructions.md");
    let existing = if path.exists() {
        fs::read_to_string(&path).map_err(|source| AdoptError::Io {
            path: path.clone(),
            source,
        })?
    } else {
        String::new()
    };
    let content = if existing.is_empty() {
        CODEX_INSTRUCTIONS_TEMPLATE.to_owned()
    } else {
        replace_or_append_block(&existing, CODEX_INSTRUCTIONS_TEMPLATE)
    };
    fs::write(&path, content).map_err(|source| AdoptError::Io { path, source })?;
    Ok(())
}

/// Writes `.claude/settings.json` with derrick's scrub and caveman hooks.
///
/// - If the file does not exist, creates it from scratch.
/// - If the file exists, merges derrick's hooks in without clobbering user entries
///   (same semantics as the adopt path).
/// - `force` controls whether to prepend over conflicting hook entries.
///
/// Called by the greenfield init path; the brownfield adopt path drives the
/// same write through [`AdoptionPlan`].
pub fn write_claude_settings(repo_root: &Path, force: bool) -> Result<(), AdoptError> {
    let dir = repo_root.join(".claude");
    fs::create_dir_all(&dir).map_err(|source| AdoptError::Io {
        path: dir.clone(),
        source,
    })?;
    let path = dir.join("settings.json");
    let mut root = if path.exists() {
        let contents = fs::read_to_string(&path).map_err(|source| AdoptError::Io {
            path: path.clone(),
            source,
        })?;
        serde_json::from_str::<Value>(&contents).map_err(|error| {
            AdoptError::InvalidOptions(format!("{} is corrupt JSON: {error}", path.display()))
        })?
    } else {
        json!({})
    };
    if !root.is_object() {
        return Err(AdoptError::InvalidOptions(
            ".claude/settings.json must contain a JSON object".to_owned(),
        ));
    }

    let hooks = root
        .as_object_mut()
        .and_then(|object| {
            object
                .entry("hooks")
                .or_insert_with(|| json!({}))
                .as_object_mut()
        })
        .ok_or_else(|| {
            AdoptError::InvalidOptions(".claude/settings.json hooks must be an object".to_owned())
        })?;

    let mut blockers = Vec::new();
    let mut warnings = Vec::new();
    merge_stage_hooks(
        hooks,
        "PreToolUse",
        "derrick:scrub",
        force,
        &mut blockers,
        &mut warnings,
    );
    merge_stage_hooks(
        hooks,
        "PostToolUse",
        "derrick:caveman",
        force,
        &mut blockers,
        &mut warnings,
    );
    if !blockers.is_empty() {
        return Err(AdoptError::InvalidOptions(blockers.join("; ")));
    }
    let _ = warnings;
    let content = serde_json::to_string_pretty(&root)?;
    fs::write(&path, format!("{content}\n")).map_err(|source| AdoptError::Io { path, source })?;
    Ok(())
}

/// Writes derrick's Claude Code command files to `<repo_root>/.claude/commands/`.
///
/// When `specify` is on PATH, derrick's `speckit.*` shims are skipped and
/// `specify integration install claude` is run instead, installing the real
/// speckit skills to `.claude/skills/`.
///
/// Skips any command file that already exists so user-customised versions are
/// not clobbered. Pass `force = true` to overwrite existing files.
///
/// Called by the greenfield init path; the brownfield adopt path drives the
/// same writes through [`AdoptionPlan`].
pub fn write_claude_commands(repo_root: &Path, force: bool) -> Result<Vec<String>, AdoptError> {
    let dir = repo_root.join(".claude").join("commands");
    fs::create_dir_all(&dir).map_err(|source| AdoptError::Io {
        path: dir.clone(),
        source,
    })?;
    let speckit_available = which::which("specify").is_ok();
    let mut written = Vec::new();
    for name in COMMAND_NAMES {
        // When speckit is installed, its integration provides better versions of these commands.
        if speckit_available && name.starts_with("speckit.") {
            continue;
        }
        let path = dir.join(name);
        if path.exists() && !force {
            continue;
        }
        let content = command_template(name);
        fs::write(&path, content).map_err(|source| AdoptError::Io {
            path: path.clone(),
            source,
        })?;
        written.push(format!(".claude/commands/{name}"));
    }
    if speckit_available {
        // `specify init` creates .specify/ and installs the Claude integration in one shot.
        // Use it for greenfield repos; for projects where .specify/ already exists,
        // fall back to `specify integration install claude`.
        let specify_dir = repo_root.join(".specify");
        let args: &[&str] = if specify_dir.exists() {
            &["integration", "install", "claude"]
        } else {
            &[
                "init",
                "--here",
                "--integration",
                "claude",
                "--no-git",
                "--force",
            ]
        };
        if let Ok(status) = std::process::Command::new("specify")
            .args(args)
            .current_dir(repo_root)
            .status()
        {
            if status.success() {
                let skills_dir = repo_root.join(".claude/skills");
                if let Ok(entries) = fs::read_dir(&skills_dir) {
                    for entry in entries.flatten() {
                        let skill = entry.path().join("SKILL.md");
                        if skill.is_file() {
                            written.push(relative_path(&skill, repo_root).display().to_string());
                        }
                    }
                }
            }
        }
    }
    Ok(written)
}

/// Writes the constitution stub to `<repo_root>/<constitution_path>` if it
/// does not already exist.
///
/// Returns `true` if the file was created, `false` if it already existed.
pub fn write_constitution_stub(
    repo_root: &Path,
    constitution_path: &std::path::Path,
) -> Result<bool, AdoptError> {
    let path = repo_root.join(constitution_path);
    if path.exists() {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| AdoptError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    fs::write(&path, CONSTITUTION_STUB_TEMPLATE)
        .map_err(|source| AdoptError::Io { path, source })?;
    Ok(true)
}

/// Removes the derrick block from `<repo_root>/.codex/instructions.md`.
///
/// If the file ends up empty (or whitespace-only) after stripping, the file
/// is deleted. If the file does not exist, this is a no-op.
pub fn remove_codex_instructions(repo_root: &Path) -> Result<(), AdoptError> {
    let path = repo_root.join(".codex/instructions.md");
    if !path.exists() {
        return Ok(());
    }
    let content = fs::read_to_string(&path).map_err(|source| AdoptError::Io {
        path: path.clone(),
        source,
    })?;
    let stripped = strip_derrick_block(&content);
    if stripped.trim().is_empty() {
        fs::remove_file(&path).map_err(|source| AdoptError::Io { path, source })?;
    } else {
        fs::write(&path, stripped).map_err(|source| AdoptError::Io { path, source })?;
    }
    Ok(())
}

/// Remove the `<!-- derrick:start --> … <!-- derrick:end -->` block from `text`.
fn strip_derrick_block(text: &str) -> String {
    let Some(start) = text.find(DERRICK_BLOCK_START) else {
        return text.to_owned();
    };
    let Some(end_relative) = text[start..].find(DERRICK_BLOCK_END) else {
        return text.to_owned();
    };
    let end = start + end_relative + DERRICK_BLOCK_END.len();
    // Trim one leading newline before the block and the trailing newline after.
    let before = text[..start].trim_end_matches('\n');
    let after = text[end..].trim_start_matches('\n');
    if before.is_empty() && after.is_empty() {
        String::new()
    } else if before.is_empty() {
        after.to_owned()
    } else if after.is_empty() {
        format!("{before}\n")
    } else {
        format!("{before}\n\n{after}")
    }
}

fn replace_or_append_block(existing: &str, block: &str) -> String {
    if let Some(start) = existing.find(DERRICK_BLOCK_START) {
        if let Some(end_relative) = existing[start..].find(DERRICK_BLOCK_END) {
            let end = start + end_relative + DERRICK_BLOCK_END.len();
            let mut rendered = String::new();
            rendered.push_str(&existing[..start]);
            rendered.push_str(block.trim_end());
            rendered.push_str(&existing[end..]);
            if !rendered.ends_with('\n') {
                rendered.push('\n');
            }
            return rendered;
        }
    }

    let mut rendered = existing.trim_end().to_owned();
    if !rendered.is_empty() {
        rendered.push_str("\n\n");
    }
    rendered.push_str(block.trim_end());
    rendered.push('\n');
    rendered
}

fn tracker_prefixes(contents: &BTreeMap<PathBuf, String>) -> Vec<String> {
    let mut prefixes = BTreeSet::new();
    for content in contents.values() {
        for token in
            content.split(|character: char| !character.is_ascii_alphanumeric() && character != '-')
        {
            if let Some(prefix) = token.split_once('-').map(|(prefix, _)| prefix) {
                if (2..=8).contains(&prefix.len())
                    && prefix
                        .chars()
                        .all(|character| character.is_ascii_uppercase())
                {
                    prefixes.insert(format!("{prefix}-"));
                }
            }
        }
    }
    prefixes.into_iter().collect()
}

fn validate_prefix(prefix: &str) -> Result<(), AdoptError> {
    if (1..=6).contains(&prefix.len()) && prefix.bytes().all(|byte| byte.is_ascii_lowercase()) {
        Ok(())
    } else {
        Err(AdoptError::InvalidOptions(
            "site_prefix must match ^[a-z]{1,6}$".to_owned(),
        ))
    }
}

fn relative_path(path: &Path, root: &Path) -> PathBuf {
    path.strip_prefix(root)
        .map_or_else(|_| path.to_path_buf(), Path::to_path_buf)
}

/// Returns true when a constitution still has the derrick draft banner.
pub fn constitution_has_draft_banner(contents: &str) -> bool {
    contents.trim_start().starts_with(DRAFT_BANNER_PREFIX)
}

/// Returns true when a constitution is an unedited placeholder that needs
/// real content — either derrick's DERRICK-DRAFT stub or speckit's
/// `[PROJECT_NAME]` / `[PLACEHOLDER]` template.
pub fn constitution_needs_setup(contents: &str) -> bool {
    constitution_has_draft_banner(contents) || contents.contains("[PROJECT_NAME]")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn git_repo() -> tempfile::TempDir {
        let dir = tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
        fs::create_dir(dir.path().join(".git"))
            .unwrap_or_else(|error| panic!("git dir failed: {error}"));
        dir
    }

    fn opts() -> AdoptOptions {
        AdoptOptions {
            site_name: "demo".to_owned(),
            site_prefix: "dem".to_owned(),
            mode: SubstrateMode::Solo,
            force: false,
            no_hooks: false,
            append_agents_md: false,
            constitution: ConstitutionMode::Reference,
        }
    }

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .unwrap_or_else(|error| panic!("create parent failed: {error}"));
        }
        fs::write(path, contents).unwrap_or_else(|error| panic!("write failed: {error}"));
    }

    #[test]
    fn detect_finds_agents_and_constitution_candidates() {
        let dir = git_repo();
        write(&dir.path().join("AGENTS.md"), "Ticket ABC-123\n");
        write(&dir.path().join("CONTRIBUTING.md"), "# Contributing\n");

        let report = Adopter::new(dir.path()).detect().unwrap_or_else(|error| {
            panic!("detect failed: {error}");
        });

        assert_eq!(report.agents_md, Some(PathBuf::from("AGENTS.md")));
        assert_eq!(report.constitution, Some(PathBuf::from("CONTRIBUTING.md")));
        assert_eq!(report.tracker_prefixes, vec!["ABC-"]);
    }

    #[test]
    fn detect_finds_host_dirs_docs_and_sorted_files() {
        let dir = git_repo();
        write(&dir.path().join("CLAUDE.md"), "# Claude\n");
        write(&dir.path().join(".claude/settings.json"), "{}");
        write(&dir.path().join(".claude/agents/zeta.md"), "z");
        write(&dir.path().join(".claude/agents/alpha.md"), "a");
        write(&dir.path().join(".claude/commands/add-feature.md"), "user");
        write(&dir.path().join(".claude/skills/demo/SKILL.md"), "skill");
        write(&dir.path().join(".codex/instructions.md"), "codex");
        write(&dir.path().join(".codex/config.toml"), "");
        write(
            &dir.path().join(".github/copilot-instructions.md"),
            "copilot",
        );
        write(&dir.path().join(".github/CODEOWNERS"), "* @team\n");
        write(
            &dir.path().join(".specify/extensions/derrick/README.md"),
            "",
        );
        write(&dir.path().join("README.md"), "# Readme\n");
        write(&dir.path().join("docs/adrs/0001.md"), "# ADR\n");

        let report = Adopter::new(dir.path())
            .detect()
            .unwrap_or_else(|error| panic!("detect failed: {error}"));

        assert_eq!(
            report.claude_agents,
            vec![
                PathBuf::from(".claude/agents/alpha.md"),
                PathBuf::from(".claude/agents/zeta.md")
            ]
        );
        assert_eq!(
            report.claude_skills,
            vec![PathBuf::from(".claude/skills/demo/SKILL.md")]
        );
        assert_eq!(
            report.codex_instructions,
            Some(PathBuf::from(".codex/instructions.md"))
        );
        assert_eq!(
            report.github_copilot_instructions,
            Some(PathBuf::from(".github/copilot-instructions.md"))
        );
        assert_eq!(report.codeowners, Some(PathBuf::from(".github/CODEOWNERS")));
        assert_eq!(report.adrs_dir, Some(PathBuf::from("docs/adrs")));
        assert!(report
            .file_contents
            .contains_key(&PathBuf::from("docs/adrs/0001.md")));
    }

    #[test]
    fn propose_is_deterministic() {
        let mut report = DetectionReport {
            git_repo: true,
            agents_md: Some(PathBuf::from("AGENTS.md")),
            claude_md: Some(PathBuf::from("CLAUDE.md")),
            ..DetectionReport::default()
        };
        report
            .file_contents
            .insert(PathBuf::from("AGENTS.md"), "# Agents\n".to_owned());
        report
            .file_contents
            .insert(PathBuf::from("CLAUDE.md"), "# Claude\n".to_owned());
        let adopter = Adopter::new(".");
        let first = adopter
            .propose(&report, &opts(), None)
            .unwrap_or_else(|error| panic!("propose failed: {error}"));
        for _ in 0..10 {
            let next = adopter
                .propose(&report, &opts(), None)
                .unwrap_or_else(|error| panic!("propose failed: {error}"));
            assert_eq!(first, next);
        }
    }

    #[test]
    fn hooks_installed_for_all_matchers() {
        let report = DetectionReport {
            git_repo: true,
            ..DetectionReport::default()
        };
        let plan = Adopter::new(".")
            .propose(&report, &opts(), None)
            .unwrap_or_else(|error| panic!("propose failed: {error}"));
        let settings = plan
            .writes
            .iter()
            .find(|write| write.path == Path::new(".claude/settings.json"))
            .unwrap_or_else(|| panic!("missing settings write"));
        for matcher in CLAUDE_MATCHERS {
            assert!(settings
                .content
                .contains(&format!("\"matcher\": \"{matcher}\"")));
        }
        assert!(settings
            .content
            .contains("\"description\": \"derrick:scrub\""));
        assert!(settings
            .content
            .contains("\"description\": \"derrick:caveman\""));
    }

    #[test]
    fn hook_unmarked_same_matcher_blocks_without_force() {
        let mut report = DetectionReport {
            git_repo: true,
            claude_settings: Some(PathBuf::from(".claude/settings.json")),
            ..DetectionReport::default()
        };
        report.file_contents.insert(
            PathBuf::from(".claude/settings.json"),
            r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"echo user"}]}]}}"#
                .to_owned(),
        );

        let plan = Adopter::new(".")
            .propose(&report, &opts(), None)
            .unwrap_or_else(|error| panic!("propose failed: {error}"));

        assert!(plan
            .blockers
            .iter()
            .any(|blocker| blocker.contains("PreToolUse already has an entry on matcher `Bash`")));
    }

    #[test]
    fn constitution_modes_obey_detect_then_defer() {
        let mut report = DetectionReport {
            git_repo: true,
            speckit_cli_available: true,
            ..DetectionReport::default()
        };
        let mut options = opts();
        options.constitution = ConstitutionMode::Stub;

        let plan = Adopter::new(".")
            .propose(&report, &options, None)
            .unwrap_or_else(|error| panic!("propose failed: {error}"));
        assert!(plan
            .blockers
            .iter()
            .any(|blocker| blocker.contains("specify")));

        report.speckit_cli_available = false;
        let plan = Adopter::new(".")
            .propose(&report, &options, None)
            .unwrap_or_else(|error| panic!("propose failed: {error}"));
        assert!(plan
            .writes
            .iter()
            .any(|write| write.path == Path::new(".specify/memory/constitution.md")));
    }

    #[test]
    fn propose_blocks_and_warnings_cover_brownfield_edges() {
        let mut report = DetectionReport {
            git_repo: false,
            existing_derrick_yaml: Some(PathBuf::from("derrick.yaml")),
            claude_commands: vec![PathBuf::from(".claude/commands/add-feature.md")],
            constitution: Some(PathBuf::from("CONTRIBUTING.md")),
            specify_extensions_derrick: Some(PathBuf::from(".specify/extensions/derrick")),
            tracker_prefixes: vec!["LIN-".to_owned()],
            codeowners: Some(PathBuf::from("CODEOWNERS")),
            ..DetectionReport::default()
        };
        report
            .file_contents
            .insert(PathBuf::from("CONTRIBUTING.md"), "# Rules\n".to_owned());
        let mut options = opts();
        options.constitution = ConstitutionMode::Stub;
        options.mode = SubstrateMode::Crew;

        let plan = Adopter::new(".")
            .propose(&report, &options, None)
            .unwrap_or_else(|error| panic!("propose failed: {error}"));

        assert!(plan
            .blockers
            .iter()
            .any(|blocker| blocker.contains("inside a git repo")));
        assert!(plan
            .blockers
            .iter()
            .any(|blocker| blocker.contains("derrick.yaml already exists")));
        assert!(plan
            .blockers
            .iter()
            .any(|blocker| blocker.contains("existing Claude command")));
        assert!(plan
            .blockers
            .iter()
            .any(|blocker| blocker.contains("constitution-like doc")));
        assert!(plan
            .warnings
            .iter()
            .any(|warning| warning.contains("external-tracker adoption")));
        assert!(plan
            .warnings
            .iter()
            .any(|warning| warning.contains("Codex host hook")));
        assert!(plan
            .references
            .iter()
            .any(|reference| reference.path == Path::new("CODEOWNERS")));
    }

    #[test]
    fn propose_append_and_codex_blocks_are_idempotent() {
        let mut report = DetectionReport {
            git_repo: true,
            agents_md: Some(PathBuf::from("AGENTS.md")),
            claude_md: Some(PathBuf::from("CLAUDE.md")),
            codex_instructions: Some(PathBuf::from(".codex/instructions.md")),
            claude_agents: vec![PathBuf::from(".claude/agents/foreman.md")],
            ..DetectionReport::default()
        };
        report.file_contents.insert(
            PathBuf::from("AGENTS.md"),
            "# Agents\n\n<!-- derrick:start -->\nold\n<!-- derrick:end -->\n".to_owned(),
        );
        report
            .file_contents
            .insert(PathBuf::from("CLAUDE.md"), "# Claude\n".to_owned());
        report.file_contents.insert(
            PathBuf::from(".codex/instructions.md"),
            "user\n\n<!-- derrick:start -->\nold\n<!-- derrick:end -->\n".to_owned(),
        );
        let mut options = opts();
        options.append_agents_md = true;

        let plan = Adopter::new(".")
            .propose(&report, &options, None)
            .unwrap_or_else(|error| panic!("propose failed: {error}"));

        let agents = plan
            .writes
            .iter()
            .find(|write| write.path == Path::new("AGENTS.md"))
            .unwrap_or_else(|| panic!("missing agents append"));
        assert!(agents.content.contains("This repository is initialized"));
        assert!(!agents.content.contains("\nold\n"));
        let codex = plan
            .writes
            .iter()
            .find(|write| write.path == Path::new(".codex/instructions.md"))
            .unwrap_or_else(|| panic!("missing codex write"));
        assert!(codex.content.contains("Derrick project context"));
        assert!(plan
            .warnings
            .iter()
            .any(|warning| warning.contains("skipped existing Claude agent")));
    }

    #[test]
    fn fromdocs_requires_draft_and_writes_draft() {
        let mut options = opts();
        options.constitution = ConstitutionMode::FromDocs;
        let mut report = DetectionReport {
            git_repo: true,
            speckit_cli_available: false,
            ..DetectionReport::default()
        };

        let error = Adopter::new(".")
            .propose(&report, &options, None)
            .err()
            .unwrap_or_else(|| panic!("expected error"));
        assert!(error.to_string().contains("drafted_constitution"));

        let plan = Adopter::new(".")
            .propose(&report, &options, Some("draft"))
            .unwrap_or_else(|error| panic!("propose failed: {error}"));
        assert!(plan.writes.iter().any(|write| write.content == "draft"
            && write.path == Path::new(".specify/memory/constitution.md")));
        assert!(plan
            .warnings
            .iter()
            .any(|warning| warning.contains("unreviewed LLM prose")));

        report.speckit_cli_available = true;
        let plan = Adopter::new(".")
            .propose(&report, &options, Some("draft"))
            .unwrap_or_else(|error| panic!("propose failed: {error}"));
        assert!(plan
            .blockers
            .iter()
            .any(|blocker| blocker.contains("constitution-from-docs")));
    }

    #[test]
    fn hook_force_marked_and_corrupt_cases() {
        let mut report = DetectionReport {
            git_repo: true,
            claude_settings: Some(PathBuf::from(".claude/settings.json")),
            ..DetectionReport::default()
        };
        report.file_contents.insert(
            PathBuf::from(".claude/settings.json"),
            r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"old","description":"derrick:scrub"}]},{"matcher":"Notebook","hooks":[{"type":"command","command":"user"}]},{"hooks":[{"type":"command","command":"unknown"}]}],"PostToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"user"}]}]},"custom":true}"#
                .to_owned(),
        );
        let mut options = opts();
        options.force = true;
        let plan = Adopter::new(".")
            .propose(&report, &options, None)
            .unwrap_or_else(|error| panic!("propose failed: {error}"));
        let settings = plan
            .writes
            .iter()
            .find(|write| write.path == Path::new(".claude/settings.json"))
            .unwrap_or_else(|| panic!("missing settings"));
        assert!(settings.content.contains("\"custom\": true"));
        assert!(settings.content.contains("\"matcher\": \"Notebook\""));
        assert!(plan
            .warnings
            .iter()
            .any(|warning| warning.contains("force-prepended")));

        report
            .file_contents
            .insert(PathBuf::from(".claude/settings.json"), "{".to_owned());
        let error = Adopter::new(".")
            .propose(&report, &options, None)
            .err()
            .unwrap_or_else(|| panic!("expected corrupt json error"));
        assert!(error.to_string().contains("corrupt JSON"));
    }

    #[test]
    fn no_hooks_and_invalid_prefix_are_respected() {
        let report = DetectionReport {
            git_repo: true,
            ..DetectionReport::default()
        };
        let mut options = opts();
        options.no_hooks = true;
        let plan = Adopter::new(".")
            .propose(&report, &options, None)
            .unwrap_or_else(|error| panic!("propose failed: {error}"));
        assert!(!plan
            .writes
            .iter()
            .any(|write| write.path == Path::new(".claude/settings.json")));

        options.site_prefix = "TOOLONG".to_owned();
        let error = Adopter::new(".")
            .propose(&report, &options, None)
            .err()
            .unwrap_or_else(|| panic!("expected invalid prefix"));
        assert!(error.to_string().contains("site_prefix"));
    }

    #[tokio::test]
    async fn draft_constitution_rejects_wrong_mode() {
        let dir = git_repo();
        let report = DetectionReport::default();
        let error = Adopter::new(dir.path())
            .draft_constitution(&report, &opts())
            .await
            .err()
            .unwrap_or_else(|| panic!("expected draft mode error"));
        assert!(error.to_string().contains("FromDocs"));
    }

    #[test]
    fn banner_detection_identifies_draft_constitutions() {
        assert!(constitution_has_draft_banner(
            "<!-- DERRICK-DRAFT: remove -->\n# Constitution"
        ));
        assert!(!constitution_has_draft_banner("# Constitution"));
    }

    #[test]
    fn needs_setup_catches_draft_banner_and_speckit_placeholder() {
        assert!(constitution_needs_setup(
            "<!-- DERRICK-DRAFT: remove -->\n# Constitution"
        ));
        assert!(constitution_needs_setup(
            "# [PROJECT_NAME] Constitution\n\n## Core Principles\n"
        ));
        assert!(!constitution_needs_setup(
            "# My Project Constitution\n\n## Rules\n- Use zerolog\n"
        ));
    }

    #[tokio::test]
    async fn apply_writes_config_and_opens_substrate() {
        let dir = git_repo();
        let adopter = Adopter::new(dir.path());
        let report = adopter
            .detect()
            .unwrap_or_else(|error| panic!("detect failed: {error}"));
        let plan = adopter
            .propose(&report, &opts(), None)
            .unwrap_or_else(|error| panic!("propose failed: {error}"));

        let outcome = adopter
            .apply(&plan)
            .await
            .unwrap_or_else(|error| panic!("apply failed: {error}"));

        assert!(dir.path().join("derrick.yaml").is_file());
        assert!(dir.path().join(".derrick/derrick.db").is_file());
        assert!(outcome
            .written
            .iter()
            .any(|path| path == Path::new("derrick.yaml")));

        let report = adopter
            .detect()
            .unwrap_or_else(|error| panic!("detect failed: {error}"));
        let mut options = opts();
        options.force = true;
        let plan = adopter
            .propose(&report, &options, None)
            .unwrap_or_else(|error| panic!("propose failed: {error}"));
        adopter
            .apply(&plan)
            .await
            .unwrap_or_else(|error| panic!("second apply failed: {error}"));
        let state = fs::read_to_string(dir.path().join(".derrick/state.json"))
            .unwrap_or_else(|error| panic!("read state failed: {error}"));
        assert!(state.matches("timestamp").count() >= 2);
    }

    #[test]
    fn strip_derrick_block_removes_block_and_preserves_surroundings() {
        let text = "# Heading\n\nbefore text\n\n<!-- derrick:start -->\nderrick content\n<!-- derrick:end -->\n\nafter text\n";
        let stripped = strip_derrick_block(text);
        assert_eq!(stripped, "# Heading\n\nbefore text\n\nafter text\n");
    }

    #[test]
    fn strip_derrick_block_returns_empty_when_only_block_present() {
        let text = "<!-- derrick:start -->\nderrick content\n<!-- derrick:end -->\n";
        let stripped = strip_derrick_block(text);
        assert!(
            stripped.trim().is_empty(),
            "expected empty, got {stripped:?}"
        );
    }

    #[test]
    fn strip_derrick_block_no_op_when_block_absent() {
        let text = "# Heading\n\nno block here\n";
        assert_eq!(strip_derrick_block(text), text);
    }

    #[test]
    fn write_codex_instructions_creates_file() {
        let dir = tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
        write_codex_instructions(dir.path())
            .unwrap_or_else(|error| panic!("write failed: {error}"));
        let path = dir.path().join(".codex/instructions.md");
        assert!(path.is_file());
        let content =
            fs::read_to_string(&path).unwrap_or_else(|error| panic!("read failed: {error}"));
        assert!(content.contains(DERRICK_BLOCK_START));
        assert!(content.contains(DERRICK_BLOCK_END));
    }

    #[test]
    fn write_codex_instructions_is_idempotent() {
        let dir = tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
        write_codex_instructions(dir.path())
            .unwrap_or_else(|error| panic!("write failed: {error}"));
        write_codex_instructions(dir.path())
            .unwrap_or_else(|error| panic!("second write failed: {error}"));
        let content = fs::read_to_string(dir.path().join(".codex/instructions.md"))
            .unwrap_or_else(|error| panic!("read failed: {error}"));
        assert_eq!(content.matches(DERRICK_BLOCK_START).count(), 1);
    }

    #[test]
    fn remove_codex_instructions_deletes_when_only_block() {
        let dir = tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
        write_codex_instructions(dir.path())
            .unwrap_or_else(|error| panic!("write failed: {error}"));
        remove_codex_instructions(dir.path())
            .unwrap_or_else(|error| panic!("remove failed: {error}"));
        assert!(!dir.path().join(".codex/instructions.md").exists());
    }

    #[test]
    fn remove_codex_instructions_preserves_other_content() {
        let dir = tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
        let codex_dir = dir.path().join(".codex");
        fs::create_dir_all(&codex_dir).unwrap_or_else(|error| panic!("mkdir failed: {error}"));
        let path = codex_dir.join("instructions.md");
        fs::write(
            &path,
            "# Existing\n\nuser content\n\n<!-- derrick:start -->\nblock\n<!-- derrick:end -->\n",
        )
        .unwrap_or_else(|error| panic!("write failed: {error}"));
        remove_codex_instructions(dir.path())
            .unwrap_or_else(|error| panic!("remove failed: {error}"));
        let content =
            fs::read_to_string(&path).unwrap_or_else(|error| panic!("read failed: {error}"));
        assert!(!content.contains(DERRICK_BLOCK_START));
        assert!(content.contains("user content"));
    }

    #[test]
    fn remove_codex_instructions_noop_when_missing() {
        let dir = tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
        remove_codex_instructions(dir.path())
            .unwrap_or_else(|error| panic!("remove failed: {error}"));
    }
}
