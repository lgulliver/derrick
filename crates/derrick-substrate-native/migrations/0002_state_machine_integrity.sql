-- 0002: D31/D32 state machine integrity columns.
--
-- Must be run by a migration runner that has already disabled
-- foreign_keys outside the transaction (see migrate() in lib.rs).
-- The table-rebuild relies on FKs being off so DROP TABLE tickets
-- succeeds while ticket_labels, links, events, and owner columns
-- still reference it; rows in those tables survive and re-bind by
-- name when the new tickets table is renamed into place.

CREATE TABLE tickets_new (
    id TEXT PRIMARY KEY,
    batch TEXT NULL REFERENCES batches(name),
    ordinal INTEGER NULL,
    title TEXT NOT NULL,
    body TEXT NOT NULL DEFAULT '',
    state TEXT NOT NULL,
    owner TEXT NULL REFERENCES hands(id),
    merge_sha TEXT NULL,
    block_reason TEXT NULL,
    block_reason_detail TEXT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    CHECK (state IN ('ready', 'in_flight', 'in_review',
                     'blocked', 'done', 'rejected')),
    CHECK (block_reason IS NULL OR
           block_reason IN ('dependency', 'pr_closed_unmerged',
                            'restack_conflict', 'human')),
    CHECK ((state = 'blocked') = (block_reason IS NOT NULL)),
    CHECK (ordinal IS NULL OR batch IS NOT NULL)
);

INSERT INTO tickets_new
  (id, batch, ordinal, title, body, state, owner,
   merge_sha, block_reason, block_reason_detail,
   created_at, updated_at)
SELECT
  id, batch, ordinal, title, body, state, owner,
  NULL,
  CASE WHEN state = 'blocked' THEN 'human' ELSE NULL END,
  CASE WHEN state = 'blocked'
       THEN json_object('kind','human','note','migrated from v1')
       ELSE NULL END,
  created_at, updated_at
FROM tickets;

DROP TABLE tickets;
ALTER TABLE tickets_new RENAME TO tickets;

CREATE INDEX idx_tickets_state ON tickets(state);
CREATE INDEX idx_tickets_batch_ordinal ON tickets(batch, ordinal);
CREATE INDEX idx_tickets_owner ON tickets(owner);

ALTER TABLE foreman ADD COLUMN mode TEXT NOT NULL DEFAULT 'stopped'
  CHECK (mode IN ('stopped', 'detached', 'attached'));

ALTER TABLE events ADD COLUMN scope_kind TEXT NOT NULL
    DEFAULT 'site'
    CHECK (scope_kind IN ('ticket','batch','hand','worktree','site'));
ALTER TABLE events ADD COLUMN scope_batch TEXT NULL
    REFERENCES batches(name);
ALTER TABLE events ADD COLUMN scope_hand TEXT NULL
    REFERENCES hands(id);
ALTER TABLE events ADD COLUMN scope_run_id TEXT NULL;

UPDATE events SET scope_kind = 'ticket' WHERE ticket IS NOT NULL;

CREATE INDEX idx_events_scope_kind ON events(scope_kind);
CREATE INDEX idx_events_scope_batch ON events(scope_batch);
CREATE INDEX idx_events_scope_hand  ON events(scope_hand);

PRAGMA user_version = 2;
