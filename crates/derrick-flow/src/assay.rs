use std::fmt::Write as _;
use std::io::{IsTerminal, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use derrick_config::{Config, OnSplit};
use derrick_models::AuthStore;
use derrick_tools::{HostRegistry, HostRequest};
use tokio::sync::Semaphore;

use crate::io::{append_log, create_dir_all, parent, read_to_string, relative_to_root, write_file};
use crate::names::host_name;
use crate::template::render_template;
use crate::types::{RunError, StepExecution, StepStatus};

pub struct ReviewerOutcome {
    pub role: String,
    pub verdict: String,
    pub verdict_path: PathBuf,
    pub tokens_in: u32,
    pub tokens_out: u32,
}

pub enum ReviewerRoundOutcome {
    Decided(ReviewerOutcome),
    Skipped,
}

const ASSAY_SYSTEM: &str = "Review the speckit plan. Identify the highest risks, missing edge cases, and constitution contradictions. End with an H2 `## Verdict` followed by exactly one of: accept, revise, reject.";

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
        return match outcome.verdict.as_str() {
            "accept" => Ok(StepExecution::success(vec![relative_to_root(
                repo_root,
                outcome.verdict_path,
            )?])
            .with_tokens(tokens_in, tokens_out)),
            "reject" => Ok(StepExecution::halted(
                vec![relative_to_root(repo_root, outcome.verdict_path)?],
                "assay rejected",
            )
            .with_tokens(tokens_in, tokens_out)),
            _ => Ok(StepExecution::halted(
                vec![relative_to_root(repo_root, outcome.verdict_path)?],
                "assay requested revisions past configured rounds",
            )
            .with_tokens(tokens_in, tokens_out)),
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
            let _permit = sem.acquire_owned().await.expect("semaphore never closed");
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

pub async fn run_reviewer_rounds(
    config: &Config,
    hosts: Arc<HostRegistry>,
    repo_root: &Path,
    working_dir: &Path,
    feature_dir: &Path,
    state_prompt: &str,
    run_id: &str,
    step: &derrick_config::PipelineStep,
    log_path: &Path,
    reviewer_role: &str,
    reviewer_dir: &Path,
    state: &ExecutionState,
) -> Result<ReviewerRoundOutcome, RunError> {
    let rounds = assay_rounds(config, step, state)?;
    let spec = read_to_string(&working_dir.join(feature_dir).join("spec.md"))?;
    let constitution = read_to_string(&working_dir.join(config.guardrails().constitution_path()))?;

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

    let mut tokens_in_total: u32 = 0;
    let mut tokens_out_total: u32 = 0;

    for round in 1..=rounds {
        let plan = read_to_string(&working_dir.join(feature_dir).join("plan.md"))?;
        let prompt = format!("Task: {}\n\nPlan:\n{plan}", state_prompt);
        let cached = format!("Constitution:\n{constitution}\n\nSpec:\n{spec}");
        let (response_text, model_name, round_tokens_in, round_tokens_out) = if codex_fallback {
            let host = hosts
                .get("claude")
                .ok_or_else(|| RunError::Config("host \"claude\" is not registered".to_owned()))?;
            let full_prompt = format!("{ASSAY_SYSTEM}\n\n{cached}\n\n{prompt}");
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
                    Some(ASSAY_SYSTEM.to_owned()),
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
        let verdict_body = format!(
            "model: {model_name}\nreviewer: {reviewer_role}\nround: {round}\nverdict: {verdict}\n\n{response_text}"
        );
        write_file(&verdict_path, &verdict_body)?;
        match verdict {
            "accept" | "reject" => {
                return Ok(ReviewerRoundOutcome::Decided(ReviewerOutcome {
                    role: reviewer_role.to_owned(),
                    verdict: verdict.to_owned(),
                    verdict_path: verdict_path.clone(),
                    tokens_in: tokens_in_total,
                    tokens_out: tokens_out_total,
                }));
            }
            "revise" if round < rounds => {
                let objections =
                    suggested_revisions(&response_text).ok_or_else(|| RunError::StepFailed {
                        id: step.id().to_owned(),
                        message: "could not parse suggested revisions from reviewer response"
                            .to_owned(),
                    })?;
                replan_from_objections(config, &hosts, working_dir, state, objections).await?;
            }
            "revise" => {
                return Ok(ReviewerRoundOutcome::Decided(ReviewerOutcome {
                    role: reviewer_role.to_owned(),
                    verdict: "revise".to_owned(),
                    verdict_path: verdict_path.clone(),
                    tokens_in: tokens_in_total,
                    tokens_out: tokens_out_total,
                }));
            }
            _ => unreachable_verdict(step.id())?,
        }
    }

    Ok(ReviewerRoundOutcome::Decided(ReviewerOutcome {
        role: reviewer_role.to_owned(),
        verdict: "revise".to_owned(),
        verdict_path,
        tokens_in: tokens_in_total,
        tokens_out: tokens_out_total,
    }))
}

async fn detect_codex_fallback(config: &Config, reviewer_role: &str) -> Result<bool, RunError> {
    if std::io::stdin().is_terminal() {
        return Ok(false);
    }
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
) -> Result<(), RunError> {
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
    if !response.stdout.trim().is_empty() {
        let feature_dir = state
            .feature_dir
            .as_ref()
            .ok_or_else(|| RunError::Config("replan requires feature_dir".to_owned()))?;
        let plan_path = working_dir.join(feature_dir).join("plan.md");
        append_log(&plan_path, &response.stdout)?;
    }
    Ok(())
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
    let body = format!(
        "verdict: {final_verdict}\non_split: {policy_name}\nreviewers: {}\n\n{summary}\n",
        outcomes.len()
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
        "accept" => Ok(StepExecution::success(artifacts).with_tokens(tokens_in, tokens_out)),
        _ => Ok(StepExecution::halted(
            artifacts,
            format!("assay rejected (on_split: {policy_name})"),
        )
        .with_tokens(tokens_in, tokens_out)),
    }
}

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

pub fn suggested_revisions(text: &str) -> Option<&str> {
    let start_marker = "## Suggested revisions";
    let start = text.find(start_marker)? + start_marker.len();
    let rest = &text[start..];
    let end = rest.find("\n## ").unwrap_or(rest.len());
    Some(rest[..end].trim())
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
