use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use derrick_config::{PipelineStep, Runner as StepRunner};
use derrick_models::AuthStore;
use derrick_substrate::{
    BatchName, Hand, HandId, HandKind, NewTicket, TicketFilter, TicketId, TicketState,
};
use derrick_tools::{CopilotToolPermission, HostRegistry, HostRequest};
use owo_colors::OwoColorize;

use crate::assay::{self, ExecutionState};
use crate::clarify;
use crate::io::{create_dir_all, write_log};
use crate::names::host_name;
use crate::template::{render_template, TemplateContext};
use crate::types::{RunError, StepExecution, StepRecord, StepStatus};

#[allow(clippy::too_many_arguments)]
pub async fn execute_step(
    config: &derrick_config::Config,
    substrate: &dyn derrick_substrate::Substrate,
    hosts: Arc<HostRegistry>,
    repo_root: &std::path::Path,
    step: &PipelineStep,
    state: &mut ExecutionState,
    run_id: &str,
    manifest_path: &Path,
) -> Result<StepRecord, RunError> {
    let started_at = Utc::now();
    let log_path = state.run_dir.join(format!("step-{}.log", step.id()));
    let result = match (step.role(), step.runner()) {
        (Some(_), None) => {
            execute_role_step(config, &hosts, repo_root, step, state, &log_path).await
        }
        (None, Some(StepRunner::Derrick)) => {
            execute_derrick_step(
                config,
                substrate,
                hosts.clone(),
                repo_root,
                step,
                state,
                &log_path,
            )
            .await
        }
        (None, Some(StepRunner::Human)) => execute_human_step(config, step, state, &log_path),
        (None, Some(StepRunner::Bash)) => {
            execute_bash_step(config, step, state, repo_root, &log_path).await
        }
        _ => Err(RunError::Config(format!(
            "pipeline.{}: either supported role or runner is required",
            step.id()
        ))),
    };
    let finished_at = Utc::now();

    match result {
        Ok(StepExecution {
            status,
            artifacts,
            tokens_in,
            tokens_out,
            message,
        }) => {
            let status_str = match status {
                StepStatus::Skipped => "skipped",
                StepStatus::Success => "success",
                StepStatus::Failed => "failed",
                StepStatus::Halted => "halted",
            };
            let _ = substrate
                .record_typed_event(
                    derrick_substrate::EventScope::Worktree {
                        run_id: run_id.to_owned(),
                    },
                    derrick_substrate::EventKind::PipelineStepCompleted {
                        step_id: step.id().to_owned(),
                        status: status_str.to_owned(),
                    },
                )
                .await;
            if !message.is_empty() {
                let _ =
                    crate::io::append_log(&log_path, &format!("\n---\nstep halted: {message}\n"));
            }
            Ok(StepRecord {
                id: step.id().to_owned(),
                status,
                started_at,
                finished_at,
                log_path,
                artifacts,
                tokens_in,
                tokens_out,
            })
        }
        Err(error) => {
            let _ignored = crate::io::append_log(&log_path, &format!("{error}\n"));
            let record = StepRecord {
                id: step.id().to_owned(),
                status: StepStatus::Failed,
                started_at,
                finished_at,
                log_path,
                artifacts: Vec::new(),
                tokens_in: 0,
                tokens_out: 0,
            };
            let _ = substrate
                .record_typed_event(
                    derrick_substrate::EventScope::Worktree {
                        run_id: run_id.to_owned(),
                    },
                    derrick_substrate::EventKind::PipelineStepCompleted {
                        step_id: step.id().to_owned(),
                        status: "failed".to_owned(),
                    },
                )
                .await;
            if let Ok(mut manifest) = crate::manifest::read_manifest(manifest_path) {
                manifest.status = crate::types::RunStatus::Failed;
                manifest.finished_at = Some(finished_at);
                manifest
                    .steps
                    .push(crate::manifest::ManifestStep::from_record(&record));
                let _ignored = crate::manifest::write_manifest(manifest_path, &manifest);
            }
            Err(error)
        }
    }
}

async fn execute_role_step(
    config: &derrick_config::Config,
    hosts: &HostRegistry,
    repo_root: &std::path::Path,
    step: &PipelineStep,
    state: &mut ExecutionState,
    log_path: &Path,
) -> Result<StepExecution, RunError> {
    if let Some(host) = step.host() {
        let command = crate::io::required_step_text(step.command(), step.id(), "command")?;
        let prompt = render_template(command, &template_context(config, state)?)?;
        let prompt = inject_clarify_answers_for_plan(step.id(), state, repo_root, prompt)?;
        let host_name = host_name(host);
        let host = hosts
            .get(host_name)
            .ok_or_else(|| RunError::Config(format!("host {host_name:?} is not registered")))?;
        if step.id() == "specify" {
            create_dir_all(
                &working_dir(state, repo_root)
                    .join(".specify")
                    .join("features"),
            )?;
        }
        let mut request = HostRequest::new(prompt, working_dir(state, repo_root));
        request.headless = true;
        if host_name == "copilot" {
            request.copilot_tools = CopilotToolPermission::AllowAll;
        }
        let response = host
            .run(request)
            .await
            .map_err(|source| RunError::StepFailed {
                id: step.id().to_owned(),
                message: source.to_string(),
            })?;
        write_log(log_path, &response.stdout, &response.stderr)?;
        if step.id() == "specify" {
            state.feature_dir = Some(crate::io::read_feature_dir(working_dir(state, repo_root))?);
        }
        Ok(StepExecution::success(detect_artifacts(
            step.id(),
            state,
            repo_root,
        )))
    } else {
        let role = crate::io::required_step_text(step.role(), step.id(), "role")?;
        let prompt = step
            .command()
            .map_or_else(|| state.prompt.clone(), ToOwned::to_owned);
        let rendered = render_template(&prompt, &template_context(config, state)?)?;
        let rendered = inject_clarify_answers_for_plan(step.id(), state, repo_root, rendered)?;
        let model = derrick_models::resolve_role(
            role,
            config.roles(),
            config.models(),
            &AuthStore::from_env(),
        )
        .await?;
        let response = model
            .complete(completion_request(rendered, None, None))
            .await?;
        write_log(log_path, &response.text, "")?;
        Ok(
            StepExecution::success(detect_artifacts(step.id(), state, repo_root))
                .with_tokens(response.tokens_in, response.tokens_out),
        )
    }
}

async fn execute_derrick_step(
    config: &derrick_config::Config,
    substrate: &dyn derrick_substrate::Substrate,
    hosts: Arc<HostRegistry>,
    repo_root: &std::path::Path,
    step: &PipelineStep,
    state: &mut ExecutionState,
    log_path: &Path,
) -> Result<StepExecution, RunError> {
    match step.id() {
        "assay" => {
            let feature_dir = state
                .feature_dir
                .clone()
                .ok_or_else(|| RunError::Config("assay requires feature_dir".to_owned()))?;
            let wd = working_dir(state, repo_root).to_path_buf();
            let prompt = state.prompt.clone();
            let run_id = state.run_id.clone();
            assay::execute_assay(
                config,
                hosts,
                repo_root,
                &wd,
                &feature_dir,
                &prompt,
                &run_id,
                step,
                log_path,
                state,
            )
            .await
        }
        "clarify" => {
            let feature_dir = state.feature_dir.clone().ok_or_else(|| {
                RunError::Config("clarify requires feature_dir from specify step".to_owned())
            })?;
            let wd = working_dir(state, repo_root).to_path_buf();
            clarify::execute_clarify(
                hosts.clone(),
                repo_root,
                &wd,
                &feature_dir,
                &state.prompt,
                &state.run_id,
                log_path,
            )
            .await
        }
        "bridge" => execute_bridge(config, substrate, state, repo_root, log_path).await,
        "foreman" => execute_foreman(config, substrate, state, repo_root, log_path).await,
        other => Err(RunError::Config(format!(
            "runner derrick is not supported for step {other:?} in T010"
        ))),
    }
}

async fn execute_bridge(
    config: &derrick_config::Config,
    substrate: &dyn derrick_substrate::Substrate,
    state: &ExecutionState,
    repo_root: &Path,
    log_path: &Path,
) -> Result<StepExecution, RunError> {
    let wd = working_dir(state, repo_root);
    let feature_dir = match state.feature_dir.as_ref() {
        Some(fd) => fd.clone(),
        None => {
            write_log(log_path, "", "bridge: no feature_dir, skipping\n")?;
            return Ok(StepExecution::skipped());
        }
    };
    let tasks_path = wd.join(feature_dir).join("tasks.md");
    let tasks_text = match std::fs::read_to_string(&tasks_path) {
        Ok(t) => t,
        Err(_) => {
            write_log(log_path, "", "bridge: no tasks.md found, skipping\n")?;
            return Ok(StepExecution::skipped());
        }
    };

    let batch_name_str = step_batch_name(config, state, "bridge")
        .unwrap_or_else(|| format!("br-{}", &state.run_id[..8].to_ascii_lowercase()));
    let batch_name = BatchName::new(&batch_name_str).map_err(|e| {
        RunError::Config(format!(
            "bridge: invalid batch name {batch_name_str:?}: {e}"
        ))
    })?;

    substrate
        .create_batch(batch_name.clone())
        .await
        .map_err(|e| RunError::StepFailed {
            id: "bridge".to_owned(),
            message: format!("create_batch: {e}"),
        })?;

    let tickets = parse_tasks_from_markdown(&tasks_text, &batch_name)?;

    let created = tickets.len();
    for ticket in &tickets {
        write_log(
            log_path,
            &format!("creating ticket {}: {}\n", ticket.id, ticket.title),
            "",
        )?;
        if let Err(e) = substrate.create_ticket(ticket.clone()).await {
            write_log(log_path, &format!("  failed: {e}\n"), "")?;
        }
    }

    let prefix = config.site().prefix();
    eprintln!();
    eprintln!(
        "  {} {}",
        "\u{250c}".bright_cyan(),
        "Bridge — Ticket Plan".bold()
    );
    for ticket in &tickets {
        let ordinal = ticket.ordinal.unwrap_or(0);
        eprintln!(
            "  {} {} {} {}",
            "\u{2502}".bright_cyan(),
            format!("{}-{}", prefix, ordinal + 1).cyan(),
            "\u{2192}".cyan(),
            ticket.title.bright_white()
        );
    }
    eprintln!(
        "  {} {} {} {} {}",
        "\u{2514}".bright_cyan(),
        format!("{created} tickets").cyan(),
        "created in batch".bright_black(),
        batch_name.as_str().cyan(),
        "\u{2713}".green()
    );
    eprintln!();

    let mut artifacts = vec![];
    if let Ok(rel) = crate::io::relative_to_root(repo_root, tasks_path) {
        artifacts.push(rel);
    }
    Ok(StepExecution::success(artifacts))
}

async fn execute_foreman(
    config: &derrick_config::Config,
    substrate: &dyn derrick_substrate::Substrate,
    state: &ExecutionState,
    _repo_root: &Path,
    log_path: &Path,
) -> Result<StepExecution, RunError> {
    if state.feature_dir.is_none() {
        write_log(log_path, "", "foreman: no feature_dir, skipping\n")?;
        return Ok(StepExecution::skipped());
    }
    let batch_name_str = step_batch_name(config, state, "foreman")
        .unwrap_or_else(|| format!("br-{}", &state.run_id[..8].to_ascii_lowercase()));
    let batch_name = match BatchName::new(&batch_name_str) {
        Ok(b) => b,
        Err(_) => {
            write_log(log_path, "foreman: no valid batch name, skipping\n", "")?;
            return Ok(StepExecution::skipped());
        }
    };

    let hand_id = HandId::new(format!("{}-hand", config.site().prefix()))
        .map_err(|e| RunError::Config(format!("foreman: invalid hand id: {e}")))?;

    let _ = substrate
        .register_hand(Hand {
            id: hand_id.clone(),
            kind: HandKind::Human,
            last_seen: None,
        })
        .await;

    let filter = TicketFilter {
        batch: Some(batch_name.clone()),
        state: Some(TicketState::Ready),
        ..TicketFilter::default()
    };
    let ready = substrate
        .list_tickets(filter)
        .await
        .map_err(|e| RunError::StepFailed {
            id: "foreman".to_owned(),
            message: format!("list_tickets: {e}"),
        })?;

    if ready.is_empty() {
        write_log(log_path, "foreman: no ready tickets to dispatch\n", "")?;
        eprintln!(
            "  {} {}",
            "\u{2502}".bright_cyan(),
            "No ready tickets to dispatch.".bright_black()
        );
        return Ok(StepExecution::success(vec![]));
    }

    let prefix = config.site().prefix();
    eprintln!();
    eprintln!(
        "  {} {}",
        "\u{250c}".bright_cyan(),
        "Foreman — Dispatch".bold()
    );

    let mut dispatched = 0u32;
    for ticket in &ready {
        let ordinal = ticket.ordinal.unwrap_or(0);
        match substrate.assign_to_hand(&ticket.id, &hand_id).await {
            Ok(_) => {
                dispatched += 1;
                eprintln!(
                    "  {} {} {} {} {} {}",
                    "\u{2502}".bright_cyan(),
                    format!("{}-{}", prefix, ordinal + 1).cyan(),
                    "\u{2192}".cyan(),
                    "dispatched to".bright_black(),
                    hand_id.as_str().cyan(),
                    "\u{2713}".green()
                );
            }
            Err(e) => {
                write_log(
                    log_path,
                    &format!("dispatch failed for ticket {}: {e}\n", ticket.id),
                    "",
                )?;
                eprintln!(
                    "  {} {} {} {}",
                    "\u{2502}".bright_cyan(),
                    format!("{}-{}", prefix, ordinal + 1).cyan(),
                    "\u{2192}".cyan(),
                    format!("dispatch failed: {e}").yellow()
                );
            }
        }
    }

    let remaining = ready.len().saturating_sub(dispatched as usize);
    let remaining_str = if remaining > 0 {
        format!(" ({} remaining)", remaining)
            .bright_black()
            .to_string()
    } else {
        String::new()
    };
    eprintln!(
        "  {} {} {}",
        "\u{2514}".bright_cyan(),
        format!("{dispatched} dispatched").cyan(),
        remaining_str,
    );
    eprintln!();

    write_log(
        log_path,
        &format!("foreman: {dispatched} dispatched, {remaining} remaining\n"),
        "",
    )?;
    Ok(StepExecution::success(vec![]))
}

fn parse_tasks_from_markdown(
    text: &str,
    batch: &derrick_substrate::BatchName,
) -> Result<Vec<NewTicket>, RunError> {
    let prefix = "tsk";
    let mut tickets = Vec::new();
    let mut ordinal = 0u32;
    let mut body_lines = Vec::new();
    let mut current_title: Option<String> = None;

    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(title) = trimmed
            .strip_prefix("## ")
            .or_else(|| trimmed.strip_prefix("# "))
        {
            if let Some(prev_title) = current_title.take() {
                let body = body_lines.join("\n").trim().to_owned();
                let id_str = format!("{prefix}-{ordinal}");
                if let Ok(id) = TicketId::new(&id_str) {
                    tickets.push(
                        NewTicket::new(
                            id,
                            Some(batch.clone()),
                            Some(ordinal),
                            prev_title,
                            body,
                            vec!["task".to_owned()],
                        )
                        .map_err(|e| RunError::StepFailed {
                            id: "bridge".to_owned(),
                            message: format!("invalid ticket {id_str}: {e}"),
                        })?,
                    );
                }
                ordinal += 1;
            }
            current_title = Some(title.to_owned());
            body_lines.clear();
        } else if current_title.is_some() {
            body_lines.push(line.to_owned());
        }
    }

    if let Some(title) = current_title {
        let body = body_lines.join("\n").trim().to_owned();
        let id_str = format!("{prefix}-{ordinal}");
        if let Ok(id) = TicketId::new(&id_str) {
            tickets.push(
                NewTicket::new(
                    id,
                    Some(batch.clone()),
                    Some(ordinal),
                    title,
                    body,
                    vec!["task".to_owned()],
                )
                .map_err(|e| RunError::StepFailed {
                    id: "bridge".to_owned(),
                    message: format!("invalid ticket: {e}"),
                })?,
            );
        }
    }

    Ok(tickets)
}

fn step_batch_name(
    config: &derrick_config::Config,
    state: &ExecutionState,
    step_id: &str,
) -> Option<String> {
    let step = config.pipeline().iter().find(|s| s.id() == step_id)?;
    let raw = step.batch()?;
    let ctx = template_context(config, state).ok()?;
    render_template(raw, &ctx).ok()
}

fn execute_human_step(
    config: &derrick_config::Config,
    step: &PipelineStep,
    state: &ExecutionState,
    log_path: &Path,
) -> Result<StepExecution, RunError> {
    let prompt = crate::io::required_step_text(step.prompt(), step.id(), "prompt")?;
    let prompt = render_template(prompt, &template_context(config, state)?)?;
    write_log(log_path, &prompt, "")?;
    let mut stdout = std::io::stdout();
    stdout
        .write_all(prompt.as_bytes())
        .and_then(|()| stdout.write_all(b"\n"))
        .and_then(|()| stdout.flush())
        .map_err(|source| RunError::Io {
            path: PathBuf::from("<stdout>"),
            source,
        })?;
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .map_err(|source| RunError::Io {
            path: PathBuf::from("<stdin>"),
            source,
        })?;
    if answer.trim().eq_ignore_ascii_case("y") || answer.trim().eq_ignore_ascii_case("yes") {
        Ok(StepExecution::success(Vec::new()))
    } else {
        Ok(StepExecution::halted(Vec::new(), "checkpoint declined"))
    }
}

async fn execute_bash_step(
    config: &derrick_config::Config,
    step: &PipelineStep,
    state: &ExecutionState,
    repo_root: &std::path::Path,
    log_path: &Path,
) -> Result<StepExecution, RunError> {
    use tokio::process::Command;
    let command = crate::io::required_step_text(step.command(), step.id(), "command")?;
    let command = render_template(command, &template_context(config, state)?)?;
    let working_dir = working_dir(state, repo_root).to_path_buf();
    let output = Command::new("bash")
        .arg("-lc")
        .arg(command)
        .current_dir(&working_dir)
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|source| RunError::Io {
            path: working_dir,
            source,
        })?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    write_log(log_path, &stdout, &stderr)?;
    if output.status.success() {
        Ok(StepExecution::success(Vec::new()))
    } else {
        Err(RunError::StepFailed {
            id: step.id().to_owned(),
            message: format!("bash exited with {}", output.status),
        })
    }
}

fn detect_artifacts(
    step_id: &str,
    state: &ExecutionState,
    repo_root: &std::path::Path,
) -> Vec<PathBuf> {
    const FEATURE_JSON: &str = ".specify/feature.json";
    let mut candidates = Vec::new();
    match step_id {
        "specify" => {
            candidates.push(PathBuf::from(FEATURE_JSON));
            if let Some(feature_dir) = &state.feature_dir {
                candidates.push(feature_dir.join("spec.md"));
            }
        }
        "plan" => {
            if let Some(feature_dir) = &state.feature_dir {
                candidates.push(feature_dir.join("plan.md"));
            }
        }
        "tasks" => {
            if let Some(feature_dir) = &state.feature_dir {
                candidates.push(feature_dir.join("tasks.md"));
            }
        }
        "assay" => {
            if let Some(feature_dir) = &state.feature_dir {
                candidates.push(feature_dir.join("assay/verdict.md"));
            }
        }
        _ => {}
    }
    candidates
        .into_iter()
        .filter(|path| working_dir(state, repo_root).join(path).exists())
        .collect()
}

fn template_context(
    config: &derrick_config::Config,
    state: &ExecutionState,
) -> Result<TemplateContext, RunError> {
    Ok(TemplateContext {
        prompt: state.prompt.clone(),
        site_name: config.site().name().to_owned(),
        site_prefix: config.site().prefix().to_owned(),
        feature_dir: state.feature_dir.clone(),
        run_id: state.run_id.clone(),
    })
}

pub(crate) fn inject_clarify_answers_for_plan(
    step_id: &str,
    state: &ExecutionState,
    repo_root: &Path,
    prompt: String,
) -> Result<String, RunError> {
    if step_id != "plan" {
        return Ok(prompt);
    }
    let Some(feature_dir) = &state.feature_dir else {
        return Ok(prompt);
    };
    let clarify_path = working_dir(state, repo_root)
        .join(feature_dir)
        .join("clarify.md");
    if !clarify_path.exists() {
        return Ok(prompt);
    }
    let clarify = std::fs::read_to_string(&clarify_path).map_err(|source| RunError::Io {
        path: clarify_path.clone(),
        source,
    })?;
    Ok(format!(
        "{prompt}\n\nApply these accepted clarifications when producing the plan:\n\n{clarify}"
    ))
}

fn working_dir<'a>(state: &'a ExecutionState, repo_root: &'a std::path::Path) -> &'a Path {
    state.worktree_path.as_deref().unwrap_or(repo_root)
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::assay::ExecutionState;

    #[test]
    fn injects_clarifications_for_plan() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let feature_dir = tmp.path().join("specs/001-test");
        std::fs::create_dir_all(&feature_dir).expect("create feature dir");
        std::fs::write(feature_dir.join("clarify.md"), "Answer: GraphQL\n").expect("write clarify");

        let mut state = ExecutionState::new(
            "test".to_owned(),
            "run-1".to_owned(),
            tmp.path().join(".derrick/runs/run-1"),
        );
        state.feature_dir = Some(PathBuf::from("specs/001-test"));

        let prompt = super::inject_clarify_answers_for_plan(
            "plan",
            &state,
            tmp.path(),
            "/speckit.plan".to_owned(),
        )
        .expect("inject prompt");

        assert!(prompt.contains("Apply these accepted clarifications"));
        assert!(prompt.contains("Answer: GraphQL"));
    }
}
