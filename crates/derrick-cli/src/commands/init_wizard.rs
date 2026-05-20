use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::Path;

use derrick_adopt::ConstitutionMode;

use crate::commands::init::{
    available_model_ids, recommended_role_bindings, validate_prefix, RoleBindings,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AiConfigurationStyle {
    Recommended,
    OneTool,
    PerStage,
}

impl AiConfigurationStyle {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Recommended => "recommended defaults",
            Self::OneTool => "one tool for all stages",
            Self::PerStage => "per-stage configuration",
        }
    }
}

pub(crate) struct WizardInput<'a> {
    pub(crate) repo_root: &'a Path,
    pub(crate) has_existing_config: bool,
    pub(crate) likely_existing_project: bool,
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
    pub(crate) roles: RoleBindings,
    pub(crate) ai_style: AiConfigurationStyle,
    pub(crate) constitution: ConstitutionMode,
    pub(crate) append_agents_md: bool,
    pub(crate) no_hooks: bool,
    pub(crate) vscode: bool,
    pub(crate) jetbrains: bool,
    pub(crate) force: bool,
}

pub(crate) enum WizardSelection {
    Proceed(WizardOutput),
    Cancelled,
}

pub(crate) fn run(input: WizardInput<'_>) -> Result<WizardSelection, crate::CliError> {
    println!("Derrick setup wizard");
    println!("Derrick will initialise orchestration config for this repository.");
    println!("Repository: {}", input.repo_root.display());
    println!(
        "Existing derrick.yaml: {}",
        yes_no(input.has_existing_config)
    );
    println!(
        "Looks like an existing project: {}",
        yes_no(input.likely_existing_project)
    );
    println!();

    let init_type = prompt_select(
        "What are you setting up?",
        &["Adopt existing repo", "Start fresh"],
        if input.default_greenfield { 1 } else { 0 },
    )?;
    let greenfield = init_type == 1;

    let site_name = prompt_text("Project name", &input.default_site_name)?;

    let prefix = loop {
        let value = prompt_text("Derrick prefix / Ticket prefix", &input.default_prefix)?;
        match validate_prefix(&value) {
            Ok(()) => break value,
            Err(error) => {
                eprintln!("{error}");
                eprintln!("Please use lowercase ASCII, 1 to 6 characters.");
            }
        }
    };

    let mode = match prompt_select(
        "Operating mode",
        &[
            "solo — local-first, minimal orchestration",
            "copilot — optimised for GitHub Copilot CLI workflows",
            "crew — richer multi-role orchestration",
        ],
        mode_to_index(input.default_mode),
    )? {
        0 => crate::commands::InitMode::Solo,
        1 => crate::commands::InitMode::Copilot,
        _ => crate::commands::InitMode::Crew,
    };

    println!();
    println!("AI tools");
    println!("Derrick can use different AI tools for different stages. You can keep the recommended defaults or choose per stage.");

    let available_model_ids = available_model_ids();
    let role_defaults = recommended_role_bindings(mode, &available_model_ids);
    let ai_mode = prompt_select(
        "How would you like to configure AI tools?",
        &[
            "Use recommended defaults",
            "Use one tool for all stages",
            "Choose per stage",
        ],
        0,
    )?;

    let (ai_style, roles) = match ai_mode {
        0 => (AiConfigurationStyle::Recommended, role_defaults),
        1 => {
            let selected = prompt_model(
                "Select one tool/model for all stages",
                &input.available_models,
                role_defaults.executor.as_str(),
            )?;
            (
                AiConfigurationStyle::OneTool,
                RoleBindings::one_model(selected),
            )
        }
        _ => {
            let proposer = prompt_model(
                "Planning / proposal",
                &input.available_models,
                role_defaults.proposer.as_str(),
            )?;
            let drafter = prompt_model(
                "Drafting specs/tasks",
                &input.available_models,
                role_defaults.drafter.as_str(),
            )?;
            let reviewer = prompt_model(
                "Review / critique",
                &input.available_models,
                role_defaults.reviewer.as_str(),
            )?;
            let executor = prompt_model(
                "Execution / implementation",
                &input.available_models,
                role_defaults.executor.as_str(),
            )?;
            let summariser = prompt_model(
                "Summary / handoff",
                &input.available_models,
                role_defaults.summariser.as_str(),
            )?;
            (
                AiConfigurationStyle::PerStage,
                RoleBindings {
                    proposer,
                    drafter,
                    reviewer,
                    executor,
                    summariser,
                },
            )
        }
    };

    validate_role_models(&roles, &available_model_ids)?;

    let (constitution, append_agents_md, no_hooks) = if greenfield {
        (ConstitutionMode::Reference, false, input.no_hooks_forced)
    } else {
        let constitution = match prompt_select(
            "Constitution setup",
            &[
                "Reference existing docs",
                "Generate stub",
                "Draft from docs",
            ],
            constitution_to_index(input.default_constitution),
        )? {
            0 => ConstitutionMode::Reference,
            1 => ConstitutionMode::Stub,
            _ => ConstitutionMode::FromDocs,
        };
        let append_agents_md =
            prompt_yes_no("Append AGENTS.md guidance?", input.default_append_agents_md)?;
        let no_hooks = if input.no_hooks_forced {
            true
        } else {
            !prompt_yes_no("Install Codex instructions/hooks?", true)?
        };
        (constitution, append_agents_md, no_hooks)
    };

    let vscode = prompt_yes_no("Write VS Code tasks config?", input.default_vscode)?;
    let jetbrains = prompt_yes_no(
        "Write JetBrains run configurations?",
        input.default_jetbrains,
    )?;
    let force = prompt_yes_no("Enable force overwrite?", input.default_force)?;

    print_preview(
        &input,
        greenfield,
        &site_name,
        &prefix,
        mode,
        ai_style,
        &roles,
        constitution,
        append_agents_md,
        no_hooks,
        vscode,
        jetbrains,
        force,
    );

    if !prompt_yes_no("Proceed with these changes?", true)? {
        return Ok(WizardSelection::Cancelled);
    }

    Ok(WizardSelection::Proceed(WizardOutput {
        greenfield,
        site_name,
        prefix,
        mode,
        roles,
        ai_style,
        constitution,
        append_agents_md,
        no_hooks,
        vscode,
        jetbrains,
        force,
    }))
}

fn print_preview(
    input: &WizardInput<'_>,
    greenfield: bool,
    site_name: &str,
    prefix: &str,
    mode: crate::commands::InitMode,
    ai_style: AiConfigurationStyle,
    roles: &RoleBindings,
    constitution: ConstitutionMode,
    append_agents_md: bool,
    no_hooks: bool,
    vscode: bool,
    jetbrains: bool,
    force: bool,
) {
    println!();
    println!("Preview");
    println!("Repository path: {}", input.repo_root.display());
    println!(
        "Init type: {}",
        if greenfield {
            "fresh project"
        } else {
            "existing repo"
        }
    );
    println!("Project name: {site_name}");
    println!("Derrick prefix / Ticket prefix: {prefix}");
    println!("Operating mode: {}", mode.as_str());
    println!("AI tool configuration: {}", ai_style.label());
    println!("Role bindings:");
    println!("  Planning: {}", roles.proposer);
    println!("  Drafting: {}", roles.drafter);
    println!("  Review: {}", roles.reviewer);
    println!("  Execution: {}", roles.executor);
    println!("  Summary: {}", roles.summariser);
    if !greenfield {
        println!(
            "Constitution handling: {}",
            constitution_label(constitution)
        );
        println!(
            "AGENTS.md guidance: {}",
            if append_agents_md { "yes" } else { "no" }
        );
    }
    println!(
        "Codex instructions/hooks enabled: {}",
        if no_hooks { "no" } else { "yes" }
    );
    println!("VS Code config: {}", yes_no(vscode));
    println!("JetBrains config: {}", yes_no(jetbrains));
    println!("Force overwrite enabled: {}", yes_no(force));
    println!("Expected writes:");
    println!("  - derrick.yaml");
    println!("  - .derrick/.gitignore");
    println!("  - .derrick/derrick.db");
    if !no_hooks {
        println!("  - .codex/instructions.md");
    }
    if vscode {
        println!("  - .vscode/tasks.json");
    }
    if jetbrains {
        println!("  - .idea/runConfigurations/*.xml");
    }
    println!();
}

fn prompt_text(prompt: &str, default: &str) -> Result<String, crate::CliError> {
    print!("{prompt} [{default}]: ");
    io::stdout().flush().map_err(|error| crate::CliError::Io {
        path: "<stdout>".into(),
        source: error,
    })?;
    let mut buffer = String::new();
    let read = io::stdin()
        .read_line(&mut buffer)
        .map_err(|error| crate::CliError::Io {
            path: "<stdin>".into(),
            source: error,
        })?;
    if read == 0 {
        return Ok(default.to_owned());
    }
    let value = buffer.trim();
    if value.is_empty() {
        Ok(default.to_owned())
    } else {
        Ok(value.to_owned())
    }
}

fn prompt_yes_no(prompt: &str, default_yes: bool) -> Result<bool, crate::CliError> {
    let default_label = if default_yes { "Y/n" } else { "y/N" };
    loop {
        print!("{prompt} [{default_label}]: ");
        io::stdout().flush().map_err(|error| crate::CliError::Io {
            path: "<stdout>".into(),
            source: error,
        })?;
        let mut buffer = String::new();
        let read = io::stdin()
            .read_line(&mut buffer)
            .map_err(|error| crate::CliError::Io {
                path: "<stdin>".into(),
                source: error,
            })?;
        if read == 0 {
            return Ok(default_yes);
        }
        let answer = buffer.trim().to_ascii_lowercase();
        if answer.is_empty() {
            return Ok(default_yes);
        }
        if matches!(answer.as_str(), "y" | "yes") {
            return Ok(true);
        }
        if matches!(answer.as_str(), "n" | "no") {
            return Ok(false);
        }
        eprintln!("Please answer yes or no.");
    }
}

fn prompt_select(
    prompt: &str,
    options: &[&str],
    default_index: usize,
) -> Result<usize, crate::CliError> {
    println!("{prompt}");
    for (index, option) in options.iter().enumerate() {
        println!("  {}. {}", index + 1, option);
    }
    loop {
        let default_display = default_index + 1;
        print!("Select [default {default_display}]: ");
        io::stdout().flush().map_err(|error| crate::CliError::Io {
            path: "<stdout>".into(),
            source: error,
        })?;
        let mut buffer = String::new();
        let read = io::stdin()
            .read_line(&mut buffer)
            .map_err(|error| crate::CliError::Io {
                path: "<stdin>".into(),
                source: error,
            })?;
        if read == 0 {
            return Ok(default_index);
        }
        let trimmed = buffer.trim();
        if trimmed.is_empty() {
            return Ok(default_index);
        }
        if let Ok(choice) = trimmed.parse::<usize>() {
            if (1..=options.len()).contains(&choice) {
                return Ok(choice - 1);
            }
        }
        eprintln!("Please enter a number between 1 and {}.", options.len());
    }
}

fn prompt_model(
    prompt: &str,
    models: &[(&str, &str)],
    default_model_id: &str,
) -> Result<String, crate::CliError> {
    let default = models
        .iter()
        .position(|(id, _)| *id == default_model_id)
        .unwrap_or(0);
    println!("{prompt}");
    for (index, (id, description)) in models.iter().enumerate() {
        println!("  {}. {} ({})", index + 1, id, description);
    }
    let selected = prompt_select(
        "Choose model",
        &models.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        default,
    )?;
    Ok(models[selected].0.to_owned())
}

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
        ConstitutionMode::Reference => "Reference existing docs",
        ConstitutionMode::Stub => "Generate stub",
        ConstitutionMode::FromDocs => "Draft from docs",
        _ => "Reference existing docs",
    }
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ai_style_labels_match_preview_text() {
        assert_eq!(
            AiConfigurationStyle::Recommended.label(),
            "recommended defaults"
        );
        assert_eq!(
            AiConfigurationStyle::OneTool.label(),
            "one tool for all stages"
        );
        assert_eq!(
            AiConfigurationStyle::PerStage.label(),
            "per-stage configuration"
        );
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
}
