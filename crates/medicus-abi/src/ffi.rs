//! Raw C ABI. Everything here must be expressible in C; `include/medicus_addon.h` is generated
//! from this module (see `tests/header.rs`).
//!
//! # Rules (normative, see ADR-0001)
//! - Strings are UTF-8 `(ptr, len)` ([`MdStr`]), never NUL-terminated.
//! - [`MdStr`] arguments are borrowed for the duration of the call only.
//! - [`MdBuffer`]s are owned by the receiver, which must call `release` exactly once. Memory is
//!   always freed by the side that allocated it.
//! - Every [`MdCompletion`] / [`MdStreamCompletion`] is completed exactly once, from any thread,
//!   possibly before the call that received it returns.
//! - An [`MdReadStream`] passed to a function is owned by the receiver, which must call `release`.
//!   At most one `read` is outstanding at a time. `release` may be called while a read is still
//!   outstanding (the consumer stopped early); the producer must still complete that read and free
//!   its resources only afterwards.
//! - Structs and vtables starting with `size` may grow by appending fields in minor versions.
//! - Capability instances must be safe to call concurrently from multiple threads.

use core::ffi::c_void;

/// Major ABI version. While it is 0, every minor change is breaking.
pub const MD_ABI_MAJOR: u32 = 0;
/// Minor ABI version.
pub const MD_ABI_MINOR: u32 = 1;
/// `(major << 16) | minor`, as returned by `medicus_addon_abi_version`.
pub const MD_ABI_VERSION: u32 = (MD_ABI_MAJOR << 16) | MD_ABI_MINOR;

/// Status code of an operation. `MD_OK` is success; on failure the completion's result buffer may
/// carry a UTF-8 error message.
pub type MdStatus = i32;
pub const MD_OK: MdStatus = 0;
pub const MD_INVALID_ARGUMENT: MdStatus = 1;
pub const MD_NOT_FOUND: MdStatus = 2;
pub const MD_ALREADY_EXISTS: MdStatus = 3;
pub const MD_REJECTED: MdStatus = 4;
pub const MD_RESOURCE_EXHAUSTED: MdStatus = 5;
pub const MD_UNAVAILABLE: MdStatus = 6;
pub const MD_CANCELLED: MdStatus = 7;
pub const MD_UNSUPPORTED: MdStatus = 8;
pub const MD_INTERNAL: MdStatus = 9;
pub const MD_PANICKED: MdStatus = 10;

/// Capability kinds for `MdHostApi::register_capability`.
pub const MD_CAP_BLOB_STORE: u32 = 1;
pub const MD_CAP_EVENT_HOOK: u32 = 2;

/// Log levels for `MdHostApi::log`.
pub const MD_LOG_ERROR: u32 = 1;
pub const MD_LOG_WARN: u32 = 2;
pub const MD_LOG_INFO: u32 = 3;
pub const MD_LOG_DEBUG: u32 = 4;
pub const MD_LOG_TRACE: u32 = 5;

/// Borrowed UTF-8 string or byte slice.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct MdStr {
    pub ptr: *const u8,
    pub len: usize,
}

/// Owned bytes. The receiver calls `release(ctx, ptr, len)` exactly once (if non-null).
#[repr(C)]
#[derive(Debug)]
pub struct MdBuffer {
    pub ptr: *mut u8,
    pub len: usize,
    pub ctx: *mut c_void,
    pub release: Option<unsafe extern "C" fn(ctx: *mut c_void, ptr: *mut u8, len: usize)>,
}

/// One-shot callback delivering a status and an owned result buffer.
#[repr(C)]
#[derive(Debug)]
pub struct MdCompletion {
    pub ctx: *mut c_void,
    pub complete: unsafe extern "C" fn(ctx: *mut c_void, status: MdStatus, result: MdBuffer),
}

/// Advisory cancellation signal for an in-flight call. `is_cancelled` may be null (never
/// cancelled). Valid until the call's completion has fired.
#[repr(C)]
#[derive(Debug)]
pub struct MdCancel {
    pub size: usize,
    pub ctx: *mut c_void,
    pub is_cancelled: Option<unsafe extern "C" fn(ctx: *mut c_void) -> u8>,
}

/// Functions of a pull-based byte stream.
#[repr(C)]
#[derive(Debug)]
pub struct MdReadStreamVtable {
    pub size: usize,
    /// Reads the next chunk. An empty buffer with `MD_OK` signals end of stream.
    pub read: unsafe extern "C" fn(instance: *mut c_void, done: MdCompletion),
    /// Releases the stream. Called exactly once, possibly while a `read` is still outstanding.
    pub release: unsafe extern "C" fn(instance: *mut c_void),
}

/// An owned byte stream.
#[repr(C)]
#[derive(Debug)]
pub struct MdReadStream {
    pub instance: *mut c_void,
    pub vtable: *const MdReadStreamVtable,
}

/// One-shot callback delivering a stream (on `MD_OK`) or an error message.
#[repr(C)]
#[derive(Debug)]
pub struct MdStreamCompletion {
    pub ctx: *mut c_void,
    pub complete: unsafe extern "C" fn(
        ctx: *mut c_void,
        status: MdStatus,
        stream: MdReadStream,
        error: MdBuffer,
    ),
}

/// Functions the host provides to an addon. Valid for the lifetime of the process.
#[repr(C)]
#[derive(Debug)]
pub struct MdHostApi {
    pub size: usize,
    pub abi_version: u32,
    /// Opaque host context, passed back to every function.
    pub ctx: *mut c_void,
    pub log: unsafe extern "C" fn(ctx: *mut c_void, level: u32, target: MdStr, message: MdStr),
    /// The addon's configuration as JSON (owned by the caller).
    pub config: unsafe extern "C" fn(ctx: *mut c_void) -> MdBuffer,
    /// Registers a capability instance. `vtable` points to the vtable struct matching `kind` and
    /// must stay valid for the lifetime of the process.
    pub register_capability: unsafe extern "C" fn(
        ctx: *mut c_void,
        kind: u32,
        name: MdStr,
        vtable: *const c_void,
        instance: *mut c_void,
    ) -> MdStatus,
}

/// Information returned by `medicus_addon_init`. Must stay valid for the lifetime of the process.
#[repr(C)]
#[derive(Debug)]
pub struct MdAddonInfo {
    pub size: usize,
    pub name: MdStr,
    pub version: MdStr,
    /// `<arch>-<os>`, compared against the host's.
    pub target: MdStr,
}

/// Optional byte range `[start, end)`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct MdRange {
    pub has_range: u8,
    pub start: u64,
    pub end: u64,
}

/// `MD_CAP_BLOB_STORE`. Results are JSON unless stated otherwise.
#[repr(C)]
#[derive(Debug)]
pub struct MdBlobStoreVtable {
    pub size: usize,
    /// Takes ownership of `data`. Result: `{"size": u64}`.
    pub put: unsafe extern "C" fn(
        instance: *mut c_void,
        key: MdStr,
        data: MdReadStream,
        cancel: MdCancel,
        done: MdCompletion,
    ),
    /// Result: a stream of the blob (or range) contents.
    pub get: unsafe extern "C" fn(
        instance: *mut c_void,
        key: MdStr,
        range: MdRange,
        cancel: MdCancel,
        done: MdStreamCompletion,
    ),
    /// Result: `null` or `{"size": u64}`.
    pub stat: unsafe extern "C" fn(
        instance: *mut c_void,
        key: MdStr,
        cancel: MdCancel,
        done: MdCompletion,
    ),
    /// Result: empty. Idempotent.
    pub remove: unsafe extern "C" fn(
        instance: *mut c_void,
        key: MdStr,
        cancel: MdCancel,
        done: MdCompletion,
    ),
    /// Result: `null` or `{"capacity": u64, "used": u64, "available": u64}`.
    pub usage: unsafe extern "C" fn(instance: *mut c_void, cancel: MdCancel, done: MdCompletion),
    /// Called once at shutdown; no other call is in flight or follows.
    pub destroy: unsafe extern "C" fn(instance: *mut c_void),
}

/// `MD_CAP_EVENT_HOOK`.
#[repr(C)]
#[derive(Debug)]
pub struct MdEventHookVtable {
    pub size: usize,
    /// Synchronous. Returns the event filter as JSON (`{"kinds": [...]}`), owned by the caller.
    pub filter: unsafe extern "C" fn(instance: *mut c_void) -> MdBuffer,
    /// `event` is the JSON event envelope. Result: empty. A failure requests redelivery.
    pub on_event: unsafe extern "C" fn(
        instance: *mut c_void,
        event: MdStr,
        cancel: MdCancel,
        done: MdCompletion,
    ),
    pub destroy: unsafe extern "C" fn(instance: *mut c_void),
}

/// Signature of the exported `medicus_addon_abi_version` symbol.
pub type MdAbiVersionFn = unsafe extern "C" fn() -> u32;
/// Signature of the exported `medicus_addon_init` symbol. Returns NULL on failure.
pub type MdInitFn = unsafe extern "C" fn(host: *const MdHostApi) -> *const MdAddonInfo;
