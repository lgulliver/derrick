use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use derrick_config::{Config, OnSplit};
use derrick_models::AuthStore;
use derrick_tools::{HostRegistry, HostRequest};
use owo_colors::OwoColorize;
use tokio::sync::Semaphore;

use crate::io::{append_log, create_dir_all, parent, read_to_string, relative_to_root, write_file};
use crate::names::host_name;
use crate::template::render_template;
use crate::types::{RunError, StepExecution};

pub struct ReviewerOutcome {
    pub role: String,
    pub verdict: String,
    pub verdict_path: PathBuf,
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub constitution_violations: Vec<String>,
    pub rounds_used: u32,
}

pub enum ReviewerRoundOutcome {
    Decided(ReviewerOutcome),
    Skipped,
}

const ASSAY_SYSTEM_BASE: &str = "Review the speckit plan. Identify the highest risks, missing edge cases, and constitution contradictions. End with an H2 `## Verdict` followed by exactly one of: accept, revise, reject.";

#[allow(clippy::too_many_arguments)]
pub async fn execute_assay(
    config: &Config,
    hosts: Arc<HostRegistry>,
    repo_root: &Path,
    working_dir: &Path,
    feature_dir: &Path,
    state_prompt: &str,
    run_id: &str,
    step: &derrick_config::PipelineStep,
    log_path: &Path,
    state: &mut ExecutionState,
) -> Result<StepExecution, RunError> {
    let reviewers: Vec<String> = config.tools().assay().reviewers().to_vec();
    let on_split = config.tools().assay().on_split();
    let fallback_role = config.tools().assay().role().to_owned();

    if reviewers.len() <= 1 {
        let reviewer_role = reviewers
            .first()
            .cloned()
            .unwrap_or_else(|| fallback_role.clone());
        let reviewer_dir = working_dir.join(feature_dir).join("assay");
        let outcome = match run_reviewer_rounds(
            config,
            hosts.clone(),
            repo_root,
            working_dir,
            feature_dir,
            state_prompt,
            run_id,
            step,
            log_path,
            &reviewer_role,
            &reviewer_dir,
            state,
        )
        .await?
        {
            ReviewerRoundOutcome::Skipped => return Ok(StepExecution::skipped()),
            ReviewerRoundOutcome::Decided(outcome) => outcome,
        };
        let (tokens_in, tokens_out) = (outcome.tokens_in, outcome.tokens_out);
        let verdict_path_rel = relative_to_root(repo_root, outcome.verdict_path)?;

        return match outcome.verdict.as_str() {
            "accept" => {
                if outcome.constitution_violations.is_empty() {
                    Ok(StepExecution::success(vec![verdict_path_rel])
                        .with_tokens(tokens_in, tokens_out))
                } else if config.tools().assay().auto_execute() {
                    eprintln!(
                        "  {} {} {}",
                        "\u{26a0}".yellow(),
                        "Constitution violation — cannot auto-execute".yellow(),
                        "\u{26a0}".yellow()
                    );
                    for viol in &outcome.constitution_violations {
                        eprintln!("  {}  {}", "\u{2022}".yellow(), viol.yellow());
                    }
                    Ok(StepExecution::halted(
                        vec![verdict_path_rel],
                        format!(
                            "Constitution violations prevent auto-execute:\n{}",
                            outcome.constitution_violations.join("\n")
                        ),
                    )
                    .with_tokens(tokens_in, tokens_out))
                } else {
                    eprintln!(
                        "  {} {} {}",
                        "\u{26a0}".yellow(),
                        "Constitution violation detected".yellow(),
                        "\u{26a0}".yellow()
                    );
                    for viol in &outcome.constitution_violations {
                        eprintln!("  {}  {}", "\u{2022}".yellow(), viol.yellow());
                    }
                    eprintln!(
                        "  {} {}",
                        "Constitution changes require human approval.".yellow(),
                        "Override?".yellow()
                    );
                    eprint!("  {} Accept anyway? [y/N] ", "\u{276f}".cyan());
                    std::io::stderr().flush().ok();
                    let mut answer = String::new();
                    std::io::stdin()
                        .read_line(&mut answer)
                        .map_err(|source| RunError::Io {
                            path: PathBuf::from("<stdin>"),
                            source,
                        })?;
                    if answer.trim().eq_ignore_ascii_case("y")
                        || answer.trim().eq_ignore_ascii_case("yes")
                    {
                        Ok(StepExecution::success(vec![verdict_path_rel])
                            .with_tokens(tokens_in, tokens_out))
                    } else {
                        Ok(StepExecution::halted(
                            vec![verdict_path_rel],
                            format!(
                                "Constitution violations rejected by human:\n{}",
                                outcome.constitution_violations.join("\n")
                            ),
                        )
                        .with_tokens(tokens_in, tokens_out))
                    }
                }
            }
            "reject" => Ok(
                StepExecution::halted(vec![verdict_path_rel], "assay rejected")
                    .with_tokens(tokens_in, tokens_out),
            ),
            _ => {
                let msg =
                    if outcome.verdict == "revise" && outcome.constitution_violations.is_empty() {
                        format!(
                            "Revise loop exhausted after {} rounds. Latest review: {}",
                            outcome.rounds_used, outcome.role
                        )
                    } else if outcome.verdict == "revise" {
                        format!(
                            "Constitution violations persist after revise loop:\n{}",
                            outcome.constitution_violations.join("\n")
                        )
                    } else {
                        format!("assay completed with verdict: {}", outcome.verdict)
                    };
                eprintln!("  {} {}", "\u{26a0}".yellow(), msg.yellow());
                Ok(StepExecution::halted(vec![verdict_path_rel], msg)
                    .with_tokens(tokens_in, tokens_out))
            }
        };
    }

    let assay_max = config.parallelism().assay_max().max(1) as usize;
    let semaphore = Arc::new(Semaphore::new(assay_max));
    let mut handles: Vec<tokio::task::JoinHandle<Result<ReviewerRoundOutcome, RunError>>> =
        Vec::with_capacity(reviewers.len());

    for reviewer_role in &reviewers {
        let reviewer_dir = working_dir
            .join(feature_dir)
            .join("assay")
            .join(reviewer_role);
        create_dir_all(&reviewer_dir)?;

        let sem = semaphore.clone();
        let config = config.clone();
        let hosts = hosts.clone();
        let repo_root = repo_root.to_path_buf();
        let working_dir = working_dir.to_path_buf();
        let feature_dir = feature_dir.to_path_buf();
        let state_prompt = state_prompt.to_owned();
        let run_id = run_id.to_owned();
        let step = step.clone();
        let reviewer_log = state
            .run_dir
            .join(format!("step-{}-{}.log", step.id(), reviewer_role));
        let role = reviewer_role.clone();
        let state_clone = state.clone();

        handles.push(tokio::task::spawn(async move {
            let _permit = sem
                .acquire_owned()
                .await
                .map_err(|_| RunError::Config("semaphore closed unexpectedly".to_owned()))?;
            run_reviewer_rounds(
                &config,
                hosts,
                &repo_root,
                &working_dir,
                &feature_dir,
                &state_prompt,
                &run_id,
                &step,
                &reviewer_log,
                &role,
                &reviewer_dir,
                &state_clone,
            )
            .await
        }));
    }

    let mut outcomes: Vec<ReviewerOutcome> = Vec::with_capacity(reviewers.len());
    let mut any_skipped = false;
    for handle in handles {
        match handle
            .await
            .map_err(|e| RunError::Config(format!("reviewer task join: {e}")))?
        {
            Ok(ReviewerRoundOutcome::Decided(outcome)) => outcomes.push(outcome),
            Ok(ReviewerRoundOutcome::Skipped) => any_skipped = true,
            Err(e) => {
                tracing::error!(
                    run_id = %run_id,
                    error = %e,
                    "reviewer in multi-reviewer assay failed"
                );
                return Err(e);
            }
        }
    }
    if any_skipped {
        return Ok(StepExecution::skipped());
    }

    let combined_path = working_dir
        .join(feature_dir)
        .join("assay")
        .join("verdict.md");
    reconcile_verdicts(&outcomes, on_split, &combined_path, repo_root)
}

fn assay_system_prompt(config: &Config) -> String {
    if config.tools().assay().strict() {
        format!(
            "{}\nBe especially harsh — use a lower confidence threshold for flagging issues. Default to revise unless clearly sound.",
            ASSAY_SYSTEM_BASE
        )
    } else {
        ASSAY_SYSTEM_BASE.to_owned()
    }
}

fn phase_name(round: usize) -> &'static str {
    if round == 1 {
        "Cross-Examination"
    } else {
        "Deliberation"
    }
}

fn write_debate_transcript(
    transcript_path: &Path,
    phase: &str,
    round: usize,
    max_rounds: usize,
    model_name: &str,
    verdict: &str,
    body: &str,
) -> Result<(), RunError> {
    let entry = format!(
        "## {} (round {}/{})\n**Reviewer:** {}\n**Verdict:** {}\n\n{}\n\n---\n\n",
        phase, round, max_rounds, model_name, verdict, body
    );
    append_log(transcript_path, &entry)
}

fn write_rebuttal_transcript(transcript_path: &Path, replan_delta: &str) -> Result<(), RunError> {
    let entry = format!("## Rebuttal\n\n{}\n\n---\n\n", replan_delta);
    append_log(transcript_path, &entry)
}

fn write_verdict_transcript(
    transcript_path: &Path,
    final_verdict: &str,
    rounds_used: usize,
) -> Result<(), RunError> {
    let entry = format!(
        "## Verdict\n\n**Final:** {}\n**Rounds used:** {}\n",
        final_verdict, rounds_used
    );
    append_log(transcript_path, &entry)
}

#[allow(clippy::too_many_arguments)]
pub async fn run_reviewer_rounds(
    config: &Config,
    hosts: Arc<HostRegistry>,
    _repo_root: &Path,
    working_dir: &Path,
    feature_dir: &Path,
    state_prompt: &str,
    _run_id: &str,
    step: &derrick_config::PipelineStep,
    log_path: &Path,
    reviewer_role: &str,
    reviewer_dir: &Path,
    state: &ExecutionState,
) -> Result<ReviewerRoundOutcome, RunError> {
    let rounds = assay_rounds(config, step, state)?;
    let spec = read_to_string(&working_dir.join(feature_dir).join("spec.md"))?;
    let constitution = read_to_string(&working_dir.join(config.guardrails().constitution_path()))?;
    let auto_execute = config.tools().assay().auto_execute();

    let codex_fallback = detect_codex_fallback(config, reviewer_role).await?;
    if codex_fallback {
        if hosts.get("claude").is_none() {
            tracing::warn!(
                step = "assay",
                reviewer = reviewer_role,
                reason = "codex requires TTY; claude host not registered, skipping assay"
            );
            return Ok(ReviewerRoundOutcome::Skipped);
        }
        tracing::warn!(
            step = "assay",
            reviewer = reviewer_role,
            reason = "codex requires TTY; falling back to claude reviewer"
        );
    }

    let verdict_path = reviewer_dir.join("verdict.md");
    create_dir_all(parent(&verdict_path)?)?;

    let transcript_path = reviewer_dir.join("debate.md");
    write_file(&transcript_path, "")?;

    let mut tokens_in_total: u32 = 0;
    let mut tokens_out_total: u32 = 0;
    let mut last_constitution_violations: Vec<String> = Vec::new();
    let mut max_rounds = rounds;
    let mut round = 1usize;

    while round <= max_rounds {
        let plan = read_to_string(&working_dir.join(feature_dir).join("plan.md"))?;
        let prompt = format!("Task: {}\n\nPlan:\n{plan}", state_prompt);
        let cached = format!("Constitution:\n{constitution}\n\nSpec:\n{spec}");
        let phase = phase_name(round);

        eprint!(
            "\r  {} {} {} (round {}/{})...   ",
            step.id().cyan(),
            "\u{2696}".cyan(),
            phase.cyan(),
            round,
            max_rounds,
        );
        std::io::stderr().flush().ok();

        let (response_text, model_name, round_tokens_in, round_tokens_out) = if codex_fallback {
            let host = hosts
                .get("claude")
                .ok_or_else(|| RunError::Config("host \"claude\" is not registered".to_owned()))?;
            let full_prompt = format!("{}\n\n{cached}\n\n{prompt}", assay_system_prompt(config));
            let host_response = host
                .run(HostRequest {
                    headless: true,
                    ..HostRequest::new(full_prompt, working_dir)
                })
                .await
                .map_err(|source| RunError::StepFailed {
                    id: step.id().to_owned(),
                    message: source.to_string(),
                })?;
            (host_response.stdout, "claude".to_owned(), 0u32, 0u32)
        } else {
            let model = derrick_models::resolve_role(
                reviewer_role,
                config.roles(),
                config.models(),
                &AuthStore::from_env(),
            )
            .await?;
            let name = model.name().to_owned();
            let response = model
                .complete(completion_request(
                    prompt,
                    Some(cached),
                    Some(assay_system_prompt(config)),
                ))
                .await?;
            (response.text, name, response.tokens_in, response.tokens_out)
        };
        tokens_in_total = tokens_in_total.saturating_add(round_tokens_in);
        tokens_out_total = tokens_out_total.saturating_add(round_tokens_out);
        append_log(log_path, &response_text)?;

        let verdict = parse_verdict(&response_text).ok_or_else(|| RunError::StepFailed {
            id: step.id().to_owned(),
            message: "could not parse verdict from reviewer response".to_owned(),
        })?;
        last_constitution_violations = detect_constitution_violations(&response_text);

        write_debate_transcript(
            &transcript_path,
            phase,
            round,
            max_rounds,
            &model_name,
            verdict,
            &response_text,
        )?;

        eprint!("\r                                            \r");
        match verdict {
            "accept" => {
                eprintln!(
                    "  {} {} {} {} accept {}",
                    step.id().cyan(),
                    "\u{2696}".cyan(),
                    phase.cyan(),
                    "\u{2192}".cyan(),
                    "\u{2713}".green()
                );
            }
            "revise" => {
                eprintln!(
                    "  {} {} {} {} revise",
                    step.id().cyan(),
                    "\u{2696}".cyan(),
                    phase.cyan(),
                    "\u{2192}".cyan()
                );
            }
            "reject" => {
                eprintln!(
                    "  {} {} {} {} reject {}",
                    step.id().cyan(),
                    "\u{2696}".cyan(),
                    phase.cyan(),
                    "\u{2192}".cyan(),
                    "\u{2717}".red()
                );
            }
            _ => {
                eprintln!(
                    "  {} {} {} {} unknown verdict",
                    step.id().cyan(),
                    "\u{2696}".cyan(),
                    phase.cyan(),
                    "\u{2192}".cyan()
                );
            }
        }

        let verdict_body = format!(
            "model: {model_name}\nreviewer: {reviewer_role}\nround: {round}\nverdict: {verdict}\nphase: {phase}\n\n{response_text}"
        );
        write_file(&verdict_path, &verdict_body)?;

        match verdict {
            "accept" => {
                write_verdict_transcript(&transcript_path, "accept", round)?;
                let violations = last_constitution_violations.clone();
                return Ok(ReviewerRoundOutcome::Decided(ReviewerOutcome {
                    role: reviewer_role.to_owned(),
                    verdict: "accept".to_owned(),
                    verdict_path: verdict_path.clone(),
                    tokens_in: tokens_in_total,
                    tokens_out: tokens_out_total,
                    constitution_violations: violations,
                    rounds_used: round as u32,
                }));
            }
            "reject" => {
                write_verdict_transcript(&transcript_path, "reject", round)?;
                return Ok(ReviewerRoundOutcome::Decided(ReviewerOutcome {
                    role: reviewer_role.to_owned(),
                    verdict: "reject".to_owned(),
                    verdict_path: verdict_path.clone(),
                    tokens_in: tokens_in_total,
                    tokens_out: tokens_out_total,
                    constitution_violations: Vec::new(),
                    rounds_used: round as u32,
                }));
            }
            "revise" if round < max_rounds => {
                let objections =
                    suggested_revisions(&response_text).ok_or_else(|| RunError::StepFailed {
                        id: step.id().to_owned(),
                        message: "could not parse suggested revisions from reviewer response"
                            .to_owned(),
                    })?;
                eprintln!(
                    "  {} {} {}...",
                    step.id().cyan(),
                    "\u{2694}".cyan(),
                    "Rebuttal".cyan()
                );
                let replan_delta =
                    replan_from_objections(config, &hosts, working_dir, state, objections).await?;
                write_rebuttal_transcript(&transcript_path, &replan_delta)?;
            }
            "revise" => {
                // Rounds exhausted
                if auto_execute {
                    eprintln!(
                        "  {} {} Maximum {} rounds reached — models could not agree.",
                        step.id().cyan(),
                        "\u{26a0}".yellow(),
                        max_rounds
                    );
                    write_verdict_transcript(&transcript_path, "revise", round)?;
                    let violations = last_constitution_violations.clone();
                    return Ok(ReviewerRoundOutcome::Decided(ReviewerOutcome {
                        role: reviewer_role.to_owned(),
                        verdict: "revise".to_owned(),
                        verdict_path: verdict_path.clone(),
                        tokens_in: tokens_in_total,
                        tokens_out: tokens_out_total,
                        constitution_violations: violations,
                        rounds_used: round as u32,
                    }));
                }
                eprintln!(
                    "  {} {} Maximum {} rounds reached. Continue with more?",
                    "\u{276f}".cyan(),
                    step.id().cyan(),
                    max_rounds
                );
                eprint!("  {} Continue assay rounds? [y/N] ", "\u{276f}".cyan());
                std::io::stderr().flush().ok();
                let mut answer = String::new();
                std::io::stdin()
                    .read_line(&mut answer)
                    .map_err(|source| RunError::Io {
                        path: PathBuf::from("<stdin>"),
                        source,
                    })?;
                if answer.trim().eq_ignore_ascii_case("y")
                    || answer.trim().eq_ignore_ascii_case("yes")
                {
                    let objections = suggested_revisions(&response_text).ok_or_else(|| {
                        RunError::StepFailed {
                            id: step.id().to_owned(),
                            message: "could not parse suggested revisions from reviewer response"
                                .to_owned(),
                        }
                    })?;
                    eprintln!(
                        "  {} {} {} and extending rounds...",
                        step.id().cyan(),
                        "\u{2694}".cyan(),
                        "Rebuttal".cyan()
                    );
                    let replan_delta =
                        replan_from_objections(config, &hosts, working_dir, state, objections)
                            .await?;
                    write_rebuttal_transcript(&transcript_path, &replan_delta)?;
                    max_rounds = max_rounds.saturating_add(10);
                    round += 1;
                    continue;
                }
                write_verdict_transcript(&transcript_path, "revise", round)?;
                let violations = last_constitution_violations.clone();
                return Ok(ReviewerRoundOutcome::Decided(ReviewerOutcome {
                    role: reviewer_role.to_owned(),
                    verdict: "revise".to_owned(),
                    verdict_path: verdict_path.clone(),
                    tokens_in: tokens_in_total,
                    tokens_out: tokens_out_total,
                    constitution_violations: violations,
                    rounds_used: round as u32,
                }));
            }
            _ => unreachable_verdict(step.id())?,
        }

        round += 1;
    }

    write_verdict_transcript(&transcript_path, "revise", rounds)?;
    let violations = last_constitution_violations.clone();
    Ok(ReviewerRoundOutcome::Decided(ReviewerOutcome {
        role: reviewer_role.to_owned(),
        verdict: "revise".to_owned(),
        verdict_path,
        tokens_in: tokens_in_total,
        tokens_out: tokens_out_total,
        constitution_violations: violations,
        rounds_used: rounds as u32,
    }))
}

async fn detect_codex_fallback(config: &Config, reviewer_role: &str) -> Result<bool, RunError> {
    let Some(model_name) = config.roles().get(reviewer_role) else {
        return Ok(false);
    };
    let Some(model_def) = config.models().get(model_name) else {
        return Ok(false);
    };
    let codex_family = model_name.eq_ignore_ascii_case("codex")
        || model_name.to_ascii_lowercase().starts_with("codex")
        || model_def
            .cli()
            .is_some_and(|cli| cli.split_whitespace().next() == Some("codex"));
    Ok(codex_family)
}

async fn replan_from_objections(
    config: &Config,
    hosts: &HostRegistry,
    working_dir: &Path,
    state: &ExecutionState,
    objections: &str,
) -> Result<String, RunError> {
    let plan_step = config
        .pipeline()
        .iter()
        .find(|step| step.id() == "plan")
        .ok_or_else(|| RunError::Config("assay revise requires a plan step".to_owned()))?;
    let host = plan_step
        .host()
        .ok_or_else(|| RunError::Config("assay revise requires plan step host".to_owned()))?;
    let host_name = host_name(host);
    let host = hosts
        .get(host_name)
        .ok_or_else(|| RunError::Config(format!("host {host_name:?} is not registered")))?;
    let prompt = format!(
        "The reviewer raised the following objections. Produce a delta to plan.md that addresses each. Do not rewrite the plan from scratch.\n\n{objections}"
    );
    let response = host
        .run(HostRequest::new(prompt, working_dir))
        .await
        .map_err(|source| RunError::StepFailed {
            id: "plan".to_owned(),
            message: source.to_string(),
        })?;
    let delta = response.stdout.trim().to_owned();
    if !delta.is_empty() {
        let feature_dir = state
            .feature_dir
            .as_ref()
            .ok_or_else(|| RunError::Config("replan requires feature_dir".to_owned()))?;
        let plan_path = working_dir.join(feature_dir).join("plan.md");
        append_log(&plan_path, &delta)?;
    }
    Ok(delta)
}

pub fn reconcile_verdicts(
    outcomes: &[ReviewerOutcome],
    on_split: OnSplit,
    combined_path: &Path,
    repo_root: &Path,
) -> Result<StepExecution, RunError> {
    let all_accept = outcomes.iter().all(|o| o.verdict == "accept");
    let summary = outcomes
        .iter()
        .map(|o| format!("- {}: {}", o.role, o.verdict))
        .collect::<Vec<_>>()
        .join("\n");

    let final_verdict: &str = match on_split {
        OnSplit::Reject => {
            if all_accept {
                "accept"
            } else {
                "reject"
            }
        }
        OnSplit::Majority => {
            let accepts = outcomes.iter().filter(|o| o.verdict == "accept").count();
            let rejects = outcomes.iter().filter(|o| o.verdict != "accept").count();
            if accepts > rejects {
                "accept"
            } else {
                "reject"
            }
        }
        OnSplit::Human => {
            if all_accept {
                "accept"
            } else {
                let mut stdout = std::io::stdout();
                let _ = writeln!(stdout, "Reviewer split:");
                for o in outcomes {
                    let _ = writeln!(stdout, "  {}: {}", o.role, o.verdict);
                }
                let _ = write!(stdout, "Accept overall? [y/N] ");
                let _ = stdout.flush();
                let mut answer = String::new();
                std::io::stdin()
                    .read_line(&mut answer)
                    .map_err(|source| RunError::Io {
                        path: PathBuf::from("<stdin>"),
                        source,
                    })?;
                let trimmed = answer.trim();
                if trimmed.eq_ignore_ascii_case("y") || trimmed.eq_ignore_ascii_case("yes") {
                    "accept"
                } else {
                    "reject"
                }
            }
        }
    };

    let policy_name = match on_split {
        OnSplit::Reject => "reject",
        OnSplit::Human => "human",
        OnSplit::Majority => "majority",
    };
    let rounds_used = outcomes.iter().map(|o| o.rounds_used).max().unwrap_or(0);
    let body = format!(
        "verdict: {final_verdict}\non_split: {policy_name}\nreviewers: {}\nrounds_used: {rounds_used}\n\n{summary}\n",
        outcomes.len(),
    );
    crate::io::write_file(combined_path, &body)?;
    let combined_rel = relative_to_root(repo_root, combined_path.to_path_buf())?;
    let mut artifacts = vec![combined_rel];
    for o in outcomes {
        if let Ok(rel) = relative_to_root(repo_root, o.verdict_path.clone()) {
            artifacts.push(rel);
        }
    }

    let tokens_in: u32 = outcomes
        .iter()
        .map(|o| o.tokens_in)
        .fold(0u32, |a, b| a.saturating_add(b));
    let tokens_out: u32 = outcomes
        .iter()
        .map(|o| o.tokens_out)
        .fold(0u32, |a, b| a.saturating_add(b));

    let all_constitution_clean = outcomes
        .iter()
        .all(|o| o.constitution_violations.is_empty());

    match final_verdict {
        "accept" if all_constitution_clean => {
            Ok(StepExecution::success(artifacts).with_tokens(tokens_in, tokens_out))
        }
        "accept" => {
            let violations: Vec<&str> = outcomes
                .iter()
                .flat_map(|o| o.constitution_violations.iter().map(|s| s.as_str()))
                .collect();
            let msg = format!(
                "Constitution violations detected:\n{}",
                violations.join("\n")
            );
            eprintln!(
                "  {} {}",
                "\u{26a0}".yellow(),
                "Constitution violations found in multi-reviewer assay".yellow()
            );
            for v in &violations {
                eprintln!("  {}  {}", "\u{2022}".yellow(), v.yellow());
            }
            Ok(StepExecution::halted(artifacts, msg).with_tokens(tokens_in, tokens_out))
        }
        _ => Ok(StepExecution::halted(
            artifacts,
            format!("assay rejected (on_split: {policy_name})"),
        )
        .with_tokens(tokens_in, tokens_out)),
    }
}

/// Parse the verdict from the reviewer's response text.
/// Looks for `## Verdict` heading followed by accept/revise/reject.
pub fn parse_verdict(text: &str) -> Option<&'static str> {
    let mut in_verdict = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.eq_ignore_ascii_case("## Verdict") {
            in_verdict = true;
            continue;
        }
        if in_verdict {
            match trimmed.to_ascii_lowercase().as_str() {
                "accept" => return Some("accept"),
                "revise" => return Some("revise"),
                "reject" => return Some("reject"),
                "" => {}
                _ if trimmed.starts_with("## ") => return None,
                _ => {}
            }
        }
    }
    None
}

/// Extract suggested revisions from the reviewer's response.
/// Looks for `## Suggested revisions` heading.
pub fn suggested_revisions(text: &str) -> Option<&str> {
    let start_marker = "## Suggested revisions";
    let start = text.find(start_marker)? + start_marker.len();
    let rest = &text[start..];
    let end = rest.find("\n## ").unwrap_or(rest.len());
    Some(rest[..end].trim())
}

/// Detect constitution violations in the reviewer's response.
/// Looks for a section heading that mentions "Constitution" (any heading level)
/// and extracts the bullet/bold-numbered items as violation descriptions.
/// Only lines that look like structured items are extracted — body paragraphs
/// between items are skipped to avoid false positives.
fn detect_constitution_violations(text: &str) -> Vec<String> {
    let mut violations = Vec::new();
    let mut in_constitution_section = false;

    for line in text.lines() {
        let trimmed = line.trim();

        if in_constitution_section && is_markdown_heading(trimmed) {
            break;
        }

        if !in_constitution_section {
            if is_markdown_heading(trimmed) {
                let content = trimmed.trim_start_matches('#').trim();
                if content.to_ascii_lowercase().contains("constitution") {
                    in_constitution_section = true;
                }
            }
            continue;
        }

        let is_violation_line =
            trimmed.starts_with("**") || trimmed.starts_with("- ") || trimmed.starts_with("* ");
        if is_violation_line {
            let cleaned = clean_violation_text(trimmed);
            if !cleaned.is_empty() {
                violations.push(cleaned);
            }
        }
    }

    violations
}

fn clean_violation_text(line: &str) -> String {
    let mut cleaned = line.trim();
    if let Some(rest) = cleaned
        .strip_prefix("- ")
        .or_else(|| cleaned.strip_prefix("* "))
    {
        cleaned = rest.trim_start();
    }
    cleaned = strip_markdown_emphasis(cleaned);
    cleaned = cleaned.trim_start_matches(|c: char| c.is_ascii_digit());
    cleaned = cleaned.trim_start();
    if let Some(rest) = cleaned
        .strip_prefix('.')
        .or_else(|| cleaned.strip_prefix(')'))
    {
        cleaned = rest.trim_start();
    }
    strip_markdown_emphasis(cleaned).to_owned()
}

fn strip_markdown_emphasis(mut s: &str) -> &str {
    loop {
        let trimmed = s.trim();
        let next = if (trimmed.starts_with("**") || trimmed.starts_with("__"))
            && (trimmed.ends_with("**") || trimmed.ends_with("__"))
            && trimmed.len() > 4
        {
            Some(&trimmed[2..trimmed.len() - 2])
        } else if (trimmed.starts_with('*') || trimmed.starts_with('_'))
            && (trimmed.ends_with('*') || trimmed.ends_with('_'))
            && trimmed.len() > 2
        {
            Some(&trimmed[1..trimmed.len() - 1])
        } else {
            None
        };
        match next {
            Some(inner) => s = inner,
            None => return trimmed,
        }
    }
}

fn is_markdown_heading(s: &str) -> bool {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return false;
    }
    let chars: Vec<char> = trimmed.chars().collect();
    let hash_count = chars.iter().take_while(|&&c| c == '#').count();
    hash_count >= 1 && hash_count < chars.len() && chars[hash_count] == ' '
}

pub fn unreachable_verdict<T>(step_id: &str) -> Result<T, RunError> {
    Err(RunError::StepFailed {
        id: step_id.to_owned(),
        message: "unsupported verdict".to_owned(),
    })
}

fn assay_rounds(
    config: &Config,
    step: &derrick_config::PipelineStep,
    state: &ExecutionState,
) -> Result<usize, RunError> {
    let raw = step
        .rounds()
        .unwrap_or_else(|| config.tools().assay().rounds());
    let rendered = if raw == "{{tools.assay.rounds}}" {
        config.tools().assay().rounds().to_owned()
    } else {
        render_template(raw, &template_context(config, state)?)?
    };
    rendered.parse::<usize>().map_err(|error| {
        RunError::Config(format!(
            "pipeline.{}.rounds: expected positive integer: {error}",
            step.id()
        ))
    })
}

fn template_context(
    config: &Config,
    state: &ExecutionState,
) -> Result<crate::template::TemplateContext, RunError> {
    Ok(crate::template::TemplateContext {
        prompt: state.prompt.clone(),
        site_name: config.site().name().to_owned(),
        site_prefix: config.site().prefix().to_owned(),
        feature_dir: state.feature_dir.clone(),
        run_id: state.run_id.clone(),
    })
}

fn completion_request(
    prompt: String,
    cached_prefix: Option<String>,
    system: Option<String>,
) -> derrick_models::CompletionRequest {
    use std::time::Duration;
    derrick_models::CompletionRequest {
        cached_prefix,
        prompt,
        system,
        max_tokens: Some(4096),
        temperature: Some(0.2),
        timeout: Duration::from_secs(600),
    }
}

#[derive(Clone)]
pub struct ExecutionState {
    pub prompt: String,
    pub run_id: String,
    pub run_dir: PathBuf,
    pub feature_dir: Option<PathBuf>,
    pub worktree_path: Option<PathBuf>,
}

impl ExecutionState {
    pub fn new(prompt: String, run_id: String, run_dir: PathBuf) -> Self {
        Self {
            prompt,
            run_id,
            run_dir,
            feature_dir: None,
            worktree_path: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_constitution_violations_parses_section() {
        let text = r#"Some stuff

## Constitution Contradictions

**1. Missing test coverage plan (hard violation)**

Every new public function needs a test.

**2. No error handling**

Another violation.

## Other Section

irrelevant"#;
        let violations = detect_constitution_violations(text);
        assert_eq!(violations.len(), 2);
        assert!(violations[0].contains("Missing test coverage plan"));
        assert!(violations[1].contains("No error handling"));
    }

    #[test]
    fn detect_constitution_violations_no_false_positives() {
        let text = r#"This is a review with no constitution issues.

## Verdict
accept"#;
        let violations = detect_constitution_violations(text);
        assert!(violations.is_empty());
    }

    #[test]
    fn detect_constitution_violations_case_insensitive() {
        let text = r#"## constitutional concerns

- Violation one
- Violation two

## Next Section"#;
        let violations = detect_constitution_violations(text);
        assert_eq!(violations.len(), 2);
    }

    #[test]
    fn parse_verdict_accept() {
        assert_eq!(parse_verdict("## Verdict\naccept"), Some("accept"));
        assert_eq!(parse_verdict("## Verdict\n\nrevise"), Some("revise"));
        assert_eq!(parse_verdict("## Verdict\n\nreject"), Some("reject"));
    }

    #[test]
    fn parse_verdict_none() {
        assert_eq!(parse_verdict("no verdict here"), None);
        assert_eq!(parse_verdict("## Verdict\ninvalid"), None);
    }

    #[test]
    fn phase_names_are_correct() {
        assert_eq!(phase_name(1), "Cross-Examination");
        assert_eq!(phase_name(2), "Deliberation");
        assert_eq!(phase_name(10), "Deliberation");
    }

    #[test]
    fn load_assay_violations_from_real_log() {
        let text = include_str!("../testdata/assay-revise-verdict.md");
        let verdict = parse_verdict(text);
        assert_eq!(verdict, Some("revise"));
        let violations = detect_constitution_violations(text);
        assert!(
            !violations.is_empty(),
            "expected constitution violations in real assay log"
        );
        assert!(
            violations[0].contains("test coverage"),
            "first violation should mention test coverage: {:?}",
            violations[0]
        );
    }

    #[test]
    fn is_markdown_heading_variants() {
        assert!(is_markdown_heading("# Heading"));
        assert!(is_markdown_heading("## Heading"));
        assert!(is_markdown_heading("### Heading"));
        assert!(is_markdown_heading("#### Heading"));
        assert!(!is_markdown_heading("Not a heading"));
        assert!(!is_markdown_heading(""));
        assert!(!is_markdown_heading("#NoSpace"));
        assert!(!is_markdown_heading("###"));
        assert!(is_markdown_heading(" # heading after space")); // trims whitespace
    }

    #[test]
    fn unreachable_verdict_returns_error() {
        let result = unreachable_verdict::<()>("assay");
        assert!(result.is_err());
        match result {
            Err(RunError::StepFailed { id, message }) => {
                assert_eq!(id, "assay");
                assert_eq!(message, "unsupported verdict");
            }
            _ => panic!("expected StepFailed"),
        }
    }

    #[test]
    fn assay_system_prompt_strict_and_normal() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            tmp.path().join("derrick.yaml"),
            "tools:\n  assay:\n    strict: true\n",
        )
        .expect("write config");
        let strict_config = Config::load_layered(tmp.path()).expect("load strict config");
        assert_eq!(
            assay_system_prompt(&strict_config),
            format!(
                "{}\nBe especially harsh — use a lower confidence threshold for flagging issues. Default to revise unless clearly sound.",
                ASSAY_SYSTEM_BASE
            )
        );

        let normal_tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            normal_tmp.path().join("derrick.yaml"),
            "tools:\n  assay:\n    strict: false\n",
        )
        .expect("write config");
        let normal_config = Config::load_layered(normal_tmp.path()).expect("load normal config");
        assert_eq!(
            assay_system_prompt(&normal_config),
            ASSAY_SYSTEM_BASE.to_owned()
        );
        assert_eq!(
            ASSAY_SYSTEM_BASE,
            "Review the speckit plan. Identify the highest risks, missing edge cases, and constitution contradictions. End with an H2 `## Verdict` followed by exactly one of: accept, revise, reject."
        );
    }

    #[test]
    fn write_transcripts_creates_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("debate.md");

        write_debate_transcript(
            &path,
            "Cross-Examination",
            1,
            10,
            "test-model",
            "revise",
            "body text",
        )
        .unwrap();
        assert!(path.exists());
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("Cross-Examination (round 1/10)"));
        assert!(content.contains("test-model"));
        assert!(content.contains("revise"));
        assert!(content.contains("body text"));

        write_rebuttal_transcript(&path, "replan delta").unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("Rebuttal"));
        assert!(content.contains("replan delta"));

        write_verdict_transcript(&path, "accept", 3).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("accept"));
        assert!(content.contains("**Rounds used:** 3"));
    }

    #[test]
    fn detect_constitution_violations_h3_heading() {
        let text = r#"Some stuff

### Constitution Contradictions

**1. Missing test coverage plan (hard violation)**

## Other Section"#;
        let violations = detect_constitution_violations(text);
        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("test coverage"));
    }

    #[test]
    fn detect_constitution_violations_strips_markdown_markers() {
        let text = r#"## Constitution Contradictions

**1. Missing test coverage plan (hard violation)**

## Verdict
revise"#;
        let violations = detect_constitution_violations(text);
        assert_eq!(
            violations,
            vec!["Missing test coverage plan (hard violation)"]
        );
    }

    #[test]
    fn detect_constitution_violations_no_matches() {
        let text = r#"## Something Else

**1. Not a violation**

Random text."#;
        let violations = detect_constitution_violations(text);
        assert!(violations.is_empty());
    }
}
