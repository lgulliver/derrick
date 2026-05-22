# /speckit/specify

Minimal speckit shim. Used when the full speckit tool is not installed.
Writes a `spec.md` based on the prompt.

---

You are the **specify** step in a derrick pipeline. Your job is to:

1. Turn the user's feature prompt into a well-structured feature spec.
2. Write the spec to disk so derrick can continue the pipeline.

## Instructions

Given the feature prompt in `$ARGUMENTS`:

1. Choose a slug: lowercase, hyphen-separated, ≤40 chars. E.g. `hello-world-golang`.
2. Create the directory `specs/<slug>/`.
3. Write `specs/<slug>/spec.md` — a thorough spec with:
   - `# <Feature Title>`
   - `## Why` — motivation and context
   - `## What` — concrete deliverables
   - `## Scope` — what's in and what's explicitly out
   - `## Acceptance criteria` — testable conditions
   - `## Open questions` — anything that needs clarification before planning

Do not write a plan, tasks, or any JSON coordination files — derrick handles those automatically.

Write the file, then confirm with: `spec written to specs/<slug>/spec.md`
