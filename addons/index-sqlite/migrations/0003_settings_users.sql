-- Server state besides the archive (ADR-0004 §3). As for jobs, each row is a JSON `body`; the
-- other columns exist for keys, lookups and pruning. Times are Unix milliseconds.

-- Settings revisions are append-only; the latest one is in effect.
CREATE TABLE settings (
    revision   INTEGER PRIMARY KEY,
    created_at INTEGER NOT NULL,
    body       TEXT NOT NULL
);

CREATE TABLE users (
    name TEXT PRIMARY KEY,
    body TEXT NOT NULL
);

-- Sessions and API tokens; only the hash of the secret is stored.
CREATE TABLE credentials (
    id          TEXT PRIMARY KEY,
    user_name   TEXT NOT NULL REFERENCES users (name) ON DELETE CASCADE,
    secret_hash TEXT NOT NULL UNIQUE,
    created_at  INTEGER NOT NULL,
    expires_at  INTEGER,
    body        TEXT NOT NULL
);
CREATE INDEX credentials_user ON credentials (user_name, created_at);
CREATE INDEX credentials_expiry ON credentials (expires_at);
