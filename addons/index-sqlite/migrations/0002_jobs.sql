-- Persisted outbound jobs (PLAN 3.5). The job itself is the JSON `body`; the other columns
-- exist for scheduling and listing. Times are Unix milliseconds.
CREATE TABLE jobs (
    id         TEXT PRIMARY KEY,
    state      TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    not_before INTEGER NOT NULL,
    body       TEXT NOT NULL
);
CREATE INDEX jobs_due ON jobs (state, not_before);
CREATE INDEX jobs_created ON jobs (created_at);
