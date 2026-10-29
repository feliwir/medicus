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
mod degraded;
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
mod secrets;
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
    /// Users and credentials, in the state store (ADR-0005), which also holds the settings.
    accounts: Arc<auth::Accounts>,
    caps: addons::Capabilities,
    /// The configuration file, and the effective configuration the node started with.
    bootstrap: Config,
    cfg: Config,
    /// The index providers the bootstrap configures (the state store's), rather than the
    /// settings.
    index_addons: BTreeSet<String>,
    /// Serializes changes to the settings.
    settings: tokio::sync::Mutex<SettingsState>,
    /// Cancelled when a restart is requested.
    restart: CancellationToken,
    /// The `dicomweb` addon's listener, for the `DICOMweb` proxy.
    dicomweb: Option<proxy::Upstream>,
    /// Set if the archive index is unavailable: the archive's operations fail, and the services
    /// and background tasks do not run (ADR-0005 §3).
    degraded: Option<degraded::Degraded>,
}

/// Opens the archive on its index. If that is not the state store and it cannot be opened — its
/// addon failed to load, or the index fails — the archive is opened degraded instead.
async fn open_archive(
    caps: &mut addons::Capabilities,
    cfg: &Config,
) -> anyhow::Result<(Archive, Option<degraded::Degraded>)> {
    let failure = match caps.index_failure.take() {
        Some(e) => e,
        None => match Archive::new(caps, &cfg.archive).await {
            Ok(archive) => return Ok((archive, None)),
            // Failures of the index's calls, not of the configuration.
            Err(e)
                if cfg.archive.index != cfg.state_index()
                    && e.downcast_ref::<medicus_core::Error>().is_some() =>
            {
                e
            }
            Err(e) => return Err(e),
        },
    };
    let degraded = degraded::Degraded {
        index: cfg.archive.index.clone(),
        error: format!("{failure:#}"),
    };
    let index = Arc::new(degraded::UnavailableIndex::new(&degraded));
    let archive = Archive::degraded(caps, &cfg.archive, index)?;
    Ok((archive, Some(degraded)))
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
    /// Loads the state store, reads the settings from it (importing them from `bootstrap` on the
    /// first start), then loads the other addons, including the archive index, and wires the
    /// archive.
    pub async fn open(bootstrap: Config) -> anyhow::Result<Self> {
        let loading = addons::load_index(&bootstrap, &tokio::runtime::Handle::current())?;
        let state = loading.state_store(&bootstrap)?;
        let index_addons = loading.loaded();
        let (revision, settings) = settings::load(&*state, &bootstrap, &index_addons).await?;
        let cfg = settings.apply(bootstrap.clone(), &index_addons)?;
        tracing::info!(
            revision,
            state = cfg.state_index(),
            index = %cfg.archive.index,
            "settings loaded"
        );
        let live = Arc::new(live::Live::from_config(&cfg)?);
        let mut caps = loading.finish(&cfg)?;
        let (archive, degraded) = open_archive(&mut caps, &cfg).await?;
        let archive = Arc::new(archive);
        archive.set_partition_stores(partition_stores(&cfg))?;
        let jobs = Arc::new(jobs::Jobs::new(
            archive.clone(),
            live.clone(),
            caps.remote_nodes.clone(),
            cfg.jobs.clone(),
        ));
        let accounts = Arc::new(auth::Accounts::open(state.clone()).await?);
        if let Some(loader) = &caps.loader {
            loader.bind_services(Arc::new(services::NodeServices {
                archive: archive.clone(),
                live: live.clone(),
                jobs: jobs.clone(),
                accounts: accounts.clone(),
            }));
        }
        match &degraded {
            None => tracing::info!(?archive, "archive ready"),
            Some(d) => tracing::error!(
                index = %d.index,
                error = %d.error,
                "the archive index is unavailable: the server runs degraded, with the admin UI and \
                 API only; correct the settings and restart"
            ),
        }
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
            degraded,
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
    /// Secret placeholders in addon configurations keep the values in effect (ADR-0005 §7).
    ///
    /// Fails with `Conflict` if `expected` is not the latest revision, and with
    /// `InvalidArgument` if the settings are invalid; nothing is stored then.
    pub async fn apply_settings(
        &self,
        expected: u64,
        mut settings: Settings,
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
        settings
            .restore_secrets(&state.current, &self.caps.available)
            .map_err(invalid)?;
        settings
            .check_no_password_in_urls(&state.current, &self.caps.available)
            .map_err(invalid)?;
        let cfg = settings
            .apply(self.bootstrap.clone(), &self.index_addons)
            .map_err(invalid)?;
        let live = live::LiveState::from_config(&cfg).map_err(invalid)?;
        if settings.addons != state.current.addons
            || settings.archive.index != state.current.archive.index
        {
            let selection = addons::check_selection(&self.caps.available, &cfg).map_err(invalid)?;
            addons::check_archive_index(&self.caps.available, &selection, &cfg).map_err(invalid)?;
            for (name, entry) in cfg.addons.entries.iter().filter(|(_, e)| e.enabled) {
                if let Some(manifest) = self.caps.available.get(name) {
                    addons::check_config(manifest, &cfg, &entry.config).map_err(invalid)?;
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
        self.accounts.state().put_settings(&revision).await?;
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

    /// The archive index `settings` name, if it is not the one the archive runs on and that one
    /// holds instances: the change would orphan them (ADR-0005 §5). While degraded, whether it
    /// holds any is unknown, and the change is allowed: it may be the way out.
    pub(crate) async fn orphaning_index_change(
        &self,
        settings: &Settings,
    ) -> medicus_core::Result<Option<String>> {
        let index = settings
            .archive
            .index
            .as_deref()
            .unwrap_or(self.bootstrap.state_index());
        if index == self.cfg.archive.index || self.degraded.is_some() {
            return Ok(None);
        }
        let stored = self.archive.index.stored_bytes().await?;
        Ok(stored.iter().any(|s| s.bytes > 0).then(|| index.to_owned()))
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
        let mut background = tokio::task::JoinSet::new();
        background.spawn(prune_credentials(self.accounts.clone(), stop.clone()));
        let mut deliveries = if self.degraded.is_none() {
            background.spawn(capacity::monitor(self.archive.clone(), stop.clone()));
            background.spawn(self.jobs.clone().run(stop.clone()));
            events::spawn(&self.archive, self.caps.event_hooks.clone(), &stop)
        } else {
            tokio::task::JoinSet::new()
        };
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
        while background.join_next().await.is_some() {}
        Ok(if self.restart.is_cancelled() {
            Exit::Restart
        } else {
            Exit::Shutdown
        })
    }

    /// Starts the addons' background services (e.g. the DIMSE listener); none while degraded.
    pub async fn start_services(&self) -> anyhow::Result<()> {
        if self.degraded.is_some() {
            return Ok(());
        }
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
        if self.degraded.is_some() {
            return;
        }
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
