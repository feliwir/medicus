//! The host's services as seen from an addon.

use medicus_abi::bridge::{completion, export_stream, stream_completion};
use medicus_abi::ffi::{MdCancel, MdStr};
use medicus_core::capability::index::{InstanceLocation, QueryResult, ResourceUid};
use medicus_core::capability::{ByteStream, HostServices, Ingested, Peer};
use medicus_core::event::Origin;
use medicus_core::query::Query;
use medicus_core::uid::SopInstanceUid;
use medicus_core::{Error, Result};
use serde::Serialize;
use serde::de::DeserializeOwned;

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
    fn start_ingest(&self, data: ByteStream, origin: &[u8]) -> medicus_abi::bridge::Completion {
        let api = self.0.api();
        let (done, fut) = completion();
        let data = export_stream(data, runtime());
        // SAFETY: the host API lives for the process; ownership of `data` moves to the host and
        // `origin` outlives the call.
        unsafe {
            (api.ingest)(
                api.ctx,
                data,
                MdStr::from_bytes(origin),
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
    async fn ingest(&self, data: ByteStream, origin: Origin) -> Result<Ingested> {
        let origin = serde_json::to_vec(&origin).map_err(|e| Error::internal(e.to_string()))?;
        let result = self.start_ingest(data, &origin).await?;
        serde_json::from_slice(result.as_slice())
            .map_err(|e| Error::internal(format!("invalid ingest result: {e}")))
    }

    async fn query(&self, query: &Query) -> Result<QueryResult> {
        self.call("query", query).await
    }

    async fn locate(&self, resource: &ResourceUid) -> Result<Vec<InstanceLocation>> {
        self.call("locate", resource).await
    }

    async fn open_instance(&self, instance: &SopInstanceUid) -> Result<ByteStream> {
        let api = self.0.api();
        let fut = {
            let (done, fut) = stream_completion();
            // SAFETY: the host API lives for the process; the UID outlives the call.
            unsafe {
                (api.open_instance)(
                    api.ctx,
                    MdStr::new(instance.as_str()),
                    MdCancel::none(),
                    done,
                );
            };
            fut
        };
        fut.await
    }

    async fn peers(&self) -> Result<Vec<Peer>> {
        self.call("peers", &()).await
    }
}
