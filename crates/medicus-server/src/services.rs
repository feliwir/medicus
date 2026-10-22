//! The host services offered to addons: the archive plus node-level configuration (peers).

use std::sync::Arc;

use medicus_core::Result;
use medicus_core::capability::index::{InstanceLocation, QueryResult, ResourceUid};
use medicus_core::capability::{ByteStream, Endpoint, HostServices, Ingested, Peer};
use medicus_core::event::Origin;
use medicus_core::query::Query;
use medicus_core::uid::SopInstanceUid;

use crate::archive::Archive;

#[derive(Debug)]
pub(crate) struct NodeServices {
    pub(crate) archive: Arc<Archive>,
    pub(crate) peers: Vec<Peer>,
}

impl NodeServices {
    pub(crate) fn new(
        archive: Arc<Archive>,
        peers: impl IntoIterator<Item = (String, Endpoint)>,
    ) -> Self {
        Self {
            archive,
            peers: peers
                .into_iter()
                .map(|(name, endpoint)| Peer { name, endpoint })
                .collect(),
        }
    }
}

#[async_trait::async_trait]
impl HostServices for NodeServices {
    async fn ingest(&self, data: ByteStream, origin: Origin) -> Result<Ingested> {
        self.archive.ingest(data, origin).await
    }

    async fn query(&self, query: &Query) -> Result<QueryResult> {
        self.archive.query(query).await
    }

    async fn locate(&self, resource: &ResourceUid) -> Result<Vec<InstanceLocation>> {
        self.archive.index.locate(resource).await
    }

    async fn open_instance(&self, instance: &SopInstanceUid) -> Result<ByteStream> {
        Ok(self.archive.retrieve(instance).await?.1)
    }

    async fn peers(&self) -> Result<Vec<Peer>> {
        Ok(self.peers.clone())
    }
}
