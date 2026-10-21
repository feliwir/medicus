//! C vtables → core capability traits.

use std::ffi::c_void;
use std::ops::Range;

use async_trait::async_trait;
use medicus_abi::bridge::{
    Completion, OwnedBuffer, StreamCompletion, completion, export_stream, stream_completion,
};
use medicus_abi::ffi::{MdBlobStoreVtable, MdCancel, MdEventHookVtable, MdRange, MdStr};
use medicus_core::BlobKey;
use medicus_core::capability::{
    BlobMeta, BlobStore, ByteStream, EventFilter, EventHook, StorageUsage,
};
use medicus_core::event::Event;
use medicus_core::{Error, Result};
use serde::de::DeserializeOwned;
use tokio::runtime::Handle;

fn from_json<T: DeserializeOwned>(buf: &OwnedBuffer) -> Result<T> {
    serde_json::from_slice(buf.as_slice())
        .map_err(|e| Error::internal(format!("addon returned invalid JSON: {e}")))
}

/// A `BlobStore` implemented by an addon.
pub(crate) struct FfiBlobStore {
    pub addon: String,
    pub vtable: &'static MdBlobStoreVtable,
    pub instance: *mut c_void,
    /// Runtime serving reads of streams we hand to the addon.
    pub rt: Handle,
}

// SAFETY: the ABI requires capability instances to be callable concurrently from any thread.
unsafe impl Send for FfiBlobStore {}
// SAFETY: as above.
unsafe impl Sync for FfiBlobStore {}

// The `start_*` functions perform the synchronous FFI call and return a future that no longer
// holds raw pointers, keeping the async trait methods `Send`.
impl FfiBlobStore {
    fn start_put(&self, key: &BlobKey, data: ByteStream) -> Completion {
        let (done, fut) = completion();
        let data = export_stream(data, &self.rt);
        // SAFETY: live instance; `key` outlives the call; ownership of `data` moves to the addon.
        unsafe {
            (self.vtable.put)(
                self.instance,
                MdStr::new(key.as_str()),
                data,
                MdCancel::none(),
                done,
            );
        };
        fut
    }

    fn start_get(&self, key: &BlobKey, range: Option<&Range<u64>>) -> StreamCompletion {
        let (done, fut) = stream_completion();
        let range = MdRange::from_range(range);
        // SAFETY: live instance; `key` outlives the call.
        unsafe {
            (self.vtable.get)(
                self.instance,
                MdStr::new(key.as_str()),
                range,
                MdCancel::none(),
                done,
            );
        };
        fut
    }

    fn start_stat(&self, key: &BlobKey) -> Completion {
        let (done, fut) = completion();
        // SAFETY: live instance; `key` outlives the call.
        unsafe {
            (self.vtable.stat)(
                self.instance,
                MdStr::new(key.as_str()),
                MdCancel::none(),
                done,
            );
        };
        fut
    }

    fn start_delete(&self, key: &BlobKey) -> Completion {
        let (done, fut) = completion();
        // SAFETY: live instance; `key` outlives the call.
        unsafe {
            (self.vtable.remove)(
                self.instance,
                MdStr::new(key.as_str()),
                MdCancel::none(),
                done,
            );
        };
        fut
    }

    fn start_usage(&self) -> Completion {
        let (done, fut) = completion();
        // SAFETY: live instance.
        unsafe { (self.vtable.usage)(self.instance, MdCancel::none(), done) };
        fut
    }
}

#[async_trait]
impl BlobStore for FfiBlobStore {
    async fn put(&self, key: &BlobKey, data: ByteStream) -> Result<BlobMeta> {
        from_json(&self.start_put(key, data).await?)
    }

    async fn get(&self, key: &BlobKey, range: Option<Range<u64>>) -> Result<ByteStream> {
        self.start_get(key, range.as_ref()).await
    }

    async fn stat(&self, key: &BlobKey) -> Result<Option<BlobMeta>> {
        from_json(&self.start_stat(key).await?)
    }

    async fn delete(&self, key: &BlobKey) -> Result<()> {
        self.start_delete(key).await.map(drop)
    }

    async fn usage(&self) -> Result<Option<StorageUsage>> {
        from_json(&self.start_usage().await?)
    }
}

impl Drop for FfiBlobStore {
    fn drop(&mut self) {
        tracing::debug!(addon = %self.addon, "destroying blob store");
        // SAFETY: the last reference is gone, so no call is in flight; destroyed exactly once.
        unsafe { (self.vtable.destroy)(self.instance) };
    }
}

/// An `EventHook` implemented by an addon.
pub(crate) struct FfiEventHook {
    pub addon: String,
    pub vtable: &'static MdEventHookVtable,
    pub instance: *mut c_void,
    /// Queried once at registration.
    pub filter: EventFilter,
}

// SAFETY: as for `FfiBlobStore`.
unsafe impl Send for FfiEventHook {}
// SAFETY: as for `FfiBlobStore`.
unsafe impl Sync for FfiEventHook {}

impl FfiEventHook {
    /// Queries the addon's filter. An empty or invalid answer means "all events".
    pub(crate) fn query_filter(vtable: &MdEventHookVtable, instance: *mut c_void) -> EventFilter {
        // SAFETY: live instance; the returned buffer is owned by us.
        let buf = unsafe { OwnedBuffer::from_raw((vtable.filter)(instance)) };
        if buf.is_empty() {
            return EventFilter::default();
        }
        from_json(&buf).unwrap_or_default()
    }

    fn start_on_event(&self, json: &[u8]) -> Completion {
        let (done, fut) = completion();
        // SAFETY: live instance; `json` outlives the call.
        unsafe {
            (self.vtable.on_event)(
                self.instance,
                MdStr::from_bytes(json),
                MdCancel::none(),
                done,
            );
        };
        fut
    }
}

#[async_trait]
impl EventHook for FfiEventHook {
    fn filter(&self) -> EventFilter {
        self.filter.clone()
    }

    async fn on_event(&self, event: &Event) -> Result<()> {
        let json = serde_json::to_vec(event).map_err(|e| Error::internal(e.to_string()))?;
        self.start_on_event(&json).await.map(drop)
    }
}

impl Drop for FfiEventHook {
    fn drop(&mut self) {
        tracing::debug!(addon = %self.addon, "destroying event hook");
        // SAFETY: as for `FfiBlobStore`.
        unsafe { (self.vtable.destroy)(self.instance) };
    }
}
