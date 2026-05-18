use std::path::Path;

use derrick_config::{render_init_template, Config, InitTemplateVars};
use derrick_substrate_native::NativeSubstrate;

use crate::commands::InitArgs;
use crate::exit_code::CliExitCode;
use crate::{create_dir_all, current_repo_root, message, native_paths, read_config, write_file};

const INIT_TEMPLATE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../templates/derrick.yaml.in"
));
const BROWNFIELD_T011: &str = "Brownfield init (the default) is provided by `derrick-adopt` (T011), which is not yet implemented. For a fresh repo use `derrick init --greenfield`. Existing repos with AGENTS.md / CLAUDE.md / .specify/ / existing trackers should wait for T011 to land before being initialised.";

pub(crate) async fn execute(args: InitArgs) -> Result<CliExitCode, crate::CliError> {
    let repo_root = current_repo_root()?;
    if !args.greenfield {
        eprintln!("{BROWNFIELD_T011}");
        return Ok(CliExitCode::Failure);
    }

    greenfield_init(&repo_root, args).await
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
