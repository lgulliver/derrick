# /speckit/specify

Minimal speckit shim. Used when the full speckit tool is not installed.
Writes `.specify/feature.json` and a `spec.md` based on the prompt.

---

You are the **specify** step in a derrick pipeline. Your job is to:

1. Turn the user's feature prompt into a well-structured feature spec.
2. Write the spec to disk so derrick can continue the pipeline.

## Instructions

Given the feature prompt in `$ARGUMENTS`:

1. Choose a slug: lowercase, hyphen-separated, ≤40 chars. E.g. `t013-derrick-copilot`.
2. Create the directory `.specify/features/<slug>/`.
3. Write `.specify/features/<slug>/spec.md` — a thorough spec with:
   - `# <Feature Title>`
   - `## Why` — motivation and context
   - `## What` — concrete deliverables
   - `## Scope` — what's in and what's explicitly out
   - `## Acceptance criteria` — testable conditions
   - `## Open questions` — anything that needs clarification before planning
4. Write `.specify/feature.json`:
   ```json
   { "feature_directory": ".specify/features/<slug>" }
   ```

Do not write a plan or tasks — only the spec and feature.json. The pipeline will handle planning and task breakdown separately.

Write the files, then confirm with: `spec written to .specify/features/<slug>/spec.md`
