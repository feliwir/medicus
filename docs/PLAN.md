# medicus — Architecture & Implementation Plan

Status: draft for review · 2026-10-21

A lightweight, modular PACS (DICOM archive) in Rust, licensed under **MIT**. Orthanc (`refs/`) is used as
a **reference for features and DICOM edge cases only**. Its architecture, API layout and schema are not
copied. Orthanc is GPLv3, which is incompatible with MIT, so we also must not copy its code.
Dependencies must have MIT-compatible licences; `cargo-deny` enforces an allow-list
(MIT, Apache-2.0, BSD, ISC, Zlib, Unicode, MPL-2.0 for unmodified files only).

**Core vs addon rule.** The core contains only what *every* deployment needs and what all addons share:
the domain model, ingest pipeline, query engine, retrieve service, event outbox, job engine, addon host
and admin API. **Anything that adapts to the outside world and could be swapped, omitted or exist twice is
an addon**: protocols (DIMSE, DICOMweb), index databases and blob stores. Both protocol front-ends are
therefore addons on an equal footing. A DICOMweb-only or DIMSE-only deployment simply doesn't load the
other addon.

## 1. Goals and non-goals

**Goals (v1)**
- Receive and serve DICOM over DIMSE: C-ECHO, C-STORE, C-FIND, C-MOVE, C-GET, both as SCP and as SCU.
- DICOMweb: QIDO-RS, WADO-RS (instances, series, studies, metadata, bulk data), STOW-RS.
- Index databases: SQLite and PostgreSQL. Blob storage: local filesystem and S3-compatible storage
  (Azure and GCS cost little extra through `object_store`).
- **Everything above except the core runs as dynamically loaded native addons (`.so`/`.dylib`/`.dll`)**
  that talk to the core through a stable, versioned C ABI.
- A small versioned admin REST API (JSON + OpenAPI) for configuration, peers, jobs, health and metrics.
- Single binary + addon directory. A small TOML/environment bootstrap; everything else is configured
  at runtime through the admin API and the admin UI, and stored in the index (ADR-0004).
- An **admin UI** (addon `admin-ui`): first-launch setup, addons, nodes, partitions, access and settings,
  plus a DICOM browser and stack viewer over DICOMweb (ADR-0004, M8).
- **Scale range (§3.9)**: from *1 study per day* on a small box (SQLite + local disk, near-zero idle
  footprint, zero-config start) up to **at least 1 TB/day** sustained on one node (PostgreSQL + S3 or fast
  disk), with the archive growing to hundreds of TB and about 1 billion instances. Both ends use the same
  binary; only the addons and config differ.

**Non-goals (v1)**: transcoding, a diagnostic viewer (MPR, measurements, hanging protocols), HA clustering, Modality Worklist,
Storage Commitment, anonymization, scripting. The design must not block any of these (see §9).

## 2. Ecosystem decisions

| Concern | Choice | Reason |
|---|---|---|
| DICOM objects/parsing | `dicom-object`, `dicom-core`, `dicom-dictionary-std`, `dicom-json` 0.10 | De-facto standard Rust DICOM stack |
| Network (UL/PDU) | `dicom-ul` 0.10, `async` + `async-tls` features | Async association negotiation, P-DATA, TLS, pluggable `AccessControl`/`Negotiation` |
| DIMSE | **own crate `medicus-dimse`** | `dicom-ul` has no command-set/DIMSE layer and no C-FIND/MOVE/GET SCP. Kept independent so it could be upstreamed |
| Async runtime | `tokio` | Required by `dicom-ul` async, `axum`, `sqlx` |
| HTTP | `axum` 0.8 + `tower-http` | Tower middleware for auth, tracing, compression, limits |
| OpenAPI | `utoipa` | Spec generated from code |
| SQL | `sqlx` 0.9 (sqlite + postgres) + a small own statement builder in `index-sql-common` | Async-native, built-in migrations, bundled SQLite; see §3.8 for diesel comparison |
| Blob storage | `object_store` | One API for local, S3, Azure and GCS, streaming and multipart upload |
| WASM addons | `wasmtime` (component model) inside the optional `wasm-host` addon; WIT package `medicus:addon` | Sandboxed, portable, any language, for policy and integration logic (ADR-0002) |
| Addon loading | `libloading` + **own `#[repr(C)]` ABI**, header generated with `cbindgen` | `abi_stable` is unmaintained (last release 2023). `stabby` would make addons Rust-only. A plain C ABI is the most durable choice and lets C/C++/Zig addons exist |
| Config | `figment` (TOML + `MEDICUS_*` env) | Layered, typed with serde |
| Errors | `thiserror` in libraries, `anyhow` only in the binary | Common Rust practice |
| Observability | `tracing`, `tracing-subscriber`, `metrics` + Prometheus exporter, optional OpenTelemetry | |
| Testing | `cargo-nextest`, `proptest`, `testcontainers` (Postgres, MinIO), `cargo-fuzz` | |
| DIMSE validation | **DCMTK 3.7.1** (`refs/dcmtk`, built to `refs/dcmtk-install`) | Reference implementation for DIMSE; drives the interop suite (§5.1) |

## 3. Architecture

```
                ┌─────────────────────────────── medicus-server (core binary) ───────────────────────────────┐
                │  Ingest pipeline ──▶ IndexStore / BlobStore ◀── Query engine      Retrieve service           │
                │   (filters)              │                                         Job engine ─▶ executors    │
                │                     Event outbox ─▶ dispatcher ─▶ subscribers      Peers registry            │
                │  axum: /api/v1 (admin) + addon-mounted routes                                               │
                │  medicus-host: loads addons, adapts their C vtables to core traits, implements host API     │
                └───────────────────────────────────────────▲───────────────────────────────────────────────────┘
                                         C ABI (medicus-abi) │ host API ⇅ provider vtables
   ┌─────────────────────┬──────────────────────┬────────────┼──────────────┬──────────────┬────────────────┐
 dimse                 dicomweb           index-sqlite  index-postgres  storage-fs   storage-s3   (third-party)
 own TCP listener      HttpRoutes          IndexStore    IndexStore      BlobStore    BlobStore
 (dicom-ul +           QIDO/WADO/STOW      JobStore      JobStore
  medicus-dimse)       RemoteNode (later:
 RemoteNode, JobExec    STOW-out)
   ▲                     ▲
 DIMSE peers           HTTP clients
```

### 3.1 Workspace layout

```
Cargo.toml                 workspace: shared deps, lints, profiles; edition 2024, pinned MSRV
crates/
  medicus-core/            domain model and traits, no I/O (UIDs, levels, Query AST, events, errors)
  medicus-dimse/           plain library (no FFI, no medicus-core): DIMSE on dicom-ul, command sets,
                           SCP dispatcher, SCU client; reusable outside medicus, used by addons/dimse
  medicus-abi/             #[repr(C)] types + vtables only; ABI_VERSION; cbindgen → include/medicus_addon.h
  medicus-addon-sdk/       safe Rust SDK for addon authors: traits, export_addon! macro, async bridge
  medicus-host/            loading, version checks, vtable → trait adapters, host API implementation
  medicus-server/          the core binary: config, ingest, query, retrieve, events, jobs, admin API, wiring
                           (contains no protocol, database or storage code)
  medicus-testkit/         fixtures + backend conformance suites (any IndexStore/BlobStore must pass)
addons/
  index-sql-common/        shared schema/query builder (rlib, linked into both index addons)
  dimse/  dicomweb/  index-sqlite/  index-postgres/  storage-fs/  storage-s3/  events-webhook/
  eviction-basic/          LRU / FIFO / age-based retention and tiering rules (EvictionPolicy)
  example-hooks/           minimal reference addon (logs events, rejects by rule); used by ABI tests
  example-c/               minimal addon written in C against include/medicus_addon.h; CI-tested
  wasm-host/               native addon that runs WASM components (wasmtime) and registers their capabilities
wit/                       medicus:addon WIT package (worlds: event-hook, instance-filter, eviction-policy, …)
include/                   medicus_addon.h (generated, checked in, public contract)
tools/                     medicus-bench (load generator + scenario runner)
docs/  adr/  PLAN.md  dicom/ (standard)      refs/ (not committed)
```

`unsafe_code = "forbid"` in every crate except `medicus-abi`, `medicus-host` and `medicus-addon-sdk`.
All unsafe code lives in those three crates, behind safe APIs, and is reviewed as such.

### 3.2 Core domain (`medicus-core`)
- **Identity** comes from DICOM UIDs, using newtypes (`StudyInstanceUid`, `SeriesInstanceUid`,
  `SopInstanceUid`) with validation per PS3.5 §9. There are no synthetic hash IDs in public APIs. Backends
  can use internal surrogate keys.
- **Hierarchy**: Patient → Study → Series → Instance. A patient is keyed by `(PatientID, IssuerOfPatientID)`.
- **Indexed attributes** per level: the Required and Unique keys of the Patient Root and Study Root Q/R
  information models (PS3.4 C.6) as typed columns, plus **operator-configured extra tags** in an `extra`
  binary-JSON column per level (SQLite JSONB blob / PostgreSQL `jsonb`) holding the original DICOM JSON
  (returned in results) and normalized match forms (used by queries). Tags added to a non-empty index
  are reported as unfiltered until backfilled. Counts and `ModalitiesInStudy`/`SOPClassesInStudy` are
  computed by correlated subqueries, not stored (owner decision; optimize later if needed).
- **Metadata is not duplicated.** The original Part 10 file is the only stored copy; the index records
  the length of a prefix that contains its complete header, so an instance's metadata is a byte range
  read. A derived **per-series header cache** (one blob, binary) serves WADO-RS `/metadata`, QIDO
  `includefield` and post-filtering on non-indexed keys without one read per instance; DICOM JSON is
  produced at the edges only.
- **Query AST** is backend-neutral and built from both C-FIND identifiers and QIDO parameters. It
  implements PS3.4 C.2.2.2 matching: single value, UID list, wildcard, range, sequence and universal
  matching. Paging, sorting and `fuzzymatching` come later. Each backend compiles the AST to SQL. Keys
  that are not indexed are post-filtered in the core, so every backend behaves the same.
- **Traits**: `IndexStore` (transactions; insert/delete/query; series and study statistics), `BlobStore`
  (put stream / get range / delete / exists), `JobStore`, `InstanceFilter`, `EventHook`, `HttpRoutes`.
  These are what addons implement through the ABI. In-process test doubles implement them directly.

### 3.3 Ingest pipeline
Every entry point uses the same pipeline: C-STORE, STOW-RS and import from a folder (`medicus import`).
1. **Header prefix**: buffer the beginning of the stream and let dicom-rs parse it with
   `read_until(PixelData)`; the bytes are offered through a reader that reports "need more" at the end
   of the buffer while the stream continues, so dicom-rs alone decides where the header ends (no own
   DICOM parsing). Attempts double in size, the total is capped by `max_header_bytes`. Validate the
   required UIDs. The number of bytes dicom-rs consumed is recorded as the header prefix length.
2. **Filter hooks** run on the parsed header in a defined order and may accept, reject (with a DIMSE status
   or HTTP code), or rewrite non-bulk attributes.
3. **Streaming write (fast path)**: `BlobStore::put` streams the (possibly re-encoded) header followed by
   the remaining bytes **as they arrive**, without an intermediate copy. Large instances use multipart
   upload.
   **Slow path**: if the prefix cap is exceeded, elements follow Pixel Data, or a filter needs the whole
   dataset, the instance spills to a temporary file first. This is counted in metrics so it can be tuned.
4. Steps 1–3 hold only the header in memory, so memory use does not depend on instance size
   (whole-slide and multi-frame objects of several GB work).
5. One `IndexStore` transaction inserts or updates all levels, records the blob key, and appends an
   `instance.stored` event to the outbox (§3.7). Under load, the index writer **group-commits**:
   instances that finish within a short window (e.g. ≤ 5 ms or ≤ 256 instances) share one transaction.
   Each sender is still acknowledged only after *its* commit.
6. Commit. The event becomes visible to subscribers only now. `series.stable`/`study.stable` are emitted
   by a scheduler (driven by last-update timestamps in the index, so it survives restarts) after a
   configurable period with no new instances.

If a duplicate SOP Instance UID arrives, the configured policy applies: `reject | keep-existing | overwrite`.
Failures between steps 4 and 5 leave orphan blobs, and a periodic GC removes them. Ordering blob-first
means the index never points at missing data.

### 3.4 Protocol front-ends (addons)
Both front-ends are translators between a wire protocol and the core's protocol-neutral services
(ingest, query, retrieve, peers, jobs). They contain no storage or index logic.

**`medicus-dimse` library** (no FFI, no dependency on `medicus-core`, so it can be tested and fuzzed in isolation)
- Encodes and decodes command sets (PS3.7 §9/§10, Annex E). Messages are typed: `CStoreRq`, `CFindRsp`, …
- An SCP **dispatcher** routes by Affected SOP Class to service handlers (traits). It handles the
  pending/final response flow, C-CANCEL, and both message ID fields.
  Presentation contexts are built from the registered handlers through `dicom-ul`'s `Negotiation`.
- The SCU client covers C-ECHO, C-STORE (with transfer syntax selection), C-FIND, C-MOVE and C-GET.
  C-GET needs SCP/SCU role selection.

**`dimse` addon** (uses `medicus-dimse`; the addon owns its TCP/TLS listener)
- C-STORE → host `ingest` (streamed, with `origin = dimse{…}`); C-FIND → host `query`;
  C-MOVE → host `retrieve` (instance list + blob streams) plus `peers` lookup of the destination AE, then
  C-STORE sub-operations over its own SCU association; C-GET → sub-operations on the same association.
- AE access control (calling AE, IP, TLS client certificate) uses `dicom-ul` `AccessControl` plus its
  addon config. It publishes `association.opened/closed` through the host API.
- It provides the **`RemoteNode`** capability for protocol `dimse` (echo, send, find, move) and a
  **`JobExecutor`** for `send` jobs to DIMSE peers. Admin `/peers/{id}/echo` and "send study to peer"
  therefore work without the core knowing DIMSE.

**`dicomweb` addon**: QIDO-RS (→ `query`), WADO-RS (→ `locate`/`open_instance`/`metadata`) and
STOW-RS (→ `ingest`, `origin = dicomweb{…}`), per node at `/nodes/{path}/rs` (default node also at
`/rs`). *As built (M5):* like `dimse`, the addon owns its HTTP listener (own port); the `HttpRoutes`
capability (routes mounted into the core's server, streamed across the ABI) is deferred.
Authentication until `HttpAuth` exists: a trusted reverse proxy's user header (`user_header`). Later it adds a `RemoteNode` for protocol `dicomweb`
(STOW-out, QIDO/WADO against remote archives).

Because both front-ends use only the public host API, that API is proven complete for DIMSE-class
workloads. Third parties can add further protocols (for example HL7/FHIR imaging bridges) the same way.

### 3.5 Jobs
A small persistent job engine for outbound work: sends to peers (admin-triggered), and later STOW-out
and forwarding rules. The core schedules jobs, persists them and handles retries with backoff,
cancellation and progress. **Execution is delegated to `JobExecutor` capabilities** chosen by job kind
and peer protocol, so a `send` to a DIMSE peer runs in the `dimse` addon and one to a DICOMweb peer in the
`dicomweb` addon. Jobs are exposed through `/api/v1/jobs`. Persistence goes through `JobStore`, which the
index addon implements. C-MOVE sub-operations are association-bound and run directly in the `dimse`
addon, not as jobs.

### 3.6 Admin REST API (`/api/v1`)
`/system`, `/health` (live/ready), `/metrics`, `/peers` (list + echo), `/jobs` (list, get, cancel),
`/addons` (loaded addons and their versions), and `/events` (§3.7). **The admin API never moves data**:
instances come in and go out through DIMSE and DICOMweb only (decision of 2026-10-22), so there is no
upload, download, delete or "send" endpoint. Front-ends start sends through the host method `submit_job`
(e.g. a DICOMweb send transaction, exposed by the job's `transaction_uid`). Deletion comes from eviction
(§3.10). The OpenAPI document is served at `/api/v1/openapi.json`.

*From M8 (ADR-0004):* the admin API also manages the settings (nodes, partitions, peers, addons, access,
archive and job settings; versioned, with history and rollback), users and sessions, first-launch setup
and restarts. It authenticates callers (sessions, API tokens) and checks the `admin.*` permissions of
ADR-0003. It still never moves data: the admin UI's data browser reads through DICOMweb, which the core
reverse-proxies under `/rs`.

### 3.7 Events: react to changes, whatever the source
**One emission point.** Protocol front-ends never emit storage events themselves. C-STORE (direct, or as a
C-MOVE/C-GET destination), STOW-RS and folder import all go through the ingest pipeline
(§3.3). Deletions, jobs and the stable-study scheduler go through their own services. So
`InstanceStored` is identical whether data came via DIMSE or DICOMweb, and the source shows up only in the
event's `origin` field.

**Envelope** (JSON, versioned schema, same over ABI, HTTP and webhooks):
```json
{ "v": 1, "seq": 48121, "id": "0192…(UUIDv7)", "time": "2026-10-21T20:11:05Z",
  "kind": "instance.stored",
  "resource": { "study": "1.2…", "series": "1.2…", "instance": "1.2…", "sop_class": "1.2.840.10008.5.1.4.1.1.2",
                "transfer_syntax": "1.2.840.10008.1.2.1", "size": 524288, "blob_store": "default",
                "patient_id": "…" },
  "origin": { "type": "dimse", "calling_ae": "MODALITY1", "called_ae": "MEDICUS", "peer": "10.0.0.7:51234",
              "association": "…" },
  "detail": { "duplicate": "none" } }
```
`origin.type` is one of `dimse | dicomweb | admin_api | import | job | internal`. Each type has its own
fields, for example `dicomweb` carries `remote_addr`, `principal` and `request_id`.

**Catalogue v1**: `instance.stored`, `instance.rejected` (filter or validation, with reason),
`instance.deleted`, `series.stable`, `study.stable`, `series.deleted`, `study.deleted`,
`study.evicting`, `study.evicted`, `study.moved`, `storage.pressure`, `storage.full`, `storage.recovered`,
`job.started/progress/finished/failed`, `peer.unreachable`, `addon.unhealthy`. Addons can add namespaced
kinds (e.g. `dimse.association.opened/closed`). New kinds are additive; consumers must ignore kinds they
don't know.

**Delivery guarantees: transactional outbox.**
- Data-changing events are written to an `events` table in the **same index transaction** as the change.
  So there are no lost events (data stored but no event) and no phantom events (event but rollback).
- A dispatcher reads the outbox in `seq` order. Each **durable subscriber** has a persisted cursor,
  which gives at-least-once delivery that survives restarts and consumer outages, with replay from any
  `seq`. Consumers deduplicate on `id`.
- By default, events are delivered in order per study (partitioned by Study UID), with configurable
  parallelism across studies. Retries use backoff. After N failures, events go to a dead-letter list
  visible in the admin API.
- Retention: outbox rows are pruned once all durable cursors have passed them and the retention window
  has expired.

**Ways to react**
1. **`EventHook` addon capability**: a durable subscriber with filters on kind, level, SOP class, origin
   and AE.
2. **Admin API**: `GET /api/v1/events?since=<seq>&kind=…` (polling or long-poll, with cursor) and
   `GET /api/v1/events/stream` (Server-Sent Events, resumable via `Last-Event-ID`). External systems can
   react without writing an addon.
3. **First-party `events-webhook` addon**: HTTP POST with an HMAC signature and retries. AMQP, Kafka
   and NATS sinks can follow as further addons.

**Before storage vs after storage.** `InstanceFilter` runs synchronously *before* storage (accept, reject,
modify) and is the only kind of hook that can affect ingest. Event subscribers run *after* commit and can
never block or slow the pipeline.

### 3.8 Database access: sqlx (+ sea-query) vs diesel
Both are mature (diesel 2.3, sqlx 0.9). For medicus, sqlx is the better fit:

| | sqlx | diesel |
|---|---|---|
| Async | Native, tokio | Sync core. Async through the separate `diesel-async`, where SQLite is only a sync wrapper |
| Main query shape here | **Dynamic SQL from the Query AST** (optional keys, wildcards, ranges, UID lists, paging). `sea-query` builds dialect-correct SQL and `sqlx` executes it | Boxed DSL queries work, but backend-generic dynamic queries need heavy trait bounds |
| Compile-time checking | `query!` macros for static queries (offline mode with `cargo sqlx prepare`) | Strongest: full type-checked DSL from `schema.rs` |
| Migrations | Built in, embeddable in the addon binary | `diesel_migrations`, also embeddable |
| Cross-platform | SQLite bundled (compiled through `cc`), Postgres in pure Rust, so no system libraries needed | `libpq`/`libsqlite3` system libraries by default (bundling is possible but needs extra setup, especially on Windows) |

diesel's main strength, a statically typed DSL for a fixed set of queries, matters little here: the hot
path is a runtime-built query, and each backend is its own addon anyway. **Decision: sqlx**, with a small
dialect-aware statement builder in `index-sql-common` (sea-query turned out unnecessary): query
compilation, schema mapping and all write logic are shared by both backends, which only implement
transactions and statement execution; one conformance suite (including a parity test against the
in-memory matcher) guards both dialects.

### 3.9 Scale: from 1 study/day to ≥ 1 TB/day
**Sizing assumptions for the top end.** 1 TB/day is about 12 MB/s on average. We design for **10× peaks,
≈ 120 MB/s** (a saturated 1 GbE link). At a typical CT/MR instance size of ~0.5 MB that is about
2 million instances/day, with peaks of ~250 instances/s and ≥ 50 concurrent associations or HTTP uploads.
Single objects can be several GB (WSI, multi-frame, mammo tomosynthesis). Yearly growth is about
365 TB and 700 million instances, so the index must handle **≥ 1 billion instances**.
Reference hardware for the target: 8 vCPU, 32 GB RAM, NVMe, PostgreSQL on the same or an adjacent host,
S3 or local storage with ≥ 200 MB/s write throughput.

**Profiles** (same binary, different addons and config)
| | Small | Large |
|---|---|---|
| Index | `index-sqlite` (WAL, one writer task + reader pool) | `index-postgres` |
| Storage | `storage-fs` | `storage-s3` or `storage-fs` on fast disk |
| Target | Idle RSS < 50 MB, starts with no config, a Raspberry-Pi class box is enough | ≥ 1 TB/day sustained, 1e9 instances |

The SQLite limit will be measured, not assumed. The benchmark suite determines where SQLite stops meeting
the latency targets, and the docs state that figure as the threshold for switching to Postgres.

**What makes the top end work**
- **Streaming ingest** with one write of the bytes (§3.3) and a memory footprint that does not depend on
  instance size.
- **Backpressure everywhere**: global and per-source concurrency limits (semaphores). When saturated, the
  DIMSE addon stops reading PDUs and HTTP stops reading bodies, rather than buffering without limit.
  Overload makes clients slower, not the process out of memory.
- **Small, write-optimized index**: only query keys go in the database, metadata stays in the files
  (§3.2), and writes are group-committed (§3.3). Counts are computed on read, so concurrent senders
  writing into the same study never contend on a counter row.
- **PostgreSQL schema for growth**: surrogate `bigint` keys; B-tree indexes on UIDs and common query
  keys; `pg_trgm` for leading-wildcard matching; the `events` outbox **partitioned by day**, so pruning
  drops partitions instead of deleting rows. Instance tables are designed so that hash partitioning by
  study can be added later without breaking the API.
- **Retrieve at full speed**: ranged, streamed blob reads with read-ahead. C-MOVE sub-operations are
  pipelined and can optionally use several parallel associations to the destination. WADO-RS multipart
  is streamed.
- **Background work stays off the hot path**: series metadata consolidation, stable-study detection,
  orphan GC and outbox pruning run as throttled background jobs.
- **Operational visibility**: Prometheus metrics for ingest rate, bytes/s, p50/p99 store latency,
  slow-path ratio, queue depths, DB pool saturation and outbox lag per subscriber.

**Prepared for, not in v1**: several nodes behind a load balancer on shared Postgres + S3. The core
keeps no node-local state except temp files. Outbox dispatch and job pickup use `FOR UPDATE SKIP LOCKED`
leases, so horizontal scaling later is a deployment change, not a redesign. Storage tiering
(hot NVMe → S3 → cold) is covered by eviction with `move_to` (§3.10).

**Benchmarks are acceptance criteria** (`tools/medicus-bench`, plus DCMTK `storescu` in parallel and a
STOW-RS load generator):
| Scenario | Pass criterion |
|---|---|
| Idle, small profile | RSS < 50 MB, ~0 % CPU |
| Sustained ingest, large profile, 1 h | ≥ 120 MB/s mixed CT/MR (DIMSE + STOW), p99 C-STORE < 250 ms, flat RSS |
| Large objects | 4 GB single instance via C-STORE and STOW-RS, RSS increase < 100 MB |
| Query at 1e9 rows (synthetic index) | Study-level QIDO/C-FIND with typical keys p99 < 200 ms |
| Retrieve | WADO-RS study and C-MOVE ≥ 120 MB/s |
| Event lag | Outbox lag p99 < 1 s at full ingest rate |
A scaled-down version of these benchmarks runs nightly in CI to catch performance regressions.
The full run happens before each release.

### 3.10 Storage lifecycle: quotas and eviction
When a store runs out of space (physically, or past a configured quota), old data must be evicted
automatically. Because medical data has legal retention periods and deletion cannot be undone, the
work is split:

- The **core** owns the *mechanism and the safety rules*: measuring usage, thresholds, protections,
  crash-safe execution, audit and events.
- An **`EvictionPolicy` addon** owns the *decision*: what to evict, in what order, and whether to delete it
  or move it to another store.

A policy addon can only ever *propose*. The core validates every candidate against rules that no addon
can override.

**Measuring usage (core)**
- The index tracks bytes per instance, series and study, and per `BlobStore`.
- `BlobStore` gets an optional `usage()` call returning capacity, used and available bytes.
  `storage-fs` implements it with the OS free-space query on all three platforms. Object stores (S3) have
  no physical limit, so the core uses the index's byte accounting against a configured `quota_bytes`.
- If the SQLite index or the spill directory share a volume with a store, that volume's free space
  counts as well, so the index never fills the disk it lives on.
- **Last access** per study (WADO-RS, C-MOVE/C-GET) is tracked in memory and flushed to
  the index in batches, at most once per study every N minutes (default 15). This allows LRU policies
  without a database write on every read. It can be switched off.

**Thresholds per store** (configurable, in % or absolute bytes)
| Threshold | Default | Effect |
|---|---|---|
| `high_watermark` | 85 % | Eviction starts; `storage.pressure` event |
| `low_watermark` | 75 % | Eviction stops (hysteresis, avoids flapping) |
| `critical` | 95 % | **Ingest into that store is refused**: DIMSE status `A700` (Out of Resources), HTTP `507`; `storage.full` event. Retrieval keeps working. When usage drops below `high_watermark`, ingest resumes automatically and `storage.recovered` is published |

The `critical` guard works even with **no eviction addon loaded**, so medicus never crashes or corrupts
data on a full disk. Eviction itself is **disabled until a policy is configured**: nothing is ever
deleted automatically by default.

**Protections (core, not overridable by addons)**
- `min_retention`: a global minimum age, counted from when the data was received. Optional per-source
  overrides (per calling AE or origin) may only *raise* it. This is where legal retention goes, for
  example 10 years.
- **Protected studies**: a manual pin through the admin API, or set by a filter or hook addon.
- **Busy or unsettled**: studies that are not yet `study.stable`, that have an active ingest, retrieve,
  C-MOVE or send job (tracked through short-lived leases), or that are still waiting on a durable event
  subscriber configured as `required_before_evict` (e.g. "forwarded to the long-term archive").
- **Archive confirmation** (optional rule): evict only if a configured peer confirms it holds the study,
  checked through `RemoteNode::find` (and Storage Commitment later).

**Policy addon contract (`EvictionPolicy` capability)**
- `on_pressure(PressureInfo { store, usage, bytes_to_free, cursor })` → a page of candidates
  `[{ study_uid, action: delete | move_to(store), reason }]`. The core calls it again with the cursor until
  usage is below `low_watermark` or the policy returns no more candidates.
- `on_schedule()` runs periodically (cron-like interval in the addon's config) for **time-based
  retention** that is independent of disk pressure, such as "delete MG studies from AE X after 2 years".
- Policies query the index through the host API. The Query AST gains `order_by`
  (received, study date, last access, size) and per-study size and access fields, so typical policies
  are a single paged query.
- Several policies can be loaded. They are chained by priority, and the first policy that returns
  candidates wins for that round.

**Execution (core): crash-safe and idempotent, at study level**
1. Atomically mark the study `evicting` (in the index) and append `study.evicting` to the outbox. New
   ingest into an `evicting` study waits until eviction finishes, then recreates the study. Queries no
   longer return it.
2. Delete blobs, or for `move_to`: copy, verify size and checksum, switch the blob key, then delete the
   old blob. This step can be retried after a crash.
3. Remove the index rows (or update the store reference for a move), write the eviction audit record,
   and append `study.evicted` with `{ reason, policy, action, bytes_freed }`.

Eviction runs as a throttled background job with its own I/O budget, so it doesn't slow ingest. Series
level is possible as an option, but study level is the default, because partially present studies
confuse clinicians and viewers.

**`move_to` makes medicus usable as a cache tier.** For example, local NVMe holds the last N days,
older studies move to S3, and later they move to a cold archive. Retrieve reads transparently from
whichever store an instance is in.

**Admin API and observability**
- `GET /api/v1/storage`: per-store usage and watermarks.
- `POST /api/v1/eviction/preview`: a **dry run** of the active policies, showing what would be evicted
  and why.
- `POST /api/v1/eviction/run`: trigger a round manually.
- `PUT/DELETE /api/v1/studies/{uid}/protection`: pin or unpin a study.
- `GET /api/v1/eviction/log`: the persistent audit log of everything evicted (who, what, why, policy,
  bytes).
- Metrics: usage ratio, bytes evicted per hour, protected-but-candidate count (evidence of
  misconfigured retention), and time spent in `critical`.

**First-party addon `eviction-basic`** provides configurable rule sets:
- **LRU** (by last access) or **FIFO** (by received time), optionally per store.
- **Age-based retention rules** matching modality, calling AE, origin or SOP class, e.g.
  `{ match = { modality = "CR" }, older_than = "5y", action = "delete" }`.
- **Tiering rules**: `{ older_than = "30d", action = { move_to = "s3-archive" } }`.

Third parties can write any other policy in Rust or C, such as "keep only the latest 3 studies per
patient" or "ask the RIS first".

### 3.11 Nodes, partitions and access control (ADR-0003)
- **Partition**: the unit of data separation (multi-tenancy). Every hierarchy row, blob, event and
  job belongs to one partition; UIDs are unique per partition; transfers between partitions copy.
  Store, quota, duplicate policy, extra tags and eviction are set per partition.
- **Node**: a named endpoint (DIMSE AE title and/or DICOMweb path `/nodes/{node}/rs`), backed by one
  local partition (N nodes : 1 partition) or by one remote peer (proxy for query/retrieve/store).
- **Principal → roles → grants**: permissions per node are `query`, `retrieve`, `store` (into the
  node) and `send` (transfers out of it), plus scoped admin permissions. v1 from config; later OIDC
  claims, DIMSE User Identity and TLS certificates, and an admin-managed store.
- **Enforcement**: protocol addons only authenticate; every host call carries `{node, principal}`
  (ABI 0.6), and the core authorizes and scopes to the node's partition. Denials: A-ASSOCIATE-RJ,
  0124H, HTTP 401/403, and an `access.denied` event.
- Zero-config: partition `default` and node `MEDICUS` exist implicitly.

## 4. Addon system (dynamic native libraries)

This is the most consequential design and should get an ADR (`docs/adr/0001-addon-abi.md`) before coding.

### 4.1 Principles
1. **The C ABI is the only contract.** Rust types such as `DataSet`, `String`, `Vec` or futures never cross
   the boundary. Addons are built with their own compiler version, std and tokio.
2. **Stable shapes cross the boundary, evolving payloads are serialized.** Hot and simple calls (blob
   read/write, filter verdicts) use C structs. Complex, evolving data (query AST, configuration, events,
   job payloads) goes as versioned JSON, and datasets as DICOM JSON or Part 10 bytes. This keeps C structs
   small and is how we avoid `StorageArea3` / `DatabaseBackendV4` churn.
3. **Capability-based registration.** An addon receives a `HostApi` table and calls
   `register(capability_kind, vtable_ptr)` once per capability it provides. New capability kinds can be
   added without affecting existing addons.
4. **Versioning.** `medicus_addon_entry` returns a descriptor containing `abi_major` and `abi_minor`, the
   addon name and version, and the required host features. The host rejects a different major version.
   Every struct and vtable starts with a `size` field, so a minor version can append fields and both
   sides can detect them.
5. **No panics or unwinding across the boundary.** The SDK wraps every entry point in `catch_unwind` and
   maps panics to an error status. The host then marks the addon unhealthy.
6. **No hot unloading.** `dlclose` with Rust thread-locals and background threads is unsound. Addons
   load at startup and stay loaded until the process exits.
7. **Trust model.** Addons are fully trusted native code. They load only from configured paths, with
   optional SHA-256 pinning in config. This must be documented clearly.

### 4.2 Calls and async across the boundary
- **Completion callbacks**: any operation that may block takes `(args…, MdCompletion { ctx, on_done })`
  and returns at once. The addon calls `on_done(ctx, status, result)` exactly once, from any thread. On
  the host side this becomes a `oneshot` future. Cancellation uses an `MdCancelToken` handle passed
  with the call.
- **Ownership**: buffers are `MdBuffer { ptr, len, release }`, and whoever allocates provides `release`.
  Strings are UTF-8 `(ptr, len)`, never null-terminated.
- **Streaming**: large blobs and HTTP bodies use `MdReadStream` / `MdWriteStream` vtables (async
  chunked reads and writes through completions), so instances never have to fit in memory.
- **The SDK hides all of this.** Addon authors implement normal async Rust traits:
  ```rust
  struct FsStorage { root: PathBuf }
  #[medicus_addon_sdk::async_trait]
  impl BlobStore for FsStorage { async fn put(&self, key: &BlobKey, body: ByteStream) -> Result<()> { … } }
  medicus_addon_sdk::export_addon!(name = "storage-fs", init = |ctx, cfg: FsConfig| {
      ctx.register_blob_store(FsStorage::new(cfg)?)
  });
  ```
  The SDK runs a tokio runtime inside each addon and bridges futures to completions in both directions.
- **Static builds**: the same addon crates can also be linked into the binary (`builtin-*` Cargo
  features) and registered through the same vtables. That gives a single-file distribution, and the ABI
  code path is exercised in every test.
- **Chunk sizes**: streams move data in chunks of 256 KB–1 MB, so FFI calls stay at a few per instance and
  per MB. At 120 MB/s, crossing the boundary costs much less than the I/O it carries.

### 4.2a Non-Rust addons
Supported as a first-class *possibility*. The Rust SDK remains the primary, most convenient path.
- `include/medicus_addon.h` is a **public, versioned contract**. It is generated by `cbindgen` from
  `medicus-abi`, checked in, and diffed in CI: any change must come with an ABI version bump and an
  entry in `docs/abi-changelog.md`. It is published with every release.
- The ABI only uses C-expressible constructs: fixed-width integers, `(ptr, len)` UTF-8 strings,
  `size`-prefixed structs, function-pointer tables and opaque handles. No Rust layouts, no `bool` of
  unspecified size, no unions, no variadics.
- **CI keeps it honest**: a minimal C addon (`addons/example-c/`, a hook plus an in-memory `BlobStore`) is
  built with the platform's C compiler (gcc/clang/MSVC) on all three OSes and runs through the same ABI
  tests and the `BlobStore` conformance suite.
- Docs: `docs/addon-abi.md` gives the threading rules (which callbacks may come from which thread),
  ownership rules (who frees which buffer) and the exactly-once completion contract, written for C
  authors.

### 4.3 Capabilities in v1 (provider side)
| Capability | Used for | Notes |
|---|---|---|
| `IndexStore` | SQLite, Postgres | Exactly one active instance. Query AST as JSON; result sets streamed in pages |
| `JobStore` | persisted jobs | Usually provided by the index addon |
| `BlobStore` | fs, S3 | One or more named stores. Routing rules (default store, per-AE) in config. Optional `usage()` for quotas (§3.10) |
| `HttpRoutes` | DICOMweb, auth, custom APIs | The addon claims a path prefix; streaming request/response, multipart is the addon's job |
| `InstanceFilter` | accept/reject/modify on ingest | Ordered by configured priority |
| `EventHook` | InstanceStored, StudyStable, Deleted, JobFinished… | Asynchronous and non-blocking for ingest. At-least-once delivery within one process |
| `HttpAuth` | authentication/authorization | Wraps all HTTP routes including `/api/v1` |
| `RemoteNode` | DIMSE peers (`dimse`), later DICOMweb peers | Protocol-specific client: echo, send, find, move. Peers in config/admin API carry a `protocol` field |
| `JobExecutor` | `send` jobs (and future kinds) | Selected by job kind + peer protocol; reports progress through host API |
| `EvictionPolicy` | `eviction-basic`, custom policies | Proposes eviction or move candidates under pressure or on a schedule; the core enforces protections and executes (§3.10) |

**Host API (consumer side)**, which every protocol addon is built on:
- logging (routed to `tracing`), the addon's config as JSON, metrics
- `ingest` (streamed instance + `origin`) → verdict / status
- `query` (Query AST) → paged results
- `retrieve` (UIDs at any level) → instance list + metadata + blob read streams
- `peers` lookup, `submit_job`, job progress reporting
- `publish_event` for addon-owned, namespaced, non-durable event kinds (e.g. `dimse.association.opened`).
  Data events (`instance.*`, `study.*`) can only be produced by the core.
- `report_listener` (bound address, readiness), feeding `/health` and `/system`

This is what lets DIMSE and DICOMweb both be pure addons.

### 4.5 WASM addons (sandboxed, portable)
Native addons are fast but fully trusted, and they need one build per platform. For **policy and
integration logic**, medicus also runs **WebAssembly components** (ADR-0002). Examples are
reacting to events, ingest filters, eviction and retention rules, routing, and small HTTP endpoints.
- Delivered by the native addon **`wasm-host`** (wasmtime). It loads `.wasm` components and
  registers their capabilities through the normal ABI. The core never links wasmtime, and to the
  core a WASM `EventHook` looks exactly like a native one.
- Interface: the WIT package `medicus:addon`, with one world per capability (`event-hook`,
  `instance-filter`, `eviction-policy`, later `http-routes`). Host imports are `log`, `config`, and
  later a read-only `query`. Evolving payloads are JSON strings, the same schemas as the C ABI.
- **Sandboxed by default**: no filesystem, network or clock access unless granted. Each component
  has a memory limit, a per-call deadline (epoch interruption) and an instance pool for
  concurrency. Outbound HTTP is restricted to an allow-list. A trap fails only that call, and the
  instance is replaced.
- **One artifact for all platforms**: the same `.wasm` runs on Linux, macOS and Windows. Compiled
  code is cached on disk.
- **Not offered to WASM**: `BlobStore`, `IndexStore`, `JobStore` and protocol listeners. Bulk data
  and native I/O stay in native addons.
- Guests can be written in Rust (`wit-bindgen`), C/C++, Go, JS or Python. A Rust guest SDK crate
  (`medicus-wasm-guest`) gives the same ergonomics as the native SDK.
- Tests: one capability conformance suite runs against a native and a WASM implementation of the
  same hook and filter, so the two kinds cannot drift apart.

## 5. Engineering practices
- **Workspace hygiene**: shared `[workspace.dependencies]`, `[workspace.lints]` (clippy `all` plus selected
  `pedantic`, `unwrap_used`/`expect_used` denied outside tests), `rustfmt.toml`, `cargo-deny` (licences,
  advisories, duplicates), `Cargo.lock` committed.
- **API discipline**: libraries return typed errors. Use newtypes for UIDs, AE titles and blob keys
  instead of strings. Parse at the edges and keep invariants in the types. Nothing blocking runs on
  async threads (`spawn_blocking` for CPU-heavy parsing).
- **Testing pyramid**
  - Unit tests and `proptest` for the matching rules, command-set codec and UID validation.
  - **Conformance suites** in `medicus-testkit`: one generic suite for `IndexStore` and one for
    `BlobStore`, run against every backend (SQLite in memory, Postgres and MinIO via
    `testcontainers`) **through the loaded `.so`**.
  - **ABI tests**: build `example-hooks` as a `cdylib`, then load it and test version mismatch, panic
    isolation and cancellation.
  - **DIMSE interop tests against DCMTK** (§5.1). DICOMweb is tested with `dicomweb-client` (Python) in CI.
  - **Event tests**: the same study sent via C-STORE, STOW-RS and folder import must produce equivalent
    `instance.stored` events (differing only in `origin`). Tests also kill the process mid-ingest and
    after commit to check that cursors replay and that no event is lost or phantom.
  - **Fuzzing** (`cargo-fuzz`) of the command-set decoder and the DICOM JSON/query parsers.
  - Test data from `refs/orthanc-tests` and the pydicom test files, checked for licences before vendoring.
- **CI** (GitHub Actions) runs a **Linux, macOS and Windows matrix**: fmt, clippy, nextest, deny, docs, a
  weekly fuzz smoke run, and integration jobs (service containers on Linux). Release builds produce the binary, the first-party addons and Docker images.
- **Docs**: ADRs for every significant decision, rustdoc on public APIs, and a DICOM Conformance
  Statement (PS3.2) maintained from M4 onwards.

### 5.1 DIMSE validation with DCMTK
An integration-test crate (`tests/interop`) starts `medicus-server` with SQLite and fs storage on random
ports and drives the DCMTK command-line tools. The tools are found through `MEDICUS_DCMTK_BIN` (locally
`refs/dcmtk-install/bin`) and then `PATH`. Locally, tests are skipped with a notice if DCMTK is missing.
**In CI a missing DCMTK is a failure.**

| medicus role | DCMTK tool(s) | Scenarios |
|---|---|---|
| Verification SCP | `echoscu` | Accepted and rejected calling/called AE, TLS (`+tls`) |
| Storage SCP | `storescu`, `dcmsend` | Many SOP classes; implicit/explicit VR and compressed transfer syntaxes as stored (`-xi`, `+xe`, `-xs`…); PDU sizes (`-pdu`); 1,000+ instances in one association; duplicates; abort mid-transfer; unsupported SOP class → presentation context rejected |
| | `dcmdump`, `dcmcjpeg`/`dcmcrle` | `dcmdump` checks that retrieved files are byte- and tag-identical to what was sent; the compressors generate compressed test data |
| Q/R Find SCP | `findscu` | Patient root and study root, every level, all matching types (wildcard, range, UID list, universal, sequence), empty results, C-CANCEL |
| Q/R Move SCP | `movescu` + `storescp` as destination | Destination known or unknown, sub-operation counts and pending responses, partial failure (destination refuses some), cancel |
| Q/R Get SCP | `getscu` | Role selection, sub-operations on the same association |
| Storage SCU (send) | `storescp`, `dcmrecv` | Admin-triggered send job, transfer syntax negotiation fallback, peer down → retry / `peer.unreachable` event |
| Q/R SCU (client) | `dcmqrscp` (reference archive) | medicus C-FIND/C-MOVE/C-GET against a DCMTK archive (needed for future "fetch from PACS") |

The tests check DIMSE status codes and sub-operation counters (PS3.4 C.4), stored content, and the
emitted events. Each scenario's DCMTK log is kept as a CI artifact. CI pins **DCMTK 3.7.1**, built from
source on all three OSes and cached by version, so every platform uses the same tools.

### 5.2 Cross-platform (Linux, macOS, Windows)
All three platforms are first-class, built and tested in CI:
- **Targets**: `x86_64`/`aarch64` Linux (gnu, plus musl for static server builds), `x86_64`/`aarch64`
  macOS, `x86_64` Windows (MSVC).
- **Addons** are `.so`, `.dylib` or `.dll`; filenames are resolved with `libloading::library_filename`.
  Each addon ships a small manifest (`<name>.addon.toml`: name, version, ABI version, library file).
  An addon is built per target triple; the host refuses an addon built for a different triple or ABI major.
- **Windows specifics**: each DLL can have its own CRT heap. The ABI already requires buffers to be
  released by whoever allocated them (`MdBuffer.release`), so this is safe.
  Exports use `extern "C"` + `#[unsafe(no_mangle)]`.
- **Pure-Rust stack where possible**: rustls (`ring` provider, to avoid the cmake/nasm toolchain on
  Windows), bundled SQLite, Postgres driver in pure Rust, `object_store`. The only C code compiled is
  SQLite through `cc`, so no system libraries are needed.
- **Filesystem storage**: blob keys are opaque and hash-fanned (`ab/cd/<uuid>.dcm`), never derived from
  UIDs or patient data. This avoids Windows reserved names, case-insensitive filesystems and path-length
  limits. Writes are atomic (temp file + `persist`, which also works on Windows). Paths always use `Path`.
- **Service integration** (M7): systemd unit, launchd plist, Windows Service (`windows-service` crate).
  macOS release binaries are signed and notarized.

## 6. Milestones

| # | Milestone | Content | Done when |
|---|---|---|---|
| M0 | Foundations | Workspace, CI, lints, `medicus-core` types (UIDs, levels, Query AST), config, tracing; ADR-0001 (ABI) | CI green; ADR accepted |
| M1 | **ABI spike → ABI v0** | `medicus-abi`, `medicus-host`, `medicus-addon-sdk`, `example-hooks`; completions, streams, panics, version checks | A `.so` addon loads, registers a hook and a `BlobStore`, and passes ABI tests |
| M2 | Store locally + events (+ first ingest benchmark) | `storage-fs`, `index-sqlite`, conformance suites, ingest pipeline, **outbox, dispatcher, `EventHook`, `/api/v1/events` (+ SSE)**, `/health`, **usage accounting + `critical` ingest guard** | Importing a study stores it and yields replayable `instance.stored` events; suites pass on all 3 OSes |
| M2b | WASM addons | `wit/` package, `wasm-host` addon (event-hook + instance-filter worlds), limits/sandbox, compile cache, Rust guest SDK, example `.wasm` hook | The same hook scenario passes natively and as WASM on all 3 OSes; a trapping or looping component cannot affect ingest |
| M3 | DIMSE receive | `medicus-dimse` codec + SCP dispatcher; `dimse` addon with C-ECHO, C-STORE SCP; DCMTK interop harness | DCMTK `echoscu`/`storescu`/`dcmsend` scenarios (§5.1) pass on all 3 OSes; fuzzing has started |
| M4 | Query/Retrieve | Core retrieve service, peers, job engine; in `dimse`: C-FIND SCP (patient and study root, all levels), C-MOVE SCP + SCU sub-operations, C-GET SCP, `RemoteNode` + `JobExecutor` (Storage SCU) | DCMTK `findscu`/`movescu`/`getscu`/`storescp`/`dcmqrscp` scenarios pass |
| M4b | Nodes, partitions, access control (ADR-0003) | Core model and config (partitions, nodes, principals, roles); partition-aware index schema and `IndexStore` (rewritten migrations), isolation conformance tests; per-partition store/quota/policy; call context + core authorization (ABI 0.6); `dimse` with several AE titles, principal mapping, 0124H, node-aware C-MOVE; events/jobs/admin API scoped | The same UIDs live independently in two partitions; DCMTK scenarios per node pass, with denied operations refused; no cross-partition leak in the isolation suite |
| M4c (after M5) | Remote nodes (proxies) | `RemoteNode` find/fetch/store; query, retrieve (on-demand fetch) and store through a node backed by a peer, over DIMSE and DICOMweb; optional local cache partition with a size limit (LRU) | DCMTK `dcmqrscp` as the remote archive: query, retrieve and store through a remote node work; permissions are checked before forwarding |
| M5 | DICOMweb addon | Node-scoped (`/nodes/{node}/rs`): QIDO-RS, WADO-RS (no transcoding: as-stored or 406), STOW-RS; `events-webhook` addon | `dicomweb-client` suite passes; OHIF can browse; STOW-RS and C-STORE produce equivalent events |
| M6 | Scale-out backends | `index-postgres`, `storage-s3` (`object_store`) | Same conformance suites pass on Postgres and MinIO |
| M6b | Lifecycle | `EvictionPolicy` capability, protections, crash-safe evict/move executor, last-access tracking, admin storage/eviction API, `eviction-basic` addon | Fill-the-disk test: sustained ingest into a capped store stays between the watermarks for 1 h with no refused C-STORE; a crash mid-eviction recovers; protected and young studies are never touched |
| M7 | Hardening + scale | TLS (DIMSE + HTTP), `HttpAuth` addon (basic/OIDC), metrics, orphan GC, series metadata consolidation, backpressure tuning, Postgres partitioning, `medicus-bench`, Docker | All §3.9 benchmark criteria met on reference hardware; security review done |
| M8 | Admin UI & viewer (ADR-0004) | Addon manifests and dependencies, settings/users/sessions in the index (ABI 0.7), setup mode, hot apply of nodes and access, restart; local accounts, `admin.*` permissions; `HttpRoutes` v1; admin API write surface + OpenAPI; DICOMweb proxy; `admin-ui` addon (React, Tailwind, shadcn/ui): setup wizard, dashboard, configuration and access screens, DICOM browser and cornerstone3D stack viewer | From an empty data directory the wizard sets up a working archive; nodes added in the UI answer C-ECHO and QIDO without a restart; a restricted user sees only permitted nodes; a study can be browsed and viewed |

M1 comes first on purpose: the ABI is the riskiest part and everything else builds on it. M3 is the
largest chunk of work, because a DIMSE layer has to be written.

## 7. Key risks
| Risk | Mitigation |
|---|---|
| Eviction deletes data that must be kept | Off by default; core-enforced `min_retention` and protections that addons cannot override; dry-run preview; audit log; property tests that protected studies are never selected |
| DIMSE layer effort and correctness | Separate crate, typed messages, fuzzing, DCMTK interop from day one; Orthanc's `DicomNetworking/` and PS3.7 for edge cases |
| Async FFI complexity, use-after-free, double completion | One completion primitive, implemented once in SDK and host; Miri/ASan on ABI tests; debug assertions for exactly-once completion |
| ABI churn | Serialized payloads for evolving data, `size`-prefixed structs, capability registration, ADR-governed changes |
| Memory on large studies | Streaming everywhere (header-prefix ingest, streamed blobs, paged queries) |
| Missing 1 TB/day on one node | Benchmarks from M2 onwards (small scale) instead of only at the end; index kept small (§3.2); group commit; the slow-path ratio and DB saturation are visible in metrics |
| SQLite limits hit by users at the small end | Measured threshold documented; online migration tool SQLite → Postgres (later) |
| WADO-RS transfer syntax negotiation without transcoding | v1 returns as-stored or 406; a transcoding capability comes later via `dicom-pixeldata` codecs |
| Schema evolution in two SQL backends | `sqlx migrate` per backend, with shared schema definitions in `index-sql-common` |

## 8. Open questions for the owner
None currently.

Resolved: WASM addons via `wasm-host` (§4.5, ADR-0002) · disk eviction via core mechanism + `EvictionPolicy` addons (§3.10) · scale from 1 study/day to ≥ 1 TB/day (§3.9) · non-Rust addons via a public C header (§4.2a) · MIT licence · DIMSE and DICOMweb are both addons (core vs addon rule, top) · all three
platforms (§5.2) · sqlx over diesel (§3.8) · DCMTK as the DIMSE reference (§5.1).

## 9. Later (designed for, not built in v1)
Transcoding WADO-RS · Storage Commitment (N-ACTION/N-EVENT-REPORT) · Modality Worklist
addon · anonymization and modification jobs · forwarding/routing rules · ATNA audit logging (PS3.15 A.5) ·
`eviction-policy` and `http-routes` WASM worlds · MPR and measurements in the viewer · OIDC login in the admin UI · multi-node deployments on Postgres + S3.
