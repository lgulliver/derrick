# dark-factory

> A production line that runs lights-out — no humans on the floor, work
> flowing from intake to shipment by itself.

`dark-factory` is a unified layer over [speckit](https://github.com/github/spec-kit),
[courtroom](https://github.com/lgulliver/courtroom), and
[gastown](https://github.com/lgulliver/gastown). One install, one config,
one command (`/add-feature`).

**Status:** pre-implementation. See [DESIGN.md](./DESIGN.md) for the
architecture and pipeline contract before any code lands.

## What it does

```
$ curl -fsSL https://dark-factory.dev/install | bash    # one-time
$ cd ~/repos/my-project && df init                       # one-time per repo
$ # then in Claude Code:
$ /add-feature build a webhook ingest endpoint with idempotent dedupe
```

That single slash command runs the full pipeline:

```
spec → clarify → plan → checkpoint → courtroom → analyze →
tasks → tasks-to-beads → gt prime --role mayor
```

Each step is configurable in your repo's `dark-factory.yaml`. Steps can
be skipped per-invocation (`--no-clarify`, `--no-checkpoint`,
`--dry-run`) or removed from your pipeline entirely.

## Read next

- [DESIGN.md](./DESIGN.md) — the full architecture, pipeline schema,
  install flow, and open questions.
