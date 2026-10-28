//! `dimse`: the archive's DICOM network services: C-ECHO, C-STORE (ingest), and C-FIND, C-MOVE
//! and C-GET in the Patient Root and Study Root models.
//!
//! ```toml
//! [addons.dimse]
//! config = { listen = "0.0.0.0:11112" }
//! ```
//!
//! One listener serves every node with a DIMSE AE title (ADR-0003): the called AE title selects
//! the node, and each call to the host carries the node and the caller (calling AE title and
//! address), so the core decides what the caller may do and in which partition. Received
//! instances go through the host's ingest, exactly like uploads over `DICOMweb`, so events and
//! deduplication are the same whatever the protocol.

mod find;
mod remote;
mod retrieve;

/// The Query/Retrieve information models served, for the conformance statement.
pub use find::FIND_SOP_CLASSES;
pub use retrieve::RETRIEVE_SOP_CLASSES;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use medicus_addon_sdk::{Host, Registrar, async_trait, runtime, static_addon};
use medicus_core::capability::{ByteStream, HostServices as _, Service};
use medicus_core::event::Origin;
use medicus_core::uid::SopInstanceUid;
use medicus_core::{AeTitle, CallContext, Error, ErrorKind, Permission, Result};
use medicus_dimse::{
    AssociationInfo, Destination, FindRequest, Handler, Instance, Matches, RetrieveRequest, Scp,
    ScpOptions, Status, StoreRequest,
};
use serde::Deserialize;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Address and port to listen on.
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
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
    fn scp_options(&self, ae_titles: impl IntoIterator<Item = String>) -> ScpOptions {
        let mut options = ScpOptions::new(ae_titles);
        options
            .storage_sop_classes
            .extend(self.extra_sop_classes.iter().cloned());
        options
            .transfer_syntaxes
            .clone_from(&self.transfer_syntaxes);
        options.retrieve_sop_classes = retrieve::RETRIEVE_SOP_CLASSES
            .iter()
            .map(|c| (*c).to_owned())
            .collect();
        options.find_sop_classes = find::FIND_SOP_CLASSES
            .iter()
            .map(|c| (*c).to_owned())
            .collect();
        options.max_pdu_length = self.max_pdu_length;
        options.idle_timeout =
            (self.idle_timeout_secs > 0).then(|| Duration::from_secs(self.idle_timeout_secs));
        options.max_associations = self.max_associations;
        options
    }
}

/// Serves the nodes' DIMSE AE titles through the host's services.
#[derive(Debug)]
struct Nodes {
    host: Host,
    /// Called AE title → node name.
    by_ae: HashMap<String, String>,
}

fn sub_operation_failure(e: &Error) -> Status {
    if e.kind == ErrorKind::PermissionDenied {
        Status::from(e)
    } else {
        Status::new(Status::UNABLE_TO_PERFORM_SUBOPERATIONS).with_comment(e.message.clone())
    }
}

impl Nodes {
    /// The call context of an association: its node (by called AE title) and its caller.
    fn context(&self, association: &AssociationInfo) -> Result<CallContext> {
        let node = self.by_ae.get(&association.called_ae).ok_or_else(|| {
            Error::not_found(format!("no node with AE title {:?}", association.called_ae))
        })?;
        let ae = |raw: &str, what: &str| {
            AeTitle::parse(raw).map_err(|e| Error::invalid_argument(format!("{what}: {e}")))
        };
        Ok(CallContext::new(
            node.clone(),
            Origin::Dimse {
                calling_ae: ae(&association.calling_ae, "calling AE title")?,
                called_ae: ae(&association.called_ae, "called AE title")?,
                peer: association.peer.to_string(),
                association: Some(association.id),
            },
        ))
    }
}

#[async_trait]
impl Handler for Nodes {
    async fn admit(&self, association: Arc<AssociationInfo>) -> bool {
        let Ok(context) = self.context(&association) else {
            return false;
        };
        match self.host.permissions(&context).await {
            Ok(permissions) => !permissions.is_empty(),
            Err(e) => {
                tracing::warn!(association = %association.id, error = %e, "cannot check permissions");
                false
            }
        }
    }

    async fn store(
        &self,
        association: Arc<AssociationInfo>,
        request: StoreRequest,
        data: ByteStream,
    ) -> Status {
        let result = match self.context(&association) {
            Ok(context) => self.host.ingest(&context, data).await,
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

    async fn find(
        &self,
        association: Arc<AssociationInfo>,
        request: FindRequest,
        identifier: dicom_object::InMemDicomObject,
    ) -> Result<Matches, Status> {
        let context = self.context(&association).map_err(|e| Status::from(&e))?;
        self.host
            .authorize(&context, Permission::Query)
            .await
            .map_err(|e| Status::from(&e))?;
        let find = find::Find::parse(&request.sop_class, &identifier)?;
        tracing::debug!(association = %association.id, level = %find.query.level, keys = find.query.keys.len(), "C-FIND");
        Ok(find.run(self.host, context, association.called_ae.clone()))
    }

    async fn retrieve(
        &self,
        association: Arc<AssociationInfo>,
        request: RetrieveRequest,
        identifier: dicom_object::InMemDicomObject,
    ) -> Result<Vec<Instance>, Status> {
        let context = self.context(&association).map_err(|e| Status::from(&e))?;
        if request.destination.is_some() {
            // A C-MOVE sends the instances on: the caller needs `send` as well as `retrieve`.
            self.host
                .authorize(&context, Permission::Send)
                .await
                .map_err(|e| Status::from(&e))?;
        }
        let instances =
            retrieve::instances(&self.host, &context, &request.sop_class, &identifier).await?;
        tracing::debug!(association = %association.id, destination = ?request.destination, instances = instances.len(), "retrieve");
        Ok(instances)
    }

    async fn open(
        &self,
        association: Arc<AssociationInfo>,
        instance: &Instance,
    ) -> Result<ByteStream, Status> {
        let context = self
            .context(&association)
            .map_err(|e| sub_operation_failure(&e))?;
        let uid = SopInstanceUid::parse(&instance.sop_instance).map_err(|e| {
            Status::new(Status::UNABLE_TO_PERFORM_SUBOPERATIONS).with_comment(e.to_string())
        })?;
        self.host
            .open_instance(&context, &uid)
            .await
            .map_err(|e| sub_operation_failure(&e))
    }

    async fn destination(
        &self,
        _association: Arc<AssociationInfo>,
        ae_title: &str,
    ) -> Option<Destination> {
        if self.by_ae.contains_key(ae_title) {
            return Some(Destination::Local {
                ae_title: ae_title.to_owned(),
            });
        }
        retrieve::destination(&self.host, ae_title).await
    }

    async fn copy(
        &self,
        association: Arc<AssociationInfo>,
        instance: &Instance,
        destination: &str,
    ) -> Status {
        let (Ok(context), Ok(uid), Some(node)) = (
            self.context(&association),
            SopInstanceUid::parse(&instance.sop_instance),
            self.by_ae.get(destination),
        ) else {
            return Status::new(Status::UNABLE_TO_PERFORM_SUBOPERATIONS);
        };
        match self.host.copy_instance(&context, &uid, node).await {
            Ok(_) => Status::success(),
            Err(e) => {
                tracing::warn!(association = %association.id, instance = %uid, destination, error = %e, "copy to a local node failed");
                sub_operation_failure(&e)
            }
        }
    }
}

/// The listener, started and stopped by the host. The served AE titles are read from the host's
/// nodes when it starts.
#[derive(Debug)]
struct Listener {
    host: Host,
    cfg: Config,
    running: Mutex<Option<(CancellationToken, JoinHandle<()>)>>,
}

#[async_trait]
impl Service for Listener {
    async fn start(&self) -> Result<()> {
        let by_ae: HashMap<String, String> = self
            .host
            .nodes()
            .await?
            .into_iter()
            // Remote nodes are proxies; serving them over DIMSE is not supported yet.
            .filter(|n| !n.remote)
            .filter_map(|n| Some((n.ae_title?.as_str().to_owned(), n.name)))
            .collect();
        if by_ae.is_empty() {
            tracing::warn!("no local node has a DIMSE AE title; the DIMSE listener is not started");
            return Ok(());
        }
        let scp = Arc::new(Scp::new(
            self.cfg.scp_options(by_ae.keys().cloned()),
            Nodes {
                host: self.host,
                by_ae: by_ae.clone(),
            },
        ));
        let listen = self.cfg.listen;
        let listener = TcpListener::bind(listen).await.map_err(|e| {
            Error::new(
                ErrorKind::Unavailable,
                format!("cannot listen on {listen}: {e}"),
            )
        })?;
        let addr = listener.local_addr().unwrap_or(listen);
        let token = CancellationToken::new();
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
        let mut ae_titles: Vec<_> = by_ae.keys().collect();
        ae_titles.sort();
        tracing::info!(listen = %addr, ?ae_titles, "DIMSE listening");
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
            tracing::info!("DIMSE stopped");
        }
        Ok(())
    }
}

pub fn init(reg: &mut Registrar) -> Result<()> {
    let cfg: Config = reg.config()?;
    tracing::info!(listen = %cfg.listen, "DIMSE addon configured");
    reg.register_remote_node("dimse", remote::Dimse { host: reg.host() })?;
    let listener = Listener {
        host: reg.host(),
        cfg,
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
