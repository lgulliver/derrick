//! `derrick stack ...` subcommands. See DESIGN.md §8.5 and T014.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use derrick_config::{Config, StackBackendKind, SubstrateBackendKind};
use derrick_stack::{
    NativeStackBackend, NoneStackBackend, OpenPrParams, RestackOutcome, RestackParams,
    StackBackend, StackNavEntry, StackNode, descendants_of, render_nav_section, topological_order,
    upsert_nav_section,
};
use derrick_substrate::{
    BatchName, BlockReason, EventKind, EventScope, Substrate, Ticket, TicketFilter, TicketId,
    TicketState,
};
use derrick_substrate_native::NativeSubstrate;

use crate::commands::{StackArgs, StackCommand, StackRestackArgs, StackSubmitArgs};
use crate::exit_code::CliExitCode;
use crate::{current_repo_root, message, native_paths, read_config};

pub(crate) async fn execute(args: StackArgs) -> Result<CliExitCode, crate::CliError> {
    let repo_root = current_repo_root()?;
    let config = read_config(&repo_root)?;
    if config.tools().substrate().backend() != SubstrateBackendKind::Native {
        return Err(message(
            "derrick stack requires tools.substrate.backend: native",
        ));
    }
    let substrate =
        NativeSubstrate::open(native_paths(&repo_root, &config), config.site().clone()).await?;
    let result = match args.command {
        StackCommand::Show => stack_show(&substrate).await,
        StackCommand::Restack(restack) => {
            stack_restack(restack, &config, &repo_root, &substrate).await
        }
        StackCommand::Submit(submit) => stack_submit(submit, &config, &repo_root, &substrate).await,
    };
    substrate.close().await?;
    result
}

fn build_backend(
    repo_root: &Path,
    config: &Config,
) -> Result<Arc<dyn StackBackend>, crate::CliError> {
    let stack_cfg = config.tools().git().stacking();
    let backend: Arc<dyn StackBackend> = match stack_cfg.backend() {
        StackBackendKind::Native => Arc::new(NativeStackBackend::new(
            repo_root.to_path_buf(),
            stack_cfg.force_push(),
        )),
        StackBackendKind::None => Arc::new(NoneStackBackend),
    };
    Ok(backend)
}

/// A ticket plus the data the stack engine needs to place and stack it.
struct StackTicket {
    ticket: Ticket,
    /// `blocks` predecessor ids restricted to the working set.
    parents: Vec<TicketId>,
}

/// Load the working set of tickets (optionally filtered by batch) together
/// with their in-set `blocks` predecessors, so the engine can build the DAG.
async fn load_stack_tickets(
    substrate: &NativeSubstrate,
    batch: Option<&str>,
) -> Result<Vec<StackTicket>, crate::CliError> {
    let mut tickets = substrate
        .list_tickets(TicketFilter::default())
        .await
        .map_err(|error| message(format!("list tickets: {error}")))?;
    if let Some(filter) = batch {
        tickets.retain(|t| {
            t.batch
                .as_ref()
                .map(|b| b.as_str() == filter)
                .unwrap_or(false)
        });
    }
    let in_set: HashSet<String> = tickets.iter().map(|t| t.id.as_str().to_owned()).collect();
    let mut out = Vec::with_capacity(tickets.len());
    for ticket in tickets {
        let preds = substrate
            .blocks_predecessors(&ticket.id)
            .await
            .map_err(|error| message(format!("read predecessors: {error}")))?;
        let parents = preds
            .into_iter()
            .filter(|p| in_set.contains(p.as_str()))
            .collect();
        out.push(StackTicket { ticket, parents });
    }
    Ok(out)
}

/// Build the deterministic stack order (root-first) from the working set.
fn stack_order(stack: &[StackTicket]) -> Result<Vec<String>, crate::CliError> {
    let nodes: Vec<StackNode> = stack
        .iter()
        .map(|s| StackNode {
            id: s.ticket.id.as_str().to_owned(),
            ordinal: s.ticket.ordinal,
            parents: s.parents.iter().map(|p| p.as_str().to_owned()).collect(),
        })
        .collect();
    topological_order(&nodes)
        .map_err(|cycle| message(format!("stack has a dependency cycle among: {cycle:?}")))
}

/// The branch a ticket's PR should target: its highest-ordinal in-set parent's
/// branch, or `main` for roots.
fn parent_branch_of(
    stack_ticket: &StackTicket,
    by_id: &BTreeMap<String, &StackTicket>,
    branch_pattern: &str,
    target_branch: &str,
) -> String {
    let pick = stack_ticket
        .parents
        .iter()
        .filter_map(|p| by_id.get(p.as_str()).copied())
        .max_by_key(|p| p.ticket.ordinal.unwrap_or(0));
    match pick {
        None => target_branch.to_owned(),
        Some(parent) => {
            let batch = parent
                .ticket
                .batch
                .as_ref()
                .map(|b| b.as_str().to_owned())
                .unwrap_or_default();
            derrick_stack::compute_branch_name(branch_pattern, &batch, parent.ticket.id.as_str())
        }
    }
}

async fn stack_show(substrate: &NativeSubstrate) -> Result<CliExitCode, crate::CliError> {
    let stack = load_stack_tickets(substrate, None).await?;
    let order = stack_order(&stack)?;
    let by_id: BTreeMap<String, &StackTicket> = stack
        .iter()
        .map(|s| (s.ticket.id.as_str().to_owned(), s))
        .collect();

    // Depth of each node in the stack DAG, for tree-indenting the PR view so
    // the stack reads as a graph rather than a flat table. A node's depth is
    // one more than the max depth of its in-set parents (roots are depth 0).
    let mut depth: BTreeMap<String, usize> = BTreeMap::new();
    for id in &order {
        let st = by_id.get(id).expect("ordered id present");
        let d = st
            .parents
            .iter()
            .filter_map(|p| depth.get(p.as_str()).copied())
            .max()
            .map(|m| m + 1)
            .unwrap_or(0);
        depth.insert(id.clone(), d);
    }

    println!(
        "{:<28} {:<14} {:<10} {:<14} {:<6} HEALTH",
        "TICKET", "BATCH", "STATE", "PARENT", "PR"
    );
    for id in &order {
        let st = by_id.get(id).expect("ordered id present");
        let ticket = &st.ticket;
        let metadata = substrate
            .most_recent_in_review_metadata(&ticket.id)
            .await
            .map_err(|error| message(format!("read metadata for {}: {error}", ticket.id)))?;
        let pr = metadata
            .as_ref()
            .and_then(|m| m.pr_url.as_deref())
            .unwrap_or("-");
        let health = match &ticket.block_reason {
            Some(BlockReason::RestackConflict { .. }) => "restack-conflict",
            _ => "ok",
        };
        // Parent ticket id (highest-ordinal in-set parent), or "main" for roots.
        let parent = st
            .parents
            .iter()
            .filter_map(|p| by_id.get(p.as_str()).copied())
            .max_by_key(|p| p.ticket.ordinal.unwrap_or(0))
            .map(|p| p.ticket.id.as_str().to_owned())
            .unwrap_or_else(|| "main".to_owned());
        let indent = "  ".repeat(*depth.get(id).unwrap_or(&0));
        let labelled = format!("{indent}{}", ticket.id.as_str());
        println!(
            "{:<28} {:<14} {:<10} {:<14} {:<6} {}",
            labelled,
            ticket.batch.as_ref().map(BatchName::as_str).unwrap_or("-"),
            ticket.state.to_string(),
            parent,
            pr,
            health,
        );
    }
    Ok(CliExitCode::Success)
}

async fn stack_restack(
    args: StackRestackArgs,
    config: &Config,
    repo_root: &Path,
    substrate: &NativeSubstrate,
) -> Result<CliExitCode, crate::CliError> {
    let backend = build_backend(repo_root, config)?;
    let target_branch = "main".to_owned();
    let branch_pattern = config.tools().git().stacking().branch_pattern().to_owned();

    let stack = load_stack_tickets(substrate, args.batch.as_deref()).await?;
    let order = stack_order(&stack)?;
    let by_id: BTreeMap<String, &StackTicket> = stack
        .iter()
        .map(|s| (s.ticket.id.as_str().to_owned(), s))
        .collect();
    let nodes: Vec<StackNode> = stack
        .iter()
        .map(|s| StackNode {
            id: s.ticket.id.as_str().to_owned(),
            ordinal: s.ticket.ordinal,
            parents: s.parents.iter().map(|p| p.as_str().to_owned()).collect(),
        })
        .collect();

    let mut restacked = 0_usize;
    let mut conflicts = 0_usize;
    let mut skipped = 0_usize;
    // Tickets whose subtree is poisoned: a conflict at an ancestor means we
    // cannot trust this branch's base yet (D19 bails the whole subtree but
    // leaves independent subtrees alone).
    let mut blocked_subtree: HashSet<String> = HashSet::new();

    // Process parents before children so a branch is only restacked once its
    // parent is current.
    for id in &order {
        if blocked_subtree.contains(id) {
            skipped += 1;
            continue;
        }
        let st = by_id.get(id).expect("ordered id present");
        let ticket = &st.ticket;
        if !matches!(ticket.state, TicketState::InFlight | TicketState::InReview) {
            continue;
        }
        let Some(metadata) = substrate
            .most_recent_in_review_metadata(&ticket.id)
            .await
            .map_err(|error| message(format!("read metadata: {error}")))?
        else {
            continue;
        };

        let new_parent = parent_branch_of(st, &by_id, &branch_pattern, &target_branch);
        // Roots already sit on target_branch; nothing to cascade for them.
        if new_parent == target_branch && st.parents.is_empty() {
            continue;
        }
        let outcome = backend
            .restack(RestackParams {
                branch: metadata.branch.clone(),
                old_parent: target_branch.clone(),
                new_parent: new_parent.clone(),
                repo_root: repo_root.to_path_buf(),
            })
            .await
            .map_err(|error| message(format!("restack {}: {error}", metadata.branch)))?;
        match outcome {
            RestackOutcome::Restacked => {
                // A local rebase alone leaves the remote stale; publish the
                // rewritten branch with --force-with-lease so the open PR
                // reflects the new parent. Mirrors the foreman's restack path
                // (§8.5 step 4). When the force_push policy is off the native
                // backend reports NotSupported; like the foreman, warn and
                // continue rather than failing the whole restack run.
                if let Err(error) = backend.force_push(&metadata.branch, repo_root).await {
                    println!(
                        "restacked {} onto {} but force-push failed: {error}",
                        metadata.branch, new_parent
                    );
                } else {
                    println!("restacked {} onto {}", metadata.branch, new_parent);
                }
                restacked += 1;
            }
            RestackOutcome::Conflict { recipe } => {
                // D19: bail this subtree only. Block the ticket, surface the
                // recipe, and poison every transitive descendant so we don't
                // restack a child onto an unresolved parent. Independent
                // subtrees keep going.
                if let Err(error) = substrate
                    .block_ticket(
                        &ticket.id,
                        BlockReason::RestackConflict {
                            recipe: recipe.clone(),
                        },
                    )
                    .await
                {
                    println!("conflict on {} but block failed: {error}", metadata.branch);
                }
                println!("conflict on {}: resolve with: {}", metadata.branch, recipe);
                for descendant in descendants_of(&nodes, id) {
                    blocked_subtree.insert(descendant);
                }
                conflicts += 1;
            }
            _ => {}
        }
    }
    println!("restack summary: ok={restacked} conflicts={conflicts} skipped={skipped}");
    Ok(CliExitCode::Success)
}

async fn stack_submit(
    args: StackSubmitArgs,
    config: &Config,
    repo_root: &Path,
    substrate: &NativeSubstrate,
) -> Result<CliExitCode, crate::CliError> {
    let backend = build_backend(repo_root, config)?;
    let target_branch = "main".to_owned();
    let branch_pattern = config.tools().git().stacking().branch_pattern().to_owned();
    let draft = config.tools().git().stacking().draft();

    let stack = load_stack_tickets(substrate, args.batch.as_deref()).await?;
    let order = stack_order(&stack)?;
    let by_id: BTreeMap<String, &StackTicket> = stack
        .iter()
        .map(|s| (s.ticket.id.as_str().to_owned(), s))
        .collect();

    // Track each ticket's branch and current PR URL so we can build the nav
    // table once every PR exists. Walk in stack order so parents get PRs
    // before children (a child's base branch must already exist).
    let mut branch_of: BTreeMap<String, String> = BTreeMap::new();
    let mut pr_url_of: BTreeMap<String, Option<String>> = BTreeMap::new();

    let mut opened = 0_usize;
    let mut retargeted = 0_usize;
    for id in &order {
        let st = by_id.get(id).expect("ordered id present");
        let ticket = &st.ticket;
        // Only tickets with pushed branches participate (InFlight while a hand
        // is open, InReview once a PR is expected).
        if !matches!(ticket.state, TicketState::InFlight | TicketState::InReview) {
            continue;
        }
        let Some(metadata) = substrate
            .most_recent_in_review_metadata(&ticket.id)
            .await
            .map_err(|error| message(format!("read metadata: {error}")))?
        else {
            continue;
        };
        branch_of.insert(id.clone(), metadata.branch.clone());
        let parent_branch = parent_branch_of(st, &by_id, &branch_pattern, &target_branch);

        if metadata.pr_url.is_none() {
            // No PR yet: open one with the correct base.
            let info = backend
                .open_pr(OpenPrParams {
                    branch: metadata.branch.clone(),
                    parent_branch,
                    title: ticket.title.clone(),
                    body: ticket.body.clone(),
                    draft,
                    repo_root: repo_root.to_path_buf(),
                })
                .await
                .map_err(|error| message(format!("open_pr: {error}")))?;
            // Re-record InReview metadata with the new pr_url / pr_number /
            // head_sha so future ticks see the published PR. The ticket may
            // already be InReview, so emit a fresh event directly rather than
            // calling `transition_to_in_review` (which requires InFlight).
            substrate
                .record_typed_event(
                    EventScope::Ticket(ticket.id.clone()),
                    EventKind::TicketTransitionedToInReview {
                        branch: metadata.branch.clone(),
                        pr_url: Some(info.url.clone()),
                        pr_number: Some(info.number),
                        head_sha: info.head_sha,
                    },
                )
                .await
                .map_err(|error| message(format!("update metadata: {error}")))?;
            println!("opened {} for {} → {}", info.url, ticket.id, info.number);
            pr_url_of.insert(id.clone(), Some(info.url));
            opened += 1;
        } else {
            // PR exists: ensure its base matches the computed parent. We can't
            // cheaply read the current base without a gh round-trip, so always
            // issue the retarget; gh treats a no-op base change idempotently.
            // Backends without retarget support (e.g. `none`) report
            // NotSupported — surface it but keep walking the stack.
            match backend
                .retarget_pr(&metadata.branch, &parent_branch, repo_root)
                .await
            {
                Ok(()) => {
                    println!("retargeted {} base → {parent_branch}", metadata.branch);
                    retargeted += 1;
                }
                Err(error) => {
                    println!("retarget {} skipped: {error}", metadata.branch);
                }
            }
            pr_url_of.insert(id.clone(), metadata.pr_url.clone());
        }
    }

    // Maintain the stack-navigation section in every PR body. Build the
    // ordered entry list once, then upsert the marked section into each PR.
    let entries: Vec<StackNavEntry> = order
        .iter()
        .filter(|id| branch_of.contains_key(*id))
        .map(|id| {
            let st = by_id.get(id).expect("ordered id present");
            StackNavEntry {
                ticket_id: id.clone(),
                title: st.ticket.title.clone(),
                pr_url: pr_url_of.get(id).cloned().flatten(),
            }
        })
        .collect();
    let nav_ids: Vec<&String> = order
        .iter()
        .filter(|id| branch_of.contains_key(*id))
        .collect();
    for (index, id) in nav_ids.iter().enumerate() {
        // Only update PRs that actually exist.
        if pr_url_of.get(*id).cloned().flatten().is_none() {
            continue;
        }
        let branch = branch_of.get(*id).expect("branch recorded");
        let section = render_nav_section(&entries, index);
        let existing = backend
            .pr_body(branch, repo_root)
            .await
            .unwrap_or(None)
            .unwrap_or_default();
        let updated = upsert_nav_section(&existing, &section);
        if let Err(error) = backend.set_pr_body(branch, &updated, repo_root).await {
            println!("nav update for {branch} skipped: {error}");
        }
    }

    println!("submit summary: opened={opened} retargeted={retargeted}");
    Ok(CliExitCode::Success)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::*;

    fn config_with_backend(backend: &str) -> Config {
        let yaml = format!(
            r#"
version: 1
site:
  name: test-site
  prefix: tst
models:
  claude-sonnet:
    provider: anthropic
    model: claude-sonnet-4-6
roles:
  drafter: claude-sonnet
  proposer: claude-sonnet
  reviewer: claude-sonnet
tools:
  speckit:
    enabled: true
    version: ">=0.4.0"
  assay:
    enabled: true
    role: reviewer
    reviewers: [reviewer]
    rounds: 1
  substrate:
    backend: native
    mode: solo
  copilot:
    enabled: false
    agent_identity: derrick-hand
  git:
    stacking:
      backend: {backend}
pipeline: []
guardrails:
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
        );
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("derrick.yaml");
        fs::write(&path, yaml).expect("write yaml");
        Config::load_from_path(&path).expect("load config")
    }

    /// The native backend is derrick's only real stacking engine (D72) and
    /// builds without any third-party binary on PATH.
    #[test]
    fn native_config_builds_native_backend() {
        let config = config_with_backend("native");
        let backend = build_backend(Path::new("."), &config).expect("build native backend");
        assert_eq!(backend.kind(), "native");
    }

    /// The `none` backend disables stacking and builds unconditionally.
    #[test]
    fn none_config_builds_none_backend() {
        let config = config_with_backend("none");
        let backend = build_backend(Path::new("."), &config).expect("build none backend");
        assert_eq!(backend.kind(), "none");
    }

    /// A removed third-party backend name must fail config load with the
    /// actionable D72 error rather than silently building anything.
    #[test]
    fn graphite_backend_name_is_rejected_at_config_load() {
        let yaml = removed_backend_yaml("graphite");
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("derrick.yaml");
        fs::write(&path, yaml).expect("write yaml");
        let err = Config::load_from_path(&path).expect_err("graphite must be rejected");
        let text = err.to_string();
        assert!(text.contains("graphite"), "got: {text}");
        assert!(text.contains("native"), "got: {text}");
    }

    fn removed_backend_yaml(backend: &str) -> String {
        format!(
            r#"
version: 1
site:
  name: test-site
  prefix: tst
models:
  claude-sonnet:
    provider: anthropic
    model: claude-sonnet-4-6
roles:
  drafter: claude-sonnet
  proposer: claude-sonnet
  reviewer: claude-sonnet
tools:
  speckit:
    enabled: true
    version: ">=0.4.0"
  assay:
    enabled: true
    role: reviewer
    reviewers: [reviewer]
    rounds: 1
  substrate:
    backend: native
    mode: solo
  copilot:
    enabled: false
    agent_identity: derrick-hand
  git:
    stacking:
      backend: {backend}
pipeline: []
guardrails:
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

    // ---- engine-level integration tests -------------------------------------

    use derrick_substrate::{Hand, HandId, HandKind, InReviewMetadata, LinkKind, NewTicket};

    /// A recording `gh` stub on PATH. `gh pr create` prints a unique URL;
    /// `gh pr view --json body` prints the empty string; everything else
    /// (`pr edit --base`, `pr edit --body`) exits 0. Every argv line is logged.
    struct FakeGh {
        _tmp: tempfile::TempDir,
        log_path: std::path::PathBuf,
        old_path: Option<String>,
    }

    impl FakeGh {
        fn install() -> Self {
            let tmp = tempfile::tempdir().expect("tempdir");
            let bin = tmp.path().join("bin");
            std::fs::create_dir_all(&bin).expect("mkdir");
            let log = tmp.path().join("gh.log");
            let gh = bin.join("gh");
            // `pr create` must print a URL ending in a number so the native
            // backend can parse the PR number. A counter file keeps them unique.
            let counter = tmp.path().join("counter");
            std::fs::write(&counter, "100").expect("seed counter");
            let script = format!(
                "#!/bin/sh\n\
                 printf '%s\\n' \"$*\" >> '{log}'\n\
                 if [ \"$1 $2\" = 'pr create' ]; then\n\
                   n=$(cat '{counter}'); n=$((n+1)); printf '%s' \"$n\" > '{counter}'\n\
                   echo \"https://github.com/o/r/pull/$n\"\n\
                 elif [ \"$1 $2\" = 'pr view' ]; then\n\
                   echo ''\n\
                 fi\n\
                 exit 0\n",
                log = log.display(),
                counter = counter.display(),
            );
            std::fs::write(&gh, script).expect("write gh");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut p = std::fs::metadata(&gh).expect("stat").permissions();
                p.set_mode(0o755);
                std::fs::set_permissions(&gh, p).expect("chmod");
            }
            let old_path = std::env::var("PATH").ok();
            let new_path = match &old_path {
                Some(e) => format!("{}:{}", bin.display(), e),
                None => bin.display().to_string(),
            };
            unsafe { std::env::set_var("PATH", new_path) };
            Self {
                _tmp: tmp,
                log_path: log,
                old_path,
            }
        }

        fn lines(&self) -> Vec<String> {
            std::fs::read_to_string(&self.log_path)
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect()
        }
    }

    impl Drop for FakeGh {
        fn drop(&mut self) {
            unsafe {
                match &self.old_path {
                    Some(p) => std::env::set_var("PATH", p),
                    None => std::env::remove_var("PATH"),
                }
            }
        }
    }

    static PATH_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Run an async body serially with PATH-mutation isolation. Uses a sync
    /// `block_on` so the guard never crosses an `.await` (clippy
    /// await_holding_lock).
    fn run_serial<F: std::future::Future<Output = ()>>(body: F) {
        let _g = PATH_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(body);
    }

    fn native_config() -> (tempfile::TempDir, Config, std::path::PathBuf) {
        let config = config_with_backend("native");
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo_root = tmp.path().to_path_buf();
        std::fs::create_dir_all(repo_root.join(".derrick")).expect("mkdir .derrick");
        (tmp, config, repo_root)
    }

    fn git(repo_root: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(repo_root)
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed");
    }

    /// Initialise a real git repo with `main` and the named feature branches so
    /// the native backend's `git rev-parse` (after `gh pr create`) succeeds.
    fn init_repo_with_branches(repo_root: &Path, branches: &[&str]) {
        git(repo_root, &["init", "-q", "-b", "main"]);
        git(repo_root, &["config", "user.email", "t@example.com"]);
        git(repo_root, &["config", "user.name", "Test"]);
        git(repo_root, &["config", "commit.gpgsign", "false"]);
        std::fs::write(repo_root.join("base.txt"), "base\n").expect("write");
        git(repo_root, &["add", "."]);
        git(repo_root, &["commit", "-q", "--no-gpg-sign", "-m", "init"]);
        for branch in branches {
            git(repo_root, &["branch", branch]);
        }
    }

    async fn open_sub(config: &Config, repo_root: &Path) -> NativeSubstrate {
        let paths = derrick_substrate_native::NativeConfig {
            db_path: repo_root.join(".derrick").join("derrick.db"),
            worktree_root: repo_root.join(".derrick/worktrees"),
        };
        NativeSubstrate::open(paths, config.site().clone())
            .await
            .expect("open substrate")
    }

    /// Create a ticket and drive it to InReview with the given branch + PR.
    async fn ticket_in_review(
        sub: &NativeSubstrate,
        id: &str,
        batch: &str,
        ordinal: u32,
        branch: &str,
        pr_url: Option<&str>,
    ) {
        let batch_name = BatchName::new(batch).expect("batch");
        let _ = sub.create_batch(batch_name.clone()).await;
        let nt = NewTicket::new(
            TicketId::new(id).expect("id"),
            Some(batch_name),
            Some(ordinal),
            "title",
            "body",
            Vec::new(),
        )
        .expect("new ticket");
        sub.create_ticket(nt).await.expect("create");
        let hand = HandId::new(format!("h-{id}")).expect("hand id");
        sub.register_hand(Hand {
            id: hand.clone(),
            kind: HandKind::Human,
            last_seen: None,
            pid: None,
        })
        .await
        .expect("register hand");
        let tid = TicketId::new(id).expect("id");
        sub.assign_to_hand(&tid, &hand).await.expect("assign");
        sub.transition_to_in_review(
            &tid,
            InReviewMetadata {
                branch: branch.to_owned(),
                pr_url: pr_url.map(str::to_owned),
                pr_number: pr_url.map(|_| 100),
                head_sha: format!("{id}-sha"),
            },
        )
        .await
        .expect("in review");
    }

    /// stack_order places parents before children and parent_branch_of
    /// resolves a child's base to its in-set parent's branch (cascade order
    /// correctness). No gh required.
    #[tokio::test]
    async fn stack_order_and_parents_are_cascade_correct() {
        let (_tmp, config, repo_root) = native_config();
        let sub = open_sub(&config, &repo_root).await;
        ticket_in_review(&sub, "drk-1", "alpha", 1, "derrick/alpha/drk-1", Some("u1")).await;
        ticket_in_review(&sub, "drk-2", "alpha", 2, "derrick/alpha/drk-2", Some("u2")).await;
        ticket_in_review(&sub, "drk-3", "alpha", 3, "derrick/alpha/drk-3", Some("u3")).await;
        // chain: drk-2 blocks on drk-1, drk-3 blocks on drk-2.
        sub.link(
            &TicketId::new("drk-2").unwrap(),
            &TicketId::new("drk-1").unwrap(),
            LinkKind::Blocks,
        )
        .await
        .expect("link 2->1");
        sub.link(
            &TicketId::new("drk-3").unwrap(),
            &TicketId::new("drk-2").unwrap(),
            LinkKind::Blocks,
        )
        .await
        .expect("link 3->2");

        let stack = load_stack_tickets(&sub, Some("alpha")).await.expect("load");
        let order = stack_order(&stack).expect("order");
        assert_eq!(order, vec!["drk-1", "drk-2", "drk-3"]);

        let by_id: BTreeMap<String, &StackTicket> = stack
            .iter()
            .map(|s| (s.ticket.id.as_str().to_owned(), s))
            .collect();
        let pattern = "derrick/{{batch}}/{{ticket_id}}";
        let root = by_id.get("drk-1").unwrap();
        let mid = by_id.get("drk-2").unwrap();
        let leaf = by_id.get("drk-3").unwrap();
        assert_eq!(parent_branch_of(root, &by_id, pattern, "main"), "main");
        assert_eq!(
            parent_branch_of(mid, &by_id, pattern, "main"),
            "derrick/alpha/drk-1"
        );
        assert_eq!(
            parent_branch_of(leaf, &by_id, pattern, "main"),
            "derrick/alpha/drk-2"
        );
        sub.close().await.expect("close");
    }

    /// stack_submit opens a PR for the ticket missing one, retargets the
    /// existing child PR's base via `gh pr edit --base`, and writes a single
    /// nav section into each PR body (idempotent across runs).
    #[test]
    fn submit_opens_retargets_and_maintains_one_nav_section() {
        run_serial(submit_opens_retargets_and_maintains_one_nav_section_inner());
    }

    async fn submit_opens_retargets_and_maintains_one_nav_section_inner() {
        let fake = FakeGh::install();
        let (_tmp, config, repo_root) = native_config();
        init_repo_with_branches(
            &repo_root,
            &[
                "derrick/alpha/drk-1",
                "derrick/alpha/drk-2",
                "derrick/alpha/drk-3",
            ],
        );
        let sub = open_sub(&config, &repo_root).await;
        // drk-1 root already has a PR; drk-2 child already has a PR (stale
        // base on main); drk-3 child has a branch but NO PR yet.
        ticket_in_review(
            &sub,
            "drk-1",
            "alpha",
            1,
            "derrick/alpha/drk-1",
            Some("https://github.com/o/r/pull/1"),
        )
        .await;
        ticket_in_review(
            &sub,
            "drk-2",
            "alpha",
            2,
            "derrick/alpha/drk-2",
            Some("https://github.com/o/r/pull/2"),
        )
        .await;
        ticket_in_review(&sub, "drk-3", "alpha", 3, "derrick/alpha/drk-3", None).await;
        sub.link(
            &TicketId::new("drk-2").unwrap(),
            &TicketId::new("drk-1").unwrap(),
            LinkKind::Blocks,
        )
        .await
        .unwrap();
        sub.link(
            &TicketId::new("drk-3").unwrap(),
            &TicketId::new("drk-2").unwrap(),
            LinkKind::Blocks,
        )
        .await
        .unwrap();

        let args = StackSubmitArgs {
            batch: Some("alpha".to_owned()),
        };
        stack_submit(args, &config, &repo_root, &sub)
            .await
            .expect("submit");

        let calls = fake.lines();
        // A PR was created for drk-3 with base = its parent branch (drk-2).
        assert!(
            calls.iter().any(|c| c.contains("pr create")
                && c.contains("--base derrick/alpha/drk-2")
                && c.contains("--head derrick/alpha/drk-3")),
            "expected pr create for drk-3 onto drk-2, got: {calls:?}",
        );
        // drk-2's existing PR base was retargeted onto its parent (drk-1).
        assert!(
            calls
                .iter()
                .any(|c| c.contains("pr edit derrick/alpha/drk-2 --base derrick/alpha/drk-1")),
            "expected retarget of drk-2 onto drk-1, got: {calls:?}",
        );
        // Each PR body got exactly one nav section written.
        let body_edits: Vec<&String> = calls
            .iter()
            .filter(|c| c.contains("pr edit") && c.contains("--body"))
            .collect();
        assert!(!body_edits.is_empty(), "expected nav body edits");

        sub.close().await.expect("close");
    }
}
