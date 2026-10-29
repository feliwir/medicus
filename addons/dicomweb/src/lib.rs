//! `dicomweb`: the archive's `DICOMweb` services (PS3.18): QIDO-RS, WADO-RS and STOW-RS.
//!
//! ```toml
//! [addons.dicomweb]
//! config = { listen = "0.0.0.0:8080" }
//! ```
//!
//! Every node with a `DICOMweb` path is served at its service root, by default `/nodes/{path}/rs`
//! (ADR-0003); the default node also at `/rs`. Both are configurable (`node_root`,
//! `default_root`), e.g. `/dicom-web/{node}` and `/dicom-web`. Each request reaches the host with the node and the caller, so the core
//! decides what the caller may do and in which partition, exactly as for DIMSE. Instances are
//! returned as stored: a request for another transfer syntax is answered with 406. Rendered
//! resources and thumbnails are decoded with dicom-pixeldata and served as JPEG, PNG or GIF.
//!
//! The addon runs its own HTTP listener. Callers authenticate with the server's own credentials:
//! an API token (`Authorization: Bearer`) or the session cookie of the admin UI, checked through
//! the host. Requests that change data and are authenticated by the cookie alone need an
//! `X-Requested-With` header. Alternatively a reverse proxy authenticates: with `user_header`
//! set, the header it names (e.g. `X-Forwarded-User`) is taken as the authenticated principal —
//! only safe when every request comes through that proxy. Otherwise callers are anonymous.

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
use medicus_addon_sdk::{
    AddonManifest, Capability, Host, Registrar, async_trait, runtime, static_addon,
};
use medicus_core::capability::{HostServices as _, Service};
use medicus_core::{Error, ErrorKind, Result};
use serde::Deserialize;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Address and port to listen on.
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    /// Base of the URLs in responses (e.g. `https://pacs.example.org`); default: from the
    /// request's `Host` (and `X-Forwarded-Proto`) header.
    #[serde(default)]
    pub base_url: Option<String>,
    /// The service root of each node; `{node}` stands for the node's `DICOMweb` path. It must
    /// begin with a fixed segment other than `api` and `health`, which the server uses.
    #[serde(default = "default_node_root")]
    #[schemars(extend("pattern" = NODE_ROOT_PATTERN))]
    pub node_root: String,
    /// The service root of the default node.
    #[serde(default = "default_default_root")]
    #[schemars(extend("pattern" = ROOT_PATTERN))]
    pub default_root: String,
    /// The node path also served at `default_root`; default: the only node, if there is one.
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

/// Segments of a service root: unreserved URL characters only.
const SEGMENT: &str = "[A-Za-z0-9._~-]+";

/// A service root: `/`-separated segments, the first not `api` or `health` (the server's).
const ROOT_PATTERN: &str = r"^/(?!(api|health)(/|$))[A-Za-z0-9._~-]+(/[A-Za-z0-9._~-]+)*$";

/// A node's service root: like [`ROOT_PATTERN`], with `{node}` as one segment after the first.
const NODE_ROOT_PATTERN: &str =
    r"^/(?!(api|health)(/|$))[A-Za-z0-9._~-]+(/[A-Za-z0-9._~-]+)*/\{node\}(/[A-Za-z0-9._~-]+)*$";

fn default_node_root() -> String {
    "/nodes/{node}/rs".into()
}

fn default_default_root() -> String {
    "/rs".into()
}

impl Config {
    /// Checks the service roots, as the schema does, and that they cannot be confused.
    fn check_roots(&self) -> Result<()> {
        let segments =
            |root: &str| -> Vec<String> { root.split('/').skip(1).map(str::to_owned).collect() };
        let valid = |root: &str, node: bool| {
            let parts = segments(root);
            root.starts_with('/')
                && !matches!(parts.first().map(String::as_str), Some("api" | "health"))
                && parts.iter().filter(|p| *p == "{node}").count() == usize::from(node)
                && parts.first().is_some_and(|p| p != "{node}")
                && parts.iter().all(|p| {
                    p == "{node}"
                        || (!p.is_empty()
                            && p.chars()
                                .all(|c| c.is_ascii_alphanumeric() || "._~-".contains(c)))
                })
        };
        if !valid(&self.node_root, true) {
            return Err(Error::invalid_argument(format!(
                "node_root {:?} must be a path like /nodes/{{node}}/rs: segments of {SEGMENT}, \
                 {{node}} once and not first, and not under /api or /health",
                self.node_root
            )));
        }
        if !valid(&self.default_root, false) {
            return Err(Error::invalid_argument(format!(
                "default_root {:?} must be a path like /rs: segments of {SEGMENT}, not under /api \
                 or /health",
                self.default_root
            )));
        }
        // The default root must not be the root of some node: requests would be ambiguous.
        let node = segments(&self.node_root);
        let default = segments(&self.default_root);
        let overlaps = node.len() == default.len()
            && node
                .iter()
                .zip(&default)
                .all(|(n, d)| n == "{node}" || n == d);
        if overlaps {
            return Err(Error::invalid_argument(format!(
                "default_root {:?} is the root of a node under node_root {:?}",
                self.default_root, self.node_root
            )));
        }
        Ok(())
    }
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
        // Starting again (after the nodes changed) replaces the running listener: it has to
        // release the port first.
        self.stop().await?;
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
            node_root: self.cfg.node_root.clone(),
            default_root: self.cfg.default_root.clone(),
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
        *self
            .running
            .lock()
            .map_err(|_| Error::internal("listener state poisoned"))? = Some((token, task));
        tracing::info!(
            listen = %addr,
            nodes = ?paths,
            node_root = %self.cfg.node_root,
            default_root = %self.cfg.default_root,
            "DICOMweb listening"
        );
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
    cfg.check_roots()?;
    tracing::info!(listen = %cfg.listen, "DICOMweb addon configured");
    let listener = Listener {
        host: reg.host(),
        cfg,
        running: Mutex::new(None),
    };
    reg.register_service("http", listener)
}

/// What this addon provides and needs (ADR-0004 §2).
pub fn manifest() -> AddonManifest {
    AddonManifest::new("dicomweb", env!("CARGO_PKG_VERSION"))
        .provides([Capability::Service])
        .requires([Capability::IndexStore, Capability::BlobStore])
        .config_schema(medicus_addon_sdk::config_schema::<Config>())
}

/// Entry points for linking this addon into the server binary.
pub static ADDON: medicus_addon_sdk::abi::StaticAddon =
    static_addon!(manifest = manifest(), init = init);

#[cfg(test)]
mod tests {
    use super::*;

    fn roots(node_root: &str, default_root: &str) -> Result<()> {
        let mut cfg: Config = serde_json::from_value(serde_json::json!({})).unwrap();
        cfg.node_root = node_root.into();
        cfg.default_root = default_root.into();
        cfg.check_roots()
    }

    #[test]
    fn service_roots() {
        for (node, default) in [
            ("/nodes/{node}/rs", "/rs"),
            ("/dicom-web/{node}", "/dicom-web"),
            ("/pacs/{node}/dicom-web/v1", "/pacs/dicom-web"),
        ] {
            roots(node, default).unwrap_or_else(|e| panic!("{node} {default}: {e}"));
        }
        for (node, default) in [
            ("/{node}/rs", "/rs"),
            ("/nodes/rs", "/rs"),
            ("/nodes/{node}/{node}", "/rs"),
            ("/nodes/{node}/", "/rs"),
            ("nodes/{node}", "/rs"),
            ("/api/{node}", "/rs"),
            ("/health/{node}", "/rs"),
            ("/nodes/{node}", "/"),
            ("/nodes/{node}", "/api"),
            ("/nodes/{node} x", "/rs"),
            // `/dicom-web/rs` would also be the root of a node `rs`.
            ("/dicom-web/{node}", "/dicom-web/rs"),
        ] {
            assert!(roots(node, default).is_err(), "{node} {default}");
        }
    }
}
