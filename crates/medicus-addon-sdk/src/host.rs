//! The host's services as seen from an addon.

use medicus_abi::bridge::{completion, export_stream};
use medicus_abi::ffi::{MdCancel, MdStr};
use medicus_core::capability::{ByteStream, HostServices, Ingested};
use medicus_core::event::Origin;
use medicus_core::{Error, Result};

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
}

#[async_trait]
impl HostServices for Host {
    async fn ingest(&self, data: ByteStream, origin: Origin) -> Result<Ingested> {
        let origin = serde_json::to_vec(&origin).map_err(|e| Error::internal(e.to_string()))?;
        let result = self.start_ingest(data, &origin).await?;
        serde_json::from_slice(result.as_slice())
            .map_err(|e| Error::internal(format!("invalid ingest result: {e}")))
    }
}
