//! `dicomweb`: the archive's `DICOMweb` services (PS3.18): QIDO-RS, WADO-RS and STOW-RS.
//!
//! ```toml
//! [addons.dicomweb]
//! config = { listen = "0.0.0.0:8080" }
//! ```
//!
//! Every node with a `DICOMweb` path is served at `/nodes/{path}/rs` (ADR-0003); the default
//! node also at `/rs`. Each request reaches the host with the node and the caller, so the core
//! decides what the caller may do and in which partition, exactly as for DIMSE. Instances are
//! returned as stored: a request for another transfer syntax is answered with 406. Rendered
//! resources and thumbnails are decoded with dicom-pixeldata and served as JPEG, PNG or GIF.
//!
//! The addon runs its own HTTP listener. Authentication is left to a reverse proxy for now:
//! with `user_header` set, the header it names (e.g. `X-Forwarded-User`) is taken as the
//! authenticated principal; without it, every caller is anonymous.

mod error;
mod multipart;
mod qido;
mod render;
mod stow;
mod wado;
mod web;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::http::HeaderName;
use medicus_addon_sdk::{Host, Registrar, async_trait, runtime, static_addon};
use medicus_core::capability::{HostServices as _, Service};
use medicus_core::{Error, ErrorKind, Result};
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
    /// Base of the URLs in responses (e.g. `https://pacs.example.org`); default: from the
    /// request's `Host` (and `X-Forwarded-Proto`) header.
    #[serde(default)]
    pub base_url: Option<String>,
    /// The node path also served at `/rs`; default: the only node, if there is one.
    #[serde(default)]
    pub default_node: Option<String>,
    /// Header carrying the user authenticated by a trusted reverse proxy. Only set this if
    /// nobody can reach the listener without going through that proxy.
    #[serde(default)]
    pub user_header: Option<String>,
    /// Origins allowed to call the API from a browser (CORS), e.g. a viewer's
    /// `http://viewer.example.org`; `*` allows any. Empty: no CORS headers.
    #[serde(default)]
    pub cors_origins: Vec<String>,
}

fn default_listen() -> SocketAddr {
    SocketAddr::from(([0, 0, 0, 0], 8080))
}

/// The HTTP listener, started and stopped by the host. The served nodes are read from the host
/// when it starts.
#[derive(Debug)]
struct Listener {
    host: Host,
    cfg: Config,
    running: Mutex<Option<(CancellationToken, JoinHandle<()>)>>,
}

#[async_trait]
impl Service for Listener {
    async fn start(&self) -> Result<()> {
        let nodes: HashMap<String, String> = self
            .host
            .nodes()
            .await?
            .into_iter()
            // Remote nodes are proxies; serving them is not supported yet (M4c).
            .filter(|n| !n.remote)
            .filter_map(|n| Some((n.dicomweb_path?, n.name)))
            .collect();
        if nodes.is_empty() {
            tracing::warn!(
                "no local node has a DICOMweb path; the DICOMweb listener is not started"
            );
            return Ok(());
        }
        let default_path = match &self.cfg.default_node {
            Some(path) if nodes.contains_key(path) => Some(path.clone()),
            Some(path) => {
                return Err(Error::invalid_argument(format!(
                    "default_node {path:?} is not a node's DICOMweb path"
                )));
            }
            None if nodes.len() == 1 => nodes.keys().next().cloned(),
            None => None,
        };
        let user_header = self
            .cfg
            .user_header
            .as_deref()
            .map(HeaderName::try_from)
            .transpose()
            .map_err(|e| Error::invalid_argument(format!("user_header: {e}")))?;
        let mut paths: Vec<_> = nodes.keys().cloned().collect();
        paths.sort();
        let state = web::State {
            host: self.host,
            nodes: Arc::new(nodes),
            default_path,
            base_url: self.cfg.base_url.clone(),
            user_header,
        };
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
        let app = web::router(state, &self.cfg.cors_origins)?
            .into_make_service_with_connect_info::<SocketAddr>();
        let task = runtime().spawn(async move {
            let served = axum::serve(listener, app)
                .with_graceful_shutdown(async move { shutdown.cancelled().await })
                .await;
            if let Err(e) = served {
                tracing::error!(error = %e, "DICOMweb listener failed");
            }
        });
        let previous = self
            .running
            .lock()
            .map_err(|_| Error::internal("listener state poisoned"))?
            .replace((token, task));
        if let Some((token, _)) = previous {
            token.cancel();
        }
        tracing::info!(listen = %addr, nodes = ?paths, "DICOMweb listening");
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
            tracing::info!("DICOMweb stopped");
        }
        Ok(())
    }
}

pub fn init(reg: &mut Registrar) -> Result<()> {
    let cfg: Config = reg.config()?;
    tracing::info!(listen = %cfg.listen, "DICOMweb addon configured");
    let listener = Listener {
        host: reg.host(),
        cfg,
        running: Mutex::new(None),
    };
    reg.register_service("http", listener)
}

/// Entry points for linking this addon into the server binary.
pub static ADDON: medicus_addon_sdk::abi::StaticAddon = static_addon!(
    name = "dicomweb",
    version = env!("CARGO_PKG_VERSION"),
    init = init
);
