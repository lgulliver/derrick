# /speckit/tasks

Read `.specify/feature.json`, then `spec.md`, `plan.md`, and `analysis.md` (if present)
from the feature directory.

Write a `tasks.md` in the same directory — a flat, ordered list of implementation tasks
ready for ticket dispatch:

- `# Tasks: <Feature Title>`
- One `## Task N: <title> <!-- complexity: low|standard|heavy -->` section per
  task. End every task heading with a `<!-- complexity: ... -->` marker that
  estimates the task's size/complexity (`low` for small mechanical changes,
  `standard` for ordinary work, `heavy` for large or intricate work). The
  foreman uses this to pick the best model per ticket (D67). Each task contains:
  - **Crate**: which crate owns this task
  - **Depends on**: other task numbers (if any)
  - **What**: 2–4 sentence description
  - **Done when**: concrete, testable acceptance condition

Keep tasks small enough that each is a single coherent unit of work (roughly
one crate, one PR). Aim for 3–8 tasks total.

Confirm with: `tasks written to <feature_dir>/tasks.md`
