//! The host services offered to addons, and the one place where their access is checked
//! (ADR-0003): every data call names a node and a caller, and runs in the node's partition only
//! if the caller has the permission.

use std::sync::Arc;

use medicus_core::capability::index::{InstanceLocation, QueryResult, ResourceUid};
use medicus_core::capability::{
    ByteStream, HostServices, Ingested, Job, JobKind, JobProgress, JobState, Peer,
};
use medicus_core::event::{Event, EventKind, Origin};
use medicus_core::query::Query;
use medicus_core::uid::SopInstanceUid;
use medicus_core::{CallContext, ErrorKind, NodeInfo, Permission, Result};
use serde_json::json;
use uuid::Uuid;

use crate::access::{Caller, Directory, Node};
use crate::archive::Archive;
use crate::jobs::Jobs;

#[derive(Debug)]
pub(crate) struct NodeServices {
    pub(crate) archive: Arc<Archive>,
    pub(crate) directory: Arc<Directory>,
    pub(crate) jobs: Arc<Jobs>,
}

impl NodeServices {
    /// Who is calling. Addons may present a job origin only for a job that is running on the
    /// context's node (its executor); anything else they claim on the core's behalf is
    /// anonymous.
    async fn caller(&self, context: &CallContext) -> Caller<'_> {
        if let Origin::Job { job_id } = &context.origin {
            let verified = self
                .jobs
                .get(*job_id)
                .await
                .is_ok_and(|job| job.state == JobState::Running && job.node == context.node);
            return if verified {
                Caller::System
            } else {
                Caller::Anonymous
            };
        }
        self.directory.caller(&context.origin, true)
    }

    /// The context's node if the caller has `permission` on it; denials are logged and published.
    async fn require(&self, context: &CallContext, permission: Permission) -> Result<&Node> {
        let caller = self.caller(context).await;
        match self.directory.require(context, &caller, permission) {
            Ok(node) => Ok(node),
            Err(e) => {
                if e.kind == ErrorKind::PermissionDenied {
                    tracing::warn!(node = %context.node, %caller, ?permission, "access denied");
                    self.publish_denial(context, permission).await;
                }
                Err(e)
            }
        }
    }

    async fn publish_denial(&self, context: &CallContext, permission: Permission) {
        let mut event = Event::new(EventKind::AccessDenied, context.origin.clone())
            .with_node(context.node.clone())
            .with_detail(json!({ "permission": permission }));
        if let Ok(partition) = self
            .directory
            .node(&context.node)
            .and_then(|n| n.partition())
        {
            event = event.with_partition(partition.clone());
        }
        match self.archive.index.append_event(&event).await {
            Ok(seq) => self.archive.events_committed(seq),
            Err(e) => tracing::warn!(error = %e, "cannot publish access.denied"),
        }
    }
}

#[async_trait::async_trait]
impl HostServices for NodeServices {
    async fn ingest(&self, context: &CallContext, data: ByteStream) -> Result<Ingested> {
        let node = self.require(context, Permission::Store).await?;
        self.archive
            .ingest_through(
                Some(&node.name),
                node.partition()?,
                data,
                context.origin.clone(),
            )
            .await
    }

    async fn query(&self, context: &CallContext, query: &Query) -> Result<QueryResult> {
        let node = self.require(context, Permission::Query).await?;
        self.archive.query(node.partition()?, query).await
    }

    async fn locate(
        &self,
        context: &CallContext,
        resource: &ResourceUid,
    ) -> Result<Vec<InstanceLocation>> {
        let node = self.require(context, Permission::Retrieve).await?;
        self.archive.index.locate(node.partition()?, resource).await
    }

    async fn open_instance(
        &self,
        context: &CallContext,
        instance: &SopInstanceUid,
    ) -> Result<ByteStream> {
        let node = self.require(context, Permission::Retrieve).await?;
        Ok(self.archive.retrieve(node.partition()?, instance).await?.1)
    }

    async fn copy_instance(
        &self,
        context: &CallContext,
        instance: &SopInstanceUid,
        destination: &str,
    ) -> Result<Ingested> {
        let source = self.require(context, Permission::Retrieve).await?;
        self.require(context, Permission::Send).await?;
        let target = CallContext::new(destination, context.origin.clone());
        let destination = self.require(&target, Permission::Store).await?;
        let (_, data) = self.archive.retrieve(source.partition()?, instance).await?;
        self.archive
            .ingest_through(
                Some(&destination.name),
                destination.partition()?,
                data,
                context.origin.clone(),
            )
            .await
    }

    async fn authorize(&self, context: &CallContext, permission: Permission) -> Result<()> {
        self.require(context, permission).await.map(|_| ())
    }

    async fn permissions(&self, context: &CallContext) -> Result<Vec<Permission>> {
        let node = self.directory.node(&context.node)?;
        let caller = self.caller(context).await;
        Ok(self
            .directory
            .permissions(&caller, &node.name)
            .into_iter()
            .collect())
    }

    async fn nodes(&self) -> Result<Vec<NodeInfo>> {
        Ok(self.directory.nodes().map(Node::info).collect())
    }

    async fn peers(&self) -> Result<Vec<Peer>> {
        Ok(self.jobs.peers().cloned().collect())
    }

    async fn submit_job(&self, context: &CallContext, kind: JobKind) -> Result<Job> {
        let node = self.require(context, Permission::Send).await?;
        self.jobs.submit(node.partition()?, &node.name, kind).await
    }

    async fn job_progress(&self, job: Uuid, progress: JobProgress) -> Result<bool> {
        self.jobs.progress(job, progress).await
    }
}
