# Spec providers — speckit, native, import

> Design decision: D85 (DESIGN.md §12), refining D2/D3. Pipeline: §4 / §5.3.

derrick's pipeline turns a feature prompt into a **spec → plan → tasks** trio of
files under `specs/<NNN>-<slug>/` that the rest of the pipeline (`clarify`,
`assay`, `bridge`) consumes. How those files get *produced* is now selectable:
the `specify`/`plan`/`tasks` steps route through a **spec provider**.

All three providers write the **same artifacts** (`spec.md`, `plan.md`,
`tasks.md` + `.specify/feature.json`), so nothing downstream changes when you
switch providers.

| Provider | What it does | When to use |
|---|---|---|
| `speckit` (default) | Shells the `/speckit.*` slash-commands to your host CLI — the original behaviour. | You already use speckit; zero change. |
| `native` | derrick generates the spec itself, in-process: survey-grounded, clarify-first, schema-validated. | You want generation without speckit, grounded in your real code and token-efficient. |
| `import` | Bring your own spec/PRD from a local file; derrick normalises it into the schema. | You already wrote a spec/PRD and want derrick to plan and decompose it. |

The default is `speckit` — existing repos and a fresh `derrick init` behave
exactly as before unless you opt in.

---

## Selecting a provider

In `derrick.yaml`:

```yaml
tools:
  specify:
    provider: speckit          # speckit (default) | native | import
    import:
      source: docs/PRD.md      # required for import; local file path (v1)
      plan: native             # native (default) | speckit | import
      tasks: native            # native (default) | speckit | import
```

`derrick init` asks which provider to use and writes the right shape:

- **speckit** → explicit `host:`+`command: "/speckit.specify …"` steps (self-documenting).
- **native** → `provider: native` and *bare* `specify`/`plan`/`tasks` steps (no `host`/`command`), which the seam routes to the native generator.
- **import** → `provider: import` with a commented `source:` stub to fill in.

**Back-compat:** the provider is consulted only for a *bare* spec step. A step
that pins its own `host:`+`command:` (the explicit speckit form) always runs
verbatim, so hand-tuned pipelines are never reinterpreted. `CONFIG_VERSION` is
unchanged.

---

## `native` — derrick-owned generation

The native generator (the `derrick-specify` crate) produces the trio in-process
through your configured host model, with three accuracy levers:

1. **Survey-grounding.** Before drafting, derrick queries the
   [survey index](./survey.md) for the symbols/files relevant to your prompt and
   writes them into the spec's `grounding:` front-matter **itself** — the model
   is told to reference only those, so it can't invent symbol or path names. With
   no index present it degrades gracefully (drafts behaviourally, no fabricated
   names).
2. **Clarify-first.** The interactive clarify Q&A runs *before* the spec is
   drafted (not after), so ambiguity is resolved up front instead of triggering a
   re-draft. Non-interactive runs auto-accept the recommended answers.
3. **Schema validation.** `spec.md`/`plan.md` carry YAML front-matter
   (`intent`, `requirements`, `acceptance`, `covers`, …) plus required headings.
   derrick validates each artifact deterministically (no model call) and, on a
   hard rejection, runs **one** bounded repair pass before failing the step.

Token efficiency comes from clarify-first (avoids a re-spec loop), survey
grounding (replaces model-side `grep`/`glob` fan-out), deterministic validation
(replaces a `/speckit.analyze` model pass), and roughneck/caveman/prompt-caching
on the model calls.

**Scope note:** the native path covers **spec → plan → tasks**. The optional
`analyze` step is not part of the seam — a default pipeline keeps it as a
`/speckit.analyze` step, so a fully speckit-free setup should drop or replace
`analyze`.

---

## `import` — bring your own spec

Point derrick at a spec/PRD you already have:

```bash
# one-off, for a single drill run (highest precedence, no config edit):
derrick drill "…" --spec docs/PRD.md

# or normalise a source into specs/<NNN>-<slug>/spec.md and stop:
derrick spec import docs/PRD.md
```

derrick reads the source and either **passes it through** (if it already matches
the spec schema exactly) or **normalises** it with a single model call into the
schema, then validates it. Downstream `plan`/`tasks` are produced per
`import.plan` / `import.tasks` (default `native`).

**v1 supports a local file path only.** Remote sources (a GitHub issue, a Notion
or Confluence page) return a clear "not supported yet" error — derrick's own
process can't call agent-side MCP tools, so export the document to a local file
for now. (`file:` and `file:///abs/path` are accepted; `file://<authority>/…` is
rejected.) Adding a remote-source integration would need IT approval under the
AI policy.

`--spec` cannot be combined with resuming an existing run — start a fresh drill.

---

## Checking your setup

`derrick doctor` reports the active provider (`spec provider: …`) and scopes the
speckit-on-PATH check accordingly: it only checks for speckit when the provider
is `speckit` (or a step explicitly pins `/speckit.*`). Under `native`/`import`, a
missing speckit binary is not a problem. For `native` it checks the spec roles
resolve to a model; for `import` it validates the `source`.
