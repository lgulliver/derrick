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
    let config = match read_config(&repo_root) {
        Ok(c) => Some(c),
        Err(CliError::Config(derrick_config::ConfigError::Io { ref source, .. }))
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            None
        }
        Err(e) => return Err(e),
    };

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
    // Fall back to the default config when derrick.yaml is missing so that
    // built-in profiles are always inspectable before `derrick init`.
    let config = match read_config(&repo_root) {
        Ok(c) => c,
        Err(CliError::Config(derrick_config::ConfigError::Io { ref source, .. }))
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            derrick_config::Config::defaults()
        }
        Err(e) => return Err(e),
    };

    let desc = config
        .profiles()
        .get(name)
        .and_then(|p| p.description())
        .unwrap_or_else(|| builtin_description(name));

    let original_roles: std::collections::HashMap<String, String> = config
        .roles()
        .as_map()
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    let profiled = config
        .with_profile(name)
        .map_err(|e| crate::message(e.to_string()))?;

    let mut all_changes: Vec<(String, String)> = profiled
        .roles()
        .as_map()
        .iter()
        .filter(|(k, v)| original_roles.get(*k).map(String::as_str) != Some(v.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    all_changes.sort_by_key(|(k, _)| k.clone());

    println!("Profile: {name}");
    println!("  {desc}");
    println!();
    println!("Stage bindings changed by this profile:");
    if all_changes.is_empty() {
        println!("  (no bindings changed — aliases not found or profile has no overrides)");
    } else {
        // Group `assay-reviewer-N` entries back into a single `assay` display
        // line so the output reflects the stage name the profile defines.
        let (mut assay_reviewers, other): (Vec<_>, Vec<_>) = all_changes
            .into_iter()
            .partition(|(k, _)| k.starts_with("assay-reviewer-"));
        assay_reviewers.sort_by_key(|(k, _)| {
            k.strip_prefix("assay-reviewer-")
                .and_then(|n| n.parse::<usize>().ok())
                .unwrap_or(0)
        });
        if !assay_reviewers.is_empty() {
            let aliases: Vec<&str> = assay_reviewers.iter().map(|(_, v)| v.as_str()).collect();
            println!("  {:<18}  [{}]", "assay", aliases.join(", "));
        }
        for (stage, alias) in &other {
            println!("  {stage:<18}  {alias}");
        }
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
