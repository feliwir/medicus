-- medicus index schema, version 1 (PostgreSQL). Mirrors addons/index-sqlite/migrations; see there
-- for the meaning of the columns.
-- All integers are BIGINT: the shared code decodes every integer column as i64.
-- Text is COLLATE "C" (byte order, as SQLite's BINARY): DICOM matching, range matching and
-- ordering must not depend on the database's locale, and prefix LIKE can then use plain indexes.
-- Surrogate keys are identities; they never appear in public APIs.

CREATE TABLE patients (
    id                  BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    partition           TEXT COLLATE "C" NOT NULL,
    patient_id          TEXT COLLATE "C" NOT NULL,
    issuer              TEXT COLLATE "C" NOT NULL,
    patient_name        TEXT COLLATE "C",
    patient_name_folded TEXT COLLATE "C",
    birth_date          TEXT COLLATE "C",
    sex                 TEXT COLLATE "C",
    extra               JSONB,
    UNIQUE (partition, patient_id, issuer)
);
CREATE INDEX patients_name ON patients (partition, patient_name_folded);

CREATE TABLE studies (
    id               BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    partition        TEXT COLLATE "C" NOT NULL,
    patient_fk       BIGINT NOT NULL REFERENCES patients (id),
    study_uid        TEXT COLLATE "C" NOT NULL,
    study_date       TEXT COLLATE "C",
    study_time       TEXT COLLATE "C",
    study_time_key   TEXT COLLATE "C",
    accession        TEXT COLLATE "C",
    study_id         TEXT COLLATE "C",
    referring        TEXT COLLATE "C",
    referring_folded TEXT COLLATE "C",
    description      TEXT COLLATE "C",
    institution      TEXT COLLATE "C",
    received_at      BIGINT NOT NULL,
    updated_at       BIGINT NOT NULL,
    last_accessed_at BIGINT,
    extra            JSONB,
    UNIQUE (partition, study_uid)
);
CREATE INDEX studies_patient ON studies (patient_fk);
CREATE INDEX studies_date ON studies (partition, study_date);
CREATE INDEX studies_accession ON studies (partition, accession);
CREATE INDEX studies_received ON studies (partition, received_at);

CREATE TABLE series (
    id              BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    partition       TEXT COLLATE "C" NOT NULL,
    study_fk        BIGINT NOT NULL REFERENCES studies (id),
    series_uid      TEXT COLLATE "C" NOT NULL,
    modality        TEXT COLLATE "C",
    series_number   BIGINT,
    description     TEXT COLLATE "C",
    series_date     TEXT COLLATE "C",
    series_time     TEXT COLLATE "C",
    series_time_key TEXT COLLATE "C",
    body_part       TEXT COLLATE "C",
    station         TEXT COLLATE "C",
    pps_start_date  TEXT COLLATE "C",
    updated_at      BIGINT NOT NULL,
    extra           JSONB,
    UNIQUE (partition, series_uid)
);
CREATE INDEX series_study ON series (study_fk, modality);

CREATE TABLE instances (
    id               BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    partition        TEXT COLLATE "C" NOT NULL,
    series_fk        BIGINT NOT NULL REFERENCES series (id),
    sop_uid          TEXT COLLATE "C" NOT NULL,
    sop_class        TEXT COLLATE "C" NOT NULL,
    instance_number  BIGINT,
    content_date     TEXT COLLATE "C",
    content_time     TEXT COLLATE "C",
    content_time_key TEXT COLLATE "C",
    num_frames       BIGINT,
    transfer_syntax  TEXT COLLATE "C" NOT NULL,
    size             BIGINT NOT NULL,
    blob_store       TEXT COLLATE "C" NOT NULL,
    blob_key         TEXT COLLATE "C" NOT NULL,
    header_length    BIGINT NOT NULL,
    received_at      BIGINT NOT NULL,
    extra            JSONB,
    UNIQUE (partition, sop_uid)
);
CREATE INDEX instances_series ON instances (series_fk, sop_class);

CREATE TABLE extra_tags (
    tag   TEXT PRIMARY KEY,
    vr    TEXT COLLATE "C" NOT NULL,
    level TEXT COLLATE "C" NOT NULL,
    state TEXT COLLATE "C" NOT NULL
);

-- The transactional outbox (PLAN 3.7). Identity values are never reused; gaps from rolled-back
-- transactions are harmless. Commit order follows seq because inserts take an advisory lock.
CREATE TABLE events (
    seq       BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    id        TEXT COLLATE "C" NOT NULL,
    kind      TEXT COLLATE "C" NOT NULL,
    partition TEXT COLLATE "C",
    time      BIGINT NOT NULL,
    body      TEXT COLLATE "C" NOT NULL
);

CREATE TABLE cursors (
    subscriber TEXT PRIMARY KEY,
    seq        BIGINT NOT NULL
);
