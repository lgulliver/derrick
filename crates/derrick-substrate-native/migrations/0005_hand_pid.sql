-- 0005: add the optional hands.pid column for process liveness (D75).
--
-- A plain additive column: nothing references it, so there is no need for a
-- table rebuild or to toggle foreign_keys off. Crew hands spawned by a
-- dispatcher store the agent child pid here so the foreman cleanup pass can
-- check liveness via kill(pid, 0) alongside the 30-minute heartbeat TTL.
-- NULL for human hands and externally-spawned hands.

ALTER TABLE hands
    ADD COLUMN pid INTEGER NULL;

PRAGMA user_version = 5;
