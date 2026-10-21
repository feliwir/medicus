# Addon ABI changelog

Versions refer to `MD_ABI_MAJOR.MD_ABI_MINOR`. While the major version is 0, every minor version
is a breaking change.

## 0.1 (unreleased)

Initial ABI: two-step entry (`medicus_addon_abi_version`, `medicus_addon_init`), host API (`log`,
`config`, `register_capability`), buffers, completions, pull-based streams, advisory cancellation,
and the capabilities `MD_CAP_BLOB_STORE` and `MD_CAP_EVENT_HOOK`.
