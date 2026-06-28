use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::path::Path;

use derrick_adopt::{AdoptOptions, Adopter, ConstitutionMode};
use derrick_config::{Config, InitTemplateVars, render_init_template};
use derrick_memory::{MemoryPaths, MemoryStore, Seeds};
use derrick_substrate_native::NativeSubstrate;

use crate::commands::InitArgs;
use crate::commands::init_wizard::{WizardInput, WizardSelection};
use crate::exit_code::CliExitCode;
use crate::ui;
use crate::{create_dir_all, current_repo_root, message, native_paths, read_config, write_file};

/// The default AI profile used when no explicit profile is configured.
pub(crate) const DEFAULT_PROFILE: &str = "balanced";

const INIT_TEMPLATE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../templates/derrick.yaml.in"
));
const VSCODE_TASKS_TEMPLATE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../templates/.vscode/tasks.json"
));
const IDEA_DOCTOR_TEMPLATE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../templates/.idea/runConfigurations/derrick_doctor.xml"
));
const IDEA_OBSERVE_TEMPLATE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../templates/.idea/runConfigurations/derrick_observe.xml"
));
const IDEA_FOREMAN_TEMPLATE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../templates/.idea/runConfigurations/derrick_foreman_start.xml"
));

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RoleBindings {
    pub(crate) proposer: String,
    pub(crate) drafter: String,
    pub(crate) reviewer: String,
    pub(crate) executor: String,
    pub(crate) summariser: String,
}

impl RoleBindings {
    /// Creates bindings where every role uses the same model.
    pub(crate) fn one_model(model: String) -> Self {
        Self {
            proposer: model.clone(),
            drafter: model.clone(),
            reviewer: model.clone(),
            executor: model.clone(),
            summariser: model,
        }
    }

    /// Returns all role-to-model pairs as a fixed-size array.
    pub(crate) fn entries(&self) -> [(&'static str, &str); 5] {
        [
            ("proposer", &self.proposer),
            ("drafter", &self.drafter),
            ("reviewer", &self.reviewer),
            ("executor", &self.executor),
            ("summariser", &self.summariser),
        ]
    }
}

/// A runtime-keyed model definition the wizard can write (D79).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ModelSpec {
    pub(crate) runtime: String,
    pub(crate) model: String,
    pub(crate) base_url: Option<String>,
    pub(crate) auth_env: Option<String>,
}

/// How the wizard wants the `models`/`roles`/`ai` sections written (D79).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum AiPlan {
    /// Bind roles to the template's built-in catalogue aliases (claude-opus …).
    /// The static `models:` block is preserved; only `roles:` is rewritten.
    Catalogue(RoleBindings),
    /// Emit `ai.preset: <name>` and drop the static `models:`/`roles:` blocks;
    /// the preset generates them at load time.
    Preset(String),
    /// Replace `models:` with runtime-keyed aliases and bind `roles:` to them.
    Custom {
        /// `(alias, spec)` pairs written under `models:`.
        models: Vec<(String, ModelSpec)>,
        /// Role bindings referencing the aliases above.
        roles: RoleBindings,
    },
}

impl AiPlan {
    /// A one-line label for the preview/summary screens.
    pub(crate) fn label(&self) -> String {
        match self {
            Self::Catalogue(_) => "catalogue models".to_owned(),
            Self::Preset(name) => format!("preset: {name}"),
            Self::Custom { models, .. } => {
                let runtime = models
                    .first()
                    .map_or("custom", |(_, spec)| spec.runtime.as_str());
                format!("custom runtime: {runtime}")
            }
        }
    }

    /// The role bindings to display in the preview, when the plan pins them.
    pub(crate) fn roles(&self) -> Option<&RoleBindings> {
        match self {
            Self::Catalogue(roles) | Self::Custom { roles, .. } => Some(roles),
            Self::Preset(_) => None,
        }
    }

    /// Whether the plan's role bindings reference the built-in catalogue (and so
    /// can be validated against it).
    pub(crate) fn is_catalogue(&self) -> bool {
        matches!(self, Self::Catalogue(_))
    }
}

/// Serialises a [`ModelSpec`] into a `derrick.yaml` model mapping.
fn model_spec_mapping(spec: &ModelSpec) -> serde_yaml::Mapping {
    let mut mapping = serde_yaml::Mapping::new();
    let mut insert = |key: &str, value: &str| {
        mapping.insert(
            serde_yaml::Value::String(key.to_owned()),
            serde_yaml::Value::String(value.to_owned()),
        );
    };
    insert("runtime", &spec.runtime);
    insert("model", &spec.model);
    if let Some(base_url) = &spec.base_url {
        insert("base_url", base_url);
    }
    if let Some(auth_env) = &spec.auth_env {
        insert("auth_env", auth_env);
    }
    mapping
}

/// Applies an [`AiPlan`] to a parsed `derrick.yaml` root mapping (D79).
fn apply_ai_plan(root: &mut serde_yaml::Mapping, plan: &AiPlan) {
    let key = |name: &str| serde_yaml::Value::String(name.to_owned());
    match plan {
        AiPlan::Catalogue(roles) => {
            root.insert(
                key("roles"),
                serde_yaml::Value::Mapping(role_mapping_value(roles)),
            );
        }
        AiPlan::Preset(name) => {
            root.remove(key("models"));
            root.remove(key("roles"));
            let mut ai = serde_yaml::Mapping::new();
            ai.insert(key("preset"), serde_yaml::Value::String(name.clone()));
            root.insert(key("ai"), serde_yaml::Value::Mapping(ai));
        }
        AiPlan::Custom { models, roles } => {
            let mut models_map = serde_yaml::Mapping::new();
            for (alias, spec) in models {
                models_map.insert(
                    serde_yaml::Value::String(alias.clone()),
                    serde_yaml::Value::Mapping(model_spec_mapping(spec)),
                );
            }
            root.insert(key("models"), serde_yaml::Value::Mapping(models_map));
            root.insert(
                key("roles"),
                serde_yaml::Value::Mapping(role_mapping_value(roles)),
            );
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ResolvedInitOptions {
    greenfield: bool,
    mode: crate::commands::InitMode,
    site_name: String,
    prefix: String,
    force: bool,
    yes: bool,
    dry_run: bool,
    no_hooks: bool,
    append_agents_md: bool,
    constitution: ConstitutionMode,
    vscode: bool,
    jetbrains: bool,
    ai_plan: AiPlan,
    spec_provider: crate::commands::spec_provider_init::SpecProviderChoice,
    conventional_commits: bool,
    branch_prefix: String,
    default_profile: String,
}

/// Executes the `derrick init` subcommand (scaffolds derrick into a repository).
pub(crate) async fn execute(args: InitArgs) -> Result<CliExitCode, crate::CliError> {
    // DESIGN §5.2 step 1: prerequisites are always checked first, with no
    // partial init. This runs even under --dry-run so a dry run reports
    // missing tools and exits non-zero exactly as the real run would.
    check_prerequisites()?;
    let (repo_root, fresh_git_init) = match current_repo_root() {
        Ok(root) => (root, false),
        Err(_) => (ensure_git_repo(args.yes, args.dry_run)?, true),
    };
    let resolved = match resolve_options(&repo_root, args, fresh_git_init)? {
        Some(resolved) => resolved,
        None => return Ok(CliExitCode::Success),
    };

    let outcome = if resolved.greenfield {
        greenfield_init(&repo_root, &resolved).await
    } else {
        brownfield_init(&repo_root, &resolved).await
    };

    // D15/D65: after the config exists, run the soft (WARN-only) model/host
    // check so any catalogue or installation issues surface at init time.
    if let Ok(config) = derrick_config::Config::load_layered(&repo_root) {
        crate::commands::models::emit_soft_warnings(&config);
    }

    outcome
}

/// Re-scaffold `.claude/` skills, hooks and settings against the current
/// `derrick.yaml`, without touching the `.derrick/` database or the config
/// file itself. Equivalent to a forced brownfield re-init that preserves
/// `derrick.yaml` exactly.
pub(crate) async fn execute_reset(
    repo_root: &Path,
    yes: bool,
    dry_run: bool,
) -> Result<CliExitCode, crate::CliError> {
    check_prerequisites()?;
    let config = read_config(repo_root)?;

    let opts = AdoptOptions {
        site_name: config.site().name().to_owned(),
        site_prefix: config.site().prefix().to_owned(),
        mode: config.tools().substrate().mode(),
        force: true,
        no_hooks: false,
        append_agents_md: false,
        constitution: ConstitutionMode::Reference,
    };

    let adopter = Adopter::new(repo_root);
    let detection = adopter.detect().map_err(|e| message(e.to_string()))?;
    let mut plan = adopter
        .propose(&detection, &opts, None)
        .map_err(|e| message(e.to_string()))?;

    // Never rewrite derrick.yaml — reset preserves the config exactly.
    plan.writes.retain(|w| w.path != Path::new("derrick.yaml"));

    print_plan(&plan);

    if dry_run {
        return Ok(CliExitCode::Success);
    }

    if !yes && !plan.writes.is_empty() {
        use std::io::Write as _;
        print!("Apply these changes? [y/N] ");
        std::io::stdout().flush().ok();
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer).ok();
        if !answer.trim().eq_ignore_ascii_case("y") && !answer.trim().eq_ignore_ascii_case("yes") {
            println!("Cancelled.");
            return Ok(CliExitCode::Success);
        }
    }

    let outcome = adopter
        .apply(&plan)
        .await
        .map_err(|e| message(e.to_string()))?;
    for path in &outcome.written {
        print_written(&path.display().to_string());
    }
    Ok(CliExitCode::Success)
}

fn resolve_options(
    repo_root: &Path,
    args: InitArgs,
    fresh_git_init: bool,
) -> Result<Option<ResolvedInitOptions>, crate::CliError> {
    let mode = args.mode;
    let site_name = args
        .site
        .clone()
        .unwrap_or_else(|| default_site_name(repo_root));
    let default_prefix_value = default_prefix(&site_name);
    let prefix = args
        .prefix
        .clone()
        .unwrap_or_else(|| default_prefix_value.clone());
    let constitution = constitution_mode(&args);

    if should_run_wizard(&args) {
        let existing_default_profile = if repo_root.join("derrick.yaml").exists() {
            crate::read_config(repo_root)
                .ok()
                .and_then(|c| c.default_profile().map(str::to_owned))
        } else {
            None
        };
        let wizard_input = WizardInput {
            repo_root,
            has_existing_config: repo_root.join("derrick.yaml").exists(),
            likely_existing_project: likely_existing_project(repo_root),
            force_greenfield: fresh_git_init,
            default_greenfield: args.greenfield || fresh_git_init,
            default_site_name: site_name.clone(),
            default_prefix: default_prefix_value,
            default_mode: mode,
            default_constitution: constitution,
            default_append_agents_md: args.append_agents_md,
            no_hooks_forced: args.no_hooks,
            default_vscode: args.vscode,
            default_jetbrains: args.jetbrains,
            default_force: args.force,
            available_models: available_model_choices(),
            default_profile: existing_default_profile,
        };
        let selection = crate::commands::init_wizard::run(wizard_input)?;
        return match selection {
            WizardSelection::Cancelled => Ok(None),
            WizardSelection::Proceed(selection) => {
                validate_prefix(&selection.prefix)?;
                // Only catalogue plans reference the built-in alias set; custom
                // runtime aliases and presets are validated by `models check`.
                if let AiPlan::Catalogue(roles) = &selection.ai_plan {
                    validate_role_bindings(roles, &available_model_ids())?;
                }
                Ok(Some(ResolvedInitOptions {
                    greenfield: selection.greenfield,
                    mode: selection.mode,
                    site_name: selection.site_name,
                    prefix: selection.prefix,
                    force: selection.force,
                    yes: false,
                    dry_run: false,
                    no_hooks: selection.no_hooks,
                    append_agents_md: selection.append_agents_md,
                    constitution: selection.constitution,
                    vscode: selection.vscode,
                    jetbrains: selection.jetbrains,
                    ai_plan: selection.ai_plan,
                    spec_provider: selection.spec_provider,
                    conventional_commits: selection.conventional_commits,
                    branch_prefix: selection.branch_prefix,
                    default_profile: selection.default_profile,
                }))
            }
        };
    }

    validate_prefix(&prefix)?;
    let roles = recommended_role_bindings(mode, &available_model_ids());
    validate_role_bindings(&roles, &available_model_ids())?;

    Ok(Some(ResolvedInitOptions {
        greenfield: args.greenfield || fresh_git_init,
        mode,
        site_name,
        prefix,
        force: args.force,
        yes: args.yes,
        dry_run: args.dry_run,
        no_hooks: args.no_hooks,
        append_agents_md: args.append_agents_md,
        constitution,
        vscode: args.vscode,
        jetbrains: args.jetbrains,
        ai_plan: AiPlan::Catalogue(roles),
        // Non-interactive / `--yes` / non-TTY always defaults to speckit, which
        // leaves the config untouched (no behaviour change).
        spec_provider: crate::commands::spec_provider_init::SpecProviderChoice::Speckit,
        conventional_commits: true,
        branch_prefix: "feat/".to_owned(),
        default_profile: DEFAULT_PROFILE.to_owned(),
    }))
}

fn should_run_wizard(args: &InitArgs) -> bool {
    if args.wizard {
        if !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
            return false;
        }
        return true;
    }
    if args.no_wizard || args.yes || args.dry_run {
        return false;
    }
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

fn likely_existing_project(repo_root: &Path) -> bool {
    repo_root.join("README.md").exists()
        || repo_root.join("Cargo.toml").exists()
        || repo_root.join("package.json").exists()
}

/// Detect the Claude Code auto-memory root (`~/.claude/memory/`).
///
/// Returns `None` when the home directory cannot be determined — seeding is
/// best-effort and silently skipped in those cases so `derrick init` does not
/// fail in CI or non-Claude environments.
fn host_memory_root() -> Option<std::path::PathBuf> {
    // Claude Code's auto-memory convention: `~/.claude/memory/`. The
    // MemoryStore appends `derrick/<site>/` itself so we only supply the
    // root here. This matches the §9.A.1 description and is additive —
    // refinements to the exact path can be patched without touching callers.
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .map(|home| home.join(".claude").join("memory"))
}

/// Build the init-time memory seeds from a loaded config.
///
/// Per D55 and §9.A.1 the seeds include project facts (site name, prefix,
/// mode, primary constitution path) and a reference entry pointing at the
/// survey index location so the assistant always knows where to look.
fn build_seeds(repo_root: &Path, config: &Config) -> Seeds {
    let site = config.site();
    let mode = match config.tools().substrate().mode() {
        derrick_config::SubstrateMode::Solo => "solo",
        derrick_config::SubstrateMode::Copilot => "copilot",
        derrick_config::SubstrateMode::Crew => "crew",
    };
    let constitution_rel = config.guardrails().constitution_path();

    Seeds {
        project: vec![
            ("site-name".to_owned(), site.name().to_owned()),
            ("ticket-prefix".to_owned(), site.prefix().to_owned()),
            ("mode".to_owned(), mode.to_owned()),
            (
                "constitution-path".to_owned(),
                constitution_rel.display().to_string(),
            ),
        ],
        reference: vec![(
            "survey-index".to_owned(),
            format!(
                "Survey artifacts live under {}. Run `derrick survey` to inspect.",
                repo_root
                    .join(config.state().dir())
                    .join("survey")
                    .display()
            ),
        )],
        feedback: vec![(
            "guardrails".to_owned(),
            "Assay verdict is binding unless --no-assay is passed. \
                 Batches must never be re-ordered after creation. \
                 Do not mutate the substrate DB directly."
                .to_owned(),
        )],
    }
}

/// Apply memory seeding for a completed init. Best-effort: logs a warning
/// on failure but does not abort the init.
fn seed_memory(repo_root: &Path, config: &Config, dry_run: bool) {
    let seeds = build_seeds(repo_root, config);
    let state_dir = repo_root.join(config.state().dir());

    if dry_run {
        // Dry-run: report what would be seeded without writing anything.
        println!(
            "memory-seed   {} project facts, {} reference, {} feedback (host auto-memory)",
            seeds.project.len(),
            seeds.reference.len(),
            seeds.feedback.len(),
        );
        return;
    }

    let paths = MemoryPaths {
        host_memory_root: host_memory_root(),
        repo_state: state_dir,
    };
    match MemoryStore::open(paths, config.site()) {
        Ok(store) => match store.seed(&seeds) {
            Ok(written) => {
                for path in &written {
                    print_written(&path.display().to_string());
                }
            }
            Err(err) => {
                tracing::warn!(?err, "memory seeding failed — skipping");
            }
        },
        Err(err) => {
            tracing::warn!(?err, "failed to open memory store — skipping seed");
        }
    }
}

async fn brownfield_init(
    repo_root: &Path,
    resolved: &ResolvedInitOptions,
) -> Result<CliExitCode, crate::CliError> {
    let adopter = Adopter::new(repo_root);
    let detection = adopter.detect()?;
    let opts = AdoptOptions {
        site_name: resolved.site_name.clone(),
        site_prefix: resolved.prefix.clone(),
        mode: init_mode_to_substrate(resolved.mode),
        force: resolved.force,
        no_hooks: resolved.no_hooks,
        append_agents_md: resolved.append_agents_md,
        constitution: resolved.constitution,
    };
    let drafted_constitution = if opts.constitution == ConstitutionMode::FromDocs {
        Some(adopter.draft_constitution(&detection, &opts).await?)
    } else {
        None
    };
    let mut plan = adopter.propose(&detection, &opts, drafted_constitution.as_deref())?;
    override_plan_yaml(&mut plan, resolved)?;
    print_plan(&plan);
    if !plan.blockers.is_empty() {
        return Ok(CliExitCode::Failure);
    }
    if resolved.dry_run {
        return Ok(CliExitCode::Success);
    }

    let outcome = adopter.apply(&plan).await?;
    println!();
    println!("{}", ui::ready(&opts.site_name));
    println!();
    for path in &outcome.written {
        print_written(&path.display().to_string());
    }
    if !outcome.bookkeeping.is_empty() {
        for path in &outcome.bookkeeping {
            print_written(&path.display().to_string());
        }
    }
    if resolved.vscode {
        write_vscode_configs(repo_root)?;
    }
    if resolved.jetbrains {
        write_jetbrains_configs(repo_root)?;
    }
    // Seed memory after apply so the config and state dir are present.
    if let Ok(config) = read_config(repo_root) {
        seed_memory(repo_root, &config, resolved.dry_run);
    }
    if !resolved.yes {
        println!();
        println!("{}", ui::hint("review `git status` before committing"));
    }
    println!();
    Ok(CliExitCode::Success)
}

async fn greenfield_init(
    repo_root: &Path,
    resolved: &ResolvedInitOptions,
) -> Result<CliExitCode, crate::CliError> {
    let config_path = repo_root.join("derrick.yaml");
    if config_path.exists() && !resolved.force {
        return Err(message(format!(
            "{} already exists; rerun with --force to overwrite it",
            config_path.display()
        )));
    }

    // DESIGN §5.2: --dry-run opts out of all writes globally. Print the plan
    // of what greenfield init WOULD write, then return success without
    // touching the filesystem or opening the substrate.
    if resolved.dry_run {
        print_greenfield_plan(repo_root, resolved);
        return Ok(CliExitCode::Success);
    }

    let rendered = render_init_template(
        INIT_TEMPLATE,
        InitTemplateVars {
            site_name: &resolved.site_name,
            prefix: &resolved.prefix,
            mode: resolved.mode.as_str(),
        },
    );
    let rendered = apply_config_overrides(&rendered, resolved)?;
    write_file(&config_path, &rendered)?;

    let config = read_config(repo_root)?;
    create_dir_all(&repo_root.join(config.state().dir()))?;
    let gitignore = repo_root.join(config.state().dir()).join(".gitignore");
    write_file(&gitignore, derrick_adopt::DERRICK_GITIGNORE)?;

    let substrate =
        NativeSubstrate::open(native_paths(repo_root, &config), config.site().clone()).await?;
    substrate.close().await?;

    if !resolved.no_hooks {
        derrick_adopt::write_codex_instructions(repo_root).map_err(|e| message(e.to_string()))?;
        derrick_adopt::write_claude_settings(repo_root, resolved.force)
            .map_err(|e| message(e.to_string()))?;
        if which::which("specify").is_err() {
            ensure_speckit(resolved.yes)?;
        }
        let written_commands = derrick_adopt::write_claude_commands(repo_root, resolved.force)
            .map_err(|e| message(e.to_string()))?;
        print_written(".codex/instructions.md");
        print_written(".claude/settings.json");
        for path in &written_commands {
            print_written(path);
        }
    } else {
        // --no-hooks still needs the survey MCP tools auto-allowed, or the
        // server registered below would require manual per-tool approval.
        derrick_adopt::write_survey_permissions(repo_root).map_err(|e| message(e.to_string()))?;
        print_written(".claude/settings.json");
    }

    // Register the survey MCP server regardless of --no-hooks; the server
    // declaration is independent of the D29 hook config (D54/D57).
    derrick_adopt::write_mcp_json(repo_root).map_err(|e| message(e.to_string()))?;
    print_written(".mcp.json");

    if resolved.vscode {
        write_vscode_configs(repo_root)?;
    }
    if resolved.jetbrains {
        write_jetbrains_configs(repo_root)?;
    }

    // Seed the constitution so assay is never silently skipped on the first run.
    // This call overwrites any unedited speckit placeholder written above.
    seed_constitution(repo_root, &config, resolved.yes)?;

    // Make an initial commit so `git worktree add ... HEAD` succeeds on the
    // first `derrick drill`. Only runs when the repo has no commits yet.
    maybe_initial_commit(repo_root)?;

    // Seed the memory store now that the config and state dir exist (§9.A.1 /
    // D55). Best-effort — failures are logged, not propagated.
    seed_memory(repo_root, &config, false);

    print_summary(&config, &resolved.ai_plan);
    Ok(CliExitCode::Success)
}

fn override_plan_yaml(
    plan: &mut derrick_adopt::AdoptionPlan,
    resolved: &ResolvedInitOptions,
) -> Result<(), crate::CliError> {
    if let Some(write) = plan
        .writes
        .iter_mut()
        .find(|write| write.path == Path::new("derrick.yaml"))
    {
        write.content = apply_text_overrides(&write.content, resolved)?;
    }
    Ok(())
}

fn apply_text_overrides(
    rendered: &str,
    resolved: &ResolvedInitOptions,
) -> Result<String, crate::CliError> {
    // Preset/Custom plans restructure the models+roles region, so round-trip
    // through the serde_yaml writer (the brownfield derrick.yaml is derrick-owned
    // and freshly rendered from the template, so losing comments here is fine).
    // The catalogue plan stays line-based to preserve the template's comments.
    if !resolved.ai_plan.is_catalogue() {
        // Propagate rather than silently returning the unmodified template — a
        // swallowed error would write a config missing the chosen runtime.
        return apply_config_overrides(rendered, resolved);
    }
    let catalogue_roles = resolved
        .ai_plan
        .roles()
        .expect("catalogue plan always pins roles");

    let mut lines = rendered
        .lines()
        .map(std::borrow::ToOwned::to_owned)
        .collect::<Vec<_>>();

    if let Some(mode_index) = lines
        .iter()
        .position(|line| line.trim_start().starts_with("mode: "))
    {
        let indent = lines[mode_index]
            .chars()
            .take_while(|ch| ch.is_whitespace())
            .collect::<String>();
        lines[mode_index] = format!("{indent}mode: {}", resolved.mode.as_str());
    }

    if let Some(roles_start) = lines.iter().position(|line| line == "roles:") {
        let mut roles_end = roles_start + 1;
        while roles_end < lines.len() {
            let line = &lines[roles_end];
            if line.is_empty() || line.starts_with("  ") {
                roles_end += 1;
            } else {
                break;
            }
        }
        let replacement = vec![
            "roles:".to_owned(),
            format!("  proposer: {}", catalogue_roles.proposer),
            format!("  drafter: {}", catalogue_roles.drafter),
            format!("  reviewer: {}", catalogue_roles.reviewer),
            format!("  executor: {}", catalogue_roles.executor),
            format!("  summariser: {}", catalogue_roles.summariser),
        ];
        lines.splice(roles_start..roles_end, replacement);
    }

    if matches!(resolved.mode, crate::commands::InitMode::Crew)
        && !lines.iter().any(|line| line.trim() == "- id: bridge")
    {
        if let Some(guardrails_index) = lines.iter().position(|line| line == "guardrails:") {
            let addition = vec![
                "  - id: bridge".to_owned(),
                "    runner: derrick".to_owned(),
                "  - id: foreman".to_owned(),
                "    runner: derrick".to_owned(),
                "    executor_role: executor".to_owned(),
            ];
            lines.splice(guardrails_index..guardrails_index, addition);
        }
    }

    lines.push(format!("default_profile: {}", resolved.default_profile));

    let out = format!("{}\n", lines.join("\n"));
    // Speckit is a no-op here (returns `out` unchanged), so the template's
    // comments survive on the common catalogue path; native/import round-trip
    // through serde to bare the spec steps and set `tools.specify.provider`.
    crate::commands::spec_provider_init::apply_spec_provider(&out, resolved.spec_provider)
}

fn apply_config_overrides(
    rendered: &str,
    resolved: &ResolvedInitOptions,
) -> Result<String, crate::CliError> {
    let mut yaml: serde_yaml::Value =
        serde_yaml::from_str(rendered).map_err(|error| message(error.to_string()))?;
    let root = yaml
        .as_mapping_mut()
        .ok_or_else(|| message("rendered config is not a mapping"))?;

    let tools = nested_mapping(root, "tools")?;
    let substrate = nested_mapping(tools, "substrate")?;
    substrate.insert(
        serde_yaml::Value::String("mode".to_owned()),
        serde_yaml::Value::String(resolved.mode.as_str().to_owned()),
    );

    apply_ai_plan(root, &resolved.ai_plan);

    root.insert(
        serde_yaml::Value::String("default_profile".to_owned()),
        serde_yaml::Value::String(resolved.default_profile.clone()),
    );

    if matches!(resolved.mode, crate::commands::InitMode::Crew) {
        ensure_crew_pipeline(root)?;
    }

    let out = serde_yaml::to_string(&yaml).map_err(|error| message(error.to_string()))?;
    crate::commands::spec_provider_init::apply_spec_provider(&out, resolved.spec_provider)
}

/// Returns a mutable reference to a nested YAML mapping, creating it if absent.
pub(crate) fn nested_mapping<'a>(
    mapping: &'a mut serde_yaml::Mapping,
    key: &str,
) -> Result<&'a mut serde_yaml::Mapping, crate::CliError> {
    let key_value = serde_yaml::Value::String(key.to_owned());
    if !mapping.contains_key(&key_value) {
        mapping.insert(
            key_value.clone(),
            serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
        );
    }
    mapping
        .get_mut(&key_value)
        .and_then(serde_yaml::Value::as_mapping_mut)
        .ok_or_else(|| message(format!("{key} is not a mapping")))
}

fn role_mapping_value(roles: &RoleBindings) -> serde_yaml::Mapping {
    let mut mapping = serde_yaml::Mapping::new();
    for (role, model) in roles.entries() {
        mapping.insert(
            serde_yaml::Value::String(role.to_owned()),
            serde_yaml::Value::String(model.to_owned()),
        );
    }
    mapping
}

/// Ensures the `pipeline` section contains the required `bridge` and `foreman` steps.
pub(crate) fn ensure_crew_pipeline(root: &mut serde_yaml::Mapping) -> Result<(), crate::CliError> {
    let key = serde_yaml::Value::String("pipeline".to_owned());
    let pipeline_value = root
        .get_mut(&key)
        .ok_or_else(|| message("pipeline is missing from rendered config"))?;
    let steps = pipeline_value
        .as_sequence_mut()
        .ok_or_else(|| message("pipeline is not a sequence"))?;

    let has_bridge = steps.iter().any(|step| step_id(step) == Some("bridge"));
    if !has_bridge {
        steps.push(yaml_step(&[("id", "bridge"), ("runner", "derrick")]));
    }

    let has_foreman = steps.iter().any(|step| step_id(step) == Some("foreman"));
    if !has_foreman {
        let mut step = yaml_step(&[("id", "foreman"), ("runner", "derrick")]);
        if let Some(mapping) = step.as_mapping_mut() {
            mapping.insert(
                serde_yaml::Value::String("executor_role".to_owned()),
                serde_yaml::Value::String("executor".to_owned()),
            );
        }
        steps.push(step);
    }

    Ok(())
}

/// Builds a YAML step mapping from a slice of key-value string pairs.
pub(crate) fn yaml_step(entries: &[(&str, &str)]) -> serde_yaml::Value {
    let mut step = serde_yaml::Mapping::new();
    for (key, value) in entries {
        step.insert(
            serde_yaml::Value::String((*key).to_owned()),
            serde_yaml::Value::String((*value).to_owned()),
        );
    }
    serde_yaml::Value::Mapping(step)
}

/// Extracts the `id` string from a YAML step value, or `None` if absent.
pub(crate) fn step_id(step: &serde_yaml::Value) -> Option<&str> {
    let id_key = serde_yaml::Value::String("id".to_owned());
    step.as_mapping()?.get(&id_key)?.as_str()
}

fn validate_role_bindings(
    roles: &RoleBindings,
    available_models: &BTreeMap<String, &'static str>,
) -> Result<(), crate::CliError> {
    for (role, model) in roles.entries() {
        if !available_models.contains_key(model) {
            return Err(message(format!(
                "role `{role}` points to model `{model}`, but it is not configured under `models`"
            )));
        }
    }
    Ok(())
}

/// Returns the recommended role-to-model bindings for the given init mode and available models.
pub(crate) fn recommended_role_bindings(
    mode: crate::commands::InitMode,
    available_models: &BTreeMap<String, &'static str>,
) -> RoleBindings {
    let claude_opus = pick_model(
        available_models,
        &[
            "claude-opus",
            "claude-sonnet",
            "codex-gpt5",
            "copilot",
            "opencode",
            "aider",
        ],
    );
    let claude_sonnet = pick_model(
        available_models,
        &[
            "claude-sonnet",
            "claude-opus",
            "codex-gpt5",
            "copilot",
            "opencode",
            "aider",
        ],
    );
    let codex = pick_model(
        available_models,
        &[
            "codex-gpt5",
            "copilot",
            "opencode",
            "aider",
            "claude-sonnet",
            "claude-opus",
        ],
    );
    let copilot = pick_model(
        available_models,
        &[
            "copilot",
            "codex-gpt5",
            "opencode",
            "aider",
            "claude-sonnet",
            "claude-opus",
        ],
    );
    // Summariser favours the cheap, fast model, matching Config::defaults().
    let claude_haiku = pick_model(
        available_models,
        &[
            "claude-haiku",
            "claude-sonnet",
            "claude-opus",
            "codex-gpt5",
            "copilot",
            "opencode",
            "aider",
        ],
    );

    match mode {
        crate::commands::InitMode::Solo => RoleBindings {
            proposer: claude_sonnet.clone(),
            drafter: claude_sonnet.clone(),
            reviewer: codex.clone(),
            executor: copilot.clone(),
            summariser: claude_haiku,
        },
        crate::commands::InitMode::Copilot => RoleBindings {
            proposer: claude_sonnet.clone(),
            drafter: claude_sonnet.clone(),
            reviewer: codex.clone(),
            executor: copilot.clone(),
            summariser: claude_haiku,
        },
        crate::commands::InitMode::Crew => RoleBindings {
            proposer: claude_opus,
            drafter: claude_sonnet.clone(),
            reviewer: codex.clone(),
            executor: copilot,
            summariser: claude_haiku,
        },
    }
}

fn pick_model(available_models: &BTreeMap<String, &'static str>, candidates: &[&str]) -> String {
    for candidate in candidates {
        if available_models.contains_key(*candidate) {
            return (*candidate).to_owned();
        }
    }
    available_models
        .keys()
        .next()
        .cloned()
        .unwrap_or_else(|| "claude-sonnet".to_owned())
}

/// Returns all available model IDs mapped to their descriptions.
pub(crate) fn available_model_ids() -> BTreeMap<String, &'static str> {
    available_model_choices()
        .into_iter()
        .map(|(id, description)| (id.to_owned(), description))
        .collect()
}

/// Returns the list of (model-id, description) pairs the wizard can offer.
pub(crate) fn available_model_choices() -> Vec<(&'static str, &'static str)> {
    vec![
        ("claude-opus", "good for architecture and planning"),
        (
            "claude-sonnet",
            "balanced default for drafting and summaries",
        ),
        ("claude-haiku", "fast and cheap for summaries"),
        ("codex-gpt5", "good for code review and implementation"),
        ("copilot", "good for Copilot CLI workflows"),
        ("opencode", "good for OpenCode CLI workflows"),
        ("aider", "good for Aider CLI workflows"),
    ]
}

fn constitution_mode(args: &InitArgs) -> ConstitutionMode {
    if args.constitution_stub {
        ConstitutionMode::Stub
    } else if args.constitution_from_docs {
        ConstitutionMode::FromDocs
    } else {
        ConstitutionMode::Reference
    }
}

fn write_vscode_configs(repo_root: &Path) -> Result<(), crate::CliError> {
    let dir = repo_root.join(".vscode");
    create_dir_all(&dir)?;
    let path = dir.join("tasks.json");
    if !path.exists() {
        write_file(&path, VSCODE_TASKS_TEMPLATE)?;
        print_written(".vscode/tasks.json");
    } else {
        print_skipped(".vscode/tasks.json");
    }
    Ok(())
}

fn write_jetbrains_configs(repo_root: &Path) -> Result<(), crate::CliError> {
    let dir = repo_root.join(".idea/runConfigurations");
    create_dir_all(&dir)?;
    for (filename, content) in [
        ("derrick_doctor.xml", IDEA_DOCTOR_TEMPLATE),
        ("derrick_observe.xml", IDEA_OBSERVE_TEMPLATE),
        ("derrick_foreman_start.xml", IDEA_FOREMAN_TEMPLATE),
    ] {
        let path = dir.join(filename);
        if !path.exists() {
            write_file(&path, content)?;
            print_written(&format!(".idea/runConfigurations/{filename}"));
        } else {
            print_skipped(&format!(".idea/runConfigurations/{filename}"));
        }
    }
    Ok(())
}

/// Derives a default site name from the repository's directory name.
pub(crate) fn default_site_name(repo_root: &Path) -> String {
    repo_root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("derrick-project")
        .to_owned()
}

/// Derives a default ticket prefix from the site name (first 3 ASCII letters, lowercase).
pub(crate) fn default_prefix(site_name: &str) -> String {
    let prefix: String = site_name
        .chars()
        .filter(|character| character.is_ascii_alphabetic())
        .flat_map(char::to_lowercase)
        .take(3)
        .collect();
    if prefix.is_empty() {
        "drk".to_owned()
    } else {
        prefix
    }
}

/// Validates that a site prefix is 1–6 lowercase ASCII letters.
pub(crate) fn validate_prefix(prefix: &str) -> Result<(), crate::CliError> {
    if (1..=6).contains(&prefix.len()) && prefix.bytes().all(|byte| byte.is_ascii_lowercase()) {
        Ok(())
    } else {
        Err(message("site.prefix: must match ^[a-z]{1,6}$"))
    }
}

fn init_mode_to_substrate(mode: crate::commands::InitMode) -> derrick_config::SubstrateMode {
    match mode {
        crate::commands::InitMode::Solo => derrick_config::SubstrateMode::Solo,
        crate::commands::InitMode::Copilot => derrick_config::SubstrateMode::Copilot,
        crate::commands::InitMode::Crew => derrick_config::SubstrateMode::Crew,
    }
}

/// Print the greenfield init plan under --dry-run. Mirrors `print_plan`'s
/// layout: a `writes` block listing every path greenfield init would create,
/// conditioned on the resolved flags. Nothing is written and the substrate is
/// not opened.
fn print_greenfield_plan(repo_root: &Path, resolved: &ResolvedInitOptions) {
    const INDENT: &str = "             ";

    // The state dir / db / constitution paths match the bundled template
    // defaults (state.dir = .derrick, guardrails.constitution_path =
    // .specify/memory/constitution.md).
    let mut writes: Vec<String> = vec![
        "derrick.yaml".to_owned(),
        ".derrick/.gitignore".to_owned(),
        ".derrick/derrick.db".to_owned(),
    ];

    if resolved.no_hooks {
        writes.push(".claude/settings.json".to_owned());
    } else {
        writes.push(".codex/instructions.md".to_owned());
        writes.push(".claude/settings.json".to_owned());
        writes.push(".claude/commands/ (derrick command shims)".to_owned());
    }
    // Survey MCP server is registered regardless of --no-hooks.
    writes.push(".mcp.json".to_owned());

    if resolved.vscode {
        writes.push(".vscode/tasks.json".to_owned());
    }
    if resolved.jetbrains {
        writes.push(".idea/runConfigurations/ (derrick run configs)".to_owned());
    }

    // seed_constitution writes the constitution last.
    writes.push(".specify/memory/constitution.md".to_owned());

    // Memory seeding is reported separately (it writes to the host memory
    // dir, not the repo, so it does not fit the `writes` list).
    let memory_note =
        "memory seeds (project/reference/feedback) → ~/.claude/memory/derrick/<site>/";

    println!("dry run — greenfield init plan for {}", repo_root.display());
    let mut iter = writes.iter();
    if let Some(first) = iter.next() {
        println!("writes       {first}");
        for path in iter {
            println!("{INDENT}{path}");
        }
    }
    println!("seeds        {memory_note}");
    println!();
    println!("{}", ui::hint("re-run without --dry-run to apply"));
}

fn print_plan(plan: &derrick_adopt::AdoptionPlan) {
    const INDENT: &str = "             ";
    println!("adoption plan");
    if !plan.writes.is_empty() {
        let mut iter = plan.writes.iter();
        println!("writes       {}", iter.next().unwrap().path.display());
        for write in iter {
            println!("{INDENT}{}", write.path.display());
        }
    }
    if !plan.references.is_empty() {
        let mut iter = plan.references.iter();
        let first = iter.next().unwrap();
        println!(
            "references   {} as {}",
            first.path.display(),
            first.as_field
        );
        for reference in iter {
            println!(
                "{INDENT}{} as {}",
                reference.path.display(),
                reference.as_field
            );
        }
    }
    for warning in &plan.warnings {
        println!("warning      {warning}");
    }
    for blocker in &plan.blockers {
        println!("blocker      {blocker}");
    }
}

fn ensure_git_repo(yes: bool, dry_run: bool) -> Result<std::path::PathBuf, crate::CliError> {
    let cwd = std::env::current_dir().map_err(|source| crate::CliError::Io {
        path: std::path::PathBuf::from("."),
        source,
    })?;

    eprintln!(
        "{}",
        ui::warn("No git repository found in this directory or any parent.")
    );

    // DESIGN §5.2: --dry-run never mutates the filesystem. Report what would
    // happen and return the cwd without running `git init`.
    if dry_run {
        println!("would run `git init` in {}", cwd.display());
        return Ok(cwd);
    }

    let confirmed = if yes {
        true
    } else if std::io::stdin().is_terminal() {
        if ui::styled() {
            eprint!(
                "  \x1b[36m›\x1b[0m  Run \x1b[1mgit init\x1b[0m in {}? [Y/n] ",
                cwd.display()
            );
        } else {
            eprint!("  ›  Run `git init` in {}? [Y/n] ", cwd.display());
        }
        use std::io::Write as _;
        let _ = std::io::stderr().flush();
        let mut input = String::new();
        std::io::stdin()
            .read_line(&mut input)
            .map_err(|source| crate::CliError::Io {
                path: std::path::PathBuf::from("<stdin>"),
                source,
            })?;
        !matches!(input.trim().to_ascii_lowercase().as_str(), "n" | "no")
    } else {
        false
    };

    if !confirmed {
        return Err(message("derrick init must be run inside a git repo"));
    }

    let status = std::process::Command::new("git")
        .arg("init")
        .current_dir(&cwd)
        .status()
        .map_err(|source| crate::CliError::Io {
            path: cwd.join(".git"),
            source,
        })?;

    if !status.success() {
        return Err(message("git init failed"));
    }

    println!("{}", ui::done("git repository initialised"));

    Ok(cwd)
}

fn ensure_speckit(yes: bool) -> Result<(), crate::CliError> {
    if ui::styled() {
        eprintln!(
            "  \x1b[33m⚠\x1b[0m  \x1b[1mspeckit\x1b[0m (\x1b[1mspecify\x1b[0m) is not installed."
        );
        eprintln!(
            "     \x1b[90mSpeckit provides richer Claude Code skills for specify/plan/constitution.\x1b[0m"
        );
        eprintln!(
            "     \x1b[90mWithout it derrick will write minimal fallback shims instead.\x1b[0m"
        );
    } else {
        eprintln!("  ⚠  speckit (specify) is not installed.");
        eprintln!("     Speckit provides richer Claude Code skills for specify/plan/constitution.");
        eprintln!("     Without it derrick will write minimal fallback shims instead.");
    }

    let install = if yes {
        true
    } else if std::io::stdin().is_terminal() {
        if ui::styled() {
            eprint!(
                "  \x1b[36m›\x1b[0m  Install speckit now (\x1b[1muv tool install specify-cli\x1b[0m)? [Y/n] "
            );
        } else {
            eprint!("  ›  Install speckit now (uv tool install specify-cli)? [Y/n] ");
        }
        use std::io::Write as _;
        let _ = std::io::stderr().flush();
        let mut input = String::new();
        std::io::stdin()
            .read_line(&mut input)
            .map_err(|source| crate::CliError::Io {
                path: std::path::PathBuf::from("<stdin>"),
                source,
            })?;
        !matches!(input.trim().to_ascii_lowercase().as_str(), "n" | "no")
    } else {
        false
    };

    if !install {
        if ui::styled() {
            eprintln!("  \x1b[33m·\x1b[0m  Skipping speckit install — using fallback shims.");
        } else {
            eprintln!("  ·  Skipping speckit install — using fallback shims.");
        }
        return Ok(());
    }

    let result = std::process::Command::new("uv")
        .args(["tool", "install", "specify-cli"])
        .status();

    let status = match result {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // `uv` is not on PATH — treat the same as a failed install.
            if ui::styled() {
                eprintln!("  \x1b[33m⚠\x1b[0m  speckit install failed — `uv` not found on PATH.");
                eprintln!(
                    "     Install uv (<https://docs.astral.sh/uv/>) then run \x1b[1muv tool install specify-cli\x1b[0m."
                );
            } else {
                eprintln!("  ⚠  speckit install failed — `uv` not found on PATH.");
                eprintln!(
                    "     Install uv (https://docs.astral.sh/uv/) then run `uv tool install specify-cli`."
                );
            }
            return Ok(());
        }
        Err(source) => {
            return Err(crate::CliError::Io {
                path: std::path::PathBuf::from("uv"),
                source,
            });
        }
    };

    if !status.success() {
        if ui::styled() {
            eprintln!("  \x1b[33m⚠\x1b[0m  speckit install failed — falling back to shims.");
            eprintln!(
                "     Run \x1b[1muv tool install specify-cli\x1b[0m manually and re-run \x1b[1mderrick init\x1b[0m."
            );
        } else {
            eprintln!("  ⚠  speckit install failed — falling back to shims.");
            eprintln!("     Run `uv tool install specify-cli` manually and re-run `derrick init`.");
        }
        return Ok(());
    }

    println!("{}", ui::done("speckit installed"));
    Ok(())
}

fn print_written(path: &str) {
    println!("{}", ui::written(path));
}

fn print_skipped(path: &str) {
    println!("{}", ui::skipped(path));
}

fn print_summary(config: &Config, ai_plan: &AiPlan) {
    let steps = config
        .pipeline()
        .iter()
        .map(|step| step.id())
        .collect::<Vec<_>>()
        .join(", ");
    let name = config.site().name();
    let mode = mode_name(config.tools().substrate().mode());
    let prefix = config.site().prefix();
    let ai = ai_plan.label();

    println!();
    println!("{}", ui::ready(name));
    println!();
    println!("  {:<11}  {mode}", "mode");
    println!("  {:<11}  {prefix}", "prefix");
    println!("  {:<11}  {ai}", "ai config");
    println!("  {:<11}  {steps}", "pipeline");
    println!();
    println!("{}", ui::rule());
    println!();
    if ui::styled() {
        println!("  \x1b[36m›\x1b[0m  run \x1b[1mderrick doctor\x1b[0m to verify the install");
        println!(
            "  \x1b[36m›\x1b[0m  refine your constitution at \x1b[1m.specify/memory/constitution.md\x1b[0m"
        );
        println!("  \x1b[36m›\x1b[0m  start your first feature:");
        println!();
        println!("      \x1b[1mderrick drill\x1b[0m \x1b[2m\"describe your feature\"\x1b[0m");
    } else {
        println!("  ›  run `derrick doctor` to verify the install");
        println!("  ›  refine your constitution at .specify/memory/constitution.md");
        println!("  ›  start your first feature:");
        println!();
        println!("      derrick drill \"describe your feature\"");
    }
    println!();
}

/// Prompt the user for constitution seeds and write the seeded file.
///
/// Overwrites any unedited speckit `[PROJECT_NAME]` placeholder so that
/// `constitution_needs_setup` returns `false` and assay is not silently
/// skipped on the first `derrick drill`.
fn seed_constitution(
    repo_root: &Path,
    config: &derrick_config::Config,
    yes: bool,
) -> Result<(), crate::CliError> {
    use crate::commands::init_wizard::{format_constitution, prompt_constitution};

    let seeds = prompt_constitution(yes)?;
    let constitution_rel = config.guardrails().constitution_path();
    let constitution_abs = repo_root.join(constitution_rel);
    if let Some(parent) = constitution_abs.parent() {
        create_dir_all(parent)?;
    }
    let content = format_constitution(config.site().name(), &seeds);
    write_file(&constitution_abs, &content)?;
    print_written(&constitution_rel.display().to_string());
    Ok(())
}

/// Stage and commit all init-written files when no HEAD exists yet.
///
/// `git worktree add ... HEAD` (called by the pipeline runner) fails with
/// "fatal: invalid reference: HEAD" when the repo has no commits. This
/// function creates a single initial commit so the first `derrick drill`
/// always works.
///
/// Does nothing if the repo already has at least one commit.
fn maybe_initial_commit(repo_root: &Path) -> Result<(), crate::CliError> {
    let has_head = std::process::Command::new("git")
        .args(["rev-parse", "--verify", "HEAD"])
        .current_dir(repo_root)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if has_head {
        return Ok(());
    }

    // Stage everything derrick just wrote.
    let add_status = std::process::Command::new("git")
        .args(["add", "-A"])
        .current_dir(repo_root)
        .status()
        .map_err(|source| crate::CliError::Io {
            path: repo_root.join(".git"),
            source,
        })?;
    if !add_status.success() {
        // Non-fatal: the user can commit manually if something is odd.
        if ui::styled() {
            eprintln!(
                "  \x1b[33m⚠\x1b[0m  git add failed — run `git add -A && git commit -m \"chore: derrick init\"` before `derrick drill`."
            );
        } else {
            eprintln!(
                "  ⚠  git add failed — run `git add -A && git commit -m \"chore: derrick init\"` before `derrick drill`."
            );
        }
        return Ok(());
    }

    let commit_status = std::process::Command::new("git")
        .args(["commit", "-m", "chore: derrick init"])
        .current_dir(repo_root)
        .status()
        .map_err(|source| crate::CliError::Io {
            path: repo_root.join(".git"),
            source,
        })?;
    if !commit_status.success() {
        if ui::styled() {
            eprintln!(
                "  \x1b[33m⚠\x1b[0m  initial commit failed — run `git commit -m \"chore: derrick init\"` before `derrick drill`."
            );
        } else {
            eprintln!(
                "  ⚠  initial commit failed — run `git commit -m \"chore: derrick init\"` before `derrick drill`."
            );
        }
        return Ok(());
    }

    if ui::styled() {
        println!("  \x1b[32m·\x1b[0m  initial commit created");
    } else {
        println!("  ·  initial commit created");
    }
    Ok(())
}

/// Check that every required tool is present on PATH.
/// Returns `Err` with a styled message listing every missing tool.
///
/// Bypassed when `DERRICK_SKIP_PREREQS=1` is set (used in tests and CI
/// environments where the full tool-chain is not installed).
fn check_prerequisites() -> Result<(), crate::CliError> {
    if std::env::var_os("DERRICK_SKIP_PREREQS").is_some() {
        return Ok(());
    }
    struct Tool {
        name: &'static str,
        bins: &'static [&'static str],
        install: &'static str,
        required: bool,
    }

    let tools = [
        Tool {
            name: "git",
            bins: &["git"],
            install: "https://git-scm.com/downloads",
            required: true,
        },
        Tool {
            name: "gh (GitHub CLI)",
            bins: &["gh"],
            install: "https://cli.github.com",
            required: true,
        },
        Tool {
            name: "speckit / specify",
            bins: &["specify", "speckit"],
            install: "uv tool install specify-cli",
            required: true,
        },
    ];

    let ai_tools: &[(&str, &str)] = &[
        ("claude", "https://claude.ai/download"),
        ("codex", "https://github.com/openai/codex"),
        ("copilot", "https://github.com/features/copilot"),
        ("opencode", "https://opencode.ai"),
    ];

    let mut missing: Vec<String> = Vec::new();

    for tool in &tools {
        let found = tool.bins.iter().any(|b| which::which(b).is_ok());
        if !found && tool.required {
            if ui::styled() {
                missing.push(format!(
                    "  \x1b[31m✗\x1b[0m  \x1b[1m{}\x1b[0m\n     install: \x1b[2m{}\x1b[0m",
                    tool.name, tool.install
                ));
            } else {
                missing.push(format!(
                    "  ✗  {}\n     install: {}",
                    tool.name, tool.install
                ));
            }
        }
    }

    // At least one AI CLI must be available
    let any_ai = ai_tools.iter().any(|(bin, _)| which::which(bin).is_ok());
    if !any_ai {
        let list = ai_tools
            .iter()
            .map(|(bin, url)| format!("           {bin}  {url}"))
            .collect::<Vec<_>>()
            .join("\n");
        missing.push(format!(
            "  {}  {}  (need at least one)\n{list}",
            ui::cross(),
            ui::bold("AI provider CLI"),
        ));
    }

    if missing.is_empty() {
        return Ok(());
    }

    let mut message = String::new();
    if ui::styled() {
        message.push_str(
            "\x1b[1mMissing required tools — install them and re-run `derrick init`:\x1b[0m\n\n",
        );
    } else {
        message.push_str("Missing required tools — install them and re-run `derrick init`:\n\n");
    }
    for item in &missing {
        message.push_str(item);
        message.push('\n');
    }

    Err(crate::message(message))
}

fn mode_name(mode: derrick_config::SubstrateMode) -> &'static str {
    match mode {
        derrick_config::SubstrateMode::Solo => "solo",
        derrick_config::SubstrateMode::Copilot => "copilot",
        derrick_config::SubstrateMode::Crew => "crew",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> InitArgs {
        InitArgs {
            greenfield: false,
            mode: crate::commands::InitMode::Solo,
            site: None,
            prefix: None,
            force: false,
            yes: false,
            wizard: false,
            no_wizard: false,
            dry_run: false,
            no_hooks: false,
            append_agents_md: false,
            constitution_stub: false,
            constitution_from_docs: false,
            vscode: false,
            jetbrains: false,
        }
    }

    #[test]
    fn wizard_not_enabled_when_yes() {
        let mut value = args();
        value.yes = true;
        assert!(!should_run_wizard(&value));
    }

    #[test]
    fn wizard_not_enabled_when_dry_run() {
        let mut value = args();
        value.dry_run = true;
        assert!(!should_run_wizard(&value));
    }

    #[test]
    fn crew_recommendations_use_differentiated_roles() {
        let roles =
            recommended_role_bindings(crate::commands::InitMode::Crew, &available_model_ids());
        assert_eq!(roles.proposer, "claude-opus");
        assert_eq!(roles.drafter, "claude-sonnet");
        assert_eq!(roles.reviewer, "codex-gpt5");
    }

    fn resolved_with(plan: AiPlan) -> ResolvedInitOptions {
        ResolvedInitOptions {
            greenfield: true,
            mode: crate::commands::InitMode::Solo,
            site_name: "t".to_owned(),
            prefix: "tst".to_owned(),
            force: false,
            yes: true,
            dry_run: false,
            no_hooks: true,
            append_agents_md: false,
            constitution: ConstitutionMode::Reference,
            vscode: false,
            jetbrains: false,
            ai_plan: plan,
            spec_provider: crate::commands::spec_provider_init::SpecProviderChoice::Speckit,
            conventional_commits: true,
            branch_prefix: "feat/".to_owned(),
            default_profile: DEFAULT_PROFILE.to_owned(),
        }
    }

    /// Renders the bundled template, applies the AI plan, and loads the result
    /// as a real [`Config`] — proving the generated `derrick.yaml` parses.
    fn load_rendered(resolved: &ResolvedInitOptions) -> Config {
        let rendered = render_init_template(
            INIT_TEMPLATE,
            InitTemplateVars {
                site_name: &resolved.site_name,
                prefix: &resolved.prefix,
                mode: resolved.mode.as_str(),
            },
        );
        let out = apply_config_overrides(&rendered, resolved).expect("overrides apply");
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("derrick.yaml");
        std::fs::write(&path, out).expect("write");
        Config::load_from_path(&path).expect("generated config should load")
    }

    #[test]
    fn d79_preset_plan_writes_loadable_preset() {
        let config = load_rendered(&resolved_with(AiPlan::Preset("cli-defaults".to_owned())));
        assert!(config.models().get("strong").is_some());
        assert_eq!(
            config.models().get("executor").unwrap().resolved_runtime(),
            "copilot-cli"
        );
        assert_eq!(config.roles().get("proposer"), Some("strong"));
        // The static catalogue models are replaced by the preset's aliases.
        assert!(config.models().get("claude-opus").is_none());
    }

    #[test]
    fn d79_custom_plan_writes_runtime_keyed_model() {
        let plan = AiPlan::Custom {
            models: vec![(
                "default".to_owned(),
                ModelSpec {
                    runtime: "ollama".to_owned(),
                    model: "qwen2.5-coder:32b".to_owned(),
                    base_url: Some("http://localhost:11434".to_owned()),
                    auth_env: None,
                },
            )],
            roles: RoleBindings::one_model("default".to_owned()),
        };
        let config = load_rendered(&resolved_with(plan));
        let model = config.models().get("default").expect("default model");
        assert_eq!(model.resolved_runtime(), "ollama");
        assert_eq!(model.model(), "qwen2.5-coder:32b");
        assert_eq!(model.base_url(), Some("http://localhost:11434"));
        assert_eq!(config.roles().get("executor"), Some("default"));
        assert!(config.models().get("claude-opus").is_none());
    }

    #[test]
    fn d79_catalogue_plan_keeps_static_models() {
        let plan = AiPlan::Catalogue(recommended_role_bindings(
            crate::commands::InitMode::Solo,
            &available_model_ids(),
        ));
        let config = load_rendered(&resolved_with(plan));
        assert!(config.models().get("claude-opus").is_some());
    }

    #[test]
    fn d79_brownfield_text_path_handles_preset() {
        // apply_text_overrides delegates non-catalogue plans to the serde writer.
        let resolved = resolved_with(AiPlan::Preset("local-only".to_owned()));
        let rendered = render_init_template(
            INIT_TEMPLATE,
            InitTemplateVars {
                site_name: &resolved.site_name,
                prefix: &resolved.prefix,
                mode: resolved.mode.as_str(),
            },
        );
        let out = apply_text_overrides(&rendered, &resolved).expect("overrides apply");
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("derrick.yaml");
        std::fs::write(&path, out).expect("write");
        let config = Config::load_from_path(&path).expect("loads");
        assert_eq!(
            config.models().get("strong").unwrap().resolved_runtime(),
            "ollama"
        );
    }

    #[test]
    fn one_model_binds_all_roles() {
        let roles = RoleBindings::one_model("copilot".to_owned());
        for (_, model) in roles.entries() {
            assert_eq!(model, "copilot");
        }
    }
}
