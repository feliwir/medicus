# ADR-0001: Addon ABI — dynamically loaded native libraries with a versioned C ABI

- Status: accepted
- Date: 2026-10-21
- Context: PLAN §4

## Context

Most of medicus's functionality is delivered as addons, among them the protocol front-ends (DIMSE,
DICOMweb), index databases and blob stores. The owner decided that addons are **dynamically loaded
native libraries** (`.so` / `.dylib` / `.dll`), not compile-time crates. They must work on Linux,
macOS and Windows, and **non-Rust addons (C, C++, Zig, …) must be possible**.

Rust has no stable ABI, so Rust types (`String`, `Vec`, trait objects, futures, `serde_json::Value`,
`dicom_object::*`) cannot cross the boundary. Addons are compiled separately, possibly with another
compiler version, their own std, allocator and tokio runtime.

Orthanc's experience (`RegisterStorageArea`, `…2`, `…3`; `DatabaseBackendV4`) shows that an ABI
that hard-codes rich structures accumulates parallel versions of the same interface.

## Decision

### 1. Plain C ABI, generated header
- `crates/medicus-abi` holds only `#[repr(C)]` types, `extern "C"` function-pointer tables and
  constants. `include/medicus_addon.h` is generated from it with `cbindgen`, checked in, and diffed
  in CI. That header is **the** public contract.
- Only C-expressible types are allowed: fixed-width integers, `size_t`, pointers, `#[repr(C)]` structs,
  `#[repr(i32)]`/`#[repr(u32)]` status codes, and function pointers. Not allowed: `bool` (use `uint8_t`),
  unions, variadics, Rust enums with data, and callee-unwinding.
- Libraries `abi_stable` (unmaintained since 2023) and `stabby` (Rust-only) were rejected.

### 2. Two-step, version-checked entry
Every addon exports exactly two symbols, whose names never change:
```c
uint32_t           medicus_addon_abi_version(void);           /* (major << 16) | minor */
const MdAddonInfo *medicus_addon_init(const MdHostApi *host);  /* NULL = failed, see host log */
```
- The host calls `medicus_addon_abi_version` first and only proceeds when the major versions are
  equal and the addon's minor version is not newer than the host's. No other struct is exchanged
  before this check.
- `MdAddonInfo` carries the name, the version (UTF-8) and the target triple. The host rejects a
  mismatching target triple.
- While the major version is `0`, every minor bump may break compatibility (semver 0.x rules).

### 3. Evolving structs are size-prefixed
Every struct and function table that may grow starts with `size_t size` (`= sizeof` on the producer's
side). New fields are only ever **appended** in a minor version. A consumer reads a field only if
`size` covers it, otherwise it uses the documented default.

### 4. Capabilities are registered, not hard-wired
During `medicus_addon_init`, the addon calls `host->register_capability(kind, name, vtable, instance)`
once per capability it provides (`IndexStore`, `BlobStore`, `HttpRoutes`, `EventHook`,
`InstanceFilter`, `RemoteNode`, `JobExecutor`, `EvictionPolicy`, `HttpAuth`, …). `kind` is a `uint32_t`.
New kinds can be added without affecting existing addons, and the host answers an unknown kind with
`MD_UNSUPPORTED`.

### 5. Data across the boundary
| Shape | Type | Ownership |
|---|---|---|
| Borrowed bytes/UTF-8 | `MdStr { const uint8_t *ptr; size_t len; }` | Valid for the duration of the call only |
| Owned bytes | `MdBuffer { uint8_t *ptr; size_t len; void *ctx; void (*release)(void *ctx, uint8_t *ptr, size_t len); }` | The receiver calls `release` exactly once. Memory is freed by **whoever allocated it**, which is required on Windows, where each DLL may have its own CRT heap |
| Complex / evolving payloads | JSON in an `MdStr`/`MdBuffer`, with a schema version field | Query AST, configuration, events, job specs, peer descriptions |
| Datasets | DICOM Part 10 bytes or DICOM JSON (PS3.18 F) | |
| Large data | `MdReadStream` / `MdWriteStream` vtables, chunked (256 KB–1 MB) | Instances, HTTP bodies |

Strings are always UTF-8 `(ptr, len)`, never NUL-terminated.

### 6. Asynchrony through completions
Every operation that may block takes an `MdCompletion { void *ctx; void (*complete)(void *ctx,
int32_t status, MdBuffer result); }` and returns immediately.
- `complete` is called **exactly once**: on success, on failure, and also after cancellation (with
  `MD_CANCELLED`).
- It may be called from **any thread**, including synchronously from within the call that received it.
  Both sides must be re-entrant.
- Cancellation: async calls receive an `MdCancel` handle that the callee can poll or subscribe to.
  Cancellation is advisory. The completion still fires.
- The Rust SDK and host turn completions into futures (`oneshot`), so neither side's runtime leaks
  into the other.

### 7. Errors, panics and threads
- `int32_t` status codes (`MD_OK = 0`, `MD_INVALID_ARGUMENT`, `MD_NOT_FOUND`, `MD_ALREADY_EXISTS`,
  `MD_REJECTED`, `MD_RESOURCE_EXHAUSTED`, `MD_UNAVAILABLE`, `MD_CANCELLED`, `MD_UNSUPPORTED`,
  `MD_INTERNAL`, `MD_PANICKED`). A failing completion's `result` buffer may carry a UTF-8 message.
- **No unwinding across the boundary.** The Rust SDK wraps every entry point in `catch_unwind` and
  reports `MD_PANICKED`. The host then marks the addon unhealthy (`addon.unhealthy` event). The host
  guards its own callbacks the same way.
- Every capability instance must be safe to call **concurrently from multiple threads**.

### 8. Lifecycle and trust
- Load at startup → `medicus_addon_init` → capability `start` → … → capability `stop` at shutdown.
  Libraries are **never unloaded** (`dlclose` is unsound with Rust thread-locals and addon-owned
  threads).
- Addons are trusted native code. They are loaded only from configured search paths, with optional
  SHA-256 pinning in config. A manifest (`<name>.addon.toml`) next to the library names the file,
  version and ABI version.

### 9. One code path for static and dynamic builds
First-party addons can also be linked statically (`builtin-*` features). They still register through
the same vtables, so every test exercises the ABI.

## Consequences
- **Positive**: compiler-independent and language-independent; evolvable without parallel `V2`/`V3`
  interfaces; the async model does not depend on tokio; panics and allocators are isolated.
- **Negative**: JSON (de)serialization on control-plane calls (negligible compared with I/O, and
  hot data paths use buffers and streams); `unsafe` code in three crates (`medicus-abi`,
  `medicus-host`, `medicus-addon-sdk`) that needs careful review, Miri/ASan tests, and debug
  assertions for exactly-once completion.
- **Neutral**: authors of non-Rust addons must follow the threading and ownership rules by hand.
  `docs/addon-abi.md` documents them, and `addons/example-c` keeps them tested in CI.

## Alternatives considered
- **Compile-time crates + Cargo features**: simplest and fully type-safe, but needs a rebuild per
  addon set. Rejected by the owner.
- **WASM components (wasmtime)** as the *only* model: sandboxed and portable, but too slow and too
  constrained for storage, database and network-heavy addons. Adopted as an *additional* addon kind
  for policy and integration logic in ADR-0002.
- **`abi_stable` / `stabby`**: see §1.
