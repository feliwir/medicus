-- medicus index schema, version 1.
-- Column names follow addons/index-sql-common/src/schema.rs. Times are microseconds since the
-- Unix epoch (UTC). DA columns hold YYYYMMDD; *_time_key columns hold sortable HHMMSSffffff.

CREATE TABLE patients (
    id                  INTEGER PRIMARY KEY,
    patient_id          TEXT NOT NULL,
    issuer              TEXT NOT NULL,
    patient_name        TEXT,
    patient_name_folded TEXT,
    birth_date          TEXT,
    sex                 TEXT,
    UNIQUE (patient_id, issuer)
);
CREATE INDEX patients_name ON patients (patient_name_folded);

CREATE TABLE studies (
    id               INTEGER PRIMARY KEY,
    patient_fk       INTEGER NOT NULL REFERENCES patients (id),
    study_uid        TEXT NOT NULL UNIQUE,
    study_date       TEXT,
    study_time       TEXT,
    study_time_key   TEXT,
    accession        TEXT,
    study_id         TEXT,
    referring        TEXT,
    referring_folded TEXT,
    description      TEXT,
    institution      TEXT,
    num_series       INTEGER NOT NULL DEFAULT 0,
    num_instances    INTEGER NOT NULL DEFAULT 0,
    size_bytes       INTEGER NOT NULL DEFAULT 0,
    received_at      INTEGER NOT NULL,
    updated_at       INTEGER NOT NULL,
    last_accessed_at INTEGER
);
CREATE INDEX studies_patient ON studies (patient_fk);
CREATE INDEX studies_date ON studies (study_date);
CREATE INDEX studies_accession ON studies (accession);
CREATE INDEX studies_received ON studies (received_at);

CREATE TABLE study_modalities (
    study_fk INTEGER NOT NULL REFERENCES studies (id) ON DELETE CASCADE,
    modality TEXT NOT NULL,
    PRIMARY KEY (study_fk, modality)
);
CREATE INDEX study_modalities_modality ON study_modalities (modality);

CREATE TABLE study_sop_classes (
    study_fk  INTEGER NOT NULL REFERENCES studies (id) ON DELETE CASCADE,
    sop_class TEXT NOT NULL,
    PRIMARY KEY (study_fk, sop_class)
);

CREATE TABLE series (
    id              INTEGER PRIMARY KEY,
    study_fk        INTEGER NOT NULL REFERENCES studies (id),
    series_uid      TEXT NOT NULL UNIQUE,
    modality        TEXT,
    series_number   INTEGER,
    description     TEXT,
    series_date     TEXT,
    series_time     TEXT,
    series_time_key TEXT,
    body_part       TEXT,
    station         TEXT,
    pps_start_date  TEXT,
    num_instances   INTEGER NOT NULL DEFAULT 0,
    size_bytes      INTEGER NOT NULL DEFAULT 0,
    updated_at      INTEGER NOT NULL
);
CREATE INDEX series_study ON series (study_fk);
CREATE INDEX series_modality ON series (modality);

CREATE TABLE instances (
    id               INTEGER PRIMARY KEY,
    series_fk        INTEGER NOT NULL REFERENCES series (id),
    sop_uid          TEXT NOT NULL UNIQUE,
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
    meta_store       TEXT,
    meta_key         TEXT,
    meta_size        INTEGER NOT NULL DEFAULT 0,
    received_at      INTEGER NOT NULL
);
CREATE INDEX instances_series ON instances (series_fk);

-- The transactional outbox (PLAN 3.7). AUTOINCREMENT: sequence numbers are never reused.
CREATE TABLE events (
    seq  INTEGER PRIMARY KEY AUTOINCREMENT,
    id   TEXT NOT NULL,
    kind TEXT NOT NULL,
    time INTEGER NOT NULL,
    body TEXT NOT NULL
);

CREATE TABLE cursors (
    subscriber TEXT PRIMARY KEY,
    seq        INTEGER NOT NULL
);
