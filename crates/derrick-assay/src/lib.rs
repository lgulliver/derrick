//! derrick-assay — adversarial review flow. See DESIGN.md §7.
//!
//! Also hosts pipeline-shared utilities (`io`, `names`, `template`, `types`)
//! that the flow runner consumes, to avoid a circular dependency between
//! `derrick-flow` and `derrick-assay`.

pub mod io;
pub mod names;
pub mod template;
pub mod types;

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use derrick_config::{Config, OnSplit};
use derrick_models::{AuthStore, CompletionEvent};
use derrick_tools::{HostRegistry, HostRequest};
use futures::StreamExt;
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

const ASSAY_SYSTEM_BASE: &str = "Review the speckit plan. Identify the highest risks, missing edge cases, and constitution contradictions. End with an H2 `## Verdict` section. The final line of your response MUST be exactly `**Verdict:** accept`, `**Verdict:** revise`, or `**Verdict:** reject` (one verdict word only, on its own line). Do not wrap the verdict word in extra prose on that line.";

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
    // DESIGN §7 / D5: warn (do not block) when a reviewer shares the
    // proposer's provider, which defeats adversarial cross-examination.
    let reviewers_for_check: Vec<String> = config.tools().assay().reviewers().to_vec();
    let family_warnings = same_family_warnings(config, &reviewers_for_check);

    let mut exec = execute_assay_core(
        config,
        hosts,
        repo_root,
        working_dir,
        feature_dir,
        state_prompt,
        run_id,
        step,
        log_path,
        state,
    )
    .await?;

    // Surface the same-family warnings in the step output text so they are
    // visible to operators reading the run log, not just the tracing sink.
    if !family_warnings.is_empty() {
        let banner = family_warnings
            .iter()
            .map(|w| format!("\u{26a0} {w}"))
            .collect::<Vec<_>>()
            .join("\n");
        for w in &family_warnings {
            eprintln!("  {} {}", "\u{26a0}".yellow(), w.yellow());
        }
        exec.message = if exec.message.is_empty() {
            banner
        } else {
            format!("{}\n{banner}", exec.message)
        };
    }

    Ok(exec)
}

#[allow(clippy::too_many_arguments)]
async fn execute_assay_core(
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
                } else {
                    // Constitution risks noted — log and continue. They are
                    // informational; only an explicit `reject` halts the pipeline.
                    eprintln!(
                        "  {} {} {}",
                        "\u{26a0}".yellow(),
                        "Constitution risks noted (accepted with conditions)".yellow(),
                        "\u{26a0}".yellow()
                    );
                    for viol in &outcome.constitution_violations {
                        eprintln!("  {}  {}", "\u{2022}".yellow(), viol.yellow());
                    }
                    Ok(StepExecution::success(vec![verdict_path_rel])
                        .with_tokens(tokens_in, tokens_out))
                }
            }
            "reject" => Ok(
                StepExecution::halted(vec![verdict_path_rel], "assay rejected")
                    .with_tokens(tokens_in, tokens_out),
            ),
            _ => {
                // `revise` after rounds exhausted = accept_with_conditions.
                // Risks are logged; the pipeline continues autonomously.
                let msg =
                    if outcome.verdict == "revise" && outcome.constitution_violations.is_empty() {
                        format!(
                            "Review risks noted after {} rounds (continuing).",
                            outcome.rounds_used
                        )
                    } else if outcome.verdict == "revise" {
                        format!(
                            "Review risks noted (continuing):\n{}",
                            outcome.constitution_violations.join("\n")
                        )
                    } else {
                        format!("assay completed with verdict: {}", outcome.verdict)
                    };
                eprintln!(
                    "  {} {} {}",
                    "assay".cyan(),
                    "\u{26a0}".yellow(),
                    msg.yellow()
                );
                Ok(StepExecution::success(vec![verdict_path_rel])
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

    // Quorum (fail-closed): ALL configured reviewers must produce a parseable
    // outcome. Handles were pushed in `reviewers` order, so we can name any
    // reviewer that skips or errors. A reviewer error halts immediately. Skips
    // are tracked by role so we can distinguish the legitimate *whole-step*
    // skip (every reviewer skipped — e.g. unedited constitution placeholder)
    // from a partial skip, which is a quorum failure.
    let mut outcomes: Vec<ReviewerOutcome> = Vec::with_capacity(reviewers.len());
    let mut skipped_roles: Vec<String> = Vec::new();
    for (reviewer_role, handle) in reviewers.iter().zip(handles) {
        match handle
            .await
            .map_err(|e| RunError::Config(format!("reviewer task join: {e}")))?
        {
            Ok(ReviewerRoundOutcome::Decided(outcome)) => outcomes.push(outcome),
            Ok(ReviewerRoundOutcome::Skipped) => skipped_roles.push(reviewer_role.clone()),
            Err(e) => {
                tracing::error!(
                    run_id = %run_id,
                    reviewer = %reviewer_role,
                    error = %e,
                    "reviewer in multi-reviewer assay failed"
                );
                return Err(RunError::StepFailed {
                    id: step.id().to_owned(),
                    message: format!(
                        "assay quorum not met: reviewer '{reviewer_role}' failed: {e}"
                    ),
                });
            }
        }
    }

    if !skipped_roles.is_empty() {
        // Every reviewer skipped: this is the existing whole-step skip path
        // (e.g. constitution is still an unedited placeholder). Preserve it.
        if outcomes.is_empty() {
            return Ok(StepExecution::skipped());
        }
        // Partial skip: some reviewers produced a verdict and some did not.
        // The assay is the single gate before autonomous execution, so we must
        // not reconcile on fewer outcomes than configured. Fail closed.
        let names = skipped_roles.join(", ");
        tracing::error!(
            run_id = %run_id,
            skipped = %names,
            decided = outcomes.len(),
            configured = reviewers.len(),
            "assay quorum not met: some reviewers skipped"
        );
        return Err(RunError::StepFailed {
            id: step.id().to_owned(),
            message: format!(
                "assay quorum not met: {} of {} reviewers did not produce a verdict (skipped: {names})",
                skipped_roles.len(),
                reviewers.len(),
            ),
        });
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
    if derrick_adopt::constitution_needs_setup(&constitution) {
        eprintln!(
            "  {}  assay skipped — constitution at {} is an unedited placeholder",
            "⚠".yellow(),
            config.guardrails().constitution_path().display()
        );
        eprintln!(
            "     Edit {} and add real project rules before re-running.",
            config.guardrails().constitution_path().display()
        );
        return Ok(ReviewerRoundOutcome::Skipped);
    }
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
        // D37: the headless fallback substitutes the `claude` host for the
        // configured codex reviewer. If the proposer is also a claude-family
        // provider, the substitution silently collapses assay into same-family
        // review — emit the same same-family warning as the config-time check
        // (DESIGN §7 / D5). Do not block.
        if let Some(msg) = same_family_warning(
            role_provider(config, PROPOSER_ROLE).as_deref(),
            reviewer_role,
            // D79: the fallback substitutes the claude CLI *runtime*.
            Some("claude-cli"),
        ) {
            tracing::warn!(
                step = "assay",
                reviewer = %reviewer_role,
                fallback = "claude",
                "{msg} (headless codex fallback)"
            );
            eprintln!("  {} {}", "\u{26a0}".yellow(), msg.yellow());
        }
    }

    let verdict_path = reviewer_dir.join("verdict.md");
    create_dir_all(parent(&verdict_path)?)?;

    let transcript_path = reviewer_dir.join("debate.md");
    write_file(&transcript_path, "")?;

    let mut tokens_in_total: u32 = 0;
    let mut tokens_out_total: u32 = 0;
    let mut last_constitution_violations: Vec<String> = Vec::new();
    let max_rounds = rounds;
    let mut round = 1usize;
    let mut round_summaries: Vec<(usize, String, String, usize, String)> = Vec::new();
    let mut previous_objections: Option<String> = None;

    while round <= max_rounds {
        let (prompt, cached) = if let (2.., Some(prev)) = (round, &previous_objections) {
            // Rounds 2+: send only the latest delta with the previous objections
            let plan_delta =
                last_delta_from_plan(&working_dir.join(feature_dir).join("plan.md"), prev)?;
            let prompt = format!(
                "Task: {}\n\nPrevious objections (verify each is resolved):\n{prev}\n\nLatest plan changes:\n{plan_delta}\n\nEvaluate ONLY whether the latest changes adequately address the previous objections. Do NOT re-list previously resolved items. List ONLY remaining or new concerns.\n\nEnd your response with a line that is exactly `**Verdict:** accept`, `**Verdict:** revise`, or `**Verdict:** reject`.",
                state_prompt
            );
            let cached = format!("Constitution:\n{constitution}\n\nSpec:\n{spec}");
            (prompt, cached)
        } else {
            let plan = read_to_string(&working_dir.join(feature_dir).join("plan.md"))?;
            let prompt = format!(
                "Task: {}\n\nPlan:\n{plan}\n\nEnd your response with a line that is exactly `**Verdict:** accept`, `**Verdict:** revise`, or `**Verdict:** reject`.",
                state_prompt
            );
            let cached = format!("Constitution:\n{constitution}\n\nSpec:\n{spec}");
            (prompt, cached)
        };
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
            (
                host_response.stdout,
                "claude".to_owned(),
                host_response.tokens_in,
                host_response.tokens_out,
            )
        } else {
            let model = derrick_models::resolve_role(
                reviewer_role,
                config.roles(),
                config.models(),
                &AuthStore::from_env(),
            )
            .await?;
            let name = model.name().to_owned();
            let mut response_text = String::new();
            let mut tokens_in = 0u32;
            let mut tokens_out = 0u32;
            let mut stream = model
                .stream(completion_request(
                    prompt,
                    Some(cached),
                    Some(assay_system_prompt(config)),
                ))
                .await?;
            eprint!(
                "\r  {} {} {} (round {}/{})...  ",
                step.id().cyan(),
                "\u{2696}".cyan(),
                phase.cyan(),
                round,
                max_rounds,
            );
            std::io::stderr().flush().ok();
            while let Some(event) = stream.next().await {
                match event.map_err(|e| RunError::StepFailed {
                    id: step.id().to_owned(),
                    message: e.to_string(),
                })? {
                    CompletionEvent::Content { text } => {
                        response_text.push_str(&text);
                        // Show a compact preview of the last line
                        if text.contains('\n') {
                            let last = text.rsplit('\n').next().unwrap_or("").trim();
                            if !last.is_empty() {
                                let preview = if last.len() > 60 {
                                    format!("{}...", &last[..57])
                                } else {
                                    last.to_owned()
                                };
                                eprint!(
                                    "\r  {} {} {} (round {}/{}) {}  ",
                                    step.id().cyan(),
                                    "\u{2696}".cyan(),
                                    phase.cyan(),
                                    round,
                                    max_rounds,
                                    preview.bright_black()
                                );
                                std::io::stderr().flush().ok();
                            }
                        }
                    }
                    CompletionEvent::End {
                        tokens_in: ti,
                        tokens_out: to,
                        ..
                    } => {
                        tokens_in = ti;
                        tokens_out = to;
                    }
                    _ => {}
                }
            }
            (response_text, name, tokens_in, tokens_out)
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

        let objection_count = count_objects(&response_text);
        let objection_snippet =
            extract_first_objection(&response_text).map(|s| truncate_elide(&s, 60));

        eprint!("\r                                            \r");
        match verdict {
            "accept" => {
                let check = "\u{2713}".green().to_string();
                if let Some(ref snippet) = objection_snippet {
                    eprintln!(
                        "  {} {} {} {} accept {} — all resolved {}",
                        step.id().cyan(),
                        "\u{2696}".cyan(),
                        phase.cyan(),
                        "\u{2192}".cyan(),
                        check,
                        snippet.bright_white()
                    );
                } else {
                    eprintln!(
                        "  {} {} {} {} accept {}",
                        step.id().cyan(),
                        "\u{2696}".cyan(),
                        phase.cyan(),
                        "\u{2192}".cyan(),
                        check,
                    );
                }
            }
            "revise" => {
                let count_str = format!("({} objections)", objection_count)
                    .bright_black()
                    .to_string();
                if let Some(ref snippet) = objection_snippet {
                    eprintln!(
                        "  {} {} {} {} {} {}",
                        step.id().cyan(),
                        "\u{2696}".cyan(),
                        phase.cyan(),
                        count_str,
                        "\u{2192}".cyan(),
                        snippet.bright_white()
                    );
                } else {
                    eprintln!(
                        "  {} {} {} {} {}",
                        step.id().cyan(),
                        "\u{2696}".cyan(),
                        phase.cyan(),
                        count_str,
                        "\u{2192}".cyan(),
                    );
                }
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

        let objection_summary =
            extract_first_objection(&response_text).unwrap_or_else(|| "unspecified".to_owned());
        round_summaries.push((
            round,
            phase.to_owned(),
            verdict.to_owned(),
            objection_count,
            objection_summary.clone(),
        ));
        // Store objections for the next round's delta-focused prompt
        previous_objections = Some(response_text.clone());

        match verdict {
            "accept" => {
                write_verdict_transcript(&transcript_path, "accept", round)?;
                print_round_summaries(&round_summaries);
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
                if round < max_rounds {
                    let objections = suggested_revisions(&response_text).ok_or_else(|| {
                        RunError::StepFailed {
                            id: step.id().to_owned(),
                            message: "could not parse suggested revisions from reviewer response"
                                .to_owned(),
                        }
                    })?;
                    let replan_delta = replan_from_objections(
                        config,
                        &hosts,
                        working_dir,
                        state,
                        &format!(
                            "The reviewer REJECTED the plan. Revisions must address:\n\n{objections}"
                        ),
                    )
                    .await?;
                    write_rebuttal_transcript(&transcript_path, &replan_delta)?;
                } else {
                    write_verdict_transcript(&transcript_path, "reject", round)?;
                    print_round_summaries(&round_summaries);
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
            }
            "revise" if round < max_rounds => {
                let objections =
                    suggested_revisions(&response_text).ok_or_else(|| RunError::StepFailed {
                        id: step.id().to_owned(),
                        message: "could not parse suggested revisions from reviewer response"
                            .to_owned(),
                    })?;
                let replan_delta =
                    replan_from_objections(config, &hosts, working_dir, state, objections).await?;
                write_rebuttal_transcript(&transcript_path, &replan_delta)?;
            }
            "revise" => {
                // Rounds exhausted — surface unresolved risks to the caller,
                // which will treat them as accept_with_conditions and continue.
                // No interactive prompt; the pipeline is fully autonomous.
                eprintln!(
                    "  {} {} Maximum {} rounds reached — unresolved risks logged.",
                    step.id().cyan(),
                    "\u{26a0}".yellow(),
                    max_rounds
                );
                write_verdict_transcript(&transcript_path, "revise", round)?;
                print_round_summaries(&round_summaries);
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
    print_round_summaries(&round_summaries);
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

/// The conventional role name for the plan proposer. DESIGN §7 / D5: assay's
/// value is *different-family scrutiny*, so a reviewer must not share the
/// proposer's provider.
const PROPOSER_ROLE: &str = "proposer";

/// Resolve the runtime id bound to `role` via the config role bindings and
/// model registry. Returns `None` if the role is unbound or the bound model is
/// unknown. Read-only: consumes existing `derrick-config` APIs only.
///
/// D79: the family check now compares *runtimes* (`claude-cli`, `codex-cli`, …)
/// rather than the legacy provider name, so it still fires for runtime-only
/// configs that omit `provider`.
fn role_provider(config: &Config, role: &str) -> Option<String> {
    let model_name = config.roles().get(role)?;
    config
        .models()
        .get(model_name)
        .map(derrick_config::ModelDef::resolved_runtime)
}

/// Build a same-family warning for one reviewer if it shares the proposer's
/// provider. `effective_reviewer_provider` lets callers pass a substituted
/// provider (e.g. the D37 codex→claude headless fallback resolves to the
/// `claude` host's provider rather than the configured codex provider).
fn same_family_warning(
    proposer_provider: Option<&str>,
    reviewer_role: &str,
    effective_reviewer_provider: Option<&str>,
) -> Option<String> {
    let proposer = proposer_provider?;
    let reviewer = effective_reviewer_provider?;
    if proposer.eq_ignore_ascii_case(reviewer) {
        Some(format!(
            "reviewer '{reviewer_role}' uses the same provider as the proposer ('{proposer}') — adversarial value reduced"
        ))
    } else {
        None
    }
}

/// Compute same-family warnings for every configured reviewer against the
/// proposer, emitting a prominent `tracing::warn!` for each. Returns the
/// warning lines so the caller can surface them in the assay step output.
/// Does not block — a same-family pairing is a quality smell, not a hard error.
fn same_family_warnings(config: &Config, reviewers: &[String]) -> Vec<String> {
    let proposer_provider = role_provider(config, PROPOSER_ROLE);
    let mut warnings = Vec::new();
    for reviewer_role in reviewers {
        let reviewer_provider = role_provider(config, reviewer_role);
        if let Some(msg) = same_family_warning(
            proposer_provider.as_deref(),
            reviewer_role,
            reviewer_provider.as_deref(),
        ) {
            tracing::warn!(
                step = "assay",
                proposer_role = PROPOSER_ROLE,
                reviewer = %reviewer_role,
                provider = reviewer_provider.as_deref().unwrap_or(""),
                "{msg}"
            );
            warnings.push(msg);
        }
    }
    warnings
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
        // Append with a separator so we can extract the latest delta later
        let sep = "\n\n---\n\n";
        append_log(&plan_path, &format!("{sep}{delta}"))?;
    }
    Ok(delta)
}

pub fn reconcile_verdicts(
    outcomes: &[ReviewerOutcome],
    on_split: OnSplit,
    combined_path: &Path,
    repo_root: &Path,
) -> Result<StepExecution, RunError> {
    // Fail closed: never reconcile with no outcomes. The caller enforces the
    // quorum, but reconciling an empty set would silently "accept" nothing.
    if outcomes.is_empty() {
        return Err(RunError::Config(
            "assay reconciliation requires at least one reviewer outcome".to_owned(),
        ));
    }
    // A hard `reject` from any reviewer is a real blocker.
    // A `revise` (rounds exhausted) is treated as accept_with_conditions.
    let any_hard_reject = outcomes.iter().any(|o| o.verdict == "reject");
    let all_accept = outcomes.iter().all(|o| o.verdict == "accept");
    let summary = outcomes
        .iter()
        .map(|o| format!("- {}: {}", o.role, o.verdict))
        .collect::<Vec<_>>()
        .join("\n");

    // Derive the final verdict respecting on_split policy but treating
    // `revise` as `accept_with_conditions` rather than an automatic reject.
    let final_verdict: &str = match on_split {
        OnSplit::Reject => {
            if any_hard_reject {
                "reject"
            } else {
                "accept" // includes revise-exhausted outcomes
            }
        }
        OnSplit::Majority => {
            // Count explicit rejects as blockers; revise counts as accept.
            let hard_rejects = outcomes.iter().filter(|o| o.verdict == "reject").count();
            let non_rejects = outcomes.len().saturating_sub(hard_rejects);
            if non_rejects > hard_rejects {
                "accept"
            } else {
                "reject"
            }
        }
        OnSplit::Human => {
            // `on_split: human` used to trigger an interactive prompt.
            // In autonomous mode, fall back to majority semantics so the
            // pipeline can proceed headlessly. A deliberate `reject` still
            // halts the pipeline.
            if any_hard_reject { "reject" } else { "accept" }
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

    match final_verdict {
        "reject" => Ok(StepExecution::halted(
            artifacts,
            format!("assay rejected (on_split: {policy_name})"),
        )
        .with_tokens(tokens_in, tokens_out)),
        _ => {
            // accept or accept_with_conditions — log any constitution risks and continue.
            let violations: Vec<&str> = outcomes
                .iter()
                .flat_map(|o| o.constitution_violations.iter().map(|s| s.as_str()))
                .collect();
            let has_revise = outcomes.iter().any(|o| o.verdict == "revise");
            if !violations.is_empty() {
                eprintln!(
                    "  {} {}",
                    "\u{26a0}".yellow(),
                    "Constitution risks noted (continuing)".yellow()
                );
                for v in &violations {
                    eprintln!("  {}  {}", "\u{2022}".yellow(), v.yellow());
                }
            } else if !all_accept && has_revise {
                eprintln!(
                    "  {} {}",
                    "assay".cyan(),
                    "Unresolved risks noted — continuing.".yellow()
                );
            }
            Ok(StepExecution::success(artifacts).with_tokens(tokens_in, tokens_out))
        }
    }
}

/// Parse the verdict from the reviewer's response text — strict and fail-closed.
///
/// The verdict must appear as a *standalone token* in a recognised declaration
/// form. Three shapes are accepted (all case-insensitive, with optional
/// surrounding markdown bold/italic/inline-code wrappers):
///
///   1. A line that is exactly the verdict word — `accept`, `**revise**`,
///      `` `reject` ``.
///   2. A label line `Verdict: <word>` — e.g. `Verdict: accept`.
///   3. A bolded label line `**Verdict:** <word>` — e.g. `**Verdict:** reject`.
///
/// Substring matches inside prose (`"reject (with caveats)"`,
/// `"rejection would be inadvisable — accept"`, `"could reject"`, `"rejectX"`)
/// are deliberately NOT recognised: they are not standalone declarations.
///
/// Returns `None` (fail closed) when no recognised declaration is found, or
/// when two or more *distinct* verdict words are declared (ambiguous). The
/// caller maps `None` to a `StepFailed` that halts the pipeline and surfaces
/// the raw response.
pub fn parse_verdict(text: &str) -> Option<&'static str> {
    let mut found: Option<&'static str> = None;

    for line in text.lines() {
        let Some(word) = parse_verdict_line(line) else {
            continue;
        };
        match found {
            None => found = Some(word),
            Some(prev) if prev == word => {}
            // Two distinct verdict words declared — ambiguous, fail closed.
            Some(_) => return None,
        }
    }

    found
}

/// Recognise a single line as a standalone verdict declaration, returning the
/// canonical verdict word if so. See [`parse_verdict`] for the accepted forms.
fn parse_verdict_line(line: &str) -> Option<&'static str> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }

    // Label form: `Verdict: <word>` / `**Verdict:** <word>` / `Verdict <word>`.
    // Strip a leading markdown-wrapped `Verdict` label and an optional colon.
    if let Some(rest) = strip_verdict_label(trimmed) {
        return verdict_word(rest);
    }

    // Bare-word form: the entire line is just the (optionally wrapped) word.
    verdict_word(trimmed)
}

/// If `line` begins with a `Verdict` label (optionally markdown-wrapped, with an
/// optional trailing colon), return the remainder after the label. Otherwise
/// `None`.
fn strip_verdict_label(line: &str) -> Option<&str> {
    // Remove markdown emphasis/code wrappers around the whole `**Verdict:**`
    // token first, then re-check. We handle the common `**Verdict:**` shape by
    // stripping a leading run of `*`/`_`/`` ` `` characters.
    let after_wrap = line.trim_start_matches(['*', '_', '`']);
    let lower = after_wrap.to_ascii_lowercase();
    let rest = lower.strip_prefix("verdict")?;
    // Map the byte offset back into `after_wrap` (ASCII prefix, so offsets align).
    let rest = &after_wrap[after_wrap.len() - rest.len()..];
    // Allow `**` / `:` / whitespace between the label and the word.
    let rest = rest.trim_start_matches(|c: char| {
        c == '*' || c == '_' || c == '`' || c == ':' || c.is_whitespace()
    });
    Some(rest)
}

/// Return the canonical verdict word if `token` is exactly one verdict word
/// (after stripping optional markdown wrappers). Anything else yields `None`.
fn verdict_word(token: &str) -> Option<&'static str> {
    let cleaned = token.trim().trim_matches(['*', '_', '`']).trim();
    match cleaned.to_ascii_lowercase().as_str() {
        "accept" => Some("accept"),
        "revise" => Some("revise"),
        "reject" => Some("reject"),
        _ => None,
    }
}

/// Extract suggested revisions from the reviewer's response.
/// Looks for `## Suggested revisions` heading.
/// Extract suggested revisions from the reviewer's response.
/// First tries to find an explicit `## Suggested revisions` section.
/// Falls back to everything between the first heading and `## Verdict`.
pub fn suggested_revisions(text: &str) -> Option<&str> {
    let start_marker = "## Suggested revisions";
    if let Some(start) = text.find(start_marker) {
        let rest = &text[start + start_marker.len()..];
        let end = rest.find("\n## ").unwrap_or(rest.len());
        let extracted = rest[..end].trim();
        if !extracted.is_empty() {
            return Some(extracted);
        }
    }

    let verdict_pos = find_h2(text, "## Verdict")?;

    if let Some(first_heading) = find_first_h2(text) {
        if first_heading < verdict_pos {
            let objections = &text[first_heading..verdict_pos].trim();
            if !objections.is_empty() {
                return Some(objections);
            }
        }
    }

    let objections = text[..verdict_pos].trim();
    if objections.is_empty() || !has_structured_revision_content(objections) {
        None
    } else {
        Some(objections)
    }
}

fn find_h2(text: &str, heading: &str) -> Option<usize> {
    if text.starts_with(heading) {
        Some(0)
    } else {
        text.find(&format!("\n{heading}")).map(|pos| pos + 1)
    }
}

fn find_first_h2(text: &str) -> Option<usize> {
    if text.starts_with("## ") {
        Some(0)
    } else {
        text.find("\n## ").map(|pos| pos + 1)
    }
}

fn has_structured_revision_content(text: &str) -> bool {
    text.lines().map(str::trim).any(|line| {
        if line.is_empty() {
            return false;
        }
        line.starts_with("## ")
            || line.starts_with("### ")
            || line.starts_with("**")
            || line.starts_with("- ")
            || line.starts_with("* ")
            || line.chars().next().is_some_and(|c| c.is_ascii_digit())
    })
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
            if !cleaned.is_empty() && !is_non_violation_marker(&cleaned) {
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

/// Returns `true` when a cleaned violation line is a "nothing found" marker
/// rather than an actual violation.
///
/// LLMs often write `**No contradictions found.**` or `- None` inside the
/// Constitution Contradictions section to signal a clean review. These should
/// not be treated as violations.
fn is_non_violation_marker(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let lower = lower.trim_end_matches('.').trim();
    // Standalone "none" or "n/a".
    if lower == "none" || lower == "n/a" {
        return true;
    }
    // "No contradictions/violations/conflicts/issues [found/detected/present/identified]"
    // We require a terminal positive word so that "No error handling" (a real
    // violation) is NOT filtered — it ends with a noun, not a resolution word.
    let terminal_words = [
        "found",
        "detected",
        "present",
        "identified",
        "noted",
        "observed",
        "exist",
        "exists",
        "encountered",
    ];
    if terminal_words.iter().any(|w| lower.ends_with(w)) {
        let trigger_words = [
            "contradictions",
            "violations",
            "conflicts",
            "issues",
            "contradictions found",
            "violations found",
        ];
        if trigger_words.iter().any(|w| lower.contains(w)) {
            return true;
        }
    }
    // Catch bare "No contradictions found" even without trailing resolution word
    // when the phrase itself is conclusive.
    lower.contains("no contradictions") || lower.contains("no violations")
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

/// Extract the text of the first bold-numbered objection from a review response.
/// Looks for `**1. Title**` or `**A. Title**` and returns the title portion.
/// Falls back to any bold line if numbered extraction fails, then to the
/// first non-empty line after the preamble.
fn extract_first_objection(text: &str) -> Option<String> {
    // Try bold-numbered items: **1. Title** or **A. Title**
    if let Some(result) = extract_bold_numbered(text) {
        return Some(result);
    }

    // Try any bold line: **Some highlighted text**
    if let Some(result) = extract_any_bold(text) {
        return Some(result);
    }

    // Fallback: first non-empty, non-comment, non-verdict line
    text.lines()
        .map(|l| l.trim())
        .find(|l| {
            !l.is_empty()
                && !l.starts_with("## ")
                && !l.starts_with("### ")
                && !l.starts_with("```")
                && !l.starts_with("---")
                && !l.eq_ignore_ascii_case("## Verdict")
                && !l.starts_with("Now I have")
                && !l.starts_with("Let me")
                && l.len() > 20
        })
        .map(|l| truncate_elide(l, 80))
}

fn extract_bold_numbered(text: &str) -> Option<String> {
    let line = text.lines().find(|l| {
        let t = l.trim();
        t.starts_with("**")
            && t.len() > 5
            && (t.contains("\u{2014}") || t.contains(". ") || t.contains(") "))
    })?;
    let trimmed = line.trim();
    let stripped = strip_markdown_emphasis(trimmed);
    // After stripping **, we have "R1 — Title" or "1. Title" or "A) Title"
    // The separator is em-dash (U+2014) + space, ". ", or ") "
    let (after_sep, sep_len) = stripped
        .find("\u{2014} ")
        .map(|i| (i, "\u{2014} ".len()))  // em dash is 3 bytes + 1 space = 4
        .or_else(|| stripped.find(". ").map(|i| (i, 2)))
        .or_else(|| stripped.find(") ").map(|i| (i, 2)))
        .or_else(|| stripped.find(".  ").map(|i| (i, 3)))?;
    let sep_end = after_sep + sep_len;
    let result = stripped[sep_end..].trim();
    if result.is_empty() {
        None
    } else {
        Some(truncate_elide(result, 80))
    }
}

fn extract_any_bold(text: &str) -> Option<String> {
    let line = text.lines().find(|l| {
        let t = l.trim();
        (t.starts_with("**") && t[2..].contains("**"))
            || (t.starts_with('*') && !t.starts_with("**") && t[1..].contains('*'))
    })?;
    let result = strip_markdown_emphasis(line.trim());
    let result = result.trim();
    if result.is_empty() || result.len() < 10 {
        return None;
    }
    Some(truncate_elide(result, 80))
}

/// Count the number of objection items in a reviewer response.
/// Looks for bold items with objection identifiers
/// (`**1.`, `**R1 —`, `**E1 —`, `**C1 —`, `**A.`, etc.)
fn count_objects(text: &str) -> usize {
    text.lines()
        .filter(|l| {
            let t = l.trim();
            if !t.starts_with("**") || t.len() < 6 {
                return false;
            }
            let after = &t[2..];
            // Must start with an identifier (digit, or letter+digit)
            let first = after.as_bytes().first().copied().unwrap_or(0);
            if !first.is_ascii_alphanumeric() {
                return false;
            }
            // Must have a separator after the identifier
            let id_len = after
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .count();
            let after_id = &after[id_len..];
            after_id.contains(". ") || after_id.contains("\u{2014}") || after_id.contains(") ")
        })
        .count()
}

fn print_round_summaries(summaries: &[(usize, String, String, usize, String)]) {
    if summaries.len() <= 1 {
        return;
    }
    let final_verdict = summaries
        .last()
        .map(|(_, _, v, _, _)| v.as_str())
        .unwrap_or("");
    let total_rounds = summaries.len();
    let total_objs: usize = summaries.iter().map(|(_, _, _, c, _)| c).sum();
    let verdict_str = match final_verdict {
        "accept" => format!("{} accepted", "\u{2713}").green().to_string(),
        "reject" => format!("{} rejected", "\u{2717}").red().to_string(),
        _ => format!("{} halted", "\u{26a0}").yellow().to_string(),
    };
    eprintln!(
        "  {} {}",
        "\u{250c}".bright_cyan(),
        "Assay Deliberation".bold()
    );
    for (round, phase, verdict, count, objection) in summaries {
        let bullet = match verdict.as_str() {
            "accept" => "\u{2713}".green().to_string(),
            "reject" => "\u{2717}".red().to_string(),
            _ => "\u{2022}".cyan().to_string(),
        };
        let obj = if objection == "unspecified" {
            String::new()
        } else {
            format!(" — {}", objection.bright_white())
        };
        eprintln!(
            "  {} {} Round {}: {} ({}){}{}",
            "\u{2502}".bright_cyan(),
            bullet,
            round.to_string().cyan(),
            phase.cyan(),
            match verdict.as_str() {
                "accept" => format!("{} accept", "\u{2713}").green().to_string(),
                "reject" => format!("{} reject", "\u{2717}").red().to_string(),
                _ => format!("{} revise", count).yellow().to_string(),
            },
            if verdict == "revise" {
                format!(" objection{}", if *count == 1 { "" } else { "s" })
                    .bright_black()
                    .to_string()
            } else {
                String::new()
            },
            obj,
        );
    }
    eprintln!(
        "  {} {} {} {} rounds, {} {} {}",
        "\u{2514}".bright_cyan(),
        "Assay:".cyan(),
        format!("{total_rounds}").cyan(),
        "rounds,".cyan(),
        format!("{total_objs}").cyan(),
        "objections,".cyan(),
        verdict_str
    );
}

/// Extract the most recent delta from plan.md by finding the last
/// `---` separator and returning everything after it.
fn last_delta_from_plan(plan_path: &Path, _previous: &str) -> Result<String, RunError> {
    let content = read_to_string(plan_path)?;
    if let Some(pos) = content.rfind("\n---\n\n") {
        let delta = content[pos + 5..].trim().to_owned();
        if !delta.is_empty() {
            return Ok(delta);
        }
    }
    // No separator found — return the full plan as fallback
    read_to_string(plan_path)
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

/// Truncate a string to at most `max` characters (by `char` count), appending
/// an ellipsis (`…`) when truncation occurs. Returns the input unchanged when
/// it already fits.
fn truncate_elide(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
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
    fn parse_verdict_bold_formatting() {
        assert_eq!(parse_verdict("## Verdict\n**revise**"), Some("revise"));
        assert_eq!(parse_verdict("## Verdict\n*accept*"), Some("accept"));
        assert_eq!(parse_verdict("## Verdict\n__reject__"), Some("reject"));
    }

    #[test]
    fn parse_verdict_inline_code() {
        assert_eq!(parse_verdict("## Verdict\n`revise`"), Some("revise"));
    }

    #[test]
    fn parse_verdict_strict_table() {
        // (input, expected) — the heart of the fail-closed contract.
        let cases: &[(&str, Option<&'static str>)] = &[
            // --- Bare-word declarations (a whole line is the verdict word) ---
            ("accept", Some("accept")),
            ("revise", Some("revise")),
            ("reject", Some("reject")),
            ("## Verdict\n\nACCEPT", Some("accept")),
            ("## Verdict\n**reject**", Some("reject")),
            ("## Verdict\n`revise`", Some("revise")),
            ("## Verdict\n  __accept__  ", Some("accept")),
            // --- Label declarations ---
            ("Verdict: accept", Some("accept")),
            ("**Verdict:** reject", Some("reject")),
            ("**Verdict:** revise", Some("revise")),
            ("verdict:reject", Some("reject")),
            ("Verdict: **accept**", Some("accept")),
            ("**Verdict:** `revise`", Some("revise")),
            // The recommended end-of-response shape from the system prompt.
            (
                "Lots of analysis here.\n\nmore prose\n\n**Verdict:** accept",
                Some("accept"),
            ),
            // --- Substring / prose noise must NOT be recognised ---
            // The whole line is prose, not a standalone verdict token: fail
            // closed rather than guess. Critically it must NEVER return reject.
            ("Rejection would be inadvisable — accept", None),
            ("reject (with caveats)", None),
            ("could reject", None),
            ("do not reject this", None),
            ("rejectX", None),
            ("X reject", None),
            ("accept the plan", None),
            ("I would reject the approach entirely.", None),
            ("no verdict here", None),
            ("## Verdict\ninvalid", None),
            ("", None),
            // A prose line plus a clean declaration: declaration wins, prose
            // line is ignored (it is not a standalone token).
            (
                "I am tempted to reject but on balance:\n\n**Verdict:** revise",
                Some("revise"),
            ),
            // --- Ambiguity: two DISTINCT declarations => fail closed (None) ---
            ("accept\nreject", None),
            ("**Verdict:** accept\n**Verdict:** reject", None),
            ("Verdict: revise\n\naccept", None),
            // --- Same verdict declared twice (e.g. metadata + body) is fine ---
            ("verdict: revise\n\n## Verdict\n\nrevise", Some("revise")),
            ("accept\n\n**Verdict:** accept", Some("accept")),
        ];

        for (input, expected) in cases {
            assert_eq!(
                parse_verdict(input),
                *expected,
                "parse_verdict({input:?}) should be {expected:?}",
            );
        }
    }

    #[test]
    fn phase_names_are_correct() {
        assert_eq!(phase_name(1), "Cross-Examination");
        assert_eq!(phase_name(2), "Deliberation");
        assert_eq!(phase_name(10), "Deliberation");
    }

    #[test]
    fn suggested_revisions_fallback_extracts_body_before_verdict() {
        let text = "Now I have the full picture.

## Plan Review

### Highest Risks

**1. Clap won't read DERRICK_VERSION automatically (critical)**

The plan targets main.rs but the version is in commands/mod.rs.

### Constitution Contradictions

**1. Missing test coverage plan (hard violation)**

No tests planned for new code.

## Verdict

revise
";
        let result = suggested_revisions(text);
        assert!(result.is_some(), "should fall back to body before verdict");
        let body = result.unwrap();
        assert!(body.contains("Highest Risks"));
        assert!(body.contains("Constitution Contradictions"));
        assert!(!body.contains("## Verdict"));
    }

    #[test]
    fn suggested_revisions_prefers_explicit_section() {
        let text = "Preamble.

## Highest Risks

Irrelevant.

## Suggested revisions

only this

## Verdict

revise
";
        assert_eq!(suggested_revisions(text), Some("only this"));
    }

    #[test]
    fn suggested_revisions_no_sections_returns_none() {
        let text = "blah\n## Verdict\naccept";
        assert_eq!(suggested_revisions(text), None);
    }

    #[test]
    fn suggested_revisions_fallback_extracts_structured_prose_without_headings() {
        let text = r#"The "latest plan changes" claim that all three concerns are resolved does not match the actual file content at lines 1–107.

**Concern: All three previous objections remain unresolved in the canonical plan body (HIGH)**

The file was not edited.

- Step 3 and Step 6 still use `cargo run`
- Steps 1 and 4 still have no version pin

## Verdict

revise
"#;
        let result = suggested_revisions(text);
        assert!(
            result.is_some(),
            "should extract structured prose before verdict"
        );
        let body = result.unwrap();
        assert!(body.contains("Concern: All three previous objections remain unresolved"));
        assert!(body.contains("Step 3 and Step 6 still use `cargo run`"));
        assert!(!body.contains("## Verdict"));
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
            "Review the speckit plan. Identify the highest risks, missing edge cases, and constitution contradictions. End with an H2 `## Verdict` section. The final line of your response MUST be exactly `**Verdict:** accept`, `**Verdict:** revise`, or `**Verdict:** reject` (one verdict word only, on its own line). Do not wrap the verdict word in extra prose on that line."
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

    #[test]
    fn detect_constitution_violations_ignores_no_contradictions_found_marker() {
        // When the LLM writes "**No contradictions found.**" inside the
        // Constitution section to signal a clean review, it must not be
        // treated as a violation.
        let text = r#"### Constitution Contradictions

| Rule | Finding |
|------|---------|
| Language auto-detect | ✅ passes |
| Unit tests only | ✅ passes |

**No contradictions found.**

## Verdict

accept"#;
        let violations = detect_constitution_violations(text);
        assert!(
            violations.is_empty(),
            "expected no violations but got: {violations:?}"
        );
    }

    #[test]
    fn is_non_violation_marker_matches_common_patterns() {
        // "Nothing to report" markers must be filtered.
        assert!(is_non_violation_marker("No contradictions found."));
        assert!(is_non_violation_marker("No contradictions found"));
        assert!(is_non_violation_marker("No violations detected"));
        assert!(is_non_violation_marker("No violations noted"));
        assert!(is_non_violation_marker("None"));
        assert!(is_non_violation_marker("none."));
        assert!(is_non_violation_marker("N/A"));
        // Real violations whose first word is "No" must NOT be filtered.
        assert!(!is_non_violation_marker("No error handling"));
        assert!(!is_non_violation_marker("No test coverage"));
        assert!(!is_non_violation_marker("No conflicts"));
        // Other real violations must NOT be filtered.
        assert!(!is_non_violation_marker("Missing test coverage plan"));
        assert!(!is_non_violation_marker("Error handling omitted"));
    }

    #[test]
    fn count_objects_counts_bold_numbered_items() {
        let text = r#"Some preamble

**1. First objection**

Details.

**2. Second objection**

**A. Edge case**
"#;
        assert_eq!(count_objects(text), 3);
    }

    #[test]
    fn count_objects_ignores_non_bold_lines() {
        let text = r#"1. Plain numbered
* Bullet
- Dash
No marker"#;
        assert_eq!(count_objects(text), 0);
    }

    #[test]
    fn extract_first_objection_matches_bold_item() {
        let text = r#"Preamble

**1. Clap won't read DERRICK_VERSION automatically (critical)**

Details.

**2. Second item**
"#;
        let result = extract_first_objection(text);
        assert_eq!(
            result,
            Some("Clap won't read DERRICK_VERSION automatically (critical)".to_owned())
        );
    }

    #[test]
    fn extract_first_objection_lettered_items() {
        let text = r#"**A. No v*-matching tag, but other tags exist**

Details"#;
        let result = extract_first_objection(text);
        assert!(result.is_some());
        assert!(result.unwrap().contains("No v"));
    }

    #[test]
    fn extract_first_objection_no_match_returns_none() {
        assert_eq!(extract_first_objection("## Verdict\naccept"), None);
        assert_eq!(extract_first_objection("No bold items here"), None);
    }

    fn outcome(role: &str, verdict: &str) -> ReviewerOutcome {
        ReviewerOutcome {
            role: role.to_owned(),
            verdict: verdict.to_owned(),
            verdict_path: PathBuf::from(format!("/tmp/{role}/verdict.md")),
            tokens_in: 0,
            tokens_out: 0,
            constitution_violations: Vec::new(),
            rounds_used: 1,
        }
    }

    #[test]
    fn same_family_warning_flags_matching_provider() {
        // Same provider (case-insensitive) => warning.
        let w = same_family_warning(Some("anthropic"), "reviewer", Some("Anthropic"));
        assert!(w.is_some(), "matching provider should warn");
        let msg = w.unwrap();
        assert!(msg.contains("reviewer"));
        assert!(msg.contains("anthropic"));
        assert!(msg.contains("adversarial value reduced"));
    }

    #[test]
    fn same_family_warning_silent_on_different_provider() {
        assert_eq!(
            same_family_warning(Some("anthropic"), "reviewer", Some("openai")),
            None
        );
        // Unknown provider on either side => cannot assert same-family => silent.
        assert_eq!(same_family_warning(None, "reviewer", Some("openai")), None);
        assert_eq!(
            same_family_warning(Some("anthropic"), "reviewer", None),
            None
        );
    }

    #[test]
    fn same_family_warnings_resolves_from_config() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // proposer and reviewer both bound to anthropic-provider models.
        std::fs::write(
            tmp.path().join("derrick.yaml"),
            "version: 1\n\
             site:\n  name: test\n  prefix: tst\n\
             models:\n\
             \x20 opus:\n    provider: anthropic\n    model: claude-opus-4-8\n\
             \x20 gpt5:\n    provider: openai\n    model: gpt-5\n\
             roles:\n  proposer: opus\n  reviewer: opus\n  reviewer-x: gpt5\n",
        )
        .expect("write config");
        let config = Config::load_layered(tmp.path()).expect("load config");

        // Same-family reviewer => warning.
        let warnings = same_family_warnings(&config, &["reviewer".to_owned()]);
        assert_eq!(warnings.len(), 1, "same-family reviewer should warn");
        assert!(warnings[0].contains("reviewer"));

        // Different-family reviewer => no warning.
        let warnings = same_family_warnings(&config, &["reviewer-x".to_owned()]);
        assert!(
            warnings.is_empty(),
            "different-family reviewer must not warn"
        );

        // Mixed list => only the same-family one warns.
        let warnings =
            same_family_warnings(&config, &["reviewer".to_owned(), "reviewer-x".to_owned()]);
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn reconcile_verdicts_empty_fails_closed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let combined = tmp.path().join("verdict.md");
        let result = reconcile_verdicts(&[], OnSplit::Reject, &combined, tmp.path());
        match result {
            Err(RunError::Config(msg)) => assert!(msg.contains("at least one reviewer outcome")),
            Err(other) => panic!("expected Config error, got {other:?}"),
            Ok(_) => panic!("expected Config error, got Ok"),
        }
    }

    #[test]
    fn reconcile_verdicts_all_accept_succeeds() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let combined = tmp.path().join("verdict.md");
        let outcomes = vec![
            outcome("reviewer", "accept"),
            outcome("reviewer-2", "accept"),
        ];
        let exec = reconcile_verdicts(&outcomes, OnSplit::Reject, &combined, tmp.path())
            .expect("reconcile");
        assert_eq!(exec.status, crate::types::StepStatus::Success);
    }

    #[test]
    fn reconcile_verdicts_hard_reject_halts() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let combined = tmp.path().join("verdict.md");
        let outcomes = vec![
            outcome("reviewer", "accept"),
            outcome("reviewer-2", "reject"),
        ];
        let exec = reconcile_verdicts(&outcomes, OnSplit::Reject, &combined, tmp.path())
            .expect("reconcile");
        assert_eq!(exec.status, crate::types::StepStatus::Halted);
    }

    // ---- async round-loop coverage ------------------------------------
    //
    // Everything above is `#[test]` on pure helpers. Nothing previously drove
    // `run_reviewer_rounds`/`execute_assay_core`/`replan_from_objections`/
    // `detect_codex_fallback` as `#[tokio::test]`s — the only prior coverage
    // was one E2E smoke test (in `derrick-flow`) whose mock reviewer accepts
    // on round 1. These tests drive the real async multi-round loop:
    //   - a single reviewer that REJECTs round 1 (with suggested revisions)
    //     then ACCEPTs round 2, exercising `replan_from_objections` and the
    //     round-2 delta-prompt path via `last_delta_from_plan`.
    //   - multi-reviewer split verdicts under both `on_split: reject` and
    //     `on_split: majority`, exercising the concurrent semaphore-gated
    //     `tokio::spawn` fan-out in `execute_assay_core` and
    //     `reconcile_verdicts`.
    //   - the `detect_codex_fallback` headless-codex path, which calls the
    //     `claude` host adapter directly instead of `derrick_models::resolve_role`.
    //
    // Reviewer "models" are the real `provider: shell` adapter pointed at a
    // tiny mock script (no network, no real host CLI) — the same pattern
    // `derrick-flow`'s own `/drill` E2E tests use. The codex-fallback case
    // mocks `derrick_tools::HostAdapter` directly, matching `StaticHost` in
    // `derrick-flow/src/lib.rs`.

    use derrick_tools::{HostAdapter, HostError, HostRegistry, HostRequest, HostResponse};

    fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).expect("write script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).expect("stat script").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).expect("chmod script");
        }
        path
    }

    /// Always responds with a bare `accept` verdict.
    fn accept_script(dir: &Path) -> PathBuf {
        write_script(
            dir,
            "accept-reviewer",
            "#!/bin/sh\ncat > /dev/null\nprintf '## Verdict\\naccept\\n'\n",
        )
    }

    /// Always responds with a bare `reject` verdict (no `## Suggested
    /// revisions` section — valid only when this is the *final* round, since
    /// `run_reviewer_rounds` only parses suggested revisions when a reject
    /// triggers a replan, i.e. when `round < max_rounds`).
    fn reject_script(dir: &Path) -> PathBuf {
        write_script(
            dir,
            "reject-reviewer",
            "#!/bin/sh\ncat > /dev/null\nprintf '## Verdict\\nreject\\n'\n",
        )
    }

    /// First invocation REJECTs with a `## Suggested revisions` section (so
    /// `suggested_revisions()` can extract objections for the replan);
    /// records a state file so every subsequent invocation ACCEPTs instead.
    fn reject_then_accept_script(dir: &Path) -> PathBuf {
        let state = dir.join("reject-then-accept.seen");
        let body = format!(
            "#!/bin/sh\ncat > /dev/null\nif [ -f '{state}' ]; then\n  printf '## Verdict\\naccept\\n'\nelse\n  printf seen > '{state}'\n  printf '## Suggested revisions\\n- address concern A\\n## Verdict\\nreject\\n'\nfi\n",
            state = state.display()
        );
        write_script(dir, "reject-then-accept-reviewer", &body)
    }

    /// Creates `<tmp>/specs/001-test/{spec.md,plan.md}` plus a non-placeholder
    /// constitution, matching the layout `run_reviewer_rounds` expects.
    fn feature_workspace() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let feature_dir = PathBuf::from("specs/001-test");
        std::fs::create_dir_all(tmp.path().join(&feature_dir)).expect("mkdir feature dir");
        std::fs::write(tmp.path().join(&feature_dir).join("spec.md"), "spec body")
            .expect("write spec.md");
        std::fs::write(tmp.path().join(&feature_dir).join("plan.md"), "plan body")
            .expect("write plan.md");
        std::fs::create_dir_all(tmp.path().join(".specify/memory"))
            .expect("mkdir constitution dir");
        std::fs::write(
            tmp.path().join(".specify/memory/constitution.md"),
            "constitution",
        )
        .expect("write constitution.md");
        (tmp, feature_dir)
    }

    fn pipeline_step(config: &Config, id: &str) -> derrick_config::PipelineStep {
        config
            .pipeline()
            .iter()
            .find(|step| step.id() == id)
            .unwrap_or_else(|| panic!("pipeline step {id:?} not found in config"))
            .clone()
    }

    /// Assembles a `derrick.yaml` document from scenario-specific `models`,
    /// `roles`, `tools.assay`, and `pipeline` fragments, sharing the
    /// `site` / `tools.speckit` / `tools.substrate` / `tools.copilot` /
    /// `guardrails` / `parallelism` / `state` boilerplate that every assay
    /// test config needs but none of them vary. Each fragment must already
    /// include its own trailing newline; `assay` and `pipeline` must be
    /// indented to match the surrounding block (`assay` at 2 spaces under
    /// `tools:`, `pipeline` at 2 spaces under `pipeline:`).
    fn base_yaml(models: &str, roles: &str, assay: &str, pipeline: &str) -> String {
        format!(
            r#"version: 1
site:
  name: test
  prefix: tst
models:
{models}roles:
{roles}tools:
  speckit:
    enabled: true
    version: ">=0.4.0"
  substrate:
    backend: native
    mode: solo
  copilot:
    enabled: false
    agent_identity: derrick-hand
{assay}pipeline:
{pipeline}guardrails:
  constitution_path: .specify/memory/constitution.md
  forbid_paths: []
  required_labels: []
parallelism:
  batch_max: 8
  step_max: 4
  assay_max: 2
state:
  dir: .derrick
  log_runs: true
  worktree_root: .derrick/worktrees
"#
        )
    }

    fn single_reviewer_yaml(reviewer_cli: &Path, rounds: u32) -> String {
        let models = format!(
            "  shell-reviewer:\n    provider: shell\n    cli: \"{cli}\"\n    model: shell-reviewer\n",
            cli = reviewer_cli.display(),
        );
        let roles = "  proposer: shell-reviewer\n  reviewer: shell-reviewer\n".to_owned();
        let assay = format!(
            "  assay:\n    enabled: true\n    role: reviewer\n    reviewers: [reviewer]\n    rounds: {rounds}\n"
        );
        let pipeline = "  - id: plan\n    role: proposer\n    host: claude\n  - id: assay\n    runner: derrick\n    skippable: true\n".to_owned();
        base_yaml(&models, &roles, &assay, &pipeline)
    }

    /// Builds a `derrick.yaml` with one `provider: shell` reviewer role per
    /// `(role_name, script_path)` pair, all reviewing under `on_split`. No
    /// `plan` step is included — these scenarios use `rounds: 1`, so no
    /// reviewer ever rejects with rounds remaining and `replan_from_objections`
    /// is never invoked.
    fn multi_reviewer_yaml(reviewers: &[(&str, &Path)], on_split: &str) -> String {
        let mut models = String::new();
        let mut roles = String::new();
        let mut reviewer_names = Vec::new();
        for (role, cli) in reviewers {
            models.push_str(&format!(
                "  shell-{role}:\n    provider: shell\n    cli: \"{}\"\n    model: shell-{role}\n",
                cli.display()
            ));
            roles.push_str(&format!("  {role}: shell-{role}\n"));
            reviewer_names.push((*role).to_owned());
        }
        let first_role = reviewers[0].0;
        let reviewer_list = reviewer_names.join(", ");
        let assay = format!(
            "  assay:\n    enabled: true\n    role: {first_role}\n    reviewers: [{reviewer_list}]\n    on_split: {on_split}\n    rounds: 1\n"
        );
        let pipeline = "  - id: assay\n    runner: derrick\n    skippable: true\n".to_owned();
        base_yaml(&models, &roles, &assay, &pipeline)
    }

    /// Mock `claude` host adapter used by `replan_from_objections` (the
    /// `plan` step's `host:` binding) — returns a fixed, recognisable delta so
    /// tests can assert it landed in `plan.md`.
    struct ReplanHost;

    #[async_trait::async_trait]
    impl HostAdapter for ReplanHost {
        fn name(&self) -> &str {
            "claude"
        }

        fn is_available(&self) -> bool {
            true
        }

        async fn run(&self, _request: HostRequest) -> Result<HostResponse, HostError> {
            Ok(HostResponse {
                stdout: "REPLAN-DELTA: address concern A\n".to_owned(),
                stderr: String::new(),
                exit_code: 0,
                elapsed: std::time::Duration::from_millis(1),
                tokens_in: 0,
                tokens_out: 0,
                pid: None,
            })
        }
    }

    /// Mock `claude` host adapter for the `detect_codex_fallback` path —
    /// returns a fixed transcript and reports fixed token counts so the test
    /// can assert both flow all the way through to `ReviewerOutcome`.
    struct StaticClaudeHost {
        stdout: &'static str,
        tokens_in: u32,
        tokens_out: u32,
    }

    #[async_trait::async_trait]
    impl HostAdapter for StaticClaudeHost {
        fn name(&self) -> &str {
            "claude"
        }

        fn is_available(&self) -> bool {
            true
        }

        async fn run(&self, _request: HostRequest) -> Result<HostResponse, HostError> {
            Ok(HostResponse {
                stdout: self.stdout.to_owned(),
                stderr: String::new(),
                exit_code: 0,
                elapsed: std::time::Duration::from_millis(1),
                tokens_in: self.tokens_in,
                tokens_out: self.tokens_out,
                pid: None,
            })
        }
    }

    #[tokio::test]
    async fn execute_assay_single_reviewer_reject_then_accept_replans_and_succeeds() {
        let (tmp, feature_dir) = feature_workspace();
        let reviewer_cli = reject_then_accept_script(tmp.path());

        let yaml = single_reviewer_yaml(&reviewer_cli, 2);
        std::fs::write(tmp.path().join("derrick.yaml"), &yaml).expect("write derrick.yaml");
        let config = Config::load_from_path(&tmp.path().join("derrick.yaml")).expect("load config");

        let mut hosts = HostRegistry::empty();
        hosts.register("claude", Box::new(ReplanHost));
        let hosts = Arc::new(hosts);

        let step = pipeline_step(&config, "assay");
        let mut state = ExecutionState::new(
            "build the thing".to_owned(),
            "run-rr-1".to_owned(),
            tmp.path().join(".derrick/runs/run-rr-1"),
        );
        state.feature_dir = Some(feature_dir.clone());

        let log_path = tmp.path().join("assay.log");
        let exec = execute_assay(
            &config,
            hosts,
            tmp.path(),
            tmp.path(),
            &feature_dir,
            "build the thing",
            "run-rr-1",
            &step,
            &log_path,
            &mut state,
        )
        .await
        .expect("execute_assay should succeed once the replanned round is accepted");

        assert_eq!(exec.status, crate::types::StepStatus::Success);

        // The reject-round objections were sent to the `claude` host (the
        // `plan` step's binding) via `replan_from_objections`, which appended
        // the response to plan.md.
        let plan_after = std::fs::read_to_string(tmp.path().join(&feature_dir).join("plan.md"))
            .expect("read plan.md");
        assert!(
            plan_after.contains("REPLAN-DELTA"),
            "expected replanned delta appended to plan.md, got: {plan_after}"
        );

        let verdict =
            std::fs::read_to_string(tmp.path().join(&feature_dir).join("assay/verdict.md"))
                .expect("read verdict.md");
        assert!(verdict.contains("verdict: accept"));
        assert!(verdict.contains("round: 2"));

        let debate = std::fs::read_to_string(tmp.path().join(&feature_dir).join("assay/debate.md"))
            .expect("read debate.md");
        assert!(
            debate.contains("Rebuttal"),
            "expected a rebuttal transcript entry from the replan, got: {debate}"
        );
        assert!(debate.contains("round 1/2"));
        assert!(debate.contains("round 2/2"));
    }

    #[tokio::test]
    async fn execute_assay_multi_reviewer_split_verdict_on_split_reject_halts() {
        let (tmp, feature_dir) = feature_workspace();
        let accept_cli = accept_script(tmp.path());
        let reject_cli = reject_script(tmp.path());

        let yaml = multi_reviewer_yaml(
            &[("reviewer-a", &accept_cli), ("reviewer-b", &reject_cli)],
            "reject",
        );
        std::fs::write(tmp.path().join("derrick.yaml"), &yaml).expect("write derrick.yaml");
        let config = Config::load_from_path(&tmp.path().join("derrick.yaml")).expect("load config");

        let step = pipeline_step(&config, "assay");
        let mut state = ExecutionState::new(
            "prompt".to_owned(),
            "run-split-1".to_owned(),
            tmp.path().join(".derrick/runs/run-split-1"),
        );
        state.feature_dir = Some(feature_dir.clone());

        let exec = execute_assay(
            &config,
            Arc::new(HostRegistry::empty()),
            tmp.path(),
            tmp.path(),
            &feature_dir,
            "prompt",
            "run-split-1",
            &step,
            &tmp.path().join("assay.log"),
            &mut state,
        )
        .await
        .expect("a split verdict under on_split: reject is a halt, not an error");

        assert_eq!(exec.status, crate::types::StepStatus::Halted);

        let combined =
            std::fs::read_to_string(tmp.path().join(&feature_dir).join("assay/verdict.md"))
                .expect("read combined verdict.md");
        assert!(combined.contains("verdict: reject"));
        assert!(combined.contains("on_split: reject"));
        assert!(combined.contains("reviewers: 2"));
    }

    #[tokio::test]
    async fn execute_assay_multi_reviewer_majority_accepts_despite_one_reject() {
        let (tmp, feature_dir) = feature_workspace();
        let accept_cli = accept_script(tmp.path());
        let reject_cli = reject_script(tmp.path());

        // 3 reviewers (majority requires odd count): 2 accept, 1 reject.
        let yaml = multi_reviewer_yaml(
            &[
                ("reviewer-a", &accept_cli),
                ("reviewer-b", &accept_cli),
                ("reviewer-c", &reject_cli),
            ],
            "majority",
        );
        std::fs::write(tmp.path().join("derrick.yaml"), &yaml).expect("write derrick.yaml");
        let config = Config::load_from_path(&tmp.path().join("derrick.yaml")).expect("load config");

        let step = pipeline_step(&config, "assay");
        let mut state = ExecutionState::new(
            "prompt".to_owned(),
            "run-split-2".to_owned(),
            tmp.path().join(".derrick/runs/run-split-2"),
        );
        state.feature_dir = Some(feature_dir.clone());

        let exec = execute_assay(
            &config,
            Arc::new(HostRegistry::empty()),
            tmp.path(),
            tmp.path(),
            &feature_dir,
            "prompt",
            "run-split-2",
            &step,
            &tmp.path().join("assay.log"),
            &mut state,
        )
        .await
        .expect("majority accept should succeed");

        assert_eq!(exec.status, crate::types::StepStatus::Success);

        let combined =
            std::fs::read_to_string(tmp.path().join(&feature_dir).join("assay/verdict.md"))
                .expect("read combined verdict.md");
        assert!(combined.contains("verdict: accept"));
        assert!(combined.contains("on_split: majority"));
        assert!(combined.contains("reviewers: 3"));
    }

    #[tokio::test]
    async fn run_reviewer_rounds_codex_fallback_uses_claude_host_and_propagates_tokens() {
        let (tmp, feature_dir) = feature_workspace();

        // `roles.reviewer` is bound to a model literally named `codex`, so
        // `detect_codex_fallback` recognises it and `run_reviewer_rounds`
        // calls the `claude` host adapter directly instead of
        // `derrick_models::resolve_role` — the model's own `cli` is never
        // invoked, so it can be a harmless placeholder.
        let models =
            "  codex:\n    provider: shell\n    cli: \"true\"\n    model: codex\n".to_owned();
        let roles = "  reviewer: codex\n".to_owned();
        let assay = "  assay:\n    enabled: true\n    role: reviewer\n    reviewers: [reviewer]\n    rounds: 1\n".to_owned();
        let pipeline = "  - id: assay\n    runner: derrick\n    skippable: true\n".to_owned();
        let yaml = base_yaml(&models, &roles, &assay, &pipeline);
        std::fs::write(tmp.path().join("derrick.yaml"), &yaml).expect("write derrick.yaml");
        let config = Config::load_from_path(&tmp.path().join("derrick.yaml")).expect("load config");

        let mut hosts = HostRegistry::empty();
        hosts.register(
            "claude",
            Box::new(StaticClaudeHost {
                stdout: "## Verdict\naccept\n",
                tokens_in: 7,
                tokens_out: 9,
            }),
        );
        let hosts = Arc::new(hosts);

        let step = pipeline_step(&config, "assay");
        let mut state = ExecutionState::new(
            "prompt".to_owned(),
            "run-codex-1".to_owned(),
            tmp.path().join(".derrick/runs/run-codex-1"),
        );
        state.feature_dir = Some(feature_dir.clone());

        let reviewer_dir = tmp.path().join(&feature_dir).join("assay");
        let log_path = tmp.path().join("assay.log");

        let outcome = run_reviewer_rounds(
            &config,
            hosts,
            tmp.path(),
            tmp.path(),
            &feature_dir,
            "prompt",
            "run-codex-1",
            &step,
            &log_path,
            "reviewer",
            &reviewer_dir,
            &state,
        )
        .await
        .expect("run_reviewer_rounds should succeed via the codex-headless fallback");

        match outcome {
            ReviewerRoundOutcome::Decided(decided) => {
                assert_eq!(decided.verdict, "accept");
                assert_eq!(decided.role, "reviewer");
                assert_eq!(decided.tokens_in, 7);
                assert_eq!(decided.tokens_out, 9);
                assert_eq!(decided.rounds_used, 1);
            }
            ReviewerRoundOutcome::Skipped => panic!("expected a decided outcome, got Skipped"),
        }
    }

    #[tokio::test]
    async fn detect_codex_fallback_true_for_codex_named_model() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            tmp.path().join("derrick.yaml"),
            "tools:\n  assay:\n    reviewers: [reviewer]\n\
             models:\n  codex:\n    provider: shell\n    cli: \"true\"\n    model: codex\n\
             roles:\n  reviewer: codex\n",
        )
        .expect("write config");
        let config = Config::load_layered(tmp.path()).expect("load config");
        assert!(
            detect_codex_fallback(&config, "reviewer")
                .await
                .expect("detect_codex_fallback"),
            "a role bound to a model literally named `codex` should be recognised as codex family"
        );
    }

    #[tokio::test]
    async fn detect_codex_fallback_false_for_non_codex_model() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            tmp.path().join("derrick.yaml"),
            "tools:\n  assay:\n    reviewers: [reviewer]\n\
             models:\n  shell-reviewer:\n    provider: shell\n    cli: \"true\"\n    model: shell-reviewer\n\
             roles:\n  reviewer: shell-reviewer\n",
        )
        .expect("write config");
        let config = Config::load_layered(tmp.path()).expect("load config");
        assert!(
            !detect_codex_fallback(&config, "reviewer")
                .await
                .expect("detect_codex_fallback"),
            "a role bound to a non-codex shell model must not trigger the fallback"
        );
    }

    #[tokio::test]
    async fn detect_codex_fallback_false_for_unbound_role() {
        // `roles.get(reviewer_role)` returns `None` for a role with no
        // binding — must fail closed to `false` (never guess codex), not
        // error or panic.
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("derrick.yaml"), "version: 1\n").expect("write config");
        let config = Config::load_layered(tmp.path()).expect("load config");
        assert!(
            !detect_codex_fallback(&config, "no-such-role")
                .await
                .expect("detect_codex_fallback")
        );
    }
}
