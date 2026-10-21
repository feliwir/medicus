//! Capability trait objects → C vtables.
//!
//! Every exported function follows the same pattern: wrap the received completion in a guard
//! *first*, then do the work inside `guarded`, so that the completion fires exactly once even if
//! argument decoding or the implementation panics.

use std::ffi::c_void;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use futures::FutureExt as _;
use medicus_abi::bridge::{CompletionGuard, StreamCompletionGuard, export_stream, import_stream};
use medicus_abi::ffi::{
    MD_PANICKED, MdBlobStoreVtable, MdBuffer, MdCancel, MdCompletion, MdEventHookVtable, MdRange,
    MdReadStream, MdStr, MdStreamCompletion,
};
use medicus_core::BlobKey;
use medicus_core::capability::{BlobStore, EventHook};
use medicus_core::event::Event;
use medicus_core::{Error, Result};
use serde::Serialize;

use crate::runtime::runtime;

fn to_json<T: Serialize>(value: &T) -> Result<MdBuffer> {
    serde_json::to_vec(value)
        .map(MdBuffer::from_vec)
        .map_err(|e| Error::internal(format!("cannot serialize result: {e}")))
}

/// Runs `f`, swallowing a panic. `f` owns the completion guard, so an unwinding panic completes
/// the call with `MD_PANICKED`.
fn guarded(f: impl FnOnce()) {
    let _ = catch_unwind(AssertUnwindSafe(f));
}

/// Spawns `fut` on the addon runtime and completes `guard` with its JSON-encoded result. A panic
/// inside the future is reported as `MD_PANICKED`.
fn spawn_json<T, F>(guard: CompletionGuard, fut: F)
where
    T: Serialize,
    F: Future<Output = Result<T>> + Send + 'static,
{
    runtime().spawn(async move {
        match AssertUnwindSafe(fut).catch_unwind().await {
            Ok(result) => guard.finish(result.and_then(|v| to_json(&v))),
            Err(_) => guard.complete(MD_PANICKED, MdBuffer::from_string("addon panicked".into())),
        }
    });
}

/// # Safety
/// `key` must be a valid borrowed string for the duration of the call.
unsafe fn parse_key(key: MdStr) -> Result<BlobKey> {
    // SAFETY: forwarded to the caller.
    let s = unsafe { key.to_str() }?;
    BlobKey::parse(s).map_err(|e| Error::invalid_argument(e.to_string()))
}

// ------------------------------------------------------------------------------------------------
// BlobStore

type DynBlobStore = Arc<dyn BlobStore>;

static BLOB_STORE_VTABLE: MdBlobStoreVtable = MdBlobStoreVtable {
    size: size_of::<MdBlobStoreVtable>(),
    put: blob_put,
    get: blob_get,
    stat: blob_stat,
    remove: blob_delete,
    usage: blob_usage,
    destroy: destroy_blob_store,
};

pub(crate) fn blob_store(store: DynBlobStore) -> (*const MdBlobStoreVtable, *mut c_void) {
    (
        &raw const BLOB_STORE_VTABLE,
        Box::into_raw(Box::new(store)).cast(),
    )
}

/// # Safety
/// `instance` must come from [`blob_store`] and not be destroyed yet.
unsafe fn blob(instance: *mut c_void) -> DynBlobStore {
    // SAFETY: guaranteed by the caller.
    unsafe { &*instance.cast::<DynBlobStore>() }.clone()
}

unsafe extern "C" fn blob_put(
    instance: *mut c_void,
    key: MdStr,
    data: MdReadStream,
    _cancel: MdCancel,
    done: MdCompletion,
) {
    let guard = CompletionGuard::new(done);
    // SAFETY: ownership of `data` is transferred to us; importing first guarantees release.
    let data = unsafe { import_stream(data) };
    guarded(move || {
        // SAFETY: the host only calls live instances; `key` is valid during this call.
        let (store, key) = unsafe { (blob(instance), parse_key(key)) };
        match key {
            Ok(key) => spawn_json(guard, async move { store.put(&key, data).await }),
            Err(e) => guard.err(&e),
        }
    });
}

unsafe extern "C" fn blob_get(
    instance: *mut c_void,
    key: MdStr,
    range: MdRange,
    _cancel: MdCancel,
    done: MdStreamCompletion,
) {
    let guard = StreamCompletionGuard::new(done);
    guarded(move || {
        // SAFETY: as in `blob_put`.
        let (store, key) = unsafe { (blob(instance), parse_key(key)) };
        let key = match key {
            Ok(k) => k,
            Err(e) => return guard.err(&e),
        };
        runtime().spawn(async move {
            match AssertUnwindSafe(store.get(&key, range.to_range()))
                .catch_unwind()
                .await
            {
                Ok(Ok(stream)) => guard.ok(export_stream(stream, runtime())),
                Ok(Err(e)) => guard.err(&e),
                Err(_) => guard.err(&Error::new(
                    medicus_core::ErrorKind::Panicked,
                    "addon panicked",
                )),
            }
        });
    });
}

unsafe extern "C" fn blob_stat(
    instance: *mut c_void,
    key: MdStr,
    _cancel: MdCancel,
    done: MdCompletion,
) {
    let guard = CompletionGuard::new(done);
    guarded(move || {
        // SAFETY: as in `blob_put`.
        let (store, key) = unsafe { (blob(instance), parse_key(key)) };
        match key {
            Ok(key) => spawn_json(guard, async move { store.stat(&key).await }),
            Err(e) => guard.err(&e),
        }
    });
}

unsafe extern "C" fn blob_delete(
    instance: *mut c_void,
    key: MdStr,
    _cancel: MdCancel,
    done: MdCompletion,
) {
    let guard = CompletionGuard::new(done);
    guarded(move || {
        // SAFETY: as in `blob_put`.
        let (store, key) = unsafe { (blob(instance), parse_key(key)) };
        match key {
            Ok(key) => spawn_json(guard, async move { store.delete(&key).await }),
            Err(e) => guard.err(&e),
        }
    });
}

unsafe extern "C" fn blob_usage(instance: *mut c_void, _cancel: MdCancel, done: MdCompletion) {
    let guard = CompletionGuard::new(done);
    guarded(move || {
        // SAFETY: as in `blob_put`.
        let store = unsafe { blob(instance) };
        spawn_json(guard, async move { store.usage().await });
    });
}

pub(crate) unsafe extern "C" fn destroy_blob_store(instance: *mut c_void) {
    guarded(|| {
        // SAFETY: called exactly once per instance created by `blob_store`.
        drop(unsafe { Box::from_raw(instance.cast::<DynBlobStore>()) });
    });
}

// ------------------------------------------------------------------------------------------------
// EventHook

type DynEventHook = Arc<dyn EventHook>;

static EVENT_HOOK_VTABLE: MdEventHookVtable = MdEventHookVtable {
    size: size_of::<MdEventHookVtable>(),
    filter: hook_filter,
    on_event: hook_on_event,
    destroy: destroy_event_hook,
};

pub(crate) fn event_hook(hook: DynEventHook) -> (*const MdEventHookVtable, *mut c_void) {
    (
        &raw const EVENT_HOOK_VTABLE,
        Box::into_raw(Box::new(hook)).cast(),
    )
}

/// # Safety
/// `instance` must come from [`event_hook`] and not be destroyed yet.
unsafe fn hook(instance: *mut c_void) -> DynEventHook {
    // SAFETY: guaranteed by the caller.
    unsafe { &*instance.cast::<DynEventHook>() }.clone()
}

unsafe extern "C" fn hook_filter(instance: *mut c_void) -> MdBuffer {
    // SAFETY: the host only calls live instances.
    catch_unwind(AssertUnwindSafe(|| {
        to_json(&unsafe { hook(instance) }.filter())
    }))
    .ok()
    .and_then(Result::ok)
    .unwrap_or_else(MdBuffer::empty)
}

unsafe extern "C" fn hook_on_event(
    instance: *mut c_void,
    event: MdStr,
    _cancel: MdCancel,
    done: MdCompletion,
) {
    let guard = CompletionGuard::new(done);
    guarded(move || {
        // SAFETY: live instance; `event` is valid during this call.
        let (hook, bytes) = unsafe { (hook(instance), event.as_bytes()) };
        match serde_json::from_slice::<Event>(bytes) {
            Ok(event) => spawn_json(guard, async move { hook.on_event(&event).await }),
            Err(e) => guard.err(&Error::invalid_argument(format!("invalid event: {e}"))),
        }
    });
}

pub(crate) unsafe extern "C" fn destroy_event_hook(instance: *mut c_void) {
    guarded(|| {
        // SAFETY: called exactly once per instance created by `event_hook`.
        drop(unsafe { Box::from_raw(instance.cast::<DynEventHook>()) });
    });
}
