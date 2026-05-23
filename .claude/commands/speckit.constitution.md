# /speckit.constitution

Minimal speckit shim. Generates a project constitution from a free-form
description. Used when the full speckit tool is not installed.

---

You are authoring a project **constitution** for a derrick-managed repository.
The constitution captures the durable rules, principles, and constraints that
all plan / implementation work must respect. The plan reviewer reads it on every
run.

## Input

`$ARGUMENTS` contains the user's free-form description of their project, its
key rules, constraints, and principles. Treat it as authoritative source
material — do not invent rules that are not implied by the description.

## Instructions

1. Read `derrick.yaml` to find `guardrails.constitution_path`. If unset, fall
   back to `.specify/memory/constitution.md`. This is the **target path**.
2. Create any missing parent directories for the target path.
3. Write the constitution to the target path with the following sections (in
   order):

   ```
   # Constitution

   ## Principles
   <2–6 high-level beliefs that drive decisions, derived from $ARGUMENTS>

   ## Rules
   <concrete, testable rules. Each rule is a single sentence in the form
   "X must Y" or "X must not Y". These are what the plan reviewer enforces.>

   ## Out of scope
   <things the project explicitly does not do, or boundaries it will not cross>

   ## Reviewer checklist
   <a short checklist a human or LLM reviewer can run through to confirm a plan
   honours this constitution>
   ```

4. Keep it concise — aim for under 200 lines. Prefer specific rules over vague
   aspirations. Do **not** include the `DERRICK-DRAFT` banner; this is a real
   user-authored constitution, not a stub.
5. Do not write any other files. Do not write a plan, tasks, or spec.

After writing the file, confirm with exactly:

`constitution written to <target path>`
