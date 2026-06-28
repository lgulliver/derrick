use std::collections::{BTreeMap, HashSet};
use std::io::IsTerminal;
use std::path::Path;

use derrick_adopt::ConstitutionMode;
use inquire::validator::Validation;
use inquire::{Confirm, MultiSelect, Select, Text};

use crate::commands::init::{
    AiPlan, DEFAULT_PROFILE, ModelSpec, RoleBindings, available_model_ids,
    recommended_role_bindings, validate_prefix,
};
use crate::commands::spec_provider_init::SpecProviderChoice;

// ─── terminal style ──────────────────────────────────────────────────────────
//
// Styling is centralised in `crate::ui`; these thin aliases keep the local call
// sites readable and the splash banner self-contained.

use crate::ui;

fn is_styled() -> bool {
    ui::styled()
}

fn bold(s: &str) -> String {
    ui::bold(s)
}

fn dim(s: &str) -> String {
    ui::dim(s)
}

fn section_rule(title: &str) -> String {
    ui::section(title)
}

// ─── types ───────────────────────────────────────────────────────────────────

/// The CLI runtimes the "one CLI for everything" path offers (D79).
const CLI_RUNTIMES: [(&str, &str); 5] = [
    ("claude-cli", "claude-sonnet-4-6"),
    ("codex-cli", "gpt-5.5"),
    ("copilot-cli", "auto"),
    ("opencode-cli", "anthropic/claude-sonnet-4-6"),
    ("aider-cli", "anthropic/claude-sonnet-4-6"),
];

/// The direct-API runtimes and their default API-key env vars (D79).
const API_RUNTIMES: [(&str, &str); 3] = [
    ("anthropic-api", "ANTHROPIC_API_KEY"),
    ("openai-api", "OPENAI_API_KEY"),
    ("openai-compatible", "OPENAI_API_KEY"),
];

pub(crate) struct WizardInput<'a> {
    pub(crate) repo_root: &'a Path,
    pub(crate) has_existing_config: bool,
    pub(crate) likely_existing_project: bool,
    /// When true the wizard skips the init-type question and assumes greenfield.
    pub(crate) force_greenfield: bool,
    pub(crate) default_greenfield: bool,
    pub(crate) default_site_name: String,
    pub(crate) default_prefix: String,
    pub(crate) default_mode: crate::commands::InitMode,
    pub(crate) default_constitution: ConstitutionMode,
    pub(crate) default_append_agents_md: bool,
    pub(crate) no_hooks_forced: bool,
    pub(crate) default_vscode: bool,
    pub(crate) default_jetbrains: bool,
    pub(crate) default_force: bool,
    pub(crate) available_models: Vec<(&'static str, &'static str)>,
}

#[derive(Clone, Debug)]
pub(crate) struct WizardOutput {
    pub(crate) greenfield: bool,
    pub(crate) site_name: String,
    pub(crate) prefix: String,
    pub(crate) mode: crate::commands::InitMode,
    pub(crate) default_profile: String,
    pub(crate) ai_plan: AiPlan,
    pub(crate) spec_provider: SpecProviderChoice,
    pub(crate) constitution: ConstitutionMode,
    pub(crate) append_agents_md: bool,
    pub(crate) no_hooks: bool,
    pub(crate) vscode: bool,
    pub(crate) jetbrains: bool,
    pub(crate) force: bool,
    pub(crate) conventional_commits: bool,
    pub(crate) branch_prefix: String,
}

pub(crate) enum WizardSelection {
    Proceed(Box<WizardOutput>),
    Cancelled,
}

// ─── wizard entry point ───────────────────────────────────────────────────────

fn print_splash() {
    let styled = is_styled();
    println!();
    if styled {
        println!("  \x1b[1m╭─────────────────────────────────────────────────────────────╮\x1b[0m");
        println!(
            "  \x1b[1m│\x1b[0m                                                             \x1b[1m│\x1b[0m"
        );
        println!("  \x1b[1m│   ██████╗ ███████╗██████╗ ██████╗ ██╗ ██████╗██╗  ██╗       │\x1b[0m");
        println!("  \x1b[1m│   ██╔══██╗██╔════╝██╔══██╗██╔══██╗██║██╔════╝██║ ██╔╝       │\x1b[0m");
        println!("  \x1b[1m│   ██║  ██║█████╗  ██████╔╝██████╔╝██║██║     █████╔╝        │\x1b[0m");
        println!("  \x1b[1m│   ██║  ██║██╔══╝  ██╔══██╗██╔══██╗██║██║     ██╔═██╗        │\x1b[0m");
        println!("  \x1b[1m│   ██████╔╝███████╗██║  ██║██║  ██║██║╚██████╗██║  ██╗       │\x1b[0m");
        println!("  \x1b[1m│   ╚═════╝ ╚══════╝╚═╝  ╚═╝╚═╝  ╚═╝╚═╝ ╚═════╝╚═╝  ╚═╝       │\x1b[0m");
        println!(
            "  \x1b[1m│\x1b[0m                                                             \x1b[1m│\x1b[0m"
        );
        println!(
            "  \x1b[1m│\x1b[0m  \x1b[1msetup wizard\x1b[0m  \x1b[2m·  the load-bearing tower over an oil well\x1b[0m   \x1b[1m│\x1b[0m"
        );
        println!(
            "  \x1b[1m│\x1b[0m                                                             \x1b[1m│\x1b[0m"
        );
        println!("  \x1b[1m╰─────────────────────────────────────────────────────────────╯\x1b[0m");
    } else {
        println!("  DERRICK  setup wizard");
        println!("  The load-bearing tower over an oil well");
        println!("  {}", "─".repeat(62));
    }
    println!();
}

/// Runs the interactive `derrick init` wizard and returns the user's selections.
pub(crate) fn run(input: WizardInput<'_>) -> Result<WizardSelection, crate::CliError> {
    print_splash();
    print_info(&input);

    // Any prompt the user escapes (Esc / Ctrl-C) cancels the whole wizard.
    macro_rules! ask {
        ($e:expr) => {
            match $e? {
                Some(value) => value,
                None => return Ok(WizardSelection::Cancelled),
            }
        };
    }

    let greenfield = if input.force_greenfield {
        println!(
            "  {:<9}  {}",
            "init type",
            dim("fresh repo — greenfield assumed")
        );
        println!();
        true
    } else {
        ask!(ask_select(
            "What are you setting up?",
            &["Adopt existing repo", "Start fresh"],
            usize::from(input.default_greenfield),
        )) == 1
    };

    let site_name = ask!(ask_text("Project name", &input.default_site_name, false));
    let prefix = ask!(ask_text("Ticket prefix", &input.default_prefix, true));
    let branch_prefix = ask!(ask_text("Branch naming prefix", "feat/", false));

    let mode = match ask!(ask_select(
        "Operating mode",
        &[
            "solo      local-first, minimal orchestration",
            "copilot   optimised for GitHub Copilot CLI workflows",
            "crew      richer multi-role orchestration",
        ],
        mode_to_index(input.default_mode),
    )) {
        0 => crate::commands::InitMode::Solo,
        1 => crate::commands::InitMode::Copilot,
        _ => crate::commands::InitMode::Crew,
    };

    let profile_options: &[(&str, &str)] = &[
        (
            "balanced",
            "balanced   good quality at reasonable speed (recommended)",
        ),
        ("speed", "speed      optimise for latency"),
        ("quality", "quality    maximum reasoning quality"),
        ("cheap", "cheap      optimise for lowest cost"),
        ("local", "local      local runtimes only"),
        ("ci", "ci         non-interactive, deterministic"),
    ];
    let profile_labels: Vec<&str> = profile_options.iter().map(|(_, label)| *label).collect();
    let default_profile_idx = profile_options
        .iter()
        .position(|(alias, _)| *alias == DEFAULT_PROFILE)
        .unwrap_or(0);
    let default_profile = profile_options[ask!(ask_select(
        "Default AI profile?",
        &profile_labels,
        default_profile_idx
    ))]
    .0;

    let available_model_ids = available_model_ids();
    let role_defaults = recommended_role_bindings(mode, &available_model_ids);
    let ai_plan = match ask!(ask_select(
        "How do you want Derrick to use AI?",
        &[
            "Use my installed AI CLIs (recommended)",
            "Use one CLI for everything",
            "Choose per stage",
            "Use API keys / custom provider",
            "Local / self-hosted (Ollama, LM Studio, vLLM, …)",
        ],
        0,
    )) {
        // 1. Installed CLIs → a preset that generates ordinary config (D79).
        0 => {
            let preset_index = ask!(ask_select("Which CLI preset?", &derrick_config::PRESETS, 0,));
            AiPlan::Preset(derrick_config::PRESETS[preset_index].to_owned())
        }
        // 2. One CLI for everything → bind every stage to a single runtime.
        1 => {
            let runtime_index = ask!(ask_select(
                "Which CLI runtime?",
                &CLI_RUNTIMES.map(|(runtime, _)| runtime),
                0,
            ));
            let (runtime, default_model) = CLI_RUNTIMES[runtime_index];
            let model = ask!(ask_required_text("Model id (or `auto`)", default_model));
            single_runtime_plan(runtime, &model, None, None)
        }
        // 3. Choose per stage → catalogue alias per role (unchanged mechanism).
        2 => AiPlan::Catalogue(RoleBindings {
            proposer: ask!(ask_model(
                "Planning / proposal",
                &input.available_models,
                role_defaults.proposer.as_str(),
            )),
            drafter: ask!(ask_model(
                "Drafting specs/tasks",
                &input.available_models,
                role_defaults.drafter.as_str(),
            )),
            reviewer: ask!(ask_model(
                "Review / critique",
                &input.available_models,
                role_defaults.reviewer.as_str(),
            )),
            executor: ask!(ask_model(
                "Execution / implementation",
                &input.available_models,
                role_defaults.executor.as_str(),
            )),
            summariser: ask!(ask_model(
                "Summary / handoff",
                &input.available_models,
                role_defaults.summariser.as_str(),
            )),
        }),
        // 4. API keys / custom provider.
        3 => {
            let runtime_index = ask!(ask_select(
                "Which API runtime?",
                &API_RUNTIMES.map(|(runtime, _)| runtime),
                0,
            ));
            let (runtime, default_auth_env) = API_RUNTIMES[runtime_index];
            let model = ask!(ask_required_text("Model id", ""));
            let base_url = ask!(ask_text(
                "Base URL (blank for the provider default)",
                "",
                false,
            ));
            let auth_env = ask!(ask_text("API-key env var", default_auth_env, false));
            single_runtime_plan(runtime, &model, non_empty(base_url), non_empty(auth_env))
        }
        // 5. Local / self-hosted.
        _ => {
            let runtime_index = ask!(ask_select(
                "Which local runtime?",
                &["ollama", "openai-compatible (LM Studio / vLLM / LiteLLM)",],
                0,
            ));
            let (runtime, default_base_url) = if runtime_index == 0 {
                ("ollama", "http://localhost:11434")
            } else {
                ("openai-compatible", "http://localhost:8000/v1")
            };
            let model = ask!(ask_required_text("Model id", "qwen2.5-coder:32b"));
            let base_url = ask!(ask_text("Base URL", default_base_url, false));
            single_runtime_plan(runtime, &model, non_empty(base_url), None)
        }
    };

    if let AiPlan::Catalogue(roles) = &ai_plan {
        validate_role_models(roles, &available_model_ids)?;
    }

    // How should derrick produce specs? Speckit is the default & recommended
    // path and leaves the generated config untouched; native and import switch
    // `tools.specify.provider` and route the bare spec steps through the seam.
    let spec_provider = match ask!(ask_select(
        "How should derrick produce specs?",
        &[
            "speckit   delegate to the speckit host CLI (default & recommended)",
            "native    derrick-native spec generation",
            "import    bring your own externally-authored spec",
        ],
        0,
    )) {
        0 => SpecProviderChoice::Speckit,
        1 => SpecProviderChoice::Native,
        _ => SpecProviderChoice::Import,
    };

    let constitution = if greenfield {
        ConstitutionMode::Reference
    } else {
        match ask!(ask_select(
            "Constitution setup",
            &[
                "Reference existing docs",
                "Generate stub",
                "Draft from docs",
            ],
            constitution_to_index(input.default_constitution),
        )) {
            0 => ConstitutionMode::Reference,
            1 => ConstitutionMode::Stub,
            _ => ConstitutionMode::FromDocs,
        }
    };

    // Collapse the old chain of yes/no toggles into one multi-select with
    // sensible defaults pre-checked.
    let hooks_offered = !greenfield && !input.no_hooks_forced;
    let mut toggles: Vec<(Toggle, &str, bool)> =
        vec![(Toggle::ConventionalCommits, "Conventional commits", true)];
    if !greenfield {
        toggles.push((
            Toggle::AppendAgentsMd,
            "Append AGENTS.md guidance",
            input.default_append_agents_md,
        ));
    }
    if hooks_offered {
        toggles.push((Toggle::Hooks, "Install Codex instructions / hooks", true));
    }
    toggles.push((Toggle::VsCode, "VS Code tasks", input.default_vscode));
    toggles.push((
        Toggle::JetBrains,
        "JetBrains run configurations",
        input.default_jetbrains,
    ));
    toggles.push((
        Toggle::Force,
        "Force overwrite existing files",
        input.default_force,
    ));

    let labels: Vec<&str> = toggles.iter().map(|(_, label, _)| *label).collect();
    let defaults: Vec<usize> = toggles
        .iter()
        .enumerate()
        .filter(|(_, (_, _, on))| *on)
        .map(|(index, _)| index)
        .collect();
    let chosen_indices = ask!(ask_multiselect(
        "Options  (↑↓ move · space toggles · enter confirms)",
        &labels,
        &defaults,
    ));
    let chosen: HashSet<Toggle> = chosen_indices
        .into_iter()
        .map(|index| toggles[index].0)
        .collect();

    let conventional_commits = chosen.contains(&Toggle::ConventionalCommits);
    let append_agents_md = !greenfield && chosen.contains(&Toggle::AppendAgentsMd);
    let no_hooks = if hooks_offered {
        !chosen.contains(&Toggle::Hooks)
    } else {
        input.no_hooks_forced
    };
    let vscode = chosen.contains(&Toggle::VsCode);
    let jetbrains = chosen.contains(&Toggle::JetBrains);
    let force = chosen.contains(&Toggle::Force);

    let output = WizardOutput {
        greenfield,
        site_name,
        prefix,
        mode,
        default_profile: default_profile.to_owned(),
        ai_plan,
        spec_provider,
        constitution,
        append_agents_md,
        no_hooks,
        vscode,
        jetbrains,
        force,
        conventional_commits,
        branch_prefix,
    };

    print_preview(&input, &output);

    if !ask!(ask_confirm("Proceed?", true)) {
        return Ok(WizardSelection::Cancelled);
    }

    Ok(WizardSelection::Proceed(Box::new(output)))
}

/// Builds a [`AiPlan::Custom`] that binds every stage to a single runtime/model
/// alias named `default` (D79). Used by the one-CLI, API, and local paths.
fn single_runtime_plan(
    runtime: &str,
    model: &str,
    base_url: Option<String>,
    auth_env: Option<String>,
) -> AiPlan {
    let spec = ModelSpec {
        runtime: runtime.to_owned(),
        model: model.trim().to_owned(),
        base_url,
        auth_env,
    };
    AiPlan::Custom {
        models: vec![("default".to_owned(), spec)],
        roles: RoleBindings::one_model("default".to_owned()),
    }
}

/// Returns `Some(trimmed)` when the user entered a non-blank value, else `None`.
fn non_empty(value: String) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

/// Keys for the consolidated options multi-select.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum Toggle {
    ConventionalCommits,
    AppendAgentsMd,
    Hooks,
    VsCode,
    JetBrains,
    Force,
}

fn print_info(input: &WizardInput<'_>) {
    println!("  {:<9}  {}", "repo", input.repo_root.display());
    println!(
        "  {:<9}  {}",
        "config",
        if input.has_existing_config {
            "found".to_owned()
        } else {
            dim("not found")
        }
    );
    println!(
        "  {:<9}  {}",
        "status",
        if input.likely_existing_project {
            "existing project"
        } else {
            "new project"
        }
    );
    println!();
}

// ─── preview ─────────────────────────────────────────────────────────────────

fn print_preview(input: &WizardInput<'_>, output: &WizardOutput) {
    let WizardOutput {
        greenfield,
        site_name,
        prefix,
        mode,
        default_profile,
        ai_plan,
        spec_provider,
        constitution,
        append_agents_md,
        no_hooks,
        vscode,
        jetbrains,
        force,
        conventional_commits,
        branch_prefix,
    } = output;

    let top_rule = format!("  ╭─ Preview {}", "─".repeat(52));
    let bottom_rule = format!("  ╰{}", "─".repeat(62));
    let blank = "  │".to_owned();

    let kv = |label: &str, value: &str| -> String { format!("  │  {label:<15}  {value}") };
    let sub_kv = |label: &str, value: &str| -> String { format!("  │    {label:<13}  {value}") };
    let section = |title: &str| -> String { format!("  │  {}", bold(title)) };
    let bullet = |value: &str| -> String { format!("  │    {}  {value}", dim("·")) };

    println!();
    println!("{top_rule}");
    println!("{blank}");
    println!(
        "{}",
        kv("Repository", &input.repo_root.display().to_string())
    );
    println!(
        "{}",
        kv(
            "Init type",
            if *greenfield {
                "fresh project"
            } else {
                "existing repo"
            }
        )
    );
    println!("{}", kv("Project", site_name));
    println!("{}", kv("Prefix", prefix));
    println!("{}", kv("Mode", mode.as_str()));
    println!("{}", kv("Profile", default_profile));
    println!("{}", kv("AI config", &ai_plan.label()));
    println!("{}", kv("Spec provider", spec_provider.label()));
    if !greenfield {
        println!("{}", kv("Constitution", constitution_label(*constitution)));
        println!(
            "{}",
            kv(
                "AGENTS.md",
                if *append_agents_md { "append" } else { "skip" }
            )
        );
    }
    println!(
        "{}",
        kv("Hooks", if *no_hooks { "disabled" } else { "enabled" })
    );
    println!("{}", kv("VS Code", yes_no(*vscode)));
    println!("{}", kv("JetBrains", yes_no(*jetbrains)));
    println!("{}", kv("Force", yes_no(*force)));
    println!("{}", kv("Conv. commits", yes_no(*conventional_commits)));
    println!("{}", kv("Branch prefix", branch_prefix));
    println!("{blank}");
    println!("{}", section("Role bindings"));
    match ai_plan.roles() {
        Some(roles) => {
            println!("{}", sub_kv("Planning", &roles.proposer));
            println!("{}", sub_kv("Drafting", &roles.drafter));
            println!("{}", sub_kv("Review", &roles.reviewer));
            println!("{}", sub_kv("Execution", &roles.executor));
            println!("{}", sub_kv("Summary", &roles.summariser));
        }
        None => {
            println!("{}", sub_kv("Generated by", &ai_plan.label()));
        }
    }
    println!("{blank}");
    println!("{}", section("Files to write"));
    println!("{}", bullet("derrick.yaml"));
    println!("{}", bullet(".derrick/.gitignore"));
    println!("{}", bullet(".derrick/derrick.db"));
    if !no_hooks {
        println!("{}", bullet(".codex/instructions.md"));
        println!("{}", bullet(".claude/settings.json"));
        println!("{}", bullet(".claude/commands/speckit.*.md  (+4 others)"));
    }
    if *vscode {
        println!("{}", bullet(".vscode/tasks.json"));
    }
    if *jetbrains {
        println!("{}", bullet(".idea/runConfigurations/*.xml"));
    }
    println!("{blank}");
    println!("{bottom_rule}");
    println!();
}

// ─── prompt helpers ───────────────────────────────────────────────────────────

/// Maps an inquire result into `Option<T>`: `Esc`/`Ctrl-C` becomes `None`
/// (cancel the wizard); any other error surfaces as a `CliError`.
fn inquire_opt<T>(result: inquire::error::InquireResult<T>) -> Result<Option<T>, crate::CliError> {
    use inquire::InquireError::{OperationCanceled, OperationInterrupted};
    match result {
        Ok(value) => Ok(Some(value)),
        Err(OperationCanceled | OperationInterrupted) => Ok(None),
        Err(error) => Err(crate::message(format!("wizard prompt failed: {error}"))),
    }
}

/// A free-text prompt with an inline default. When `prefix` is set, the input
/// is validated as a ticket prefix and re-asked until valid.
fn ask_text(
    prompt: &str,
    default: &str,
    is_prefix: bool,
) -> Result<Option<String>, crate::CliError> {
    let mut text = Text::new(prompt).with_default(default);
    if is_prefix {
        text = text.with_validator(|input: &str| match validate_prefix(input) {
            Ok(()) => Ok(Validation::Valid),
            Err(error) => Ok(Validation::Invalid(error.to_string().into())),
        });
    }
    inquire_opt(text.prompt())
}

/// A free-text prompt that rejects a blank/whitespace-only answer, re-asking
/// until a non-empty value is entered. Used for model ids (D79): an empty id
/// would otherwise produce `model: ""` in the generated config and fail later
/// in a confusing way.
fn ask_required_text(prompt: &str, default: &str) -> Result<Option<String>, crate::CliError> {
    let text = Text::new(prompt)
        .with_default(default)
        .with_validator(|input: &str| {
            if input.trim().is_empty() {
                Ok(Validation::Invalid("a value is required".into()))
            } else {
                Ok(Validation::Valid)
            }
        });
    inquire_opt(text.prompt())
}

/// An arrow-key single-select returning the chosen option's index.
fn ask_select(
    prompt: &str,
    options: &[&str],
    default_index: usize,
) -> Result<Option<usize>, crate::CliError> {
    let select = Select::new(prompt, options.to_vec()).with_starting_cursor(default_index);
    Ok(inquire_opt(select.raw_prompt())?.map(|choice| choice.index))
}

/// A yes/no confirm with a default.
fn ask_confirm(prompt: &str, default_yes: bool) -> Result<Option<bool>, crate::CliError> {
    inquire_opt(Confirm::new(prompt).with_default(default_yes).prompt())
}

/// A multi-select returning the chosen option indices; `defaults` are the
/// indices pre-checked when the prompt opens.
fn ask_multiselect(
    prompt: &str,
    options: &[&str],
    defaults: &[usize],
) -> Result<Option<Vec<usize>>, crate::CliError> {
    let select = MultiSelect::new(prompt, options.to_vec()).with_default(defaults);
    Ok(inquire_opt(select.raw_prompt())?
        .map(|chosen| chosen.into_iter().map(|choice| choice.index).collect()))
}

/// A model picker: lists `id (description)` and returns the chosen model id,
/// starting on `default_model_id`.
fn ask_model(
    prompt: &str,
    models: &[(&str, &str)],
    default_model_id: &str,
) -> Result<Option<String>, crate::CliError> {
    let default = models
        .iter()
        .position(|(id, _)| *id == default_model_id)
        .unwrap_or(0);
    let labels: Vec<String> = models
        .iter()
        .map(|(id, description)| format!("{id}  ({description})"))
        .collect();
    let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
    Ok(ask_select(prompt, &label_refs, default)?.map(|index| models[index].0.to_owned()))
}

// ─── validation ──────────────────────────────────────────────────────────────

fn validate_role_models(
    roles: &RoleBindings,
    available_models: &BTreeMap<String, &'static str>,
) -> Result<(), crate::CliError> {
    for (role, model) in roles.entries() {
        if !available_models.contains_key(model) {
            return Err(crate::message(format!(
                "role `{role}` points to model `{model}`, but no model named `{model}` exists under `models`"
            )));
        }
    }
    Ok(())
}

// ─── helpers ─────────────────────────────────────────────────────────────────

fn mode_to_index(mode: crate::commands::InitMode) -> usize {
    match mode {
        crate::commands::InitMode::Solo => 0,
        crate::commands::InitMode::Copilot => 1,
        crate::commands::InitMode::Crew => 2,
    }
}

fn constitution_to_index(mode: ConstitutionMode) -> usize {
    match mode {
        ConstitutionMode::Reference => 0,
        ConstitutionMode::Stub => 1,
        ConstitutionMode::FromDocs => 2,
        _ => 0,
    }
}

fn constitution_label(mode: ConstitutionMode) -> &'static str {
    match mode {
        ConstitutionMode::Reference => "reference existing docs",
        ConstitutionMode::Stub => "generate stub",
        ConstitutionMode::FromDocs => "draft from docs",
        _ => "reference existing docs",
    }
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

// ─── constitution seeding ─────────────────────────────────────────────────────

/// Answers collected from the user to seed the initial constitution.
#[derive(Clone, Debug, Default)]
pub(crate) struct ConstitutionSeeds {
    pub(crate) language: String,
    pub(crate) testing: ConstitutionTestingStyle,
    pub(crate) architecture: String,
    pub(crate) style: String,
}

/// Testing philosophy selected during init.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum ConstitutionTestingStyle {
    #[default]
    UnitOnly,
    UnitAndIntegration,
    TestDriven,
    PropertyBased,
}

impl ConstitutionTestingStyle {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::UnitOnly => "unit tests only",
            Self::UnitAndIntegration => "unit + integration tests",
            Self::TestDriven => "TDD — write tests first",
            Self::PropertyBased => "property-based / fuzzing",
        }
    }
}

/// Prompt the user for constitution seeds.
///
/// Returns defaults immediately when `yes` is `true` or stdin is not a terminal
/// (non-interactive mode).
pub(crate) fn prompt_constitution(yes: bool) -> Result<ConstitutionSeeds, crate::CliError> {
    if yes || !std::io::stdin().is_terminal() {
        return Ok(ConstitutionSeeds::default());
    }

    println!();
    println!("{}", section_rule("project constitution"));
    println!();
    println!(
        "  {}",
        bold("The constitution tells the plan reviewer what rules to enforce.")
    );
    println!("  Answer a few questions — you can edit the file any time.");

    // Escaping any prompt here falls back to default (empty) seeds rather than
    // aborting init — the constitution can always be edited afterwards.
    let Some(language) = ask_text(
        "Primary language(s)  [e.g. Go, TypeScript, Rust]",
        "",
        false,
    )?
    else {
        return Ok(ConstitutionSeeds::default());
    };

    let testing = match ask_select(
        "Testing approach",
        &[
            "unit tests only",
            "unit + integration tests",
            "TDD — write tests first",
            "property-based / fuzzing",
        ],
        0,
    )? {
        Some(0) => ConstitutionTestingStyle::UnitOnly,
        Some(1) => ConstitutionTestingStyle::UnitAndIntegration,
        Some(2) => ConstitutionTestingStyle::TestDriven,
        Some(_) => ConstitutionTestingStyle::PropertyBased,
        None => return Ok(ConstitutionSeeds::default()),
    };

    let architecture = ask_text(
        "Architectural constraints  (optional, free text)",
        "",
        false,
    )?
    .unwrap_or_default();
    let style =
        ask_text("Style / linting notes  (optional, free text)", "", false)?.unwrap_or_default();

    Ok(ConstitutionSeeds {
        language,
        testing,
        architecture,
        style,
    })
}

/// Render `seeds` into the markdown constitution file content.
///
/// The output deliberately avoids the `<!-- DERRICK-DRAFT:` banner and
/// `[PROJECT_NAME]` placeholder so `constitution_needs_setup` returns `false`
/// and the assay step is not silently skipped.
pub(crate) fn format_constitution(site_name: &str, seeds: &ConstitutionSeeds) -> String {
    let mut out = format!("# {site_name} — Engineering Constitution\n");
    out.push_str(
        "\nThis file captures durable rules that derrick's plan reviewer enforces.\n\
         Edit it any time to add project-specific constraints.\n",
    );

    out.push_str("\n## Language and Stack\n\n");
    if seeds.language.is_empty() {
        out.push_str("No specific language constraint. Auto-detect from the codebase.\n");
    } else {
        out.push_str(&format!("Primary: {}\n", seeds.language));
    }

    out.push_str("\n## Testing Requirements\n\n");
    out.push_str(&format!("- {}\n", seeds.testing.label()));

    out.push_str("\n## Architectural Constraints\n\n");
    if seeds.architecture.is_empty() {
        out.push_str(
            "None specified at setup time. \
             Add project-specific constraints here.\n",
        );
    } else {
        for line in seeds.architecture.lines() {
            out.push_str(&format!("- {line}\n"));
        }
    }

    out.push_str("\n## Style and Linting\n\n");
    if seeds.style.is_empty() {
        out.push_str(
            "No specific style constraints. \
             Follow language-idiomatic conventions.\n",
        );
    } else {
        for line in seeds.style.lines() {
            out.push_str(&format!("- {line}\n"));
        }
    }

    out
}

// ─── tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_runtime_plan_binds_all_roles_to_one_alias() {
        let plan = single_runtime_plan("ollama", "  llama3.2 ", Some("u".to_owned()), None);
        match plan {
            AiPlan::Custom { models, roles } => {
                assert_eq!(models.len(), 1);
                let (alias, spec) = &models[0];
                assert_eq!(alias, "default");
                assert_eq!(spec.runtime, "ollama");
                assert_eq!(spec.model, "llama3.2"); // trimmed
                assert_eq!(spec.base_url.as_deref(), Some("u"));
                assert_eq!(roles.executor, "default");
                assert_eq!(roles.proposer, "default");
            }
            other => panic!("expected Custom, got {other:?}"),
        }
    }

    #[test]
    fn non_empty_trims_and_nullifies_blanks() {
        assert_eq!(non_empty("  ".to_owned()), None);
        assert_eq!(non_empty(" x ".to_owned()), Some("x".to_owned()));
    }

    #[test]
    fn role_validation_rejects_unknown_model() {
        let roles = RoleBindings {
            proposer: "missing".to_owned(),
            drafter: "claude-sonnet".to_owned(),
            reviewer: "codex-gpt5".to_owned(),
            executor: "copilot".to_owned(),
            summariser: "claude-sonnet".to_owned(),
        };
        let error = validate_role_models(&roles, &available_model_ids())
            .expect_err("should reject unknown model");
        assert!(error.to_string().contains("proposer"));
    }

    #[test]
    fn constitution_testing_style_labels_are_non_empty() {
        for style in [
            ConstitutionTestingStyle::UnitOnly,
            ConstitutionTestingStyle::UnitAndIntegration,
            ConstitutionTestingStyle::TestDriven,
            ConstitutionTestingStyle::PropertyBased,
        ] {
            assert!(!style.label().is_empty());
        }
    }

    #[test]
    fn format_constitution_empty_seeds_passes_needs_setup_check() {
        let seeds = ConstitutionSeeds::default();
        let content = format_constitution("myproject", &seeds);
        // Must not trigger constitution_needs_setup (no draft banner, no [PROJECT_NAME])
        assert!(!content.contains("DERRICK-DRAFT"));
        assert!(!content.contains("[PROJECT_NAME]"));
        assert!(content.contains("myproject"));
    }

    #[test]
    fn format_constitution_populated_seeds_contains_answers() {
        let seeds = ConstitutionSeeds {
            language: "Go".to_owned(),
            testing: ConstitutionTestingStyle::TestDriven,
            architecture: "no global state".to_owned(),
            style: "gofmt required".to_owned(),
        };
        let content = format_constitution("widget", &seeds);
        assert!(content.contains("Go"));
        assert!(content.contains("TDD — write tests first"));
        assert!(content.contains("no global state"));
        assert!(content.contains("gofmt required"));
        assert!(!content.contains("DERRICK-DRAFT"));
        assert!(!content.contains("[PROJECT_NAME]"));
    }
}
