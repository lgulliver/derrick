# /speckit/analyze

Read `.specify/feature.json`, then `spec.md` and `plan.md` from the feature directory.

Analyze both documents together and write an `analysis.md` in the same directory covering:

- `# Analysis: <Feature Title>`
- `## Spec coverage` — does the plan fully address the spec?
- `## Gaps` — anything in the spec not addressed by the plan
- `## Concerns` — implementation risks not yet mitigated
- `## Recommendation` — proceed / revise plan / revise spec

Confirm with: `analysis written to <feature_dir>/analysis.md`
