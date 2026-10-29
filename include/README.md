# Writing medicus addons in C (or any language with a C ABI)

`medicus_addon.h` is the complete, versioned contract between the medicus server and an addon.
It is generated from `crates/medicus-abi/src/ffi.rs`; a test fails whenever the two disagree.
Rust authors should use `medicus-addon-sdk` instead, which implements all rules below.

A complete, tested example is [`addons/example-c/example_c.c`](../addons/example-c/example_c.c)
(an in-memory `BlobStore` and an `EventHook` in about 463 lines of C11).

## 1. Building

Build a shared library (`.so` / `.dylib` / `.dll`) that exports exactly three symbols, declared
in the header with `MEDICUS_ADDON_EXPORT`:

```c
MEDICUS_ADDON_EXPORT uint32_t medicus_addon_abi_version(void) { return MD_ABI_VERSION; }
MEDICUS_ADDON_EXPORT MdStr medicus_addon_manifest(void);
MEDICUS_ADDON_EXPORT MdStatus medicus_addon_init(const MdHostApi *host);
```

```sh
cc -std=c11 -shared -fPIC -fvisibility=hidden -pthread -I include my_addon.c -o libmy_addon.so
cl /LD /I include my_addon.c /Femy_addon.dll        # MSVC
```

Build once per target (`<arch>-<os>`); the manifest's `target` must equal the host's, e.g.
`x86_64-linux`, `aarch64-macos`, `x86_64-windows`.

### Manifest file

Ship a manifest file `<name>.addon.toml` next to the library. The server discovers addons in its
`addons.search_paths` through these files, without loading any library, so an addon can be
listed, and enabled by name, before its code ever runs. Generate it from the built library:

```sh
medicus addon manifest libmy_addon.so     # writes my-addon.addon.toml next to it
```

It repeats the embedded manifest (TOML instead of JSON) and adds the `library` path, relative to
the file, and its `sha256`. When the addon is loaded, the server checks the checksum and refuses
a library whose embedded manifest differs from the file.

## 2. Initialization

1. The host calls `medicus_addon_abi_version()` first and rejects incompatible addons before
   anything else happens. While `MD_ABI_MAJOR` is 0, minor versions must match exactly.
2. The host calls `medicus_addon_manifest()` and parses the returned JSON (a constant string that
   lives forever):

   ```json
   {"name": "my-addon", "version": "1.0.0", "target": "x86_64-linux",
    "provides": ["blob_store"], "requires": [{"capability": "index_store"}], "required": false}
   ```

   `provides` lists the capability kinds you register (`blob_store`, `event_hook`, `index_store`,
   `service`, `remote_node`); registering any other kind is refused with `MD_REJECTED`.
   `requires` lists what your addon needs, optionally from a specific addon (`"addon": "name"`);
   the host loads those providers first, and pulls them in if they are not enabled. The host
   reads every candidate's manifest before initializing any addon, so the manifest must not
   depend on initialization.
3. The host calls `medicus_addon_init(host)`. Validate `host->size >= sizeof(MdHostApi)` and
   `host->abi_version`. Keep the `host` pointer: it stays valid for the rest of the process, and
   all its functions are thread-safe.
4. Read your configuration with `host->config(host->ctx)` (JSON; release the buffer).
5. Register each capability with `host->register_capability(ctx, kind, name, &VTABLE, instance)`.
   The vtable must live forever (a `static const`); on `MD_OK` the host owns `instance` and calls
   the vtable's `destroy` at shutdown. On any other status the host did **not** take ownership.
6. Return `MD_OK`, or another status on failure (log why first).

Registration is only possible during `medicus_addon_init`. Libraries are never unloaded.

## 3. Memory ownership

| Type | Rule |
|---|---|
| `MdStr` argument | Borrowed **for the duration of the call only**. Copy it if you need it later (e.g. a key used by an asynchronous operation). |
| `MdBuffer` you **receive** | You own it; call `release(ctx, ptr, len)` exactly once if `release` is non-NULL. |
| `MdBuffer` you **return/pass** | The receiver will call your `release`. Memory is always freed by the side that allocated it, which is mandatory on Windows, where every DLL may have its own heap. |
| `MdReadStream` you **receive** | You own it; call `vtable->release(instance)` exactly once. |
| `MdReadStream` you **return** | You must keep it valid until the host calls `release`. |
| Capability `instance` | Owned by the host after successful registration; freed by your `destroy`. |

Strings are UTF-8 with explicit length and **no** terminating NUL.

## 4. Completions (asynchrony)

Every potentially blocking call receives an `MdCompletion` (or `MdStreamCompletion`) and returns
immediately (or after completing synchronously).

- Call `done.complete(done.ctx, status, result)` **exactly once**, on success and on every error
  path. Forgetting it hangs the caller; calling it twice is undefined behaviour.
- It may be called from **any thread**, and also before the function that received it returns.
- On error, pass a UTF-8 message buffer as `result` (or an empty buffer). Status codes are the
  `MD_*` constants; prefer specific ones (`MD_NOT_FOUND`, `MD_INVALID_ARGUMENT`, ...) over
  `MD_INTERNAL`.
- JSON result formats are documented on each vtable field in the header.
- `MdCancel` is advisory: you may poll `cancel.is_cancelled(cancel.ctx)` (if non-NULL) during long
  work and complete with `MD_CANCELLED`, but you must still complete.

## 5. Streams

- Pull-based: call `vtable->read(instance, completion)`; the completion delivers the next chunk.
  **An empty chunk with `MD_OK` means end of stream.** Never return an empty chunk otherwise.
- At most one `read` may be outstanding. Issue the next read from the completion of the previous
  one (see `put_on_chunk` in the example).
- `release` may arrive while a read is still outstanding (the consumer stopped early). A stream
  you produce must still complete that read and free its resources only afterwards.
- An `MdStreamCompletion` hands over a stream with `MD_OK`. Its `data` buffer is the error message
  on failure; on success it is empty, except for `MD_CAP_HTTP_ROUTES`, where it carries the
  response head (`{"status": 200, "headers": [["content-type", "text/html"]]}`).

## 6. Threading

- Every capability function can be called **concurrently from multiple threads**; protect shared
  state (the example uses `SRWLOCK` on Windows and `pthread_mutex_t` elsewhere).
- Completions and stream reads may arrive on threads you did not create; don't rely on
  thread-local state across them.
- `destroy` is called once, when no other call is in flight.

## 7. Errors and crashes

The host contains panics of Rust addons, but **a crash in native code (segfault, abort) takes the
whole server down**. Validate inputs, check every allocation, and test with sanitizers, e.g.:

```sh
MEDICUS_C_SANITIZE=address cargo test -p medicus-host --test abi --no-run
LD_PRELOAD=$(cc -print-file-name=libasan.so) target/debug/deps/abi-<hash> c_addon
```

If you need isolation from untrusted logic, write a WASM addon instead (ADR-0002).

## 8. Compatibility

Structs and vtables starting with `size` may grow at the end in minor versions; never read a
field beyond `size`, and always set `size = sizeof(...)` in structs you create. Changes are listed
in [`CHANGELOG.md`](CHANGELOG.md).

## 9. Host services

Protocol addons reach the archive through `MdHostApi`, never through other addons:

- Every data operation names a **call context** `{"node": <node name>, "origin": <who is
  calling>}`: the core checks the caller's permission on the node (`MD_PERMISSION_DENIED`
  otherwise) and works in the node's partition only. `nodes` lists the nodes to serve.
- `ingest` takes a read stream of a Part 10 file and the call context (JSON) and completes with
  the JSON ingest outcome.
- `call` takes a method name and a JSON request and completes with the JSON response:
  `query` (a query AST, as in `medicus_core::query::Query` → `QueryResult`), `locate` (`{"level":
  "study" | "series" | "instance", "uid": ...}` → stored instances) and `peers` (`null` → configured
  peers).
- `open_instance` takes `{"context", "instance": <SOP Instance UID>}` and completes with a read
  stream of the stored file.

A protocol addon can also register a `MD_CAP_REMOTE_NODE` under its protocol's name (e.g.
`dimse`) to serve the archive's peers of that protocol: its `call` gets `{"method": "echo",
"peer": ...}` or `{"method": "send", "job": ..., "peer": ..., "instances": [...]}`, and a send
reports progress with the host method `job_progress`, whose `false` result means the job was
cancelled.

All of them fail with `MD_UNAVAILABLE` until the archive is ready; background services are only
started after that.
