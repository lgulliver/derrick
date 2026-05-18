# /speckit/plan

Read `.specify/feature.json` to find the feature directory, then read `spec.md`.

Write a `plan.md` in the same directory with:

- `# Plan: <Feature Title>`
- `## Approach` — high-level implementation strategy
- `## Steps` — ordered list of concrete implementation steps with crate/file ownership
- `## Risks` — what could go wrong and mitigations
- `## Dependencies` — other tickets or crates this depends on

Confirm with: `plan written to <feature_dir>/plan.md`
