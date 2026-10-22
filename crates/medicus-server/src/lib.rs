//! The medicus core server as a library: the `medicus` binary is a thin CLI on top of it, and
//! tools (benchmarks) and integration tests use the same API.

mod addons;
mod api;
mod archive;
mod capacity;
mod config;
mod events;
mod import;
mod ingest;

use std::future::Future;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context as _;
use tokio_util::sync::CancellationToken;

pub use archive::Archive;
pub use config::{Config, LogConfig, LogFormat};
pub use import::ImportReport;
pub use ingest::Stored;

/// A running archive: loaded addons and the wired archive service.
#[derive(Debug)]
pub struct Node {
    pub archive: Arc<Archive>,
    caps: addons::Capabilities,
    cfg: Config,
}

impl Node {
    /// Loads the addons and wires the archive.
    pub async fn open(cfg: Config) -> anyhow::Result<Self> {
        let caps = addons::load(&cfg, &tokio::runtime::Handle::current())?;
        let archive = Arc::new(Archive::new(&caps, &cfg.archive).await?);
        tracing::info!(?archive, "archive ready");
        Ok(Self { archive, caps, cfg })
    }

    /// Serves the HTTP API and delivers events until `shutdown` resolves, then stops gracefully.
    pub async fn serve(
        self,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> anyhow::Result<()> {
        let stop = CancellationToken::new();
        let mut deliveries = events::spawn(&self.archive, self.caps.event_hooks.clone(), &stop);
        let capacity = tokio::spawn(capacity::monitor(self.archive.clone(), stop.clone()));

        let state = api::AppState {
            archive: self.archive.clone(),
            name: self.cfg.server.name.clone(),
            addons: self.caps.addons.clone().into(),
        };
        let addr = self.cfg.http.listen;
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .with_context(|| format!("binding {addr}"))?;
        tracing::info!(%addr, "HTTP API listening");
        let app = api::router(state).into_make_service_with_connect_info::<std::net::SocketAddr>();
        let token = stop.clone();
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                shutdown.await;
                token.cancel();
            })
            .await
            .context("HTTP server")?;

        tracing::info!("shutting down");
        stop.cancel();
        while deliveries.join_next().await.is_some() {}
        let _ = capacity.await;
        Ok(())
    }

    /// Imports the DICOM files below `path`, `concurrency` at a time.
    pub async fn import(&self, path: &Path, concurrency: usize) -> anyhow::Result<ImportReport> {
        import::import(&self.archive, path, concurrency).await
    }
}
