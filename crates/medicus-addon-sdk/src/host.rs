//! The host's services as seen from an addon.

use medicus_abi::bridge::{completion, export_stream, stream_completion};
use medicus_abi::ffi::{MdCancel, MdStr};
use medicus_core::capability::index::{
    DicomJsonObject, InstanceLocation, QueryResult, ResourceUid,
};
use medicus_core::capability::{
    ByteStream, HostServices, Ingested, Job, JobKind, JobProgress, Peer,
};
use medicus_core::query::Query;
use medicus_core::uid::SopInstanceUid;
use medicus_core::{CallContext, Error, NodeInfo, Permission, Result};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::json;

use crate::async_trait;
use crate::registrar::HostRef;
use crate::runtime::runtime;

/// Handle to the host's services, obtained from [`Registrar::host`](crate::Registrar::host).
/// Cheap to clone; keep it in the addon's capability structs.
#[derive(Clone, Copy)]
pub struct Host(pub(crate) HostRef);

impl std::fmt::Debug for Host {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Host")
    }
}

impl Host {
    fn start_ingest(&self, data: ByteStream, context: &[u8]) -> medicus_abi::bridge::Completion {
        let api = self.0.api();
        let (done, fut) = completion();
        let data = export_stream(data, runtime());
        // SAFETY: the host API lives for the process; ownership of `data` moves to the host and
        // `context` outlives the call.
        unsafe {
            (api.ingest)(
                api.ctx,
                data,
                MdStr::from_bytes(context),
                MdCancel::none(),
                done,
            );
        };
        fut
    }

    async fn call<T: DeserializeOwned>(&self, method: &str, request: &impl Serialize) -> Result<T> {
        let api = self.0.api();
        let request = serde_json::to_vec(request).map_err(|e| Error::internal(e.to_string()))?;
        let (done, fut) = completion();
        // SAFETY: the host API lives for the process; the strings outlive the call.
        unsafe {
            (api.call)(
                api.ctx,
                MdStr::new(method),
                MdStr::from_bytes(&request),
                MdCancel::none(),
                done,
            );
        };
        let result = fut.await?;
        serde_json::from_slice(result.as_slice())
            .map_err(|e| Error::internal(format!("invalid {method} result: {e}")))
    }
}

#[async_trait]
impl HostServices for Host {
    async fn ingest(&self, context: &CallContext, data: ByteStream) -> Result<Ingested> {
        let context = serde_json::to_vec(context).map_err(|e| Error::internal(e.to_string()))?;
        let result = self.start_ingest(data, &context).await?;
        serde_json::from_slice(result.as_slice())
            .map_err(|e| Error::internal(format!("invalid ingest result: {e}")))
    }

    async fn query(&self, context: &CallContext, query: &Query) -> Result<QueryResult> {
        self.call("query", &json!({ "context": context, "query": query }))
            .await
    }

    async fn locate(
        &self,
        context: &CallContext,
        resource: &ResourceUid,
    ) -> Result<Vec<InstanceLocation>> {
        self.call(
            "locate",
            &json!({ "context": context, "resource": resource }),
        )
        .await
    }

    async fn metadata(
        &self,
        context: &CallContext,
        instance: &SopInstanceUid,
    ) -> Result<DicomJsonObject> {
        self.call(
            "metadata",
            &json!({ "context": context, "instance": instance }),
        )
        .await
    }

    async fn open_instance(
        &self,
        context: &CallContext,
        instance: &SopInstanceUid,
    ) -> Result<ByteStream> {
        let api = self.0.api();
        let request = serde_json::to_vec(&json!({ "context": context, "instance": instance }))
            .map_err(|e| Error::internal(e.to_string()))?;
        let fut = {
            let (done, fut) = stream_completion();
            // SAFETY: the host API lives for the process; the request outlives the call.
            unsafe {
                (api.open_instance)(api.ctx, MdStr::from_bytes(&request), MdCancel::none(), done);
            };
            fut
        };
        fut.await
    }

    async fn copy_instance(
        &self,
        context: &CallContext,
        instance: &SopInstanceUid,
        destination: &str,
    ) -> Result<Ingested> {
        let request =
            json!({ "context": context, "instance": instance, "destination": destination });
        self.call("copy_instance", &request).await
    }

    async fn authorize(&self, context: &CallContext, permission: Permission) -> Result<()> {
        self.call(
            "authorize",
            &json!({ "context": context, "permission": permission }),
        )
        .await
    }

    async fn permissions(&self, context: &CallContext) -> Result<Vec<Permission>> {
        self.call("permissions", &json!({ "context": context }))
            .await
    }

    async fn nodes(&self) -> Result<Vec<NodeInfo>> {
        self.call("nodes", &()).await
    }

    async fn peers(&self) -> Result<Vec<Peer>> {
        self.call("peers", &()).await
    }

    async fn authenticate(&self, secret: &str) -> Result<Option<String>> {
        self.call("authenticate", &json!({ "secret": secret }))
            .await
    }

    async fn submit_job(&self, context: &CallContext, kind: JobKind) -> Result<Job> {
        self.call("submit_job", &json!({ "context": context, "kind": kind }))
            .await
    }

    async fn job_progress(&self, job: uuid::Uuid, progress: JobProgress) -> Result<bool> {
        self.call("job_progress", &json!({ "job": job, "progress": progress }))
            .await
    }
}
