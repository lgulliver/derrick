-- 0003: expand the hands.kind CHECK constraint to admit host-CLI
-- crew executors (D66).
--
-- v1/v2 restricted `kind` to ('claude','copilot','human'). D66 adds
-- codex/opencode/aider as first-class crew executor hands, so the
-- CHECK must be widened to keep `HandKind::FromStr` and the column
-- constraint in lockstep.
--
-- SQLite cannot ALTER a CHECK constraint in place, so we rebuild the
-- table. Must be run by a migration runner that has already disabled
-- foreign_keys outside the transaction (see migrate() in lib.rs): the
-- rebuild relies on FKs being off so DROP TABLE hands succeeds while
-- tickets.owner and events.scope_hand still reference hands(id). Those
-- rows survive and re-bind by name when hands_new is renamed into place.

CREATE TABLE hands_new (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    last_seen TEXT NULL,
    CHECK (kind IN ('claude', 'copilot', 'codex', 'opencode', 'aider', 'human'))
);

INSERT INTO hands_new (id, kind, last_seen)
SELECT id, kind, last_seen FROM hands;

DROP TABLE hands;
ALTER TABLE hands_new RENAME TO hands;

PRAGMA user_version = 3;
