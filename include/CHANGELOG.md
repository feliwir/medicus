# Addon ABI changelog

Versions refer to `MD_ABI_MAJOR.MD_ABI_MINOR`. While the major version is 0, every minor version
is a breaking change.

## 0.2 (unreleased)

- Added `MD_CAP_INDEX_STORE` with `MdIndexStoreVtable`: a single `call` taking a JSON request
  (`{"method": ..., ...}`), so that new index methods do not change the ABI.

## 0.1

Initial ABI: two-step entry (`medicus_addon_abi_version`, `medicus_addon_init`), host API (`log`,
`config`, `register_capability`), buffers, completions, pull-based streams, advisory cancellation,
and the capabilities `MD_CAP_BLOB_STORE` and `MD_CAP_EVENT_HOOK`.
