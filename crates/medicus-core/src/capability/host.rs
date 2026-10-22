//! Services the host (core server) offers to addons, and background services addons run.
//!
//! Protocol front-ends (DIMSE, `DICOMweb`) are addons; they translate their wire protocol into
//! these calls, so the core handles every instance the same way regardless of the protocol.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::AeTitle;
use crate::capability::ByteStream;
use crate::capability::index::{Duplicate, InstanceLocation, QueryResult, ResourceUid};
use crate::capability::job::{Job, JobKind, JobProgress};
use crate::error::Result;
use crate::event::Origin;
use crate::query::Query;
use crate::uid::{
    SeriesInstanceUid, SopClassUid, SopInstanceUid, StudyInstanceUid, TransferSyntaxUid,
};
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

/// What the host offers addons.
#[async_trait]
pub trait HostServices: Send + Sync + 'static {
    /// Ingests a DICOM Part 10 file through the core's ingest pipeline.
    async fn ingest(&self, data: ByteStream, origin: Origin) -> Result<Ingested>;

    /// Runs a query. Unlike [`IndexStore::query`](crate::capability::IndexStore::query) the
    /// answer is complete: keys the index cannot evaluate are matched, and requested attributes
    /// it does not store are filled in, from the stored instances' headers.
    async fn query(&self, query: &Query) -> Result<QueryResult>;

    /// The stored instances of a study, series or instance.
    async fn locate(&self, resource: &ResourceUid) -> Result<Vec<InstanceLocation>>;

    /// Opens a stored instance's Part 10 file.
    async fn open_instance(&self, instance: &SopInstanceUid) -> Result<ByteStream>;

    /// The configured peers.
    async fn peers(&self) -> Result<Vec<Peer>>;

    /// Queues a job (e.g. a send requested through a protocol front-end).
    async fn submit_job(&self, kind: JobKind) -> Result<Job>;

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
