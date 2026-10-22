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
    MD_PANICKED, MdBlobStoreVtable, MdBuffer, MdCancel, MdCompletion, MdEventHookVtable,
    MdIndexStoreVtable, MdRange, MdReadStream, MdRemoteNodeVtable, MdServiceVtable, MdStr,
    MdStreamCompletion,
};
use medicus_core::BlobKey;
use medicus_core::capability::index::IndexRequest;
use medicus_core::capability::{
    BlobStore, EventHook, IndexStore, RemoteNode, RemoteRequest, Service,
};
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

// ------------------------------------------------------------------------------------------------
// IndexStore

type DynIndexStore = Arc<dyn IndexStore>;

static INDEX_STORE_VTABLE: MdIndexStoreVtable = MdIndexStoreVtable {
    size: size_of::<MdIndexStoreVtable>(),
    call: index_call,
    destroy: destroy_index_store,
};

pub(crate) fn index_store(index: DynIndexStore) -> (*const MdIndexStoreVtable, *mut c_void) {
    (
        &raw const INDEX_STORE_VTABLE,
        Box::into_raw(Box::new(index)).cast(),
    )
}

/// Decodes a request; distinguishes methods unknown to this SDK version (`Unsupported`) from
/// malformed arguments (`InvalidArgument`).
fn decode_index_request(bytes: &[u8]) -> Result<IndexRequest> {
    serde_json::from_slice(bytes).map_err(|e| {
        let method = serde_json::from_slice::<serde_json::Value>(bytes)
            .ok()
            .and_then(|v| v.get("method").and_then(|m| m.as_str().map(str::to_owned)));
        match method {
            Some(m) if !IndexRequest::METHODS.contains(&m.as_str()) => {
                Error::unsupported(format!("index method {m:?} is not supported by this addon"))
            }
            _ => Error::invalid_argument(format!("invalid index request: {e}")),
        }
    })
}

async fn dispatch_index(
    index: &dyn IndexStore,
    request: IndexRequest,
) -> Result<serde_json::Value> {
    fn json<T: Serialize>(v: &T) -> Result<serde_json::Value> {
        serde_json::to_value(v).map_err(|e| Error::internal(e.to_string()))
    }
    match request {
        IndexRequest::StoreInstance {
            record,
            policy,
            event,
        } => json(&index.store_instance(&record, policy, &event).await?),
        IndexRequest::Query { partition, query } => json(&index.query(&partition, &query).await?),
        IndexRequest::Locate {
            partition,
            resource,
        } => json(&index.locate(&partition, &resource).await?),
        IndexRequest::Delete {
            partition,
            resource,
            event,
        } => json(&index.delete(&partition, &resource, &event).await?),
        IndexRequest::AppendEvent { event } => json(&index.append_event(&event).await?),
        IndexRequest::EventsAfter { after, limit } => {
            json(&index.events_after(after, limit).await?)
        }
        IndexRequest::LastEventSeq => json(&index.last_event_seq().await?),
        IndexRequest::Cursor { subscriber } => json(&index.cursor(&subscriber).await?),
        IndexRequest::SetCursor { subscriber, seq } => {
            json(&index.set_cursor(&subscriber, seq).await?)
        }
        IndexRequest::PruneEvents { up_to } => json(&index.prune_events(up_to).await?),
        IndexRequest::StoredBytes => json(&index.stored_bytes().await?),
        IndexRequest::ConfigureExtraTags { tags } => {
            json(&index.configure_extra_tags(&tags).await?)
        }
        IndexRequest::UpdateExtra {
            partition,
            instance,
            attributes,
        } => json(
            &index
                .update_extra(&partition, &instance, &attributes)
                .await?,
        ),
        IndexRequest::MarkExtraReady { tag } => json(&index.mark_extra_ready(tag).await?),
        IndexRequest::PutJob { job } => json(&index.put_job(&job).await?),
        IndexRequest::GetJob { id } => json(&index.get_job(id).await?),
        IndexRequest::ListJobs {
            state,
            limit,
            offset,
        } => json(&index.list_jobs(state, limit, offset).await?),
        IndexRequest::ClaimJob { now } => json(&index.claim_job(now).await?),
    }
}

unsafe extern "C" fn index_call(
    instance: *mut c_void,
    request: MdStr,
    _cancel: MdCancel,
    done: MdCompletion,
) {
    let guard = CompletionGuard::new(done);
    guarded(move || {
        // SAFETY: live instance; `request` is valid during this call.
        let (index, bytes) = unsafe { (&*instance.cast::<DynIndexStore>(), request.as_bytes()) };
        let index = index.clone();
        match decode_index_request(bytes) {
            Ok(req) => spawn_json(
                guard,
                async move { dispatch_index(index.as_ref(), req).await },
            ),
            Err(e) => guard.err(&e),
        }
    });
}

pub(crate) unsafe extern "C" fn destroy_index_store(instance: *mut c_void) {
    guarded(|| {
        // SAFETY: called exactly once per instance created by `index_store`.
        drop(unsafe { Box::from_raw(instance.cast::<DynIndexStore>()) });
    });
}

// ------------------------------------------------------------------------------------------------
// RemoteNode

type DynRemoteNode = Arc<dyn RemoteNode>;

static REMOTE_NODE_VTABLE: MdRemoteNodeVtable = MdRemoteNodeVtable {
    size: size_of::<MdRemoteNodeVtable>(),
    call: remote_call,
    destroy: destroy_remote_node,
};

pub(crate) fn remote_node(node: DynRemoteNode) -> (*const MdRemoteNodeVtable, *mut c_void) {
    (
        &raw const REMOTE_NODE_VTABLE,
        Box::into_raw(Box::new(node)).cast(),
    )
}

async fn dispatch_remote(
    node: &dyn RemoteNode,
    request: RemoteRequest,
) -> Result<serde_json::Value> {
    fn json<T: Serialize>(v: &T) -> Result<serde_json::Value> {
        serde_json::to_value(v).map_err(|e| Error::internal(e.to_string()))
    }
    match request {
        RemoteRequest::Echo { peer } => json(&node.echo(&peer).await?),
        RemoteRequest::Send {
            job,
            node: info,
            peer,
            instances,
        } => json(&node.send(job, &info, &peer, &instances).await?),
    }
}

unsafe extern "C" fn remote_call(
    instance: *mut c_void,
    request: MdStr,
    _cancel: MdCancel,
    done: MdCompletion,
) {
    let guard = CompletionGuard::new(done);
    guarded(move || {
        // SAFETY: live instance; `request` is valid during this call.
        let (node, bytes) = unsafe { (&*instance.cast::<DynRemoteNode>(), request.as_bytes()) };
        let node = node.clone();
        match serde_json::from_slice::<RemoteRequest>(bytes) {
            Ok(req) => spawn_json(
                guard,
                async move { dispatch_remote(node.as_ref(), req).await },
            ),
            Err(e) => guard.err(&Error::invalid_argument(format!("invalid request: {e}"))),
        }
    });
}

pub(crate) unsafe extern "C" fn destroy_remote_node(instance: *mut c_void) {
    guarded(|| {
        // SAFETY: called exactly once per instance created by `remote_node`.
        drop(unsafe { Box::from_raw(instance.cast::<DynRemoteNode>()) });
    });
}

// ------------------------------------------------------------------------------------------------
// Service

type DynService = Arc<dyn Service>;

static SERVICE_VTABLE: MdServiceVtable = MdServiceVtable {
    size: size_of::<MdServiceVtable>(),
    start: service_start,
    stop: service_stop,
    destroy: destroy_service,
};

pub(crate) fn service(service: DynService) -> (*const MdServiceVtable, *mut c_void) {
    (
        &raw const SERVICE_VTABLE,
        Box::into_raw(Box::new(service)).cast(),
    )
}

/// # Safety
/// `instance` must come from [`service`] and not be destroyed yet.
unsafe fn service_of(instance: *mut c_void) -> DynService {
    // SAFETY: guaranteed by the caller.
    unsafe { &*instance.cast::<DynService>() }.clone()
}

unsafe extern "C" fn service_start(instance: *mut c_void, done: MdCompletion) {
    let guard = CompletionGuard::new(done);
    guarded(move || {
        // SAFETY: the host only calls live instances.
        let s = unsafe { service_of(instance) };
        spawn_json(guard, async move { s.start().await });
    });
}

unsafe extern "C" fn service_stop(instance: *mut c_void, done: MdCompletion) {
    let guard = CompletionGuard::new(done);
    guarded(move || {
        // SAFETY: the host only calls live instances.
        let s = unsafe { service_of(instance) };
        spawn_json(guard, async move { s.stop().await });
    });
}

pub(crate) unsafe extern "C" fn destroy_service(instance: *mut c_void) {
    guarded(|| {
        // SAFETY: called exactly once per instance created by `service`.
        drop(unsafe { Box::from_raw(instance.cast::<DynService>()) });
    });
}
