-- 0004: add the optional ticket complexity column driving the foreman's
-- adaptive model selection (D67).
--
-- A plain additive column: nothing references it, so there is no need for a
-- table rebuild or to toggle foreign_keys off. The CHECK keeps the column in
-- lockstep with `Complexity::FromStr` (low/standard/heavy or NULL).

ALTER TABLE tickets
    ADD COLUMN complexity TEXT NULL
    CHECK (complexity IS NULL OR complexity IN ('low', 'standard', 'heavy'));

PRAGMA user_version = 4;
