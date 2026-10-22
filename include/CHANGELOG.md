# Addon ABI changelog

Versions refer to `MD_ABI_MAJOR.MD_ABI_MINOR`. While the major version is 0, every minor version
is a breaking change.

## 0.6 (unreleased)

- Partitions (ADR-0003): index requests `query`, `locate`, `delete` and `update_extra` take a
  `partition`; instance records and locations, events (`partition`) and jobs carry one;
  `stored_bytes` returns `[{partition, store, bytes}]`.

## 0.5

- Added `MD_CAP_REMOTE_NODE` with `MdRemoteNodeVtable`: protocol clients for peers, registered
  under the protocol name. A single JSON `call` serves `echo` and `send` (send jobs).
- Added the host methods `job_progress` (via `MdHostApi::call`), with which executors report
  progress and learn when a job was cancelled, and `submit_job`, with which protocol front-ends
  queue jobs (e.g. a `DICOMweb` send).
- Index stores gained the methods `put_job`, `get_job`, `list_jobs` and `claim_job`.

## 0.4

- Added the host function `MdHostApi::call`: host services addressed by method name with JSON
  requests and responses — `query` (complete query answers), `locate` (stored instances of a study,
  series or instance) and `peers` (configured remote nodes) — so further services need no new
  function pointers.
- Added the host function `MdHostApi::open_instance`: a read stream of a stored instance's Part 10
  file, by SOP Instance UID.

## 0.3

- Added the host function `MdHostApi::ingest`: addons (protocol front-ends) hand DICOM Part 10
  files to the core's ingest pipeline.
- Added `MD_CAP_SERVICE` with `MdServiceVtable` (`start`/`stop`): background services the host
  starts once the archive is ready.

## 0.2

- Added `MD_CAP_INDEX_STORE` with `MdIndexStoreVtable`: a single `call` taking a JSON request
  (`{"method": ..., ...}`), so that new index methods do not change the ABI.

## 0.1

Initial ABI: two-step entry (`medicus_addon_abi_version`, `medicus_addon_init`), host API (`log`,
`config`, `register_capability`), buffers, completions, pull-based streams, advisory cancellation,
and the capabilities `MD_CAP_BLOB_STORE` and `MD_CAP_EVENT_HOOK`.
