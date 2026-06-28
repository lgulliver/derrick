//! Spec / plan / tasks schema + deterministic validation (DESIGN.md §5.3).
//!
//! The native spec provider produces three artifacts with a fixed, machine-
//! checkable shape so a model pass can never silently emit a malformed
//! specification:
//!
//!   * `spec.md` — YAML front-matter ([`SpecMeta`]) + required body headings.
//!   * `plan.md` — YAML front-matter ([`PlanMeta`]) + body.
//!   * `tasks.md` — NO front-matter; one `## ` H2 per ticket, byte-compatible
//!     with `derrick-flow`'s `parse_tasks_from_markdown`.
//!
//! [`validate_spec`] / [`validate_plan`] / [`validate_tasks`] each return a
//! [`Vec<Finding>`]; a [`Severity::Reject`] finding fails the artifact and
//! triggers a single bounded repair pass in [`crate::NativeSpecProvider`].
//!
//! Validation is pure (no I/O), which is what the golden corpus tests in this
//! module exercise.

use serde::{Deserialize, Serialize};

/// Severity of a validation [`Finding`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Severity {
    /// The artifact is unusable; the orchestrator must repair or fail.
    Reject,
    /// A non-fatal concern surfaced to the operator; the artifact is accepted.
    Warn,
}

/// A single validation result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    /// Stable machine code (e.g. `spec.missing_heading`).
    pub code: String,
    /// How serious the finding is.
    pub severity: Severity,
    /// Human-readable explanation, fed back to the model on a repair pass.
    pub msg: String,
}

impl Finding {
    fn reject(code: &str, msg: impl Into<String>) -> Self {
        Self {
            code: code.to_owned(),
            severity: Severity::Reject,
            msg: msg.into(),
        }
    }

    fn warn(code: &str, msg: impl Into<String>) -> Self {
        Self {
            code: code.to_owned(),
            severity: Severity::Warn,
            msg: msg.into(),
        }
    }
}

/// Returns true if any finding in `findings` is a hard reject.
pub fn has_reject(findings: &[Finding]) -> bool {
    findings.iter().any(|f| f.severity == Severity::Reject)
}

/// One requirement entry in a spec's front-matter.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Requirement {
    /// Stable id, conventionally `R<n>` (e.g. `R1`).
    pub id: String,
    /// The normative statement (a `must`).
    pub must: String,
}

/// One acceptance-criterion entry in a spec's front-matter.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Acceptance {
    /// Stable id, conventionally `A<n>` (e.g. `A1`).
    pub id: String,
    /// The criterion text (optional in the schema; the id is what is checked).
    #[serde(default)]
    pub check: Option<String>,
}

/// Grounding block derrick writes itself from the survey index. The model never
/// authors this — it is injected verbatim so symbol names cannot be invented.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Grounding {
    /// Whether the survey index was present and fresh when grounding ran.
    #[serde(default)]
    pub index_fresh: bool,
    /// Compact `path:line` symbol references pulled from the index.
    #[serde(default)]
    pub symbols: Vec<String>,
}

/// Front-matter for `spec.md`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpecMeta {
    /// Schema discriminator (e.g. `derrick.spec/v1`).
    pub schema: String,
    /// Feature slug (matches the `specs/<NNN>-<slug>` directory).
    pub slug: String,
    /// One-line intent statement.
    pub intent: String,
    /// Functional requirements; at least one is required.
    pub requirements: Vec<Requirement>,
    /// Acceptance criteria; at least one is required.
    pub acceptance: Vec<Acceptance>,
    /// Explicit out-of-scope items. The key MUST be present (a missing key is a
    /// Reject, distinct from an empty list); the list itself may be empty.
    /// Modelled as `Option` so a missing key is distinguishable from `[]`.
    #[serde(default)]
    pub non_goals: Option<Vec<String>>,
    /// Outstanding questions. The key MUST be present and MUST be empty on a
    /// clean run — a missing key OR a non-empty list is a hard Reject
    /// (clarify-first means open questions are resolved before drafting).
    /// Modelled as `Option` so a missing key is distinguishable from `[]`.
    #[serde(default)]
    pub open_questions: Option<Vec<String>>,
    /// Survey grounding, written by derrick (never by the model).
    #[serde(default)]
    pub grounding: Grounding,
}

/// Front-matter for `plan.md`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlanMeta {
    /// Schema discriminator (e.g. `derrick.plan/v1`).
    pub schema: String,
    /// Requirement ids this plan covers. Must be a superset of the spec's
    /// requirement ids.
    pub covers: Vec<String>,
    /// Repository paths this plan expects to touch. Cross-checked against the
    /// survey index (a miss is a [`Severity::Warn`]).
    #[serde(default)]
    pub touches: Vec<String>,
}

/// The current spec/plan schema discriminators.
pub const SPEC_SCHEMA: &str = "derrick.spec/v1";
/// The current plan schema discriminator.
pub const PLAN_SCHEMA: &str = "derrick.plan/v1";

/// Required body headings (after the front-matter) for a spec.
const REQUIRED_SPEC_HEADINGS: &[&str] = &[
    "## Context",
    "## Requirements",
    "## Acceptance Criteria",
    "## Out of Scope",
];

/// Splits a markdown document into `(front_matter_yaml, body)`.
///
/// Front-matter is a leading `---` fenced block. Returns `None` for the YAML
/// part when no front-matter fence is present.
pub fn split_front_matter(md: &str) -> (Option<&str>, &str) {
    let trimmed = md.strip_prefix('\u{feff}').unwrap_or(md);
    let Some(rest) = trimmed.strip_prefix("---") else {
        return (None, md);
    };
    // The opening fence must be its own line.
    let rest = rest
        .strip_prefix('\n')
        .or_else(|| rest.strip_prefix("\r\n"));
    let Some(rest) = rest else {
        return (None, md);
    };
    // Find the closing `---` fence at the start of a line.
    if let Some(end) = find_closing_fence(rest) {
        let yaml = &rest[..end.fence_start];
        let body = &rest[end.body_start..];
        (Some(yaml), body)
    } else {
        (None, md)
    }
}

struct FenceSplit {
    fence_start: usize,
    body_start: usize,
}

fn find_closing_fence(rest: &str) -> Option<FenceSplit> {
    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        let stripped = line.trim_end_matches(['\n', '\r']);
        if stripped == "---" {
            return Some(FenceSplit {
                fence_start: offset,
                body_start: offset + line.len(),
            });
        }
        offset += line.len();
    }
    None
}

/// Validates a `spec.md` document. `md` is the full file (front-matter + body).
pub fn validate_spec(md: &str) -> Vec<Finding> {
    let mut findings = Vec::new();
    let (yaml, body) = split_front_matter(md);
    let Some(yaml) = yaml else {
        findings.push(Finding::reject(
            "spec.no_front_matter",
            "spec.md is missing its YAML front-matter block (must open with a `---` fence)",
        ));
        return findings;
    };
    let meta: SpecMeta = match serde_yaml::from_str(yaml) {
        Ok(meta) => meta,
        Err(error) => {
            findings.push(Finding::reject(
                "spec.bad_front_matter",
                format!("spec front-matter did not parse: {error}"),
            ));
            return findings;
        }
    };

    if meta.schema != SPEC_SCHEMA {
        findings.push(Finding::warn(
            "spec.schema_mismatch",
            format!("expected schema {SPEC_SCHEMA:?}, found {:?}", meta.schema),
        ));
    }
    if meta.requirements.is_empty() {
        findings.push(Finding::reject(
            "spec.no_requirements",
            "spec front-matter must list at least one requirement",
        ));
    }
    if meta.acceptance.is_empty() {
        findings.push(Finding::reject(
            "spec.no_acceptance",
            "spec front-matter must list at least one acceptance criterion",
        ));
    }
    // `non_goals` must be present (a missing key is malformed front-matter),
    // but may be an empty list.
    if meta.non_goals.is_none() {
        findings.push(Finding::reject(
            "spec.missing_non_goals",
            "spec front-matter must include a `non_goals` key (it may be an empty list)",
        ));
    }
    // `open_questions` must be present AND empty on a clean run.
    match &meta.open_questions {
        None => findings.push(Finding::reject(
            "spec.missing_open_questions",
            "spec front-matter must include an `open_questions` key (empty on a clean run)",
        )),
        Some(open) if !open.is_empty() => findings.push(Finding::reject(
            "spec.open_questions",
            format!(
                "spec has {} unresolved open question(s); clarify-first requires open_questions to be empty",
                open.len()
            ),
        )),
        Some(_) => {}
    }

    for heading in REQUIRED_SPEC_HEADINGS {
        if !body_has_heading(body, heading) {
            findings.push(Finding::reject(
                "spec.missing_heading",
                format!("spec body is missing the required heading {heading:?}"),
            ));
        }
    }
    // A title H1 is required.
    if !body.lines().any(|l| l.trim_start().starts_with("# ")) {
        findings.push(Finding::reject(
            "spec.missing_title",
            "spec body must open with a `# <title>` heading",
        ));
    }

    findings
}

/// Validates a `plan.md` document against the spec's requirement ids and the
/// set of repo paths known to the survey index.
///
/// `spec_requirement_ids` is the canonical id set from the spec front-matter;
/// the plan's `covers` must be a superset. `indexed_paths` is the set of paths
/// the survey index knows about — a `touches` entry outside it is a warning, not
/// a reject (the index may be stale or the path genuinely new). Pass an empty
/// slice to skip the path cross-check (e.g. when no index is present).
pub fn validate_plan(
    md: &str,
    spec_requirement_ids: &[String],
    indexed_paths: &[String],
) -> Vec<Finding> {
    let mut findings = Vec::new();
    let (yaml, body) = split_front_matter(md);
    let Some(yaml) = yaml else {
        findings.push(Finding::reject(
            "plan.no_front_matter",
            "plan.md is missing its YAML front-matter block",
        ));
        return findings;
    };
    let meta: PlanMeta = match serde_yaml::from_str(yaml) {
        Ok(meta) => meta,
        Err(error) => {
            findings.push(Finding::reject(
                "plan.bad_front_matter",
                format!("plan front-matter did not parse: {error}"),
            ));
            return findings;
        }
    };
    // The body must carry a real plan, not just front-matter: require a `# Plan`
    // heading and at least one non-empty content line beneath it. A
    // front-matter-only plan is a Reject so it triggers the repair pass.
    if !body_has_heading(body, "# Plan") {
        findings.push(Finding::reject(
            "plan.missing_heading",
            "plan body must include a `# Plan` heading",
        ));
    }
    let has_body_content = body
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with("# Plan"))
        .any(|l| !l.trim_start().starts_with('#') || l.trim_start().starts_with("##"));
    if !has_body_content {
        findings.push(Finding::reject(
            "plan.empty_body",
            "plan body has no steps beneath the `# Plan` heading",
        ));
    }
    if meta.schema != PLAN_SCHEMA {
        findings.push(Finding::warn(
            "plan.schema_mismatch",
            format!("expected schema {PLAN_SCHEMA:?}, found {:?}", meta.schema),
        ));
    }
    let uncovered: Vec<&String> = spec_requirement_ids
        .iter()
        .filter(|id| !meta.covers.iter().any(|c| c == *id))
        .collect();
    if !uncovered.is_empty() {
        let ids = uncovered
            .iter()
            .map(|id| id.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        findings.push(Finding::reject(
            "plan.covers_gap",
            format!("plan.covers does not cover spec requirement(s): {ids}"),
        ));
    }
    if !indexed_paths.is_empty() {
        for path in &meta.touches {
            if !indexed_paths.iter().any(|p| p == path) {
                findings.push(Finding::warn(
                    "plan.touches_unindexed",
                    format!("plan.touches path {path:?} is not in the survey index"),
                ));
            }
        }
    }
    findings
}

/// Validates a `tasks.md` document. `tasks.md` has NO front-matter and must stay
/// byte-compatible with `derrick-flow`'s `parse_tasks_from_markdown`: one `## `
/// H2 per ticket, an optional `<!-- complexity: low|standard|heavy -->` marker,
/// and (recommended) a reference to a requirement id `R<n>` in the body.
pub fn validate_tasks(md: &str) -> Vec<Finding> {
    let mut findings = Vec::new();
    if split_front_matter(md).0.is_some() {
        findings.push(Finding::reject(
            "tasks.has_front_matter",
            "tasks.md must NOT carry YAML front-matter (it would break ticket parsing)",
        ));
    }

    let mut h2_count = 0usize;
    let mut current_title: Option<String> = None;
    let mut current_body: Vec<&str> = Vec::new();
    let mut sections: Vec<(String, String)> = Vec::new();

    for line in md.lines() {
        let trimmed = line.trim();
        if let Some(title) = trimmed.strip_prefix("## ") {
            h2_count += 1;
            if let Some(prev) = current_title.take() {
                sections.push((prev, current_body.join("\n")));
            }
            current_title = Some(title.to_owned());
            current_body.clear();
        } else if current_title.is_some() {
            current_body.push(line);
        }
    }
    if let Some(prev) = current_title.take() {
        sections.push((prev, current_body.join("\n")));
    }

    if h2_count == 0 {
        findings.push(Finding::reject(
            "tasks.no_h2",
            "tasks.md must contain at least one `## ` ticket heading",
        ));
    }

    for (title, body) in &sections {
        // A complexity marker may appear in the title or body; if present, it
        // must use a valid level.
        if let Some(level) =
            extract_complexity_marker(title).or_else(|| extract_complexity_marker(body))
        {
            if !matches!(level.as_str(), "low" | "standard" | "heavy") {
                findings.push(Finding::reject(
                    "tasks.bad_complexity",
                    format!(
                        "ticket {title:?} has an invalid complexity marker {level:?} \
                         (expected low|standard|heavy)"
                    ),
                ));
            }
        }
        if !references_requirement(title) && !references_requirement(body) {
            findings.push(Finding::warn(
                "tasks.no_requirement_ref",
                format!("ticket {title:?} does not reference a requirement id (e.g. R1)"),
            ));
        }
    }

    findings
}

/// Extracts a `<!-- complexity: X -->` marker value (lowercased, trimmed) if
/// present in `text`.
fn extract_complexity_marker(text: &str) -> Option<String> {
    let start = text.find("<!-- complexity:")?;
    let after = &text[start + "<!-- complexity:".len()..];
    let end = after.find("-->")?;
    Some(after[..end].trim().to_lowercase())
}

/// True if `text` references a requirement id of the form `R<digits>`.
fn references_requirement(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'R' {
            // Word-boundary on the left.
            let left_ok = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if left_ok && j > i + 1 {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// True if `body` contains `heading` as a line (after trimming leading space).
fn body_has_heading(body: &str, heading: &str) -> bool {
    body.lines().any(|l| l.trim_start() == heading)
}

/// True if `md` already conforms to the spec schema closely enough to be
/// written through verbatim (a structural passthrough) rather than normalized
/// by a model.
///
/// Used by the `import` provider: a source document that already validates with
/// no [`Severity::Reject`] findings **and** carries the exact `derrick.spec/v1`
/// schema discriminator is a real spec and is imported as-is. A document with any
/// hard reject (no front-matter, missing requirements, leftover open questions,
/// …) — or a spec-shaped document declaring a *different* `schema:` value (a
/// `spec.schema_mismatch` Warn) — is sent through one model normalization pass
/// instead, so a foreign schema is never silently passed through.
pub fn looks_like_spec(md: &str) -> bool {
    let findings = validate_spec(md);
    !has_reject(&findings) && !findings.iter().any(|f| f.code == "spec.schema_mismatch")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_spec() -> String {
        // Neutral placeholder content only — no forbidden vocabulary.
        "---\n\
         schema: derrick.spec/v1\n\
         slug: widget-export\n\
         intent: Export widgets to a file.\n\
         requirements:\n\
         \x20\x20- id: R1\n\
         \x20\x20\x20\x20must: The system exports widgets as JSON.\n\
         acceptance:\n\
         \x20\x20- id: A1\n\
         \x20\x20\x20\x20check: A JSON file is produced.\n\
         non_goals: []\n\
         open_questions: []\n\
         grounding:\n\
         \x20\x20index_fresh: true\n\
         \x20\x20symbols:\n\
         \x20\x20\x20\x20- src/lib.rs:10 export_widget\n\
         ---\n\
         # Widget Export\n\n\
         ## Context\nWe need exports.\n\n\
         ## Requirements\n- R1: export as JSON\n\n\
         ## Acceptance Criteria\n- A1: file produced\n\n\
         ## Out of Scope\nNothing.\n"
            .to_owned()
    }

    #[test]
    fn valid_spec_has_no_rejects() {
        let findings = validate_spec(&valid_spec());
        assert!(!has_reject(&findings), "unexpected rejects: {findings:?}");
    }

    #[test]
    fn spec_missing_heading_rejects() {
        let spec = valid_spec().replace("## Out of Scope\nNothing.\n", "");
        let findings = validate_spec(&spec);
        assert!(has_reject(&findings));
        assert!(findings.iter().any(|f| f.code == "spec.missing_heading"));
    }

    #[test]
    fn spec_empty_acceptance_rejects() {
        let spec = valid_spec().replace(
            "acceptance:\n  - id: A1\n    check: A JSON file is produced.\n",
            "acceptance: []\n",
        );
        let findings = validate_spec(&spec);
        assert!(has_reject(&findings));
        assert!(findings.iter().any(|f| f.code == "spec.no_acceptance"));
    }

    #[test]
    fn spec_non_empty_open_questions_rejects() {
        let spec = valid_spec().replace(
            "open_questions: []\n",
            "open_questions:\n  - which format wins\n",
        );
        let findings = validate_spec(&spec);
        assert!(has_reject(&findings));
        assert!(findings.iter().any(|f| f.code == "spec.open_questions"));
    }

    #[test]
    fn spec_no_front_matter_rejects() {
        let findings = validate_spec("# Title\n\n## Context\nx\n");
        assert!(has_reject(&findings));
        assert!(findings.iter().any(|f| f.code == "spec.no_front_matter"));
    }

    #[test]
    fn spec_missing_open_questions_key_rejects() {
        // Drop the whole key — a missing key must be distinguished from `[]`.
        let spec = valid_spec().replace("open_questions: []\n", "");
        let findings = validate_spec(&spec);
        assert!(has_reject(&findings));
        assert!(
            findings
                .iter()
                .any(|f| f.code == "spec.missing_open_questions")
        );
    }

    #[test]
    fn spec_missing_non_goals_key_rejects() {
        let spec = valid_spec().replace("non_goals: []\n", "");
        let findings = validate_spec(&spec);
        assert!(has_reject(&findings));
        assert!(findings.iter().any(|f| f.code == "spec.missing_non_goals"));
    }

    fn valid_plan() -> &'static str {
        "---\n\
         schema: derrick.plan/v1\n\
         covers: [R1]\n\
         touches:\n\
         \x20\x20- src/lib.rs\n\
         ---\n\
         # Plan\nSteps go here.\n"
    }

    #[test]
    fn valid_plan_covers_requirements() {
        let findings = validate_plan(valid_plan(), &["R1".to_owned()], &["src/lib.rs".to_owned()]);
        assert!(!has_reject(&findings), "unexpected rejects: {findings:?}");
    }

    #[test]
    fn plan_covers_gap_rejects() {
        let findings = validate_plan(
            valid_plan(),
            &["R1".to_owned(), "R2".to_owned()],
            &["src/lib.rs".to_owned()],
        );
        assert!(has_reject(&findings));
        assert!(findings.iter().any(|f| f.code == "plan.covers_gap"));
    }

    #[test]
    fn plan_touches_unindexed_warns() {
        let findings = validate_plan(
            valid_plan(),
            &["R1".to_owned()],
            &["src/other.rs".to_owned()],
        );
        assert!(!has_reject(&findings));
        assert!(
            findings
                .iter()
                .any(|f| f.code == "plan.touches_unindexed" && f.severity == Severity::Warn)
        );
    }

    #[test]
    fn plan_front_matter_only_body_rejects() {
        // Front-matter is valid, but there is no `# Plan` body — must Reject so
        // the repair pass fires rather than silently accepting an empty plan.
        let md = "---\nschema: derrick.plan/v1\ncovers: [R1]\ntouches: []\n---\n";
        let findings = validate_plan(md, &["R1".to_owned()], &[]);
        assert!(has_reject(&findings));
        assert!(
            findings
                .iter()
                .any(|f| f.code == "plan.missing_heading" || f.code == "plan.empty_body")
        );
    }

    #[test]
    fn plan_heading_without_steps_rejects() {
        let md = "---\nschema: derrick.plan/v1\ncovers: [R1]\ntouches: []\n---\n# Plan\n";
        let findings = validate_plan(md, &["R1".to_owned()], &[]);
        assert!(has_reject(&findings));
        assert!(findings.iter().any(|f| f.code == "plan.empty_body"));
    }

    #[test]
    fn valid_tasks_parse() {
        let md = "## First ticket\n<!-- complexity: standard -->\nImplements R1.\n\n\
                  ## Second ticket\nDoes R1 follow-up.\n";
        let findings = validate_tasks(md);
        assert!(!has_reject(&findings), "unexpected rejects: {findings:?}");
    }

    #[test]
    fn tasks_no_h2_rejects() {
        let findings = validate_tasks("just some prose with no headings\n");
        assert!(has_reject(&findings));
        assert!(findings.iter().any(|f| f.code == "tasks.no_h2"));
    }

    #[test]
    fn tasks_bad_complexity_marker_rejects() {
        // Use a neutral bogus level — never a forbidden vocabulary word.
        let md = "## Ticket\n<!-- complexity: bogus -->\nImplements R1.\n";
        let findings = validate_tasks(md);
        assert!(has_reject(&findings));
        assert!(findings.iter().any(|f| f.code == "tasks.bad_complexity"));
    }

    #[test]
    fn tasks_missing_requirement_ref_warns() {
        let md = "## Ticket\nNo requirement reference here.\n";
        let findings = validate_tasks(md);
        assert!(!has_reject(&findings));
        assert!(
            findings
                .iter()
                .any(|f| f.code == "tasks.no_requirement_ref" && f.severity == Severity::Warn)
        );
    }

    #[test]
    fn tasks_with_front_matter_rejects() {
        let md = "---\nschema: x\n---\n## Ticket\nImplements R1.\n";
        let findings = validate_tasks(md);
        assert!(has_reject(&findings));
        assert!(findings.iter().any(|f| f.code == "tasks.has_front_matter"));
    }

    #[test]
    fn front_matter_split_round_trips() {
        let (yaml, body) = split_front_matter("---\nk: v\n---\nbody line\n");
        assert_eq!(yaml, Some("k: v\n"));
        assert_eq!(body, "body line\n");
    }

    #[test]
    fn references_requirement_needs_digits() {
        assert!(references_requirement("touches R1 and R12"));
        assert!(!references_requirement("Rust code, Refactor"));
        assert!(!references_requirement("plain prose"));
    }

    #[test]
    fn looks_like_spec_accepts_exact_schema() {
        assert!(looks_like_spec(&valid_spec()));
    }

    #[test]
    fn looks_like_spec_rejects_foreign_schema() {
        // A spec-shaped doc with the wrong schema value validates with only a
        // Warn (`spec.schema_mismatch`), but must NOT be treated as passthrough —
        // it should be normalized into derrick's schema instead.
        let spec = valid_spec().replace("schema: derrick.spec/v1", "schema: something-else/1");
        assert!(!has_reject(&validate_spec(&spec)), "only a Warn expected");
        assert!(
            !looks_like_spec(&spec),
            "foreign schema must not be passthrough"
        );
    }

    #[test]
    fn looks_like_spec_rejects_non_spec_document() {
        assert!(!looks_like_spec("# Just a heading\n\nsome prose\n"));
    }
}
