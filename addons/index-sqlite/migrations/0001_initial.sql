-- medicus index schema, version 1.
-- Every hierarchy row carries its partition (ADR-0003); UIDs are unique per partition only, and
-- every lookup and query is restricted to one partition.
-- Column names follow addons/index-sql-common/src/schema.rs. Times are microseconds since the
-- Unix epoch (UTC). DA columns hold YYYYMMDD; *_time_key columns hold sortable HHMMSSffffff.
-- `extra` columns hold the values of configured extra tags (see extra_tags) in SQLite's binary
-- JSONB format (BLOB, SQLite >= 3.45): parsed once on write instead of on every query.

CREATE TABLE patients (
    id                  INTEGER PRIMARY KEY,
    partition           TEXT NOT NULL,
    patient_id          TEXT NOT NULL,
    issuer              TEXT NOT NULL,
    patient_name        TEXT,
    patient_name_folded TEXT,
    birth_date          TEXT,
    sex                 TEXT,
    extra               BLOB,
    UNIQUE (partition, patient_id, issuer)
);
CREATE INDEX patients_name ON patients (partition, patient_name_folded);

CREATE TABLE studies (
    id               INTEGER PRIMARY KEY,
    partition        TEXT NOT NULL,
    patient_fk       INTEGER NOT NULL REFERENCES patients (id),
    study_uid        TEXT NOT NULL,
    study_date       TEXT,
    study_time       TEXT,
    study_time_key   TEXT,
    accession        TEXT,
    study_id         TEXT,
    referring        TEXT,
    referring_folded TEXT,
    description      TEXT,
    institution      TEXT,
    received_at      INTEGER NOT NULL,
    updated_at       INTEGER NOT NULL,
    last_accessed_at INTEGER,
    extra            BLOB,
    UNIQUE (partition, study_uid)
);
CREATE INDEX studies_patient ON studies (patient_fk);
CREATE INDEX studies_date ON studies (partition, study_date);
CREATE INDEX studies_accession ON studies (partition, accession);
CREATE INDEX studies_received ON studies (partition, received_at);

CREATE TABLE series (
    id              INTEGER PRIMARY KEY,
    partition       TEXT NOT NULL,
    study_fk        INTEGER NOT NULL REFERENCES studies (id),
    series_uid      TEXT NOT NULL,
    modality        TEXT,
    series_number   INTEGER,
    description     TEXT,
    series_date     TEXT,
    series_time     TEXT,
    series_time_key TEXT,
    body_part       TEXT,
    station         TEXT,
    pps_start_date  TEXT,
    updated_at      INTEGER NOT NULL,
    extra           BLOB,
    UNIQUE (partition, series_uid)
);
-- Counts and ModalitiesInStudy are computed by correlated subqueries; these indexes keep them
-- index range scans.
CREATE INDEX series_study ON series (study_fk, modality);

CREATE TABLE instances (
    id               INTEGER PRIMARY KEY,
    partition        TEXT NOT NULL,
    series_fk        INTEGER NOT NULL REFERENCES series (id),
    sop_uid          TEXT NOT NULL,
    sop_class        TEXT NOT NULL,
    instance_number  INTEGER,
    content_date     TEXT,
    content_time     TEXT,
    content_time_key TEXT,
    num_frames       INTEGER,
    transfer_syntax  TEXT NOT NULL,
    size             INTEGER NOT NULL,
    blob_store       TEXT NOT NULL,
    blob_key         TEXT NOT NULL,
    header_length    INTEGER NOT NULL,
    received_at      INTEGER NOT NULL,
    extra            BLOB,
    UNIQUE (partition, sop_uid)
);
CREATE INDEX instances_series ON instances (series_fk, sop_class);

-- Operator-configured extra indexed tags. Values live in the `extra` JSON column of the level's
-- table as {"GGGGEEEE": [match-form values]}; state is 'ready' or 'backfilling'.
CREATE TABLE extra_tags (
    tag   TEXT PRIMARY KEY,
    vr    TEXT NOT NULL,
    level TEXT NOT NULL,
    state TEXT NOT NULL
);

-- The transactional outbox (PLAN 3.7). AUTOINCREMENT: sequence numbers are never reused.
CREATE TABLE events (
    seq       INTEGER PRIMARY KEY AUTOINCREMENT,
    id        TEXT NOT NULL,
    kind      TEXT NOT NULL,
    partition TEXT,
    time      INTEGER NOT NULL,
    body      TEXT NOT NULL
);

CREATE TABLE cursors (
    subscriber TEXT PRIMARY KEY,
    seq        INTEGER NOT NULL
);
