//! Services the host (core server) offers to addons, and background services addons run.
//!
//! Protocol front-ends (DIMSE, `DICOMweb`) are addons; they translate their wire protocol into
//! these calls, so the core handles every instance the same way regardless of the protocol.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::capability::ByteStream;
use crate::capability::index::{
    DicomJsonObject, Duplicate, InstanceLocation, QueryResult, ResourceUid,
};
use crate::capability::job::{Job, JobKind, JobProgress};
use crate::error::Result;
use crate::query::Query;
use crate::uid::{
    SeriesInstanceUid, SopClassUid, SopInstanceUid, StudyInstanceUid, TransferSyntaxUid,
};
use crate::{AeTitle, CallContext, NodeInfo, Permission};
use uuid::Uuid;

/// The result of ingesting one instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ingested {
    pub study: StudyInstanceUid,
    pub series: SeriesInstanceUid,
    pub instance: SopInstanceUid,
    pub sop_class: SopClassUid,
    pub transfer_syntax: TransferSyntaxUid,
    pub size: u64,
    /// Sequence number of the `instance.stored` event; `None` if an existing instance was kept.
    pub seq: Option<u64>,
    #[serde(flatten)]
    pub duplicate: Duplicate,
}

/// A remote node the archive exchanges data with (configured by the operator).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Peer {
    /// The peer's configured name.
    pub name: String,
    #[serde(flatten)]
    pub endpoint: Endpoint,
}

/// How a peer is reached, by protocol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "protocol", rename_all = "snake_case")]
pub enum Endpoint {
    Dimse {
        ae_title: AeTitle,
        host: String,
        port: u16,
    },
}

/// What the host offers addons. Data operations carry a [`CallContext`]: the core checks the
/// caller's permission on the node (`PermissionDenied` otherwise) and confines the call to the
/// node's partition.
#[async_trait]
pub trait HostServices: Send + Sync + 'static {
    /// Ingests a DICOM Part 10 file through the core's ingest pipeline (needs `store`).
    async fn ingest(&self, context: &CallContext, data: ByteStream) -> Result<Ingested>;

    /// Runs a query (needs `query`). Unlike
    /// [`IndexStore::query`](crate::capability::IndexStore::query) the answer is complete: keys
    /// the index cannot evaluate are matched, and requested attributes it does not store are
    /// filled in, from the stored instances' headers.
    async fn query(&self, context: &CallContext, query: &Query) -> Result<QueryResult>;

    /// The stored instances of a study, series or instance (needs `retrieve`).
    async fn locate(
        &self,
        context: &CallContext,
        resource: &ResourceUid,
    ) -> Result<Vec<InstanceLocation>>;

    /// An instance's metadata (needs `retrieve`): DICOM JSON of every attribute before the
    /// bulk pixel data, read from the stored header without touching the pixel data.
    async fn metadata(
        &self,
        context: &CallContext,
        instance: &SopInstanceUid,
    ) -> Result<DicomJsonObject>;

    /// Opens a stored instance's Part 10 file (needs `retrieve`).
    async fn open_instance(
        &self,
        context: &CallContext,
        instance: &SopInstanceUid,
    ) -> Result<ByteStream>;

    /// Copies a stored instance into the local node `destination` (a C-MOVE to a local node):
    /// needs `retrieve` and `send` on the context's node and `store` on `destination`.
    async fn copy_instance(
        &self,
        context: &CallContext,
        instance: &SopInstanceUid,
        destination: &str,
    ) -> Result<Ingested>;

    /// Checks a permission the protocol enforces itself (e.g. `send` for a C-MOVE to a peer).
    async fn authorize(&self, context: &CallContext, permission: Permission) -> Result<()>;

    /// The caller's permissions on the context's node (empty: no access at all).
    async fn permissions(&self, context: &CallContext) -> Result<Vec<Permission>>;

    /// The configured nodes, for protocol addons to serve them.
    async fn nodes(&self) -> Result<Vec<NodeInfo>>;

    /// The configured peers.
    async fn peers(&self) -> Result<Vec<Peer>>;

    /// The user a session or API token secret belongs to (ABI 0.7), for protocol addons that
    /// accept the server's own credentials: the name can then be given as the principal of the
    /// call context. `None` if the secret is unknown or expired, or the user is disabled.
    async fn authenticate(&self, secret: &str) -> Result<Option<String>>;

    /// Queues a job, e.g. a send requested through a protocol front-end (needs `send`).
    async fn submit_job(&self, context: &CallContext, kind: JobKind) -> Result<Job>;

    /// Reports the progress of a running job; returns `false` once the job was cancelled, after
    /// which the executor should stop.
    async fn job_progress(&self, job: Uuid, progress: JobProgress) -> Result<bool>;
}

/// A long-running background service of an addon (e.g. a network listener). The host starts it
/// once the archive is ready and stops it at shutdown.
#[async_trait]
pub trait Service: Send + Sync + 'static {
    /// Starts the service; returns once it is running (e.g. its socket is bound).
    async fn start(&self) -> Result<()>;

    /// Stops the service gracefully; returns once it has stopped.
    async fn stop(&self) -> Result<()>;
}
