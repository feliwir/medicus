//! `dimse`: a DICOM Storage SCP (C-ECHO, C-STORE) that ingests into the archive.
//!
//! ```toml
//! [addons.dimse]
//! config = { ae_title = "MEDICUS", listen = "0.0.0.0:11112" }
//! ```
//!
//! Received instances go through the host's ingest, exactly like uploads over HTTP, so they are
//! indexed, deduplicated and announced as `instance.stored` events with a DIMSE origin.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use medicus_addon_sdk::{Host, Registrar, async_trait, runtime, static_addon};
use medicus_core::capability::{ByteStream, HostServices as _, Service};
use medicus_core::event::Origin;
use medicus_core::{AeTitle, Error, ErrorKind, Result};
use medicus_dimse::{AssociationInfo, Scp, ScpOptions, Status, StoreHandler, StoreRequest};
use serde::Deserialize;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// This node's AE title.
    #[serde(default = "default_ae_title")]
    pub ae_title: String,
    /// Address and port to listen on.
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    /// Reject associations addressed to another Called AE Title.
    #[serde(default)]
    pub require_called_ae: bool,
    /// Calling AE titles allowed to associate; unset allows any.
    #[serde(default)]
    pub allowed_calling: Option<Vec<String>>,
    /// Storage SOP classes accepted in addition to the standard ones (e.g. private classes).
    #[serde(default)]
    pub extra_sop_classes: Vec<String>,
    /// Accepted transfer syntaxes; empty (the default) accepts any syntax dicom-rs can read.
    #[serde(default)]
    pub transfer_syntaxes: Vec<String>,
    #[serde(default = "default_max_pdu")]
    pub max_pdu_length: u32,
    /// Abort associations that have been silent for this many seconds; 0 disables the timeout.
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout_secs: u64,
    #[serde(default = "default_max_associations")]
    pub max_associations: usize,
}

fn default_ae_title() -> String {
    "MEDICUS".into()
}

fn default_listen() -> SocketAddr {
    SocketAddr::from(([0, 0, 0, 0], 11112))
}

fn default_max_pdu() -> u32 {
    64 * 1024
}

fn default_idle_timeout() -> u64 {
    300
}

fn default_max_associations() -> usize {
    64
}

impl Config {
    fn scp_options(&self) -> Result<ScpOptions> {
        let ae = AeTitle::parse(&self.ae_title)
            .map_err(|e| Error::invalid_argument(format!("ae_title: {e}")))?;
        let mut options = ScpOptions::new(ae.as_str());
        options.require_called_ae = self.require_called_ae;
        options.allowed_calling = self.allowed_calling.as_ref().map(|titles| {
            titles
                .iter()
                .map(|t| t.trim().to_owned())
                .collect::<HashSet<_>>()
        });
        options
            .storage_sop_classes
            .extend(self.extra_sop_classes.iter().cloned());
        options
            .transfer_syntaxes
            .clone_from(&self.transfer_syntaxes);
        options.max_pdu_length = self.max_pdu_length;
        options.idle_timeout =
            (self.idle_timeout_secs > 0).then(|| Duration::from_secs(self.idle_timeout_secs));
        options.max_associations = self.max_associations;
        Ok(options)
    }
}

/// Hands received instances to the host's ingest.
#[derive(Debug)]
struct Ingest {
    host: Host,
    ae_title: AeTitle,
}

impl Ingest {
    fn origin(&self, association: &AssociationInfo) -> Result<Origin> {
        let calling_ae = AeTitle::parse(&association.calling_ae)
            .map_err(|e| Error::invalid_argument(format!("calling AE title: {e}")))?;
        let called_ae =
            AeTitle::parse(&association.called_ae).unwrap_or_else(|_| self.ae_title.clone());
        Ok(Origin::Dimse {
            calling_ae,
            called_ae,
            peer: association.peer.to_string(),
            association: Some(association.id),
        })
    }
}

#[async_trait]
impl StoreHandler for Ingest {
    async fn store(
        &self,
        association: Arc<AssociationInfo>,
        request: StoreRequest,
        data: ByteStream,
    ) -> Status {
        let result = match self.origin(&association) {
            Ok(origin) => self.host.ingest(data, origin).await,
            Err(e) => Err(e),
        };
        match result {
            Ok(stored) => {
                if stored.instance.as_str() != request.sop_instance {
                    tracing::warn!(
                        association = %association.id,
                        announced = %request.sop_instance,
                        stored = %stored.instance,
                        "C-STORE-RQ announced a different SOP Instance UID than the data set"
                    );
                }
                tracing::debug!(association = %association.id, instance = %stored.instance, size = stored.size, "instance stored");
                Status::success()
            }
            Err(e) => {
                tracing::warn!(association = %association.id, instance = %request.sop_instance, error = %e, "C-STORE failed");
                Status::from(&e)
            }
        }
    }
}

/// The listener, started and stopped by the host.
#[derive(Debug)]
struct Listener {
    listen: SocketAddr,
    scp: Arc<Scp<Ingest>>,
    running: Mutex<Option<(CancellationToken, JoinHandle<()>)>>,
}

#[async_trait]
impl Service for Listener {
    async fn start(&self) -> Result<()> {
        let listener = TcpListener::bind(self.listen).await.map_err(|e| {
            Error::new(
                ErrorKind::Unavailable,
                format!("cannot listen on {}: {e}", self.listen),
            )
        })?;
        let addr = listener.local_addr().unwrap_or(self.listen);
        let token = CancellationToken::new();
        let scp = self.scp.clone();
        let shutdown = token.clone();
        let task = runtime().spawn(async move { scp.serve(listener, shutdown).await });
        let previous = self
            .running
            .lock()
            .map_err(|_| Error::internal("listener state poisoned"))?
            .replace((token, task));
        if let Some((token, _)) = previous {
            token.cancel();
        }
        tracing::info!(listen = %addr, "DIMSE storage SCP listening");
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        let running = self
            .running
            .lock()
            .map_err(|_| Error::internal("listener state poisoned"))?
            .take();
        if let Some((token, task)) = running {
            token.cancel();
            let _ = task.await;
            tracing::info!("DIMSE storage SCP stopped");
        }
        Ok(())
    }
}

pub fn init(reg: &mut Registrar) -> Result<()> {
    let cfg: Config = reg.config()?;
    let options = cfg.scp_options()?;
    let ingest = Ingest {
        host: reg.host(),
        ae_title: AeTitle::parse(&options.ae_title)
            .map_err(|e| Error::invalid_argument(e.to_string()))?,
    };
    tracing::info!(ae_title = %options.ae_title, listen = %cfg.listen, "DIMSE addon configured");
    let listener = Listener {
        listen: cfg.listen,
        scp: Arc::new(Scp::new(options, ingest)),
        running: Mutex::new(None),
    };
    reg.register_service("scp", listener)
}

/// Entry points for linking this addon into the server binary.
pub static ADDON: medicus_addon_sdk::abi::StaticAddon = static_addon!(
    name = "dimse",
    version = env!("CARGO_PKG_VERSION"),
    init = init
);
