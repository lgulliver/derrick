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

use crate::clarify;
use derrick_assay::io::write_log;
use derrick_assay::names::host_name;
use derrick_assay::template::{render_template, TemplateContext};
use derrick_assay::types::{RunError, StepExecution, StepRecord, StepStatus};
use derrick_assay::{self as assay, ExecutionState};

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
            bytes_raw,
            bytes_saved,
            roughneck_tokens_saved,
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
                let _ = derrick_assay::io::append_log(
                    &log_path,
                    &format!("\n---\nstep halted: {message}\n"),
                );
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
                bytes_raw,
                bytes_saved,
                roughneck_tokens_saved,
            })
        }
        Err(error) => {
            let _ignored = derrick_assay::io::append_log(&log_path, &format!("{error}\n"));
            let record = StepRecord {
                id: step.id().to_owned(),
                status: StepStatus::Failed,
                started_at,
                finished_at,
                log_path,
                artifacts: Vec::new(),
                tokens_in: 0,
                tokens_out: 0,
                bytes_raw: 0,
                bytes_saved: 0,
                roughneck_tokens_saved: 0,
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
                manifest.status = derrick_assay::types::RunStatus::Failed;
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
        let command = derrick_assay::io::required_step_text(step.command(), step.id(), "command")?;
        let prompt = render_template(command, &template_context(config, state)?)?;
        let prompt = inject_clarify_answers_for_plan(step.id(), state, repo_root, prompt)?;
        // Apply roughneck prompt injection if enabled.
        let prompt = if config.tools().roughneck().enabled() {
            derrick_roughneck::inject_prompt(&prompt, config.tools().roughneck().level())
        } else {
            prompt
        };
        let host_name = host_name(host);
        let host = hosts
            .get(host_name)
            .ok_or_else(|| RunError::Config(format!("host {host_name:?} is not registered")))?;
        // For the `specify` step, pre-scaffold `specs/<NNN>-<slug>/spec.md`
        // and `.specify/feature.json` BEFORE invoking the LLM. This removes
        // the model's responsibility for inventing a path and creating the
        // directory — empirically, the model frequently writes a flat file
        // (e.g. `spec.md` at repo root) which broke the old snapshot-diff
        // resolver. The prompt is amended to include the exact target path.
        let (prompt, pre_specify_dir) = if step.id() == "specify" {
            let wd = working_dir(state, repo_root);
            let feature_dir = derrick_assay::io::prescaffold_feature_dir(wd, &state.prompt)?;
            let target = feature_dir.join("spec.md");
            let amended = format!(
                "Write the spec to: {target_display}\n\
                 The file already exists as a stub — overwrite it with the full spec. \
                 Do not create a different directory or a flat file at the repo root.\n\n\
                 {prompt}",
                target_display = target.display(),
            );
            // Make the new feature_dir visible to template_context for downstream
            // re-renders and to detect_artifacts below.
            state.feature_dir = Some(feature_dir.clone());
            (amended, Some(feature_dir))
        } else {
            (prompt, None)
        };
        let prompt_len = prompt.len();
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
        // Use the larger of: CLI-reported input tokens vs prompt-length
        // estimate.  The CLI only counts the direct user message (not full
        // Claude Code session context), so this gives a better lower bound.
        let step_tokens_in = response
            .tokens_in
            .max((prompt_len as u32).saturating_div(4));
        let step_tokens_out = response.tokens_out;
        write_log(log_path, &response.stdout, &response.stderr)?;
        if let Some(feature_dir) = pre_specify_dir {
            // Post-step check: the LLM must have overwritten the pre-scaffolded
            // stub with real content. feature.json is already in place from
            // the pre-scaffold, so resume/retry semantics still work.
            let wd = working_dir(state, repo_root);
            derrick_assay::io::verify_spec_written(wd, &feature_dir).map_err(|e| {
                RunError::StepFailed {
                    id: step.id().to_owned(),
                    message: e.to_string(),
                }
            })?;
        }
        let roughneck_saved = if config.tools().roughneck().enabled() {
            derrick_roughneck::estimate_tokens_saved(
                step_tokens_out,
                config.tools().roughneck().level(),
            )
        } else {
            0
        };
        Ok(
            StepExecution::success(detect_artifacts(step.id(), state, repo_root))
                .with_tokens(step_tokens_in, step_tokens_out)
                .with_roughneck(roughneck_saved),
        )
    } else {
        let role = derrick_assay::io::required_step_text(step.role(), step.id(), "role")?;
        let prompt = step
            .command()
            .map_or_else(|| state.prompt.clone(), ToOwned::to_owned);
        let rendered = render_template(&prompt, &template_context(config, state)?)?;
        let rendered = inject_clarify_answers_for_plan(step.id(), state, repo_root, rendered)?;
        // Apply roughneck prompt injection if enabled.
        let rendered = if config.tools().roughneck().enabled() {
            derrick_roughneck::inject_prompt(&rendered, config.tools().roughneck().level())
        } else {
            rendered
        };
        let model = derrick_models::resolve_role(
            role,
            config.roles(),
            config.models(),
            &AuthStore::from_env(),
        )
        .await?;
        let prompt_len = rendered.len();
        let response = model
            .complete(completion_request(rendered, None, None))
            .await?;
        let actual_tokens_in = response
            .tokens_in
            .max((prompt_len as u32).saturating_div(4));
        write_log(log_path, &response.text, "")?;
        let roughneck_saved = if config.tools().roughneck().enabled() {
            derrick_roughneck::estimate_tokens_saved(
                response.tokens_out,
                config.tools().roughneck().level(),
            )
        } else {
            0
        };
        Ok(
            StepExecution::success(detect_artifacts(step.id(), state, repo_root))
                .with_tokens(actual_tokens_in, response.tokens_out)
                .with_roughneck(roughneck_saved),
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
    if config.tools().substrate().mode() != derrick_config::SubstrateMode::Crew {
        write_log(log_path, "", "bridge: skipped because mode is not crew\n")?;
        return Ok(StepExecution::skipped());
    }
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
        .unwrap_or_else(|| format!("br-{}", state.run_id.to_ascii_lowercase()));
    let batch_name = BatchName::new(&batch_name_str).map_err(|e| {
        RunError::Config(format!(
            "bridge: invalid batch name {batch_name_str:?}: {e}"
        ))
    })?;

    // Idempotent: reuse an existing batch (e.g. on pipeline resume) rather than
    // failing with a UNIQUE constraint error.
    let batch_exists = substrate
        .get_batch(&batch_name)
        .await
        .map_err(|e| RunError::StepFailed {
            id: "bridge".to_owned(),
            message: format!("get_batch: {e}"),
        })?
        .is_some();
    if !batch_exists {
        substrate
            .create_batch(batch_name.clone())
            .await
            .map_err(|e| RunError::StepFailed {
                id: "bridge".to_owned(),
                message: format!("create_batch: {e}"),
            })?;
    }

    let tickets = parse_tasks_from_markdown(&tasks_text, &batch_name, config.site().prefix())?;

    // Auto-remediation: for each ticket, check whether one already exists from
    // a prior run before attempting to create.
    //   • terminal (done / rejected) → delete and recreate (fresh dispatch)
    //   • active (ready / in_flight / in_review / blocked) → skip + warn
    let mut created = 0usize;
    let mut skipped = 0usize;
    for ticket in &tickets {
        let existing =
            substrate
                .get_ticket(&ticket.id)
                .await
                .map_err(|e| RunError::StepFailed {
                    id: "bridge".to_owned(),
                    message: format!("get_ticket {}: {e}", ticket.id),
                })?;

        match existing {
            Some(existing_ticket) if existing_ticket.state.is_terminal() => {
                // Terminal ticket from a previous run — delete and recreate.
                write_log(
                    log_path,
                    &format!(
                        "ticket {} is terminal ({}), deleting for re-dispatch\n",
                        ticket.id, existing_ticket.state
                    ),
                    "",
                )?;
                substrate
                    .delete_ticket(&ticket.id)
                    .await
                    .map_err(|e| RunError::StepFailed {
                        id: "bridge".to_owned(),
                        message: format!("delete_ticket {}: {e}", ticket.id),
                    })?;
                write_log(
                    log_path,
                    &format!("creating ticket {}: {}\n", ticket.id, ticket.title),
                    "",
                )?;
                substrate.create_ticket(ticket.clone()).await.map_err(|e| {
                    RunError::StepFailed {
                        id: "bridge".to_owned(),
                        message: format!("create_ticket {}: {e}", ticket.id),
                    }
                })?;
                created += 1;
            }
            Some(existing_ticket) => {
                // Active ticket — skip creation, do not clobber in-progress work.
                let msg = format!(
                    "ticket {} already active (state: {}), skipping",
                    ticket.id, existing_ticket.state
                );
                write_log(log_path, &format!("{msg}\n"), "")?;
                eprintln!(
                    "  {} {} {}",
                    "bridge".cyan(),
                    "\u{26a0}".yellow(),
                    msg.yellow()
                );
                skipped += 1;
            }
            None => {
                write_log(
                    log_path,
                    &format!("creating ticket {}: {}\n", ticket.id, ticket.title),
                    "",
                )?;
                substrate.create_ticket(ticket.clone()).await.map_err(|e| {
                    RunError::StepFailed {
                        id: "bridge".to_owned(),
                        message: format!("create_ticket {}: {e}", ticket.id),
                    }
                })?;
                created += 1;
            }
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
    let summary = if skipped > 0 {
        format!("{created} created, {skipped} skipped (active)")
    } else {
        format!("{created} tickets")
    };
    eprintln!(
        "  {} {} {} {} {}",
        "\u{2514}".bright_cyan(),
        summary.cyan(),
        "in batch".bright_black(),
        batch_name.as_str().cyan(),
        "\u{2713}".green()
    );
    eprintln!();

    let mut artifacts = vec![];
    if let Ok(rel) = derrick_assay::io::relative_to_root(repo_root, tasks_path) {
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
    if config.tools().substrate().mode() != derrick_config::SubstrateMode::Crew {
        write_log(log_path, "", "foreman: skipped because mode is not crew\n")?;
        return Ok(StepExecution::skipped());
    }
    if state.feature_dir.is_none() {
        write_log(log_path, "", "foreman: no feature_dir, skipping\n")?;
        return Ok(StepExecution::skipped());
    }

    let foreman_step = config.pipeline().iter().find(|step| step.id() == "foreman");
    let executor_role = foreman_step
        .and_then(derrick_config::PipelineStep::executor_role)
        .unwrap_or("executor");
    let model_name = config.roles().get(executor_role).ok_or_else(|| {
        RunError::Config(format!(
            "crew mode requires role `{executor_role}`, but it is not configured under `roles`"
        ))
    })?;
    let model = config.models().get(model_name).ok_or_else(|| {
        RunError::Config(format!(
            "role `{executor_role}` points to model `{model_name}`, but no model named `{model_name}` exists under `models`"
        ))
    })?;
    let hand_kind = hand_kind_for_executor(model.provider(), model.cli());
    let hand_suffix = match hand_kind {
        HandKind::Copilot => "copilot",
        HandKind::Claude => "claude",
        HandKind::Human => "human",
        _ => "human",
    };
    let hand_id = HandId::new(format!("{}-{hand_suffix}-hand", config.site().prefix()))
        .map_err(|e| RunError::Config(format!("foreman: invalid hand id: {e}")))?;

    let _ = substrate
        .register_hand(Hand {
            id: hand_id.clone(),
            kind: hand_kind,
            last_seen: None,
        })
        .await;

    let filter = TicketFilter {
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
    eprintln!(
        "  {} role {} using model {} ({})",
        "\u{2502}".bright_cyan(),
        executor_role.cyan(),
        model_name.cyan(),
        hand_kind.to_string().bright_black()
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
    prefix: &str,
) -> Result<Vec<NewTicket>, RunError> {
    let sanitized_prefix = prefix.trim();
    if sanitized_prefix.is_empty() {
        return Err(RunError::Config(
            "bridge: site prefix is empty; cannot generate ticket ids".to_owned(),
        ));
    }
    let mut tickets = Vec::new();
    let mut ordinal = 0u32;
    let mut body_lines = Vec::new();
    let mut current_title: Option<String> = None;

    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(title) = trimmed.strip_prefix("## ") {
            if let Some(prev_title) = current_title.take() {
                let body = body_lines.join("\n").trim().to_owned();
                let id_str = format!("{sanitized_prefix}-{ordinal}");
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
        let id_str = format!("{sanitized_prefix}-{ordinal}");
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

fn hand_kind_for_executor(provider: &str, cli: Option<&str>) -> HandKind {
    if provider == "copilot-cli" || cli.is_some_and(|value| value.starts_with("copilot")) {
        HandKind::Copilot
    } else if provider == "anthropic" || cli.is_some_and(|value| value.starts_with("claude")) {
        HandKind::Claude
    } else {
        HandKind::Human
    }
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
    let prompt = derrick_assay::io::required_step_text(step.prompt(), step.id(), "prompt")?;
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
    let command = derrick_assay::io::required_step_text(step.command(), step.id(), "command")?;
    let command = render_template(command, &template_context(config, state)?)?;

    // Derive a tool name from the first word of the command for scrubbing.
    // Strip any path prefix so "cargo test" → "cargo" and "/usr/bin/git" → "git".
    let tool_name = command
        .split_whitespace()
        .next()
        .map(|w| {
            std::path::Path::new(w)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(w)
        })
        .unwrap_or("");

    let working_dir = working_dir(state, repo_root).to_path_buf();
    let output = Command::new("bash")
        .arg("-lc")
        .arg(&command)
        .current_dir(&working_dir)
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|source| RunError::Io {
            path: working_dir,
            source,
        })?;

    let bytes_raw = (output.stdout.len().saturating_add(output.stderr.len())) as u32;

    let (stdout_final, stderr_final, bytes_saved) =
        if config.tools().output_compression().enabled() && !tool_name.is_empty() {
            let scrubber = derrick_scrub::Scrubber::with_defaults();
            let (out_scrubbed, out_stats) = scrubber.scrub(tool_name, &output.stdout);
            let (err_scrubbed, err_stats) = scrubber.scrub(tool_name, &output.stderr);
            let saved = (out_stats
                .bytes_in
                .saturating_sub(out_stats.bytes_out)
                .saturating_add(err_stats.bytes_in.saturating_sub(err_stats.bytes_out)))
                as u32;
            (out_scrubbed, err_scrubbed, saved)
        } else {
            (output.stdout.clone(), output.stderr.clone(), 0u32)
        };

    let stdout_str = String::from_utf8_lossy(&stdout_final);
    let stderr_str = String::from_utf8_lossy(&stderr_final);
    write_log(log_path, &stdout_str, &stderr_str)?;

    if output.status.success() {
        Ok(StepExecution::success(Vec::new()).with_compression(bytes_raw, bytes_saved))
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

pub(crate) async fn ensure_constitution(
    config: &derrick_config::Config,
    working_dir: &std::path::Path,
    hosts: Arc<HostRegistry>,
) -> Result<(), RunError> {
    let constitution_path = config.guardrails().constitution_path();
    let full_path = working_dir.join(constitution_path);
    if full_path.exists() {
        let contents = std::fs::read_to_string(&full_path).unwrap_or_default();
        if !derrick_adopt::constitution_needs_setup(&contents) {
            return Ok(());
        }
        // File exists but is still an unedited placeholder — treat as missing.
        eprintln!(
            "  {}  Constitution at {} is an unedited template.",
            "⚠".yellow(),
            constitution_path.display()
        );
    } else {
        eprintln!(
            "  {}  No constitution found at {}",
            "⚠".yellow(),
            constitution_path.display()
        );
    }
    eprintln!(
        "     {}",
        "The constitution captures durable rules that the plan reviewer enforces.".bright_black()
    );
    eprintln!(
        "     {}",
        "Describe your project's key rules, constraints, and principles below.".bright_black()
    );
    eprintln!(
        "     {}",
        "End with a blank line to submit (or just a blank line to skip and write a stub)."
            .bright_black()
    );
    eprintln!();
    eprint!("  {} ", ">".cyan());
    std::io::stdout().flush().map_err(|source| RunError::Io {
        path: PathBuf::from("<stdout>"),
        source,
    })?;

    let mut description = String::new();
    let stdin = std::io::stdin();
    loop {
        let mut line = String::new();
        let n = stdin.read_line(&mut line).map_err(|source| RunError::Io {
            path: PathBuf::from("<stdin>"),
            source,
        })?;
        if n == 0 {
            // EOF
            break;
        }
        if line.trim().is_empty() {
            break;
        }
        description.push_str(&line);
        eprint!("  {} ", ">".cyan());
        std::io::stdout().flush().map_err(|source| RunError::Io {
            path: PathBuf::from("<stdout>"),
            source,
        })?;
    }

    let description = description.trim().to_owned();

    if description.is_empty() {
        eprintln!(
            "  {}  No description provided — writing a starter stub instead.",
            "·".yellow()
        );
        derrick_adopt::write_constitution_stub(working_dir, constitution_path).map_err(|e| {
            RunError::Io {
                path: full_path.clone(),
                source: std::io::Error::other(e.to_string()),
            }
        })?;
        eprintln!("  {}  {}", "·".green(), constitution_path.display());
        return Ok(());
    }

    // Prefer the real speckit skill when `specify integration install claude` has run;
    // otherwise write derrick's shim as a fallback.
    let real_skill = working_dir
        .join(".claude/skills/speckit-constitution/SKILL.md")
        .exists();
    let constitution_command = if real_skill {
        "/speckit-constitution".to_owned()
    } else {
        let commands_dir = working_dir.join(".claude").join("commands");
        std::fs::create_dir_all(&commands_dir).map_err(|source| RunError::Io {
            path: commands_dir.clone(),
            source,
        })?;
        let shim_path = commands_dir.join("speckit.constitution.md");
        if !shim_path.exists() {
            std::fs::write(&shim_path, derrick_adopt::SPECKIT_CONSTITUTION_SHIM).map_err(
                |source| RunError::Io {
                    path: shim_path.clone(),
                    source,
                },
            )?;
        }
        "/speckit.constitution".to_owned()
    };

    eprintln!(
        "  {}  Generating constitution via claude {} …",
        "·".cyan(),
        constitution_command
    );

    let host = hosts.get("claude").ok_or_else(|| {
        RunError::Config("constitution authoring requires the claude host adapter".to_owned())
    })?;
    let prompt = format!("{constitution_command} {description}");
    let mut request = HostRequest::new(prompt, working_dir);
    request.headless = true;
    let _ = host
        .run(request)
        .await
        .map_err(|source| RunError::StepFailed {
            id: "assay".to_owned(),
            message: format!("claude /speckit.constitution failed: {source}"),
        })?;

    // Verify the host actually produced the file; if not, fall back to the stub.
    if full_path.exists() {
        eprintln!("  {}  {}", "·".green(), constitution_path.display());
        return Ok(());
    }

    eprintln!(
        "  {}  Host did not write the constitution — falling back to stub.",
        "⚠".yellow()
    );
    derrick_adopt::write_constitution_stub(working_dir, constitution_path).map_err(|e| {
        RunError::Io {
            path: full_path,
            source: std::io::Error::other(e.to_string()),
        }
    })?;
    eprintln!("  {}  {}", "·".green(), constitution_path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use derrick_assay::ExecutionState;

    use derrick_substrate::BatchName;

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

    #[test]
    fn parse_tasks_empty_text_returns_empty() {
        let batch = BatchName::new("test-batch").unwrap();
        let tickets = super::parse_tasks_from_markdown("", &batch, "tsk").unwrap();
        assert!(tickets.is_empty());
    }

    #[test]
    fn parse_tasks_extracts_headings_as_tickets() {
        let batch = BatchName::new("test-batch").unwrap();
        let text = r#"## Add build.rs with git describe

This task implements the build script.

## Update commands/mod.rs

Need to change the version attribute.

## Fix the test

Update version_matches_cargo_pkg_version.
"#;
        let tickets = super::parse_tasks_from_markdown(text, &batch, "tsk").unwrap();
        assert_eq!(tickets.len(), 3);
        assert_eq!(tickets[0].title, "Add build.rs with git describe");
        assert_eq!(tickets[0].ordinal, Some(0));
        assert_eq!(tickets[1].title, "Update commands/mod.rs");
        assert_eq!(tickets[1].ordinal, Some(1));
        assert_eq!(tickets[2].title, "Fix the test");
        assert_eq!(tickets[2].ordinal, Some(2));
    }

    #[test]
    fn parse_tasks_sets_batch_and_labels() {
        let batch = BatchName::new("feat-batch").unwrap();
        let text = "## Only task\nDetails here.\n";
        let tickets = super::parse_tasks_from_markdown(text, &batch, "tsk").unwrap();
        assert_eq!(tickets.len(), 1);
        assert_eq!(tickets[0].batch, Some(batch));
        assert!(tickets[0].labels.contains(&"task".to_owned()));
    }

    #[test]
    fn parse_tasks_no_headings_returns_empty() {
        let batch = BatchName::new("test-batch").unwrap();
        let text = "Just some text\nwithout any headings.\n";
        let tickets = super::parse_tasks_from_markdown(text, &batch, "tsk").unwrap();
        assert!(tickets.is_empty());
    }

    #[test]
    fn parse_tasks_uses_supplied_prefix() {
        let batch = BatchName::new("test-batch").unwrap();
        let text = "## Task one\nBody\n";
        let tickets = super::parse_tasks_from_markdown(text, &batch, "abc").unwrap();
        assert_eq!(tickets[0].id.as_str(), "abc-0");
    }

    #[test]
    fn hand_kind_uses_provider_for_copilot() {
        let kind = super::hand_kind_for_executor("copilot-cli", None);
        assert_eq!(kind, derrick_substrate::HandKind::Copilot);
    }

    #[test]
    fn hand_kind_uses_cli_for_shell_copilot() {
        let kind = super::hand_kind_for_executor("shell", Some("copilot"));
        assert_eq!(kind, derrick_substrate::HandKind::Copilot);
    }

    #[test]
    fn hand_kind_uses_provider_for_claude() {
        let kind = super::hand_kind_for_executor("anthropic", None);
        assert_eq!(kind, derrick_substrate::HandKind::Claude);
    }
}
