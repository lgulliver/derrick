# /speckit/specify

Minimal speckit shim. Used when the full speckit tool is not installed.
Writes a `spec.md` based on the prompt, constrained by the project constitution.

---

You are the **specify** step in a derrick pipeline. Your job is to:

1. Read the project constitution so the spec never bakes in contradictions.
2. Turn the user's feature prompt into a well-structured feature spec that is compatible with the constitution.
3. Write the spec to disk so derrick can continue the pipeline.

## Instructions

Given the feature prompt in `$ARGUMENTS`:

1. **Read the constitution first.**
   - Read `derrick.yaml` and find `guardrails.constitution_path` (default: `.specify/memory/constitution.md`).
   - Read that file. The `## Rules` section contains hard constraints — no spec may write scope or acceptance criteria that contradict them.

2. **Choose a slug**: lowercase, hyphen-separated, ≤40 chars. E.g. `hello-world-golang`.

3. **Create the directory** `specs/<slug>/`.

4. **Write `specs/<slug>/spec.md`** — a thorough spec with:
   - `# <Feature Title>`
   - `## Why` — motivation and context
   - `## What` — concrete deliverables
   - `## Scope` — what's in and what's explicitly out. Do **not** exclude anything the constitution's `## Rules` requires (e.g. if the constitution mandates 80% test coverage, do not write "No tests" in scope-out).
   - `## Acceptance criteria` — testable conditions. Must include criteria that satisfy the constitution's reviewer checklist, even if the user's prompt did not mention them.
   - `## Open questions` — anything that needs clarification **plus** any genuine tension between the user's prompt and the constitution (e.g. "The prompt asks for no external dependencies but the constitution requires zerolog — should we resolve this by treating zerolog as an approved dependency?"). Surface conflicts here rather than silently ignoring the constitution.

Do not write a plan, tasks, or any JSON coordination files — derrick handles those automatically.

Write the file, then confirm with: `spec written to specs/<slug>/spec.md`
