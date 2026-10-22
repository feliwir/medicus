//! The host services offered to addons: the archive plus node-level services (peers, jobs).

use std::sync::Arc;

use medicus_core::capability::index::{InstanceLocation, QueryResult, ResourceUid};
use medicus_core::capability::{
    ByteStream, HostServices, Ingested, Job, JobKind, JobProgress, Peer,
};
use medicus_core::event::Origin;
use medicus_core::query::Query;
use medicus_core::uid::SopInstanceUid;
use medicus_core::{Partition, Result};
use uuid::Uuid;

use crate::archive::Archive;
use crate::jobs::Jobs;

/// Until host calls carry a node (ADR-0003), everything happens in the default partition.
fn default() -> Partition {
    Partition::default_partition()
}

#[derive(Debug)]
pub(crate) struct NodeServices {
    pub(crate) archive: Arc<Archive>,
    pub(crate) jobs: Arc<Jobs>,
}

#[async_trait::async_trait]
impl HostServices for NodeServices {
    async fn ingest(&self, data: ByteStream, origin: Origin) -> Result<Ingested> {
        self.archive.ingest(&default(), data, origin).await
    }

    async fn query(&self, query: &Query) -> Result<QueryResult> {
        self.archive.query(&default(), query).await
    }

    async fn locate(&self, resource: &ResourceUid) -> Result<Vec<InstanceLocation>> {
        self.archive.index.locate(&default(), resource).await
    }

    async fn open_instance(&self, instance: &SopInstanceUid) -> Result<ByteStream> {
        Ok(self.archive.retrieve(&default(), instance).await?.1)
    }

    async fn peers(&self) -> Result<Vec<Peer>> {
        Ok(self.jobs.peers().cloned().collect())
    }

    async fn submit_job(&self, kind: JobKind) -> Result<Job> {
        self.jobs.submit(&default(), kind).await
    }

    async fn job_progress(&self, job: Uuid, progress: JobProgress) -> Result<bool> {
        self.jobs.progress(job, progress).await
    }
}
