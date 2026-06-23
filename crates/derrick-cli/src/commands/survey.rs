//! `derrick survey ...` subcommands: query the native code-graph index
//! (DESIGN.md §9.B.8, D54/D55).

use std::fs;
use std::path::{Path, PathBuf};

use derrick_survey::{
    BuildOptions, BuildReport, ImpactSet, IndexStatus, Survey, SurveyConfig, SymbolContext,
    SymbolHit,
};

use crate::commands::{
    SurveyArgs, SurveyBuildArgs, SurveyCommand, SurveyHubArgs, SurveyImpactArgs, SurveyQueryArgs,
    SurveyServeArgs, SurveySetupArgs, SurveyStatusArgs,
};
use crate::exit_code::CliExitCode;
use crate::output::OutputFormat;
use crate::{create_dir_all, current_repo_root, message};

pub(crate) async fn execute(args: SurveyArgs) -> Result<CliExitCode, crate::CliError> {
    // Setup and Hub don't open the current repo's index — Setup wires up a
    // single repo, and Hub loads its own multi-repo registry. Handle both
    // before resolving the current repo root / opening the index; every other
    // command shares the open-the-current-repo path.
    match args.command {
        SurveyCommand::Setup(setup) => {
            let repo_root = current_repo_root()?;
            run_setup(&repo_root, setup)
        }
        SurveyCommand::Hub(hub) => run_hub(hub).await,
        command => {
            let repo_root = current_repo_root()?;
            let survey = open_survey(&repo_root).await?;
            match command {
                SurveyCommand::Build(build) => run_build(&survey, build).await,
                SurveyCommand::Search(query) => run_search(&survey, query).await,
                SurveyCommand::Context(query) => run_context(&survey, query).await,
                SurveyCommand::Impact(impact) => run_impact(&survey, impact).await,
                SurveyCommand::Status(status) => run_status(&survey, status).await,
                SurveyCommand::Serve(serve) => run_serve(survey, serve).await,
                SurveyCommand::Setup(_) | SurveyCommand::Hub(_) => {
                    unreachable!("handled above")
                }
            }
        }
    }
}

async fn run_hub(args: SurveyHubArgs) -> Result<CliExitCode, crate::CliError> {
    let config = derrick_survey_hub::HubConfig::load(&args.config)
        .map_err(|error| message(format!("survey hub config: {error}")))?;
    derrick_survey_hub::serve(&config)
        .await
        .map_err(|error| message(format!("survey hub: {error}")))?;
    Ok(CliExitCode::Success)
}

async fn run_serve(survey: Survey, _args: SurveyServeArgs) -> Result<CliExitCode, crate::CliError> {
    derrick_survey::serve_stdio(survey)
        .await
        .map_err(|error| message(format!("survey serve: {error}")))?;
    Ok(CliExitCode::Success)
}

fn run_setup(repo_root: &Path, _args: SurveySetupArgs) -> Result<CliExitCode, crate::CliError> {
    // 1. Create .derrick/ and write a .gitignore that excludes the index DB.
    let derrick_dir = repo_root.join(".derrick");
    create_dir_all(&derrick_dir)?;
    let gitignore_path = derrick_dir.join(".gitignore");
    if !gitignore_path.exists() {
        fs::write(&gitignore_path, "index.db*\n")
            .map_err(|source| message(format!("write {}: {source}", gitignore_path.display())))?;
        println!("wrote  {}", gitignore_path.display());
    }

    // 2. Merge derrick-survey into .mcp.json (idempotent).
    derrick_adopt::write_mcp_json(repo_root)
        .map_err(|error| message(format!("write .mcp.json: {error}")))?;
    println!("wrote  {}", repo_root.join(".mcp.json").display());

    println!();
    println!("Survey MCP server registered. Restart your editor / agent host");
    println!("to pick up the new server, then run:");
    println!("  derrick survey build");
    Ok(CliExitCode::Success)
}

/// Index path: `.derrick/index.db`, distinct from the substrate DB (D11).
fn index_db_path(repo_root: &Path) -> PathBuf {
    repo_root.join(".derrick").join("index.db")
}

async fn open_survey(repo_root: &Path) -> Result<Survey, crate::CliError> {
    let db_path = index_db_path(repo_root);
    if let Some(parent) = db_path.parent() {
        create_dir_all(parent)?;
    }
    Survey::open(SurveyConfig {
        db_path,
        repo_root: repo_root.to_path_buf(),
        reader_pool: SurveyConfig::DEFAULT_READER_POOL,
    })
    .await
    .map_err(|error| message(format!("open survey index: {error}")))
}

async fn run_build(survey: &Survey, args: SurveyBuildArgs) -> Result<CliExitCode, crate::CliError> {
    let report = survey
        .build(BuildOptions { full: args.full })
        .await
        .map_err(|error| message(format!("survey build: {error}")))?;
    match args.format {
        OutputFormat::Json => print_json(&report)?,
        OutputFormat::Human => print_build_human(&report),
    }
    Ok(CliExitCode::Success)
}

async fn run_search(
    survey: &Survey,
    args: SurveyQueryArgs,
) -> Result<CliExitCode, crate::CliError> {
    let hits = survey
        .search(&args.query, args.limit)
        .await
        .map_err(|error| message(format!("survey search: {error}")))?;
    match args.format {
        OutputFormat::Json => print_json(&hits)?,
        OutputFormat::Human => {
            if hits.is_empty() {
                println!("No matches for {:?}.", args.query);
            } else {
                for hit in &hits {
                    print_hit(hit);
                }
            }
        }
    }
    Ok(CliExitCode::Success)
}

async fn run_context(
    survey: &Survey,
    args: SurveyQueryArgs,
) -> Result<CliExitCode, crate::CliError> {
    let context = survey
        .context(&args.query, args.limit)
        .await
        .map_err(|error| message(format!("survey context: {error}")))?;
    match args.format {
        OutputFormat::Json => print_json(&context)?,
        OutputFormat::Human => print_context_human(&context),
    }
    Ok(CliExitCode::Success)
}

async fn run_impact(
    survey: &Survey,
    args: SurveyImpactArgs,
) -> Result<CliExitCode, crate::CliError> {
    let impact = survey
        .impact(&args.symbol)
        .await
        .map_err(|error| message(format!("survey impact: {error}")))?;
    match args.format {
        OutputFormat::Json => print_json(&impact)?,
        OutputFormat::Human => match impact {
            None => println!("Symbol {:?} not found in the index.", args.symbol),
            Some(set) => print_impact_human(&set),
        },
    }
    Ok(CliExitCode::Success)
}

async fn run_status(
    survey: &Survey,
    args: SurveyStatusArgs,
) -> Result<CliExitCode, crate::CliError> {
    let status = survey
        .status()
        .await
        .map_err(|error| message(format!("survey status: {error}")))?;
    match args.format {
        OutputFormat::Json => print_json(&status)?,
        OutputFormat::Human => print_status_human(&status),
    }
    Ok(CliExitCode::Success)
}

fn print_json<T: serde::Serialize>(value: &T) -> Result<(), crate::CliError> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn print_hit(hit: &SymbolHit) {
    let sig = hit.signature.as_deref().unwrap_or("");
    println!(
        "{:<10} {}:{}-{}  {}  {}",
        hit.kind.as_str(),
        hit.path,
        hit.start_line,
        hit.end_line,
        hit.name,
        sig
    );
}

fn print_build_human(report: &BuildReport) {
    println!(
        "Indexed {} file(s), {} unchanged, {} removed.",
        report.files_indexed, report.files_unchanged, report.files_removed
    );
    println!(
        "Index now holds {} symbols, {} references.",
        report.symbols, report.refs
    );
}

fn print_context_human(context: &SymbolContext) {
    println!("Entry points:");
    for hit in &context.entry_points {
        print_hit(hit);
    }
    if !context.related.is_empty() {
        println!("\nReferences:");
        for hit in &context.related {
            print_hit(hit);
        }
    }
}

fn print_impact_human(set: &ImpactSet) {
    println!("Symbol:");
    print_hit(&set.symbol);
    println!("\nCallers ({}):", set.callers.len());
    for hit in &set.callers {
        print_hit(hit);
    }
    println!("\nCallees ({}):", set.callees.len());
    for hit in &set.callees {
        print_hit(hit);
    }
}

fn print_status_human(status: &IndexStatus) {
    println!(
        "Index: {} files, {} symbols, {} refs (schema v{}).",
        status.files, status.symbols, status.refs, status.schema_version
    );
    if status.pending.is_empty() {
        println!("Up to date with the working tree.");
    } else {
        println!("{} file(s) differ from the index:", status.pending.len());
        for pending in &status.pending {
            println!("  {:<9} {}", pending.reason, pending.path);
        }
    }
}
