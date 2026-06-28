//! `derrick cost` — estimated cost breakdown (D86).

use crate::commands::CostArgs;
use crate::exit_code::CliExitCode;
use crate::output::OutputFormat;
use crate::{CliError, current_repo_root, read_config};

pub(crate) async fn execute(args: CostArgs) -> Result<CliExitCode, CliError> {
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

    match args.format {
        OutputFormat::Human => print_human(&config),
        OutputFormat::Json => print_json(&config),
    }

    Ok(CliExitCode::Success)
}

fn print_human(config: &Option<derrick_config::Config>) {
    println!("derrick cost \u{2014} estimated spend\n");

    let Some(config) = config else {
        println!("  No derrick.yaml found. Run `derrick init` first.");
        return;
    };

    if let Some(budgets) = config.budgets() {
        println!("Budgets:");
        if let Some(b) = budgets.per_ticket() {
            println!("  per ticket  ${:.2}", b.max_cost());
        }
        if let Some(b) = budgets.daily() {
            println!("  daily       ${:.2}", b.max_cost());
        }
        if let Some(b) = budgets.monthly() {
            println!("  monthly     ${:.2}", b.max_cost());
        }
        println!();
    }

    println!("Models and estimated cost tier:");
    let mut entries: Vec<(&str, &derrick_config::ModelDef)> = config
        .models()
        .as_map()
        .iter()
        .map(|(k, v)| (k.as_str(), v))
        .collect();
    entries.sort_by_key(|(k, _)| *k);

    for (alias, model) in entries {
        let runtime = model.resolved_runtime();
        let cost = model
            .estimated()
            .and_then(|e| e.cost())
            .unwrap_or("unknown");
        let latency = model
            .estimated()
            .and_then(|e| e.latency())
            .unwrap_or("unknown");
        println!("  {alias:<20}  runtime: {runtime:<20}  cost: {cost:<10}  latency: {latency}");
    }

    println!();
    println!("Run `derrick gain` for actual token savings from recent sessions.");
}

fn print_json(config: &Option<derrick_config::Config>) {
    use serde_json::json;

    let body = if let Some(config) = config {
        let budgets = config.budgets().map(|b| {
            json!({
                "per_ticket": b.per_ticket().map(derrick_config::Budget::max_cost),
                "daily": b.daily().map(derrick_config::Budget::max_cost),
                "monthly": b.monthly().map(derrick_config::Budget::max_cost),
            })
        });

        let models: serde_json::Value = config
            .models()
            .as_map()
            .iter()
            .map(|(alias, model)| {
                (
                    alias.clone(),
                    json!({
                        "runtime": model.resolved_runtime(),
                        "cost": model.estimated().and_then(|e| e.cost()),
                        "latency": model.estimated().and_then(|e| e.latency()),
                        "quality": model.estimated().and_then(|e| e.quality()),
                    }),
                )
            })
            .collect();

        json!({
            "budgets": budgets,
            "models": models,
        })
    } else {
        json!({ "error": "no config found" })
    };

    println!("{}", serde_json::to_string(&body).unwrap_or_default());
}
