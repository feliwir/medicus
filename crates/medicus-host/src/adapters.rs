//! C vtables → core capability traits.

use std::ffi::c_void;
use std::ops::Range;

use async_trait::async_trait;
use medicus_abi::bridge::{
    Completion, OwnedBuffer, StreamCompletion, completion, export_stream, stream_completion,
};
use medicus_abi::ffi::{
    MdBlobStoreVtable, MdCancel, MdEventHookVtable, MdIndexStoreVtable, MdRange,
    MdRemoteNodeVtable, MdServiceVtable, MdStr,
};
use medicus_core::capability::index::{
    DeleteOutcome, DicomJsonObject, DuplicatePolicy, ExtraTag, ExtraTagStatus, IndexRequest,
    InstanceLocation, InstanceRecord, QueryResult, ResourceUid, StoreOutcome, StoredBytes,
};
use medicus_core::capability::{
    BlobMeta, BlobStore, ByteStream, EventFilter, EventHook, StorageUsage,
};
use medicus_core::capability::{
    IndexStore, Job, JobState, Peer, RemoteNode, RemoteRequest, SendOutcome, Service,
};
use medicus_core::event::Event;
use medicus_core::query::Query;
use medicus_core::uid::SopInstanceUid;
use medicus_core::{BlobKey, NodeInfo, Partition, Tag};
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

/// An `IndexStore` implemented by an addon (JSON request per call).
pub(crate) struct FfiIndexStore {
    pub addon: String,
    pub vtable: &'static MdIndexStoreVtable,
    pub instance: *mut c_void,
}

// SAFETY: as for `FfiBlobStore`.
unsafe impl Send for FfiIndexStore {}
// SAFETY: as for `FfiBlobStore`.
unsafe impl Sync for FfiIndexStore {}

impl FfiIndexStore {
    fn start(&self, request: &IndexRequest) -> Result<Completion> {
        let json = serde_json::to_vec(request).map_err(|e| Error::internal(e.to_string()))?;
        let (done, fut) = completion();
        // SAFETY: live instance; `json` outlives the call.
        unsafe {
            (self.vtable.call)(
                self.instance,
                MdStr::from_bytes(&json),
                MdCancel::none(),
                done,
            );
        };
        Ok(fut)
    }

    async fn call<T: DeserializeOwned>(&self, request: IndexRequest) -> Result<T> {
        let fut = self.start(&request)?;
        drop(request);
        from_json(&fut.await?)
    }
}

#[async_trait]
impl IndexStore for FfiIndexStore {
    async fn store_instance(
        &self,
        record: &InstanceRecord,
        policy: DuplicatePolicy,
        event: &Event,
    ) -> Result<StoreOutcome> {
        self.call(IndexRequest::StoreInstance {
            record: Box::new(record.clone()),
            policy,
            event: Box::new(event.clone()),
        })
        .await
    }

    async fn query(&self, partition: &Partition, query: &Query) -> Result<QueryResult> {
        self.call(IndexRequest::Query {
            partition: partition.clone(),
            query: query.clone(),
        })
        .await
    }

    async fn locate(
        &self,
        partition: &Partition,
        resource: &ResourceUid,
    ) -> Result<Vec<InstanceLocation>> {
        self.call(IndexRequest::Locate {
            partition: partition.clone(),
            resource: resource.clone(),
        })
        .await
    }

    async fn delete(
        &self,
        partition: &Partition,
        resource: &ResourceUid,
        event: &Event,
    ) -> Result<DeleteOutcome> {
        self.call(IndexRequest::Delete {
            partition: partition.clone(),
            resource: resource.clone(),
            event: event.clone(),
        })
        .await
    }

    async fn append_event(&self, event: &Event) -> Result<u64> {
        self.call(IndexRequest::AppendEvent {
            event: event.clone(),
        })
        .await
    }

    async fn events_after(&self, after: u64, limit: u32) -> Result<Vec<Event>> {
        self.call(IndexRequest::EventsAfter { after, limit }).await
    }

    async fn last_event_seq(&self) -> Result<u64> {
        self.call(IndexRequest::LastEventSeq).await
    }

    async fn cursor(&self, subscriber: &str) -> Result<Option<u64>> {
        self.call(IndexRequest::Cursor {
            subscriber: subscriber.to_owned(),
        })
        .await
    }

    async fn set_cursor(&self, subscriber: &str, seq: u64) -> Result<()> {
        self.call(IndexRequest::SetCursor {
            subscriber: subscriber.to_owned(),
            seq,
        })
        .await
    }

    async fn prune_events(&self, up_to: u64) -> Result<u64> {
        self.call(IndexRequest::PruneEvents { up_to }).await
    }

    async fn stored_bytes(&self) -> Result<Vec<StoredBytes>> {
        self.call(IndexRequest::StoredBytes).await
    }

    async fn configure_extra_tags(&self, tags: &[ExtraTag]) -> Result<Vec<ExtraTagStatus>> {
        self.call(IndexRequest::ConfigureExtraTags {
            tags: tags.to_vec(),
        })
        .await
    }

    async fn update_extra(
        &self,
        partition: &Partition,
        instance: &SopInstanceUid,
        attributes: &DicomJsonObject,
    ) -> Result<()> {
        self.call(IndexRequest::UpdateExtra {
            partition: partition.clone(),
            instance: instance.clone(),
            attributes: attributes.clone(),
        })
        .await
    }

    async fn mark_extra_ready(&self, tag: Tag) -> Result<()> {
        self.call(IndexRequest::MarkExtraReady { tag }).await
    }

    async fn put_job(&self, job: &Job) -> Result<()> {
        self.call(IndexRequest::PutJob {
            job: Box::new(job.clone()),
        })
        .await
    }

    async fn get_job(&self, id: uuid::Uuid) -> Result<Option<Job>> {
        self.call(IndexRequest::GetJob { id }).await
    }

    async fn list_jobs(
        &self,
        state: Option<JobState>,
        limit: u32,
        offset: u64,
    ) -> Result<Vec<Job>> {
        self.call(IndexRequest::ListJobs {
            state,
            limit,
            offset,
        })
        .await
    }

    async fn claim_job(&self, now: time::OffsetDateTime) -> Result<Option<Job>> {
        self.call(IndexRequest::ClaimJob { now }).await
    }
}

impl Drop for FfiIndexStore {
    fn drop(&mut self) {
        tracing::debug!(addon = %self.addon, "destroying index store");
        // SAFETY: as for `FfiBlobStore`.
        unsafe { (self.vtable.destroy)(self.instance) };
    }
}

/// A `RemoteNode` implemented by an addon.
pub(crate) struct FfiRemoteNode {
    pub addon: String,
    pub vtable: &'static MdRemoteNodeVtable,
    pub instance: *mut c_void,
}

// SAFETY: as for `FfiBlobStore`.
unsafe impl Send for FfiRemoteNode {}
// SAFETY: as for `FfiBlobStore`.
unsafe impl Sync for FfiRemoteNode {}

impl FfiRemoteNode {
    async fn call<T: DeserializeOwned>(&self, request: &RemoteRequest) -> Result<T> {
        let json = serde_json::to_vec(request).map_err(|e| Error::internal(e.to_string()))?;
        let (done, fut) = completion();
        // SAFETY: live instance; `json` outlives the call.
        unsafe {
            (self.vtable.call)(
                self.instance,
                MdStr::from_bytes(&json),
                MdCancel::none(),
                done,
            );
        };
        drop(json);
        from_json(&fut.await?)
    }
}

#[async_trait]
impl RemoteNode for FfiRemoteNode {
    async fn echo(&self, peer: &Peer) -> Result<()> {
        self.call(&RemoteRequest::Echo { peer: peer.clone() }).await
    }

    async fn send(
        &self,
        job: uuid::Uuid,
        node: &NodeInfo,
        peer: &Peer,
        instances: &[SopInstanceUid],
    ) -> Result<SendOutcome> {
        self.call(&RemoteRequest::Send {
            job,
            node: node.clone(),
            peer: peer.clone(),
            instances: instances.to_vec(),
        })
        .await
    }
}

impl Drop for FfiRemoteNode {
    fn drop(&mut self) {
        tracing::debug!(addon = %self.addon, "destroying remote node");
        // SAFETY: as for `FfiBlobStore`.
        unsafe { (self.vtable.destroy)(self.instance) };
    }
}

/// A background `Service` implemented by an addon.
pub(crate) struct FfiService {
    pub addon: String,
    pub vtable: &'static MdServiceVtable,
    pub instance: *mut c_void,
}

// SAFETY: as for `FfiBlobStore`.
unsafe impl Send for FfiService {}
// SAFETY: as for `FfiBlobStore`.
unsafe impl Sync for FfiService {}

impl FfiService {
    fn start_call(
        &self,
        f: unsafe extern "C" fn(*mut c_void, medicus_abi::ffi::MdCompletion),
    ) -> Completion {
        let (done, fut) = completion();
        // SAFETY: live instance.
        unsafe { f(self.instance, done) };
        fut
    }
}

#[async_trait]
impl Service for FfiService {
    async fn start(&self) -> Result<()> {
        self.start_call(self.vtable.start).await.map(drop)
    }

    async fn stop(&self) -> Result<()> {
        self.start_call(self.vtable.stop).await.map(drop)
    }
}

impl Drop for FfiService {
    fn drop(&mut self) {
        tracing::debug!(addon = %self.addon, "destroying service");
        // SAFETY: as for `FfiBlobStore`.
        unsafe { (self.vtable.destroy)(self.instance) };
    }
}
