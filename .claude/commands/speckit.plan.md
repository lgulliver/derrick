# /speckit/plan

Read `.specify/feature.json` to find the feature directory, then read `spec.md`
and the project constitution before writing the plan.

---

## Instructions

1. **Read the constitution first.**
   - Read `derrick.yaml` and find `guardrails.constitution_path` (default: `.specify/memory/constitution.md`).
   - Read that file. The `## Rules` section contains hard constraints that every implementation step must respect.

2. **Read the spec** from the feature directory.

3. **Reconcile before planning.** If the spec and the constitution conflict (e.g. spec says "no tests" but constitution requires 80% coverage), **side with the constitution** and note the tension in the plan's `## Risks` section. Do not silently adopt the weaker constraint.

4. **Write `plan.md`** in the same feature directory with:
   - `# Plan: <Feature Title>`
   - `## Approach` — high-level implementation strategy that satisfies both spec and constitution
   - `## Steps` — ordered list of concrete implementation steps. Each step must be compatible with the constitution's Rules (e.g. use zerolog instead of fmt.Println if the constitution requires it; include test steps if the constitution mandates coverage).
   - `## Risks` — what could go wrong, mitigations, and any spec/constitution tensions surfaced here
   - `## Dependencies` — other tickets or crates this depends on

Confirm with: `plan written to <feature_dir>/plan.md`
