PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

CREATE TABLE site (
    name TEXT NOT NULL,
    prefix TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (name)
);

CREATE TABLE batches (
    name TEXT PRIMARY KEY,
    created_at TEXT NOT NULL,
    closed_at TEXT NULL
);

CREATE TABLE hands (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    last_seen TEXT NULL,
    CHECK (kind IN ('claude', 'copilot', 'human'))
);

CREATE TABLE tickets (
    id TEXT PRIMARY KEY,
    batch TEXT NULL REFERENCES batches(name),
    ordinal INTEGER NULL,
    title TEXT NOT NULL,
    body TEXT NOT NULL DEFAULT '',
    state TEXT NOT NULL,
    owner TEXT NULL REFERENCES hands(id),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    CHECK (state IN ('ready', 'in_flight', 'blocked', 'done', 'rejected')),
    CHECK (ordinal IS NULL OR batch IS NOT NULL)
);

CREATE TABLE ticket_labels (
    ticket_id TEXT NOT NULL REFERENCES tickets(id) ON DELETE CASCADE,
    label TEXT NOT NULL,
    PRIMARY KEY (ticket_id, label)
);

CREATE TABLE links (
    from_ticket TEXT NOT NULL REFERENCES tickets(id) ON DELETE CASCADE,
    to_ticket   TEXT NOT NULL REFERENCES tickets(id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    PRIMARY KEY (from_ticket, to_ticket, kind),
    CHECK (kind IN ('blocks', 'related'))
);

CREATE TABLE events (
    id BLOB PRIMARY KEY,
    at TEXT NOT NULL,
    kind TEXT NOT NULL,
    ticket TEXT NULL REFERENCES tickets(id) ON DELETE SET NULL,
    body TEXT NOT NULL DEFAULT ''
);

CREATE TABLE foreman (
    site TEXT PRIMARY KEY REFERENCES site(name) ON DELETE CASCADE,
    pid INTEGER NULL,
    started_at TEXT NULL
);

CREATE TABLE worktrees (
    run_id TEXT PRIMARY KEY,
    branch TEXT NOT NULL,
    path TEXT NOT NULL,
    created_at TEXT NOT NULL,
    closed_at TEXT NULL
);

CREATE INDEX idx_tickets_state ON tickets(state);
CREATE INDEX idx_tickets_batch_ordinal ON tickets(batch, ordinal);
CREATE INDEX idx_tickets_owner ON tickets(owner);
CREATE INDEX idx_ticket_labels_label ON ticket_labels(label, ticket_id);
CREATE INDEX idx_links_to ON links(to_ticket);
CREATE INDEX idx_events_at ON events(at);
CREATE INDEX idx_events_ticket ON events(ticket);
CREATE INDEX idx_worktrees_branch ON worktrees(branch);

PRAGMA user_version = 1;
