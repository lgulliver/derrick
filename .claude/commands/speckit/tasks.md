# /speckit/tasks

Read `.specify/feature.json`, then `spec.md`, `plan.md`, and `analysis.md` (if present)
from the feature directory.

Write a `tasks.md` in the same directory — a flat, ordered list of implementation tasks
ready for ticket dispatch:

- `# Tasks: <Feature Title>`
- One `## Task N: <title>` section per task, each containing:
  - **Crate**: which crate owns this task
  - **Depends on**: other task numbers (if any)
  - **What**: 2–4 sentence description
  - **Done when**: concrete, testable acceptance condition

Keep tasks small enough that each is a single coherent unit of work (roughly
one crate, one PR). Aim for 3–8 tasks total.

Confirm with: `tasks written to <feature_dir>/tasks.md`
