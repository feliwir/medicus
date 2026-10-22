//! Services the host (core server) offers to addons, and background services addons run.
//!
//! Protocol front-ends (DIMSE, `DICOMweb`) are addons; they translate their wire protocol into
//! these calls, so the core handles every instance the same way regardless of the protocol.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::capability::ByteStream;
use crate::capability::index::Duplicate;
use crate::error::Result;
use crate::event::Origin;
use crate::uid::{
    SeriesInstanceUid, SopClassUid, SopInstanceUid, StudyInstanceUid, TransferSyntaxUid,
};

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

/// What the host offers addons.
#[async_trait]
pub trait HostServices: Send + Sync + 'static {
    /// Ingests a DICOM Part 10 file through the core's ingest pipeline.
    async fn ingest(&self, data: ByteStream, origin: Origin) -> Result<Ingested>;
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
