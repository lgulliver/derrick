//! `derrick profile` — list and inspect AI profiles (D86).

use derrick_config::BUILTIN_PROFILE_NAMES;

use crate::commands::{ProfileArgs, ProfileCommand};
use crate::exit_code::CliExitCode;
use crate::{CliError, current_repo_root, read_config};

pub(crate) async fn execute(args: ProfileArgs) -> Result<CliExitCode, CliError> {
    match args.command {
        ProfileCommand::List => list().await,
        ProfileCommand::Show(show_args) => show(&show_args.name).await,
    }
}

async fn list() -> Result<CliExitCode, CliError> {
    let repo_root = current_repo_root().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let config = read_config(&repo_root).ok();

    println!("Built-in profiles:");
    for name in BUILTIN_PROFILE_NAMES {
        let is_default = config
            .as_ref()
            .and_then(|c| c.default_profile())
            .is_some_and(|d| d == name);
        let marker = if is_default { " (default)" } else { "" };
        let desc = builtin_description(name);
        println!("  {name:<10}  {desc}{marker}");
    }

    if let Some(config) = &config {
        let user = config.profiles().as_map();
        if !user.is_empty() {
            println!("\nUser-defined profiles:");
            let mut names: Vec<&str> = user.keys().map(String::as_str).collect();
            names.sort();
            for name in names {
                let is_default = config.default_profile().is_some_and(|d| d == name);
                let marker = if is_default { " (default)" } else { "" };
                let desc = user[name].description().unwrap_or("");
                println!("  {name:<10}  {desc}{marker}");
            }
        }
    }

    Ok(CliExitCode::Success)
}

async fn show(name: &str) -> Result<CliExitCode, CliError> {
    let repo_root = current_repo_root().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let config = read_config(&repo_root).map_err(|e| crate::message(e.to_string()))?;

    let profiled = config
        .with_profile(name)
        .map_err(|e| crate::message(e.to_string()))?;

    let desc = profiled
        .profiles()
        .get(name)
        .and_then(|p| p.description())
        .unwrap_or_else(|| builtin_description(name));

    println!("Profile: {name}");
    println!("  {desc}");
    println!();
    println!("Stage bindings (after applying profile to current config):");
    let mut roles: Vec<(&str, &str)> = profiled
        .roles()
        .as_map()
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    roles.sort_by_key(|(k, _)| *k);
    for (role, alias) in roles {
        println!("  {role:<18}  {alias}");
    }

    Ok(CliExitCode::Success)
}

fn builtin_description(name: &str) -> &'static str {
    match name {
        "speed" => "optimise for latency: fastest runtime, smallest model",
        "balanced" => "good quality at reasonable speed (default baseline)",
        "quality" => "maximum reasoning quality: strong models, multiple reviewers",
        "cheap" => "optimise for lowest cost: CLI usage, local models",
        "local" => "use only local runtimes (Ollama, LM Studio, vLLM)",
        "ci" => "non-interactive, deterministic, suitable for automation",
        _ => "",
    }
}
