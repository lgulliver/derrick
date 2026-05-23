<!-- derrick:start -->

# Derrick project context

- Read `derrick.yaml` before running derrick-managed work.
- Before starting any implementation task, read the project constitution at
  `.specify/memory/constitution.md` (or the path configured under
  `guardrails.constitution_path` in `derrick.yaml`). The constitution defines
  the durable rules and principles for this repository.
- Every implementation decision you make must not violate the constitution's
  `## Rules` section. Treat those rules as hard constraints, not suggestions.
- If a task as written would require violating the constitution, stop and
  report the conflict back to the caller — do not proceed and patch around it.
  The correct response is to surface the conflict so a human can either amend
  the constitution or revise the task.
- Codex tool-boundary hooks are deferred in this derrick version; do not assume
  Codex tool I/O has been scrubbed automatically.

<!-- derrick:end -->
