-- Schema v2: add a key-value metadata table for index freshness tracking.
-- The only key written by derrick-survey is "last_build_ts" (Unix seconds).

CREATE TABLE IF NOT EXISTS meta (
    key    TEXT PRIMARY KEY,
    value  TEXT NOT NULL
);

PRAGMA user_version = 2;
