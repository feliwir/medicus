# ADR-0002: WebAssembly addons via a `wasm-host` native addon

- Status: accepted
- Date: 2026-10-21
- Context: PLAN §4.5, ADR-0001

## Context

ADR-0001 rejected WASM as *the* addon model: storage, index and protocol addons move large amounts
of data and need native I/O, sockets and threads. The owner asked to re-evaluate WASM as an
*additional* addon kind. Compared with native addons, WASM components offer:

| | Native addon (ADR-0001) | WASM component |
|---|---|---|
| Isolation | none: trusted code with full process privileges | sandboxed: no file, network or clock access unless granted; memory and CPU limits |
| Portability | one build per target (Linux/macOS/Windows × arch) | **one `.wasm` file runs on every platform** |
| Languages | anything with a C ABI | anything that compiles to WASM components (Rust, C/C++, Go, JS, Python, …) |
| Failure impact | a bug can crash or corrupt the server (panics are contained, segfaults are not) | traps are contained; the instance is discarded and recreated |
| Throughput | native, zero-copy | copies across linear memory, slower |

This makes WASM a good fit for **policy and integration logic**: event reactions, ingest filters,
eviction/retention rules, routing, small HTTP endpoints. It is a poor fit for storage, index and
protocol back-ends.

## Decision

1. **WASM support is a native addon, `addons/wasm-host`**, built with the Rust SDK and `wasmtime`
   (component model). It loads configured `.wasm` components and registers their capabilities
   with the core through the normal ABI. The core and `medicus-host` do not depend on wasmtime:
   deployments that don't want WASM don't load the addon, and the core vs addon rule (PLAN §1)
   holds.
2. **Interface: WIT package `medicus:addon`** (in `wit/`, versioned with semver independently of
   the C ABI). Every supported capability is one *world*, and a component may target several:
   - `event-hook`: `filter() -> list<string>`, `on-event(event: string) -> result<_, error>`
   - `instance-filter`: inspect the header of an incoming instance (DICOM JSON) →
     `accept | reject(reason) | modify(patch)`
   - `eviction-policy`: `on-pressure(info) -> list<candidate>`, `on-schedule() -> list<candidate>`
   - `http-routes` (later): bounded request/response bodies, no streaming
   - Host imports: `log`, `config() -> string`, and (later) read-only `query` against the index.

   Payloads that evolve (event envelope, query results, datasets) are passed as JSON strings, as in
   the C ABI, so one serde schema serves both. Stable small types (verdicts, error kinds,
   candidates) are WIT records and variants, which gives guest languages typed bindings.
3. **Not offered to WASM**: `BlobStore`, `IndexStore`, `JobStore`, protocol listeners. These stay
   native for performance and I/O reasons.
4. **Sandbox and limits**, configured per component (defaults in brackets):
   - memory limit [64 MiB], per-call deadline using epoch interruption [5 s],
   - instance pool size = concurrency [4]. Component instances are single-threaded, so concurrent
     calls go to different pooled instances.
   - no WASI filesystem, sockets or clocks unless granted. Outbound HTTP (`wasi:http`) only to an
     allow-list of URL prefixes (e.g. for webhooks).
   - a trap (panic, out of memory, deadline) fails that call with `Panicked` /
     `ResourceExhausted`, discards the instance and replaces it. Repeated traps mark the component
     unhealthy (`addon.unhealthy`).
5. **Packaging**: `<name>.wasm` plus `<name>.addon.toml` with `kind = "wasm"`, the worlds it
   implements, and its permissions. Compiled components are cached on disk (keyed by component hash
   and wasmtime version), so startup does not recompile.
6. **Async model**: guests see synchronous functions. The host runs calls on wasmtime's async
   support with epoch-based deadlines. Moving to WASI 0.3 native async later only changes the WIT
   package's major version, not the medicus capability traits.

## Consequences
- **Positive**: third parties can extend medicus safely, portably and in many languages without
  making the server less robust. One artifact covers all three OSes. The capability traits are
  reused unchanged, and WASM capabilities are indistinguishable from native ones for the core.
- **Negative**: `wasm-host` has a large dependency tree (wasmtime) and longer CI build times, which
  only that addon pays. There is a second interface definition (WIT) to keep in sync with the
  capability traits, enforced by conformance tests that run the same scenarios against a native and
  a WASM implementation. Per-call copying makes WASM unsuitable for bulk data, which is why bulk-data
  capabilities are excluded.

## Alternatives considered
- **WASM inside the core**: simpler wiring, but every deployment pays for wasmtime, and it would
  break the core vs addon rule.
- **Plain WASM modules (no component model)**: no typed interfaces, hand-written memory marshalling
  per language. Rejected.
- **Scripting languages (Lua, JS engines)**: similar sandboxing benefits but a single language, and
  usually weaker resource isolation. WASM covers these use cases more generally.
