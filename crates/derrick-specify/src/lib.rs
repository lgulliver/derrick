//! `derrick-specify` — the native spec generator (DESIGN.md §5.3, D85 / Phase 2).
//!
//! This crate owns derrick's *native* path through the spec-provider seam: the
//! schema + deterministic validation ([`schema`]), the survey grounding
//! pre-pass ([`grounding`]), the generalised clarify loop ([`clarify`]), and the
//! [`NativeSpecProvider`] that orchestrates **scaffold → survey pre-pass →
//! clarify-first → spec draft → plan → tasks**.
//!
//! The thin seam (the `SpecProviderKind` enum, config resolution, dispatch)
//! lives in `derrick-flow`, which depends on this crate. This crate **must not**
//! depend on `derrick-flow` (that would cycle).
//!
//! Each model sub-step calls the host model **in-process via a
//! [`derrick_tools::HostRegistry`]** (mirroring `derrick-flow`'s clarify step):
//! the role's bound model resolves to a host name + model id, and a headless
//! [`derrick_tools::HostRequest`] is dispatched. Spec/tasks use the *drafter*
//! tier, plan uses the *proposer* tier. On a hard [`schema::Severity::Reject`],
//! exactly one bounded repair pass feeds the findings + prior draft back; a
//! second failure is a [`SpecifyError::Validation`].

pub mod clarify;
pub mod grounding;
pub mod schema;

use std::path::{Path, PathBuf};

use derrick_config::{Config, cli_host_for_runtime};
use derrick_tools::{HostRegistry, HostRequest};

use crate::schema::{Finding, SpecMeta, has_reject};

/// The default host backing in-process model calls when a role's model does not
/// resolve to a known CLI host. Matches the clarify step's hardcoded host.
const DEFAULT_HOST: &str = "claude";
/// Role bound to the spec/tasks drafting tier.
const DRAFTER_ROLE: &str = "drafter";
/// Role bound to the planning tier.
const PROPOSER_ROLE: &str = "proposer";

/// Errors raised by the native spec provider.
#[derive(Debug, thiserror::Error)]
pub enum SpecifyError {
    /// The requested host adapter was not registered.
    #[error("host {host:?} is not registered for the native spec provider")]
    HostMissing {
        /// The host name that was looked up.
        host: String,
    },
    /// A host model invocation failed.
    #[error("native {phase} model call failed: {message}")]
    Host {
        /// The phase that failed.
        phase: &'static str,
        /// The underlying error message.
        message: String,
    },
    /// An artifact failed schema validation after the repair pass.
    #[error("native {phase} failed validation after one repair pass: {summary}")]
    Validation {
        /// The phase that failed.
        phase: &'static str,
        /// A joined summary of the reject findings.
        summary: String,
    },
    /// A filesystem operation failed.
    #[error("io error at {path}: {source}")]
    Io {
        /// The path involved.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
}

/// Accounting + artifacts produced by one native phase.
#[derive(Clone, Debug, Default)]
pub struct NativeOutcome {
    /// Artifact paths produced this phase, relative to the working directory.
    pub artifacts: Vec<PathBuf>,
    /// Input tokens consumed across the phase's model calls.
    pub tokens_in: u32,
    /// Output tokens produced across the phase's model calls.
    pub tokens_out: u32,
    /// Raw bytes considered for compression (grounding block size).
    pub bytes_raw: u32,
    /// Bytes saved by caveman compression of handoff context.
    pub bytes_saved: u32,
    /// Estimated output tokens saved by roughneck prompt injection.
    pub roughneck_tokens_saved: u32,
    /// Whether the phase needed a repair pass (test/telemetry signal).
    pub repaired: bool,
}

/// Everything a native phase needs. The provider never mutates the registry;
/// callers (the seam) own it. `feature_dir` is relative to `working_dir`.
pub struct NativeRequest<'a> {
    /// The raw feature request prompt.
    pub raw_prompt: &'a str,
    /// The repository root.
    pub repo_root: &'a Path,
    /// The working directory (worktree or repo root) where artifacts are
    /// written and the survey index is read.
    pub working_dir: &'a Path,
    /// The registered host adapters.
    pub hosts: &'a HostRegistry,
    /// The effective configuration (role → model → host resolution).
    pub config: &'a Config,
    /// Advisory: whether the surrounding run is interactive. The native
    /// provider itself always auto-accepts clarify recommendations (it holds no
    /// streams); interactive clarification is the dedicated `clarify` step.
    /// Retained so callers can pass through their run mode for future use.
    pub interactive: bool,
    /// The pre-scaffolded feature directory (relative to `working_dir`).
    pub feature_dir: &'a Path,
}

/// The native spec provider. Stateless; methods take a [`NativeRequest`].
#[derive(Clone, Copy, Debug, Default)]
pub struct NativeSpecProvider;

impl NativeSpecProvider {
    /// Constructs the provider.
    pub fn new() -> Self {
        Self
    }

    /// Produces `spec.md` (clarify-first, survey-grounded, schema-validated).
    ///
    /// Order: survey pre-pass → clarify the raw prompt → draft the spec with the
    /// derrick-authored grounding front-matter → validate → one repair pass on a
    /// hard reject. Writes `clarify.md` and overwrites the scaffolded `spec.md`.
    pub async fn specify(&self, req: &NativeRequest<'_>) -> Result<NativeOutcome, SpecifyError> {
        let mut outcome = NativeOutcome::default();

        // 1. Survey pre-pass (no model).
        let grounding = grounding::gather(req.working_dir, req.raw_prompt).await;
        outcome.bytes_raw = outcome.bytes_raw.saturating_add(grounding.bytes_raw);
        outcome.bytes_saved = outcome.bytes_saved.saturating_add(grounding.bytes_saved);

        // 2. Clarify-first: clarify the raw prompt + grounding, before drafting.
        let clarify_prompt =
            clarify::build_raw_prompt_questions(req.raw_prompt, &grounding.context_block);
        let (clarify_text, c_in, c_out, c_rough) = self
            .call_model(req, DRAFTER_ROLE, &clarify_prompt, "clarify")
            .await?;
        outcome.tokens_in = outcome.tokens_in.saturating_add(c_in);
        outcome.tokens_out = outcome.tokens_out.saturating_add(c_out);
        outcome.roughneck_tokens_saved = outcome.roughneck_tokens_saved.saturating_add(c_rough);

        let questions = clarify::parse_clarify_questions(&clarify_text);
        let clarify_md = if questions.is_empty() {
            "# Clarification Q&A\n\n(No clarifying questions were raised.)\n".to_owned()
        } else {
            // The native provider runs headless (it holds no input/output
            // streams), so it auto-accepts each recommendation. Interactive
            // clarification is the dedicated `clarify` step in `derrick-flow`,
            // which owns the real stdin/stderr.
            let answers = clarify::auto_accept_recommendations(&questions);
            clarify::render_clarify_markdown(&questions, &answers)
        };
        self.write(req, "clarify.md", &clarify_md)?;
        outcome.artifacts.push(req.feature_dir.join("clarify.md"));

        // 3. Spec draft + validate + bounded repair.
        let draft_prompt = build_spec_prompt(req.raw_prompt, &grounding.context_block, &clarify_md);
        let spec_body = self
            .draft_with_repair(
                req,
                DRAFTER_ROLE,
                "specify",
                &draft_prompt,
                &mut outcome,
                schema::validate_spec,
            )
            .await?;

        // Derrick injects the grounding front-matter itself: re-serialise the
        // model's spec with our authoritative grounding block, never the model's.
        let spec_md = inject_grounding(&spec_body, &grounding.front_matter);
        self.write(req, "spec.md", &spec_md)?;
        outcome.artifacts.push(req.feature_dir.join("spec.md"));
        outcome
            .artifacts
            .push(PathBuf::from(".specify/feature.json"));
        Ok(outcome)
    }

    /// Normalizes an externally-authored document into a schema-valid `spec.md`.
    ///
    /// This is the model path of the `import` provider: the seam first checks
    /// whether the source already conforms ([`schema::looks_like_spec`]) and only
    /// calls this when it does not. One drafter-tier model call rewrites
    /// `source_text` into the `derrick-specify` spec schema/template; the draft is
    /// schema-validated with one bounded repair pass, and derrick injects its own
    /// authoritative `grounding:` front-matter (the model never authors it). The
    /// scaffolded `spec.md` is overwritten with the normalized document.
    ///
    /// Unlike [`Self::specify`], there is no clarify pre-pass and no
    /// `clarify.md` — the source is taken as the operator's answer. Grounding is
    /// still gathered so the front-matter records real index symbols.
    pub async fn normalize_to_spec(
        &self,
        req: &NativeRequest<'_>,
        source_text: &str,
    ) -> Result<NativeOutcome, SpecifyError> {
        let mut outcome = NativeOutcome::default();

        // Survey pre-pass (no model) — used only to author the grounding
        // front-matter; the normalization prompt is the source document itself.
        let grounding = grounding::gather(req.working_dir, req.raw_prompt).await;
        outcome.bytes_raw = outcome.bytes_raw.saturating_add(grounding.bytes_raw);
        outcome.bytes_saved = outcome.bytes_saved.saturating_add(grounding.bytes_saved);

        let prompt = build_normalize_prompt(req.raw_prompt, source_text);
        let spec_body = self
            .draft_with_repair(
                req,
                DRAFTER_ROLE,
                "import",
                &prompt,
                &mut outcome,
                schema::validate_spec,
            )
            .await?;

        let spec_md = inject_grounding(&spec_body, &grounding.front_matter);
        self.write(req, "spec.md", &spec_md)?;
        outcome.artifacts.push(req.feature_dir.join("spec.md"));
        outcome
            .artifacts
            .push(PathBuf::from(".specify/feature.json"));
        Ok(outcome)
    }

    /// Produces `plan.md` (proposer tier; `covers` must ⊇ spec requirements).
    ///
    /// `clarifications` is the accepted clarify text the seam threads in (so the
    /// native planner sees the same answers the speckit planner would).
    pub async fn plan(
        &self,
        req: &NativeRequest<'_>,
        clarifications: Option<&str>,
    ) -> Result<NativeOutcome, SpecifyError> {
        let mut outcome = NativeOutcome::default();
        let spec_md = self.read(req, "spec.md")?;
        // Requirement ids drive the plan's `covers` check, so they MUST come
        // from a canonical, successfully-parsed spec. A missing/malformed spec
        // front-matter is a hard error here rather than a silent empty set (an
        // empty set would disable the covers check entirely).
        let spec_meta = parse_spec_meta(&spec_md).ok_or_else(|| SpecifyError::Validation {
            phase: "plan",
            summary: "spec.md front-matter is missing or malformed; cannot derive requirement ids \
                      for the plan covers-check"
                .to_owned(),
        })?;
        // `parse_spec_meta` only proves the YAML deserializes; some list fields
        // are validator-owned, so a semantically-invalid spec (e.g. zero
        // requirements, leftover open_questions) can still parse. Run the full
        // schema validation before trusting the requirement ids — otherwise an
        // empty requirements list would silently disable the plan covers-check.
        let spec_findings = schema::validate_spec(&spec_md);
        if has_reject(&spec_findings) {
            return Err(SpecifyError::Validation {
                phase: "plan",
                summary: format!(
                    "spec.md is semantically invalid; cannot plan against it: {}",
                    summarize_rejects(&spec_findings)
                ),
            });
        }
        let requirement_ids: Vec<String> = spec_meta
            .requirements
            .iter()
            .map(|r| r.id.clone())
            .collect();
        // `touches` is cross-checked only against a FULL index path set. The
        // spec's `grounding.symbols` is a capped, query-scoped subset of the
        // index, so checking against it would spuriously warn on valid paths the
        // grounding pass simply did not surface. The Survey API exposes no
        // "list every indexed path" query, so rather than warn against a
        // known-partial set we pass an empty slice, which `validate_plan` treats
        // as "skip the touches cross-check". (Documented decision per review.)
        let indexed_paths: Vec<String> = Vec::new();

        // Caveman the spec body into the plan's handoff context (never the
        // on-disk spec.md). The compressed prefix is the stable part across the
        // plan/tasks calls; prompt-cache support is folded into the prompt for
        // the in-process CLI path (see report — true cached_prefix is API-only).
        let compressed = derrick_caveman::compress(&spec_md, derrick_caveman::Intensity::Full);
        outcome.bytes_raw = u32::try_from(spec_md.len()).unwrap_or(u32::MAX);
        outcome.bytes_saved = outcome
            .bytes_raw
            .saturating_sub(u32::try_from(compressed.text.len()).unwrap_or(u32::MAX));

        let prompt = build_plan_prompt(req.raw_prompt, &compressed.text, clarifications);
        let req_ids = requirement_ids.clone();
        let idx_paths = indexed_paths.clone();
        let plan_md = self
            .draft_with_repair(
                req,
                PROPOSER_ROLE,
                "plan",
                &prompt,
                &mut outcome,
                move |md| schema::validate_plan(md, &req_ids, &idx_paths),
            )
            .await?;
        self.write(req, "plan.md", &plan_md)?;
        outcome.artifacts.push(req.feature_dir.join("plan.md"));
        Ok(outcome)
    }

    /// Produces `tasks.md` (drafter tier; byte-compatible with the bridge's
    /// `parse_tasks_from_markdown`).
    pub async fn tasks(&self, req: &NativeRequest<'_>) -> Result<NativeOutcome, SpecifyError> {
        let mut outcome = NativeOutcome::default();
        let spec_md = self.read(req, "spec.md")?;
        let plan_md = self.read(req, "plan.md")?;
        let compressed_spec = derrick_caveman::compress(&spec_md, derrick_caveman::Intensity::Full);
        let compressed_plan = derrick_caveman::compress(&plan_md, derrick_caveman::Intensity::Full);
        let raw = u32::try_from(spec_md.len() + plan_md.len()).unwrap_or(u32::MAX);
        let out = u32::try_from(compressed_spec.text.len() + compressed_plan.text.len())
            .unwrap_or(u32::MAX);
        outcome.bytes_raw = raw;
        outcome.bytes_saved = raw.saturating_sub(out);

        let prompt =
            build_tasks_prompt(req.raw_prompt, &compressed_spec.text, &compressed_plan.text);
        let tasks_md = self
            .draft_with_repair(req, DRAFTER_ROLE, "tasks", &prompt, &mut outcome, |md| {
                schema::validate_tasks(md)
            })
            .await?;
        self.write(req, "tasks.md", &tasks_md)?;
        outcome.artifacts.push(req.feature_dir.join("tasks.md"));
        Ok(outcome)
    }

    /// Drafts an artifact, validates it, and runs at most one repair pass.
    async fn draft_with_repair<F>(
        &self,
        req: &NativeRequest<'_>,
        role: &str,
        phase: &'static str,
        prompt: &str,
        outcome: &mut NativeOutcome,
        validate: F,
    ) -> Result<String, SpecifyError>
    where
        F: Fn(&str) -> Vec<Finding>,
    {
        let (draft, t_in, t_out, rough) = self.call_model(req, role, prompt, phase).await?;
        outcome.tokens_in = outcome.tokens_in.saturating_add(t_in);
        outcome.tokens_out = outcome.tokens_out.saturating_add(t_out);
        outcome.roughneck_tokens_saved = outcome.roughneck_tokens_saved.saturating_add(rough);

        let findings = validate(&draft);
        if !has_reject(&findings) {
            return Ok(draft);
        }
        // One bounded repair pass: feed the findings + prior draft back.
        let summary = summarize_rejects(&findings);
        tracing::warn!(
            target: "derrick_specify",
            phase, %summary, "native draft rejected; running one repair pass"
        );
        let repair_prompt = build_repair_prompt(prompt, &draft, &summary);
        let (repaired, r_in, r_out, r_rough) =
            self.call_model(req, role, &repair_prompt, phase).await?;
        outcome.tokens_in = outcome.tokens_in.saturating_add(r_in);
        outcome.tokens_out = outcome.tokens_out.saturating_add(r_out);
        outcome.roughneck_tokens_saved = outcome.roughneck_tokens_saved.saturating_add(r_rough);
        outcome.repaired = true;

        let findings = validate(&repaired);
        if has_reject(&findings) {
            return Err(SpecifyError::Validation {
                phase,
                summary: summarize_rejects(&findings),
            });
        }
        Ok(repaired)
    }

    /// Calls the host model bound to `role`, returning
    /// `(stdout, tokens_in, tokens_out, roughneck_tokens_saved)`.
    ///
    /// Mirrors the in-process pattern from `derrick-flow`'s clarify step:
    /// `hosts.get(host)` + a headless [`HostRequest`]. The host name and model
    /// id are resolved from the role's bound model definition.
    async fn call_model(
        &self,
        req: &NativeRequest<'_>,
        role: &str,
        prompt: &str,
        phase: &'static str,
    ) -> Result<(String, u32, u32, u32), SpecifyError> {
        let (host_name, model_id) = resolve_host_and_model(req.config, role);
        let host = req
            .hosts
            .get(&host_name)
            .ok_or_else(|| SpecifyError::HostMissing {
                host: host_name.clone(),
            })?;

        let level = req.config.tools().roughneck().level();
        let prompt = if req.config.tools().roughneck().enabled() {
            derrick_roughneck::inject_prompt(prompt, level)
        } else {
            prompt.to_owned()
        };
        let prompt_len = prompt.len();

        let mut request = HostRequest::new(prompt, req.working_dir);
        request.headless = true;
        request.model = model_id;
        let response = host
            .run(request)
            .await
            .map_err(|source| SpecifyError::Host {
                phase,
                message: source.to_string(),
            })?;

        let tokens_in = response
            .tokens_in
            .max(u32::try_from(prompt_len).unwrap_or(u32::MAX) / 4);
        let roughneck_saved = if req.config.tools().roughneck().enabled() {
            derrick_roughneck::estimate_savings(&response.stdout, level).tokens_saved
        } else {
            0
        };
        Ok((
            response.stdout,
            tokens_in,
            response.tokens_out,
            roughneck_saved,
        ))
    }

    fn write(
        &self,
        req: &NativeRequest<'_>,
        name: &str,
        content: &str,
    ) -> Result<(), SpecifyError> {
        let path = req.working_dir.join(req.feature_dir).join(name);
        std::fs::write(&path, content).map_err(|source| SpecifyError::Io { path, source })
    }

    fn read(&self, req: &NativeRequest<'_>, name: &str) -> Result<String, SpecifyError> {
        let path = req.working_dir.join(req.feature_dir).join(name);
        std::fs::read_to_string(&path).map_err(|source| SpecifyError::Io { path, source })
    }
}

/// Resolves `(host_name, model_id)` for a role's bound model.
///
/// Looks up `roles[role] → models[name]`, derives the CLI host from the model's
/// runtime via [`cli_host_for_runtime`] (defaulting to [`DEFAULT_HOST`]), and
/// forwards the model id when one is configured. Mirrors `derrick-flow`'s
/// `execute_role_step` resolution but for the in-process host path.
fn resolve_host_and_model(config: &Config, role: &str) -> (String, Option<String>) {
    let Some(model_name) = config.roles().get(role) else {
        return (DEFAULT_HOST.to_owned(), None);
    };
    let Some(def) = config.models().get(model_name) else {
        return (DEFAULT_HOST.to_owned(), None);
    };
    let runtime = def.resolved_runtime();
    let host = cli_host_for_runtime(&runtime)
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| DEFAULT_HOST.to_owned());
    let model_id = {
        let m = def.model().trim();
        if m.is_empty() {
            None
        } else {
            Some(m.to_owned())
        }
    };
    (host, model_id)
}

/// Parses a spec's front-matter into a [`SpecMeta`], returning `None` if it is
/// absent or malformed.
fn parse_spec_meta(spec_md: &str) -> Option<SpecMeta> {
    let (yaml, _body) = schema::split_front_matter(spec_md);
    serde_yaml::from_str(yaml?).ok()
}

/// Injects derrick's authoritative `grounding:` front-matter into a spec.
///
/// If the model's draft already carries front-matter, the `grounding:` key is
/// replaced; otherwise a fresh front-matter block is prepended. The model never
/// authors `grounding` — this guarantees the symbols are real index hits.
fn inject_grounding(spec_md: &str, grounding: &schema::Grounding) -> String {
    let (yaml, body) = schema::split_front_matter(spec_md);
    let grounding_yaml = serde_yaml::to_string(
        &serde_yaml::to_value(GroundingWrapper {
            grounding: grounding.clone(),
        })
        .unwrap_or(serde_yaml::Value::Null),
    )
    .unwrap_or_default();
    match yaml {
        Some(existing) => {
            // Drop any model-authored grounding, then append ours.
            let mut value: serde_yaml::Value =
                serde_yaml::from_str(existing).unwrap_or(serde_yaml::Value::Null);
            if let serde_yaml::Value::Mapping(map) = &mut value {
                map.remove(serde_yaml::Value::from("grounding"));
                map.insert(
                    serde_yaml::Value::from("grounding"),
                    serde_yaml::to_value(grounding.clone()).unwrap_or(serde_yaml::Value::Null),
                );
            }
            let merged = serde_yaml::to_string(&value).unwrap_or_default();
            format!("---\n{merged}---\n{body}")
        }
        None => {
            // No front-matter (a malformed draft) — prepend just the grounding.
            format!("---\n{grounding_yaml}---\n{spec_md}")
        }
    }
}

#[derive(serde::Serialize)]
struct GroundingWrapper {
    grounding: schema::Grounding,
}

fn summarize_rejects(findings: &[Finding]) -> String {
    findings
        .iter()
        .filter(|f| f.severity == schema::Severity::Reject)
        .map(|f| format!("[{}] {}", f.code, f.msg))
        .collect::<Vec<_>>()
        .join("; ")
}

fn build_spec_prompt(raw_prompt: &str, grounding_block: &str, clarify_md: &str) -> String {
    format!(
        "Write a specification for this feature request:\n\n{raw_prompt}\n\n\
         {grounding_block}\n\n\
         Accepted clarifications:\n{clarify_md}\n\n\
         Output a single markdown document with YAML front-matter, in this exact shape:\n\
         ---\n\
         schema: {schema}\n\
         slug: <kebab-case-slug>\n\
         intent: <one line>\n\
         requirements:\n\
         \x20\x20- id: R1\n\
         \x20\x20\x20\x20must: <normative statement>\n\
         acceptance:\n\
         \x20\x20- id: A1\n\
         \x20\x20\x20\x20check: <verifiable criterion>\n\
         non_goals: []\n\
         open_questions: []\n\
         ---\n\
         # <Title>\n\n## Context\n...\n\n## Requirements\n...\n\n## Acceptance Criteria\n...\n\n## Out of Scope\n...\n\n\
         Rules: at least one requirement and one acceptance criterion; open_questions MUST be \
         empty (resolve every ambiguity using the clarifications above); do NOT write a \
         `grounding:` key (derrick supplies it). Emit ONLY the document, no prose around it.",
        schema = schema::SPEC_SCHEMA,
    )
}

fn build_normalize_prompt(raw_prompt: &str, source_text: &str) -> String {
    format!(
        "Normalize an existing product document into derrick's specification \
         schema. Preserve the author's intent, requirements, and acceptance \
         criteria — do not invent new scope.\n\n\
         Originating request: {raw_prompt}\n\n\
         Source document to convert:\n{source_text}\n\n\
         Output a single markdown document with YAML front-matter, in this exact shape:\n\
         ---\n\
         schema: {schema}\n\
         slug: <kebab-case-slug>\n\
         intent: <one line>\n\
         requirements:\n\
         \x20\x20- id: R1\n\
         \x20\x20\x20\x20must: <normative statement>\n\
         acceptance:\n\
         \x20\x20- id: A1\n\
         \x20\x20\x20\x20check: <verifiable criterion>\n\
         non_goals: []\n\
         open_questions: []\n\
         ---\n\
         # <Title>\n\n## Context\n...\n\n## Requirements\n...\n\n## Acceptance Criteria\n...\n\n## Out of Scope\n...\n\n\
         Rules: derive at least one requirement and one acceptance criterion from \
         the source; open_questions MUST be empty (if the source leaves something \
         ambiguous, make the most faithful reasonable choice rather than leaving a \
         question); do NOT write a `grounding:` key (derrick supplies it). Emit \
         ONLY the document, no prose around it.",
        schema = schema::SPEC_SCHEMA,
    )
}

fn build_plan_prompt(raw_prompt: &str, spec_context: &str, clarifications: Option<&str>) -> String {
    let clarify = clarifications.unwrap_or("(none)");
    format!(
        "Produce an implementation plan for this feature.\n\nRequest: {raw_prompt}\n\n\
         Specification (compressed):\n{spec_context}\n\n\
         Accepted clarifications:\n{clarify}\n\n\
         Output a markdown document with YAML front-matter in this exact shape:\n\
         ---\n\
         schema: {schema}\n\
         covers: [R1]\n\
         touches:\n\
         \x20\x20- <repo/path>\n\
         ---\n\
         # Plan\n<ordered steps>\n\n\
         Rules: `covers` MUST list every requirement id from the specification; `touches` lists \
         the repository paths the plan expects to change. Emit ONLY the document.",
        schema = schema::PLAN_SCHEMA,
    )
}

fn build_tasks_prompt(raw_prompt: &str, spec_context: &str, plan_context: &str) -> String {
    format!(
        "Break this plan into tickets.\n\nRequest: {raw_prompt}\n\n\
         Specification (compressed):\n{spec_context}\n\n\
         Plan (compressed):\n{plan_context}\n\n\
         Output markdown with NO front-matter. Each ticket is a `## ` H2 heading, optionally \
         followed by a `<!-- complexity: low|standard|heavy -->` marker line, then the ticket \
         body. Each ticket body should reference the requirement id(s) it implements (e.g. R1). \
         Emit ONLY the document.",
    )
}

fn build_repair_prompt(original_prompt: &str, prior_draft: &str, findings: &str) -> String {
    format!(
        "Your previous draft was rejected by schema validation. Fix EVERY issue and re-emit the \
         complete corrected document (not a diff).\n\n\
         Validation findings to fix:\n{findings}\n\n\
         Your previous draft:\n{prior_draft}\n\n\
         Original instructions:\n{original_prompt}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_config_yaml(models_roles: &str) -> String {
        format!(
            "version: 1\n\
             site:\n  name: t\n  prefix: tst\n\
             {models_roles}\
             tools:\n  speckit:\n    enabled: true\n    version: \">=0.4.0\"\n  assay:\n    enabled: false\n    role: reviewer\n    reviewers: [reviewer]\n    rounds: 1\n  substrate:\n    backend: native\n    mode: solo\n  copilot:\n    enabled: false\n    agent_identity: derrick-hand\n\
             guardrails:\n  constitution_path: .specify/memory/constitution.md\n  forbid_paths: []\n  required_labels: []\n\
             parallelism:\n  batch_max: 8\n  step_max: 4\n  assay_max: 2\n\
             state:\n  dir: .derrick\n  log_runs: true\n  worktree_root: .derrick/worktrees\n"
        )
    }

    #[test]
    fn resolve_defaults_to_claude_when_role_absent() {
        let yaml = full_config_yaml(
            "models:\n  m:\n    provider: claude\n    model: claude-sonnet-4-6\nroles:\n  drafter: m\n  proposer: m\n  reviewer: m\n",
        );
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("derrick.yaml");
        std::fs::write(&path, yaml).expect("write");
        let config = Config::load_from_path(&path).expect("load");
        let (host, model) = resolve_host_and_model(&config, DRAFTER_ROLE);
        assert_eq!(host, "claude");
        assert_eq!(model.as_deref(), Some("claude-sonnet-4-6"));
        // An unbound role falls back to the default host with no model id.
        let (host, model) = resolve_host_and_model(&config, "nonexistent");
        assert_eq!(host, "claude");
        assert!(model.is_none());
    }

    #[test]
    fn inject_grounding_replaces_model_authored_block() {
        let model_draft = "---\nschema: derrick.spec/v1\nslug: x\ngrounding:\n  index_fresh: false\n  symbols: [\"made-up.rs:1 invented\"]\n---\n# Title\nbody\n";
        let truth = schema::Grounding {
            index_fresh: true,
            symbols: vec!["src/lib.rs:10 real_symbol".to_owned()],
        };
        let merged = inject_grounding(model_draft, &truth);
        assert!(merged.contains("real_symbol"));
        assert!(!merged.contains("invented"));
    }
}
