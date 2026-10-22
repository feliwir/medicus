# Addon ABI changelog

Versions refer to `MD_ABI_MAJOR.MD_ABI_MINOR`. While the major version is 0, every minor version
is a breaking change.

## 0.4 (unreleased)

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
