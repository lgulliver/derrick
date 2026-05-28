-- derrick-survey index schema v1 (DESIGN.md §9.B.8, D54/D55).
-- Owns .derrick/index.db, distinct from the substrate DB.

CREATE TABLE files (
    id            INTEGER PRIMARY KEY,
    path          TEXT NOT NULL UNIQUE,
    lang          TEXT NOT NULL,
    size          INTEGER NOT NULL,
    mtime         INTEGER NOT NULL,
    content_hash  TEXT NOT NULL
);

CREATE TABLE symbols (
    id          INTEGER PRIMARY KEY,
    file_id     INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    name        TEXT NOT NULL,
    kind        TEXT NOT NULL,
    start_line  INTEGER NOT NULL,
    end_line    INTEGER NOT NULL,
    signature   TEXT
);

-- A reference edge from one symbol to another. dst_symbol_id is nullable:
-- unresolved references (calls into std / extern crates / unindexed symbols)
-- keep their textual target in dst_name so impact queries still see them.
--
-- src_symbol_id CASCADEs: reparsing a file deletes its symbols and their
-- outgoing edges, which we then re-insert. dst_symbol_id is SET NULL instead,
-- so deleting a symbol does not destroy inbound edges from files we did not
-- reparse; a post-build resolution pass repopulates dst_symbol_id by name.
CREATE TABLE refs (
    src_symbol_id  INTEGER NOT NULL REFERENCES symbols(id) ON DELETE CASCADE,
    dst_symbol_id  INTEGER REFERENCES symbols(id) ON DELETE SET NULL,
    dst_name       TEXT NOT NULL,
    kind           TEXT NOT NULL
);

CREATE INDEX idx_symbols_name ON symbols(name);
CREATE INDEX idx_symbols_file ON symbols(file_id);
CREATE INDEX idx_refs_src ON refs(src_symbol_id);
CREATE INDEX idx_refs_dst ON refs(dst_symbol_id);
CREATE INDEX idx_refs_dst_name ON refs(dst_name);

-- External-content FTS5 over symbol name + signature. Kept in sync explicitly
-- by the writer (delete-by-file then re-insert), so no triggers are needed.
CREATE VIRTUAL TABLE symbols_fts USING fts5(
    name,
    signature,
    content = 'symbols',
    content_rowid = 'id'
);

PRAGMA user_version = 1;
