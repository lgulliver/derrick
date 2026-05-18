use std::path::Path;

use derrick_adopt::{AdoptOptions, Adopter, ConstitutionMode};
use derrick_config::{render_init_template, Config, InitTemplateVars};
use derrick_substrate_native::NativeSubstrate;

use crate::commands::InitArgs;
use crate::exit_code::CliExitCode;
use crate::{create_dir_all, current_repo_root, message, native_paths, read_config, write_file};

const INIT_TEMPLATE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../templates/derrick.yaml.in"
));
pub(crate) async fn execute(args: InitArgs) -> Result<CliExitCode, crate::CliError> {
    let repo_root = current_repo_root()?;
    if !args.greenfield {
        return brownfield_init(&repo_root, args).await;
    }

    greenfield_init(&repo_root, args).await
}

async fn brownfield_init(repo_root: &Path, args: InitArgs) -> Result<CliExitCode, crate::CliError> {
    let adopter = Adopter::new(repo_root);
    let detection = adopter.detect()?;
    let site_name = args
        .site
        .clone()
        .unwrap_or_else(|| default_site_name(repo_root));
    let prefix = match args.prefix.clone() {
        Some(prefix) => prefix,
        None => default_prefix(&site_name),
    };
    validate_prefix(&prefix)?;
    let constitution = if args.constitution_stub {
        ConstitutionMode::Stub
    } else if args.constitution_from_docs {
        ConstitutionMode::FromDocs
    } else {
        ConstitutionMode::Reference
    };
    let opts = AdoptOptions {
        site_name,
        site_prefix: prefix,
        mode: init_mode_to_substrate(args.mode),
        force: args.force,
        no_hooks: args.no_hooks,
        append_agents_md: args.append_agents_md,
        constitution,
    };
    let drafted_constitution = if opts.constitution == ConstitutionMode::FromDocs {
        Some(adopter.draft_constitution(&detection, &opts).await?)
    } else {
        None
    };
    let plan = adopter.propose(&detection, &opts, drafted_constitution.as_deref())?;
    print_plan(&plan);
    if !plan.blockers.is_empty() {
        return Ok(CliExitCode::Failure);
    }
    if args.dry_run {
        return Ok(CliExitCode::Success);
    }

    let outcome = adopter.apply(&plan).await?;
    println!("initialised derrick site {}", opts.site_name);
    println!("written      {}", join_paths(&outcome.written));
    if !outcome.bookkeeping.is_empty() {
        println!("bookkeeping  {}", join_paths(&outcome.bookkeeping));
    }
    if !args.yes {
        println!("next         review `git status` before committing");
    }
    Ok(CliExitCode::Success)
}

async fn greenfield_init(repo_root: &Path, args: InitArgs) -> Result<CliExitCode, crate::CliError> {
    let config_path = repo_root.join("derrick.yaml");
    if config_path.exists() && !args.force {
        return Err(message(format!(
            "{} already exists; rerun with --force to overwrite it",
            config_path.display()
        )));
    }

    let site_name = args.site.unwrap_or_else(|| default_site_name(repo_root));
    let prefix = match args.prefix {
        Some(prefix) => prefix,
        None => default_prefix(&site_name),
    };
    validate_prefix(&prefix)?;

    let rendered = render_init_template(
        INIT_TEMPLATE,
        InitTemplateVars {
            site_name: &site_name,
            prefix: &prefix,
            mode: args.mode.as_str(),
        },
    );
    write_file(&config_path, &rendered)?;

    let config = read_config(repo_root)?;
    create_dir_all(&repo_root.join(config.state().dir()))?;
    let gitignore = repo_root.join(config.state().dir()).join(".gitignore");
    write_file(&gitignore, "runs/\nstate.json\nworktrees/\n")?;

    let substrate =
        NativeSubstrate::open(native_paths(repo_root, &config), config.site().clone()).await?;
    substrate.close().await?;

    if !args.no_hooks {
        derrick_adopt::write_codex_instructions(repo_root).map_err(|e| message(e.to_string()))?;
        println!("written      .codex/instructions.md");
    }

    print_summary(&config);
    Ok(CliExitCode::Success)
}

fn default_site_name(repo_root: &Path) -> String {
    repo_root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("derrick-site")
        .to_owned()
}

fn default_prefix(site_name: &str) -> String {
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

fn validate_prefix(prefix: &str) -> Result<(), crate::CliError> {
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

fn print_plan(plan: &derrick_adopt::AdoptionPlan) {
    println!("adoption plan");
    if !plan.writes.is_empty() {
        println!("writes       {}", join_planned_writes(&plan.writes));
    }
    if !plan.references.is_empty() {
        let references = plan
            .references
            .iter()
            .map(|reference| format!("{} as {}", reference.path.display(), reference.as_field))
            .collect::<Vec<_>>()
            .join(", ");
        println!("references   {references}");
    }
    for warning in &plan.warnings {
        println!("warning      {warning}");
    }
    for blocker in &plan.blockers {
        println!("blocker      {blocker}");
    }
}

fn join_planned_writes(writes: &[derrick_adopt::PlannedWrite]) -> String {
    let paths = writes
        .iter()
        .map(|write| write.path.clone())
        .collect::<Vec<_>>();
    join_paths(&paths)
}

fn join_paths(paths: &[std::path::PathBuf]) -> String {
    paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

fn print_summary(config: &Config) {
    let steps = config
        .pipeline()
        .iter()
        .map(|step| step.id())
        .collect::<Vec<_>>()
        .join(", ");
    println!("initialised derrick site {}", config.site().name());
    println!(
        "mode         {}",
        mode_name(config.tools().substrate().mode())
    );
    println!("prefix       {}", config.site().prefix());
    println!("pipeline     {steps}");
    println!("next         run `derrick doctor` to verify the install");
}

fn mode_name(mode: derrick_config::SubstrateMode) -> &'static str {
    match mode {
        derrick_config::SubstrateMode::Solo => "solo",
        derrick_config::SubstrateMode::Copilot => "copilot",
        derrick_config::SubstrateMode::Crew => "crew",
    }
}
