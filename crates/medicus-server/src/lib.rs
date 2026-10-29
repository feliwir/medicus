//! The medicus core server as a library: the `medicus` binary is a thin CLI on top of it, and
//! tools (benchmarks) and integration tests use the same API.

mod access;
mod addons;
mod admin;
mod api;
mod archive;
mod auth;
mod capacity;
mod config;
#[cfg(test)]
mod dcmtk_tests;
#[cfg(test)]
mod dicomweb_tests;
mod events;
mod import;
mod ingest;
mod jobs;
mod live;
mod proxy;
mod query;
mod routes;
mod services;
mod settings;

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context as _;
use tokio_util::sync::CancellationToken;

pub use addons::manifest_file;
pub use archive::Archive;
pub use auth::{UserStore, UserSummary};
pub use config::{Config, LogConfig, LogFormat};
pub use import::ImportReport;
pub use medicus_core::capability::Ingested;
pub use settings::{Settings, SettingsStore, format_settings, parse_settings};

/// Deletes expired sessions and tokens every hour until `stop`.
async fn prune_credentials(accounts: Arc<auth::Accounts>, stop: CancellationToken) {
    let mut interval = tokio::time::interval(std::time::Duration::from_hours(1));
    loop {
        tokio::select! {
            () = stop.cancelled() => return,
            _ = interval.tick() => {}
        }
        match accounts.prune().await {
            Ok(0) => {}
            Ok(n) => tracing::debug!(count = n, "expired sessions and tokens deleted"),
            Err(e) => tracing::warn!(error = %e, "cannot delete expired sessions and tokens"),
        }
    }
}

/// The blob store of each partition that names one.
fn partition_stores(cfg: &Config) -> BTreeMap<medicus_core::Partition, String> {
    cfg.partitions
        .iter()
        .filter_map(|(p, c)| Some((p.clone(), c.storage.clone()?)))
        .collect()
}

/// A running archive: loaded addons and the wired archive service.
#[derive(Debug)]
pub struct Node {
    pub archive: Arc<Archive>,
    live: Arc<live::Live>,
    jobs: Arc<jobs::Jobs>,
    accounts: Arc<auth::Accounts>,
    caps: addons::Capabilities,
    /// The configuration file, and the effective configuration the node started with.
    bootstrap: Config,
    cfg: Config,
    /// The index providers, configured by the bootstrap rather than the settings.
    index_addons: BTreeSet<String>,
    /// Serializes changes to the settings.
    settings: tokio::sync::Mutex<SettingsState>,
    /// Cancelled when a restart is requested.
    restart: CancellationToken,
    /// The `dicomweb` addon's listener, for the `DICOMweb` proxy.
    dicomweb: Option<proxy::Upstream>,
}

#[derive(Debug)]
struct SettingsState {
    revision: u64,
    current: Settings,
    /// The settings the node started with; the sections that differ from them wait for a
    /// restart.
    started: Settings,
}

/// The outcome of [`Node::apply_settings`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Applied {
    pub revision: u64,
    /// The sections that differ from the settings the server started with; they take effect when
    /// it restarts. Everything else is in effect already.
    pub restart_required: Vec<&'static str>,
}

/// Why [`Node::serve`] returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    Shutdown,
    /// A restart was requested ([`Node::request_restart`]): start the server again.
    Restart,
}

impl Node {
    /// Loads the index, reads the settings from it (importing them from `bootstrap` on the first
    /// start), then loads the other addons and wires the archive.
    pub async fn open(bootstrap: Config) -> anyhow::Result<Self> {
        let loading = addons::load_index(&bootstrap, &tokio::runtime::Handle::current())?;
        let index = loading.index_store(&bootstrap.archive.index)?;
        let index_addons = loading.loaded();
        let (revision, settings) = settings::load(&*index, &bootstrap, &index_addons).await?;
        let cfg = settings.apply(bootstrap.clone(), &index_addons)?;
        tracing::info!(revision, "settings loaded");
        let live = Arc::new(live::Live::from_config(&cfg)?);
        let caps = loading.finish(&cfg)?;
        let archive = Arc::new(Archive::new(&caps, &cfg.archive).await?);
        archive.set_partition_stores(partition_stores(&cfg))?;
        let jobs = Arc::new(jobs::Jobs::new(
            archive.clone(),
            live.clone(),
            caps.remote_nodes.clone(),
            cfg.jobs.clone(),
        ));
        let accounts = Arc::new(auth::Accounts::open(archive.index.clone()).await?);
        if let Some(loader) = &caps.loader {
            loader.bind_services(Arc::new(services::NodeServices {
                archive: archive.clone(),
                live: live.clone(),
                jobs: jobs.clone(),
                accounts: accounts.clone(),
            }));
        }
        tracing::info!(?archive, "archive ready");
        let dicomweb = proxy::Upstream::of(
            &cfg,
            caps.addons.iter().any(|a| a.manifest.name == "dicomweb"),
        );
        Ok(Self {
            archive,
            live,
            jobs,
            accounts,
            caps,
            bootstrap,
            cfg,
            index_addons,
            settings: tokio::sync::Mutex::new(SettingsState {
                revision,
                current: settings.clone(),
                started: settings,
            }),
            restart: CancellationToken::new(),
            dicomweb,
        })
    }

    /// The one-time token that authorizes creating the first user, while no user exists (setup
    /// mode). Show it to the operator only.
    pub fn setup_token(&self) -> Option<String> {
        self.accounts.setup_token()
    }

    /// The settings in effect, their revision, and the sections that wait for a restart.
    pub async fn settings_status(&self) -> (u64, Settings, Vec<&'static str>) {
        let state = self.settings.lock().await;
        let restart_required = state.current.restart_sections(&state.started);
        (state.revision, state.current.clone(), restart_required)
    }

    /// The settings in effect and their revision.
    pub async fn settings(&self) -> (u64, Settings) {
        let state = self.settings.lock().await;
        (state.revision, state.current.clone())
    }

    /// Validates `settings`, stores them as the revision after `expected`, and puts what can
    /// change at runtime into effect (ADR-0004 §4): nodes, partitions, peers and access. The
    /// protocol listeners are restarted if the nodes changed. The other sections wait for a
    /// restart of the server.
    ///
    /// Fails with `Conflict` if `expected` is not the latest revision, and with
    /// `InvalidArgument` if the settings are invalid; nothing is stored then.
    pub async fn apply_settings(
        &self,
        expected: u64,
        settings: Settings,
        author: Option<String>,
        comment: Option<String>,
    ) -> medicus_core::Result<Applied> {
        use medicus_core::Error;
        let invalid = |e: anyhow::Error| Error::invalid_argument(format!("{e:#}"));
        let mut state = self.settings.lock().await;
        if expected != state.revision {
            return Err(Error::conflict(format!(
                "the settings changed since revision {expected}; the latest is {}",
                state.revision
            )));
        }
        let cfg = settings
            .apply(self.bootstrap.clone(), &self.index_addons)
            .map_err(invalid)?;
        let live = live::LiveState::from_config(&cfg).map_err(invalid)?;
        if settings.addons != state.current.addons {
            addons::check_selection(&self.caps.available, &cfg).map_err(invalid)?;
            for (name, entry) in cfg.addons.entries.iter().filter(|(_, e)| e.enabled) {
                if let Some(manifest) = self.caps.available.get(name) {
                    addons::check_config(manifest, &entry.config).map_err(invalid)?;
                }
            }
        }
        let restart_required = settings.restart_sections(&state.started);
        // A partition may name a blob store that an addon enabled in the same change provides:
        // then the stores are routed after the restart.
        let stores = partition_stores(&cfg);
        let route_stores = match self.archive.check_partition_stores(&stores) {
            Ok(()) => true,
            Err(_) if restart_required.contains(&"addons") => false,
            Err(e) => return Err(invalid(e)),
        };

        let revision = medicus_core::capability::SettingsRevision {
            revision: state.revision + 1,
            document: serde_json::to_value(&settings)
                .map_err(|e| Error::internal(e.to_string()))?,
            author: author.clone(),
            comment,
            created_at: time::OffsetDateTime::now_utc(),
        };
        self.archive.index.put_settings(&revision).await?;
        tracing::info!(
            revision = revision.revision,
            ?author,
            ?restart_required,
            "settings changed"
        );

        let nodes_changed = settings.nodes != state.current.nodes;
        if settings.live_changed(&state.current) {
            self.live.store(live);
            if route_stores {
                self.archive
                    .set_partition_stores(stores)
                    .map_err(|e| Error::internal(format!("{e:#}")))?;
            }
        }
        state.revision = revision.revision;
        state.current = settings;
        drop(state);

        let event = medicus_core::event::Event::new(
            medicus_core::event::EventKind::ConfigChanged,
            medicus_core::event::Origin::Internal {
                component: "settings".into(),
            },
        )
        .with_detail(serde_json::json!({
            "revision": revision.revision,
            "author": author,
            "restart_required": restart_required,
        }));
        match self.archive.index.append_event(&event).await {
            Ok(seq) => self.archive.events_committed(seq),
            Err(e) => tracing::warn!(error = %e, "cannot publish config.changed"),
        }

        if nodes_changed {
            // The listeners read the nodes when they start.
            self.stop_services().await;
            self.start_services().await.map_err(|e| {
                Error::internal(format!(
                    "settings revision {} is stored, but restarting the listeners failed: {e:#}",
                    revision.revision
                ))
            })?;
        }
        Ok(Applied {
            revision: revision.revision,
            restart_required,
        })
    }

    /// Asks [`Self::serve`] to stop gracefully and return [`Exit::Restart`].
    pub fn request_restart(&self) {
        tracing::info!("restart requested");
        self.restart.cancel();
    }

    /// Serves the HTTP API and delivers events until `shutdown` resolves or a restart is
    /// requested, then stops gracefully.
    pub async fn serve(
        self: Arc<Self>,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> anyhow::Result<Exit> {
        let stop = CancellationToken::new();
        let mut deliveries = events::spawn(&self.archive, self.caps.event_hooks.clone(), &stop);
        let capacity = tokio::spawn(capacity::monitor(self.archive.clone(), stop.clone()));
        let jobs = tokio::spawn(self.jobs.clone().run(stop.clone()));
        let pruning = tokio::spawn(prune_credentials(self.accounts.clone(), stop.clone()));
        self.start_services().await?;

        let state = api::AppState::new(&self);
        let addr = self.cfg.http.listen;
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .with_context(|| format!("binding {addr}"))?;
        tracing::info!(%addr, "HTTP API listening");
        let app = api::router(state).into_make_service_with_connect_info::<std::net::SocketAddr>();
        let token = stop.clone();
        let restart = self.restart.clone();
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                tokio::select! {
                    () = shutdown => {},
                    () = restart.cancelled() => {},
                }
                token.cancel();
            })
            .await
            .context("HTTP server")?;

        tracing::info!("shutting down");
        self.stop_services().await;
        stop.cancel();
        while deliveries.join_next().await.is_some() {}
        let _ = capacity.await;
        let _ = jobs.await;
        let _ = pruning.await;
        Ok(if self.restart.is_cancelled() {
            Exit::Restart
        } else {
            Exit::Shutdown
        })
    }

    /// Starts the addons' background services (e.g. the DIMSE listener).
    pub async fn start_services(&self) -> anyhow::Result<()> {
        for (name, service) in &self.caps.services {
            service
                .start()
                .await
                .with_context(|| format!("starting service {name}"))?;
            tracing::info!(service = %name, "service started");
        }
        Ok(())
    }

    /// Stops the addons' background services, in reverse order.
    pub async fn stop_services(&self) {
        for (name, service) in self.caps.services.iter().rev() {
            match service.stop().await {
                Ok(()) => tracing::info!(service = %name, "service stopped"),
                Err(e) => tracing::warn!(service = %name, error = %e, "stopping service failed"),
            }
        }
    }

    /// Imports the DICOM files below `path` into `partition`, `concurrency` at a time.
    pub async fn import(
        &self,
        partition: &medicus_core::Partition,
        path: &Path,
        concurrency: usize,
    ) -> anyhow::Result<ImportReport> {
        import::import(&self.archive, partition, path, concurrency).await
    }
}
