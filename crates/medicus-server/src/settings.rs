//! The settings document (ADR-0004 §3): everything about the server that can change while it is
//! installed, stored as revisions in the index.
//!
//! The configuration file (and the `MEDICUS_*` environment) only *bootstraps* the server: it says
//! where the data lives, where to listen and how to log, and which index to use — what is needed
//! before the settings can be read. Its other sections are the initial settings: on the first
//! start, when the index holds no settings yet, they are imported as revision 1. After that the
//! stored settings are in effect, and changes go through the admin API or `medicus settings
//! import`.
//!
//! The settings sections have the same shape as in the configuration file, so an exported
//! settings document is also a valid configuration file.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use anyhow::{Context as _, bail};
use medicus_core::Partition;
use medicus_core::capability::index::DuplicatePolicy;
use medicus_core::capability::{Endpoint, IndexStore, SettingsRevision};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::config::{
    AccessConfig, AddonEntry, CapacityConfig, Config, ExtraTagConfig, JobsConfig, NodeConfig,
    PartitionConfig, PrincipalConfig, RoleConfig,
};

/// The settings document.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub server: ServerSettings,
    pub archive: ArchiveSettings,
    /// Which addons are enabled, and their configuration. Libraries, and the index providers,
    /// are configured in the bootstrap.
    pub addons: BTreeMap<String, AddonSettings>,
    pub peers: BTreeMap<String, Endpoint>,
    pub jobs: JobsConfig,
    pub partitions: BTreeMap<Partition, PartitionConfig>,
    pub nodes: BTreeMap<String, NodeConfig>,
    pub roles: BTreeMap<String, RoleConfig>,
    pub principals: BTreeMap<String, PrincipalConfig>,
    pub access: AccessConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerSettings {
    pub name: String,
}

impl Default for ServerSettings {
    fn default() -> Self {
        Self {
            name: crate::config::ServerConfig::default().name,
        }
    }
}

/// `archive` without `index`, which is part of the bootstrap.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ArchiveSettings {
    pub storage: String,
    pub duplicate_policy: DuplicatePolicy,
    pub max_header_bytes: usize,
    pub extra_tags: Vec<ExtraTagConfig>,
    pub capacity: CapacityConfig,
}

impl Default for ArchiveSettings {
    fn default() -> Self {
        let a = crate::config::ArchiveConfig::default();
        Self {
            storage: a.storage,
            duplicate_policy: a.duplicate_policy,
            max_header_bytes: a.max_header_bytes,
            extra_tags: a.extra_tags,
            capacity: a.capacity,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddonSettings {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub config: serde_json::Value,
}

fn default_true() -> bool {
    true
}

impl Settings {
    /// The settings sections of a configuration; `bootstrap_addons` (the index providers) are
    /// left out.
    pub(crate) fn from_config(cfg: &Config, bootstrap_addons: &BTreeSet<String>) -> Self {
        Self {
            server: ServerSettings {
                name: cfg.server.name.clone(),
            },
            archive: ArchiveSettings {
                storage: cfg.archive.storage.clone(),
                duplicate_policy: cfg.archive.duplicate_policy,
                max_header_bytes: cfg.archive.max_header_bytes,
                extra_tags: cfg.archive.extra_tags.clone(),
                capacity: cfg.archive.capacity.clone(),
            },
            addons: cfg
                .addons
                .entries
                .iter()
                .filter(|(name, _)| !bootstrap_addons.contains(*name))
                .map(|(name, e)| {
                    (
                        name.clone(),
                        AddonSettings {
                            enabled: e.enabled,
                            config: e.config.clone(),
                        },
                    )
                })
                .collect(),
            peers: cfg.peers.clone(),
            jobs: cfg.jobs.clone(),
            partitions: cfg.partitions.clone(),
            nodes: cfg.nodes.clone(),
            roles: cfg.roles.clone(),
            principals: cfg.principals.clone(),
            access: cfg.access.clone(),
        }
    }

    /// The effective configuration: `bootstrap` with its settings sections replaced by these.
    /// An addon keeps its `library` and `sha256` from the bootstrap; an addon the settings do not
    /// mention keeps its bootstrap entry. Settings for `bootstrap_addons` are refused.
    pub(crate) fn apply(
        &self,
        mut bootstrap: Config,
        bootstrap_addons: &BTreeSet<String>,
    ) -> anyhow::Result<Config> {
        if let Some(name) = self
            .addons
            .keys()
            .find(|name| bootstrap_addons.contains(*name))
        {
            bail!(
                "the settings configure the index addon {name:?}, which is configured in the \
                 configuration file"
            );
        }
        let cfg = &mut bootstrap;
        cfg.server.name.clone_from(&self.server.name);
        cfg.archive.storage.clone_from(&self.archive.storage);
        cfg.archive.duplicate_policy = self.archive.duplicate_policy;
        cfg.archive.max_header_bytes = self.archive.max_header_bytes;
        cfg.archive.extra_tags.clone_from(&self.archive.extra_tags);
        cfg.archive.capacity.clone_from(&self.archive.capacity);
        for (name, addon) in &self.addons {
            let entry = cfg
                .addons
                .entries
                .entry(name.clone())
                .or_insert_with(|| AddonEntry {
                    enabled: true,
                    library: None,
                    sha256: None,
                    config: serde_json::Value::Null,
                });
            entry.enabled = addon.enabled;
            entry.config.clone_from(&addon.config);
        }
        cfg.peers.clone_from(&self.peers);
        cfg.jobs.clone_from(&self.jobs);
        cfg.partitions.clone_from(&self.partitions);
        cfg.nodes.clone_from(&self.nodes);
        cfg.roles.clone_from(&self.roles);
        cfg.principals.clone_from(&self.principals);
        cfg.access.clone_from(&self.access);
        Ok(bootstrap)
    }

    /// Checks what can be checked without loading the addons: the settings parse, and nodes,
    /// partitions and access form a valid directory.
    pub(crate) fn validate(
        &self,
        bootstrap: &Config,
        bootstrap_addons: &BTreeSet<String>,
    ) -> anyhow::Result<Config> {
        let cfg = self.apply(bootstrap.clone(), bootstrap_addons)?;
        crate::access::Directory::from_config(&cfg)?;
        Ok(cfg)
    }

    /// The sections that differ from `other` and only take effect when the server restarts:
    /// addons are loaded, and the archive and job engine configured, once at startup.
    pub(crate) fn restart_sections(&self, other: &Self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.server != other.server {
            out.push("server");
        }
        if self.archive != other.archive {
            out.push("archive");
        }
        if self.addons != other.addons {
            out.push("addons");
        }
        if self.jobs != other.jobs {
            out.push("jobs");
        }
        out
    }

    /// Whether the sections that take effect at once differ from `other`.
    pub(crate) fn live_changed(&self, other: &Self) -> bool {
        self.peers != other.peers
            || self.partitions != other.partitions
            || self.nodes != other.nodes
            || self.roles != other.roles
            || self.principals != other.principals
            || self.access != other.access
    }

    fn parse(revision: &SettingsRevision) -> anyhow::Result<Self> {
        serde_json::from_value(revision.document.clone())
            .with_context(|| format!("settings revision {} is invalid", revision.revision))
    }
}

/// The settings in effect: the latest stored revision, or, on the first start, the settings
/// sections of the bootstrap configuration, which are stored as revision 1.
pub(crate) async fn load(
    index: &dyn IndexStore,
    bootstrap: &Config,
    bootstrap_addons: &BTreeSet<String>,
) -> anyhow::Result<(u64, Settings)> {
    let from_file = Settings::from_config(bootstrap, bootstrap_addons);
    let Some(current) = index.settings_current().await? else {
        from_file.validate(bootstrap, bootstrap_addons)?;
        let revision = SettingsRevision {
            revision: 1,
            document: serde_json::to_value(&from_file)?,
            author: None,
            comment: Some("imported from the configuration".into()),
            created_at: OffsetDateTime::now_utc(),
        };
        index
            .put_settings(&revision)
            .await
            .context("storing the initial settings")?;
        tracing::info!("settings imported from the configuration as revision 1");
        return Ok((1, from_file));
    };
    let settings = Settings::parse(&current)?;
    if settings != from_file {
        tracing::warn!(
            revision = current.revision,
            "the settings in the configuration differ from the stored settings, which are in \
             effect: the configuration's settings are only imported on the first start (use \
             `medicus settings import` to apply them)"
        );
    }
    Ok((current.revision, settings))
}

/// The settings stored in a server's index, for the `medicus settings` commands. The server
/// itself does not need to run.
pub struct SettingsStore {
    index: Arc<dyn IndexStore>,
    bootstrap: Config,
    bootstrap_addons: BTreeSet<String>,
    // Keeps the index addon loaded.
    _loading: crate::addons::Loading,
}

impl std::fmt::Debug for SettingsStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SettingsStore").finish_non_exhaustive()
    }
}

impl SettingsStore {
    /// Opens the index of the server configured by `bootstrap`.
    pub fn open(bootstrap: Config) -> anyhow::Result<Self> {
        let loading = crate::addons::load_index(&bootstrap, &tokio::runtime::Handle::current())?;
        Ok(Self {
            index: loading.index_store(&bootstrap.archive.index)?,
            bootstrap_addons: loading.loaded(),
            bootstrap,
            _loading: loading,
        })
    }

    pub(crate) fn index(&self) -> Arc<dyn IndexStore> {
        self.index.clone()
    }

    /// The settings in effect: the stored ones, or before the first start the configuration's,
    /// which will be imported then.
    pub(crate) async fn current(&self) -> anyhow::Result<Settings> {
        match self.index.settings_current().await? {
            Some(revision) => Settings::parse(&revision),
            None => Ok(Settings::from_config(
                &self.bootstrap,
                &self.bootstrap_addons,
            )),
        }
    }

    /// The latest revision, or the given one.
    pub async fn get(&self, revision: Option<u64>) -> anyhow::Result<Option<SettingsRevision>> {
        Ok(match revision {
            Some(n) => self.index.settings_revision(n).await?,
            None => self.index.settings_current().await?,
        })
    }

    /// Revisions, newest first.
    pub async fn history(&self, limit: u32) -> anyhow::Result<Vec<SettingsRevision>> {
        Ok(self.index.settings_history(limit, 0).await?)
    }

    /// Validates `settings` and stores them as a new revision. They take effect when the server
    /// starts next.
    pub async fn put(
        &self,
        settings: &Settings,
        author: Option<String>,
        comment: Option<String>,
    ) -> anyhow::Result<SettingsRevision> {
        settings.validate(&self.bootstrap, &self.bootstrap_addons)?;
        let latest = self
            .index
            .settings_current()
            .await?
            .map_or(0, |r| r.revision);
        let revision = SettingsRevision {
            revision: latest + 1,
            document: serde_json::to_value(settings)?,
            author,
            comment,
            created_at: OffsetDateTime::now_utc(),
        };
        self.index.put_settings(&revision).await?;
        Ok(revision)
    }
}

#[cfg(test)]
impl SettingsStore {
    /// Stores the settings sections of `cfg` as a new revision, as a changed configuration file
    /// would have done before settings lived in the index.
    pub(crate) async fn put_config(&self, cfg: &Config) -> anyhow::Result<SettingsRevision> {
        let settings = Settings::from_config(cfg, &self.bootstrap_addons);
        self.put(&settings, None, None).await
    }
}

/// Parses a settings document from TOML (like the configuration file) or JSON.
pub fn parse_settings(text: &str, json: bool) -> anyhow::Result<Settings> {
    Ok(if json {
        serde_json::from_str(text)?
    } else {
        toml::from_str(text)?
    })
}

/// Formats a settings revision as TOML or JSON.
pub fn format_settings(revision: &SettingsRevision, json: bool) -> anyhow::Result<String> {
    let settings = Settings::parse(revision)?;
    Ok(if json {
        serde_json::to_string_pretty(&settings)? + "\n"
    } else {
        toml::to_string_pretty(&settings)?
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NodeDimseConfig;

    fn index_addons() -> BTreeSet<String> {
        BTreeSet::from(["index-sqlite".to_owned()])
    }

    fn configured() -> Config {
        let mut cfg = Config::default();
        cfg.server.name = "archive-1".into();
        cfg.archive.storage = "fast".into();
        cfg.nodes.insert(
            "A".into(),
            NodeConfig {
                partition: Some(Partition::default_partition()),
                peer: None,
                dimse: Some(NodeDimseConfig {
                    ae_title: "A".parse().unwrap(),
                }),
                dicomweb: None,
            },
        );
        for (name, library) in [("index-sqlite", None), ("dimse", Some("libdimse.so"))] {
            cfg.addons.entries.insert(
                name.into(),
                AddonEntry {
                    enabled: true,
                    library: library.map(Into::into),
                    sha256: None,
                    config: serde_json::json!({ "x": name }),
                },
            );
        }
        cfg
    }

    #[test]
    fn settings_roundtrip_through_the_configuration() {
        let cfg = configured();
        let settings = Settings::from_config(&cfg, &index_addons());
        assert!(!settings.addons.contains_key("index-sqlite"));
        assert_eq!(settings.archive.storage, "fast");

        // Applying them to a bare bootstrap yields the configuration again, libraries included.
        let mut bootstrap = Config::default();
        bootstrap.server.data_dir = cfg.server.data_dir.clone();
        bootstrap.addons.entries.insert(
            "index-sqlite".into(),
            cfg.addons.entries["index-sqlite"].clone(),
        );
        bootstrap.addons.entries.insert(
            "dimse".into(),
            AddonEntry {
                enabled: false,
                library: Some("libdimse.so".into()),
                sha256: None,
                config: serde_json::Value::Null,
            },
        );
        assert_eq!(settings.apply(bootstrap, &index_addons()).unwrap(), cfg);

        // TOML and JSON both round-trip.
        let revision = SettingsRevision {
            revision: 1,
            document: serde_json::to_value(&settings).unwrap(),
            author: None,
            comment: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
        };
        for json in [false, true] {
            let text = format_settings(&revision, json).unwrap();
            assert_eq!(parse_settings(&text, json).unwrap(), settings, "{text}");
        }
    }

    #[test]
    fn index_addons_are_not_settings() {
        let mut settings = Settings::default();
        settings.addons.insert(
            "index-sqlite".into(),
            AddonSettings {
                enabled: true,
                config: serde_json::Value::Null,
            },
        );
        let err = settings
            .apply(Config::default(), &index_addons())
            .unwrap_err();
        assert!(err.to_string().contains("index-sqlite"), "{err}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_configuration_is_imported_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut bootstrap = configured();
        bootstrap.server.data_dir = dir.path().to_owned();
        bootstrap.addons.entries.clear();
        bootstrap.archive.storage = "local".into();
        let store = SettingsStore::open(bootstrap.clone()).unwrap();
        let index = store.index.clone();
        let addons = store.bootstrap_addons.clone();
        assert_eq!(addons, index_addons());

        let (revision, imported) = load(&*index, &bootstrap, &addons).await.unwrap();
        assert_eq!(revision, 1);
        assert_eq!(imported.server.name, "archive-1");

        // Later starts use the stored settings, whatever the file says.
        let mut changed = bootstrap.clone();
        changed.server.name = "renamed-in-file".into();
        let (revision, settings) = load(&*index, &changed, &addons).await.unwrap();
        assert_eq!(revision, 1);
        assert_eq!(settings.server.name, "archive-1");

        // A new revision takes effect at the next start.
        let mut renamed = settings.clone();
        renamed.server.name = "archive-2".into();
        let stored = store
            .put(&renamed, Some("admin".into()), None)
            .await
            .unwrap();
        assert_eq!(stored.revision, 2);
        let (revision, settings) = load(&*index, &bootstrap, &addons).await.unwrap();
        assert_eq!((revision, settings), (2, renamed));
        assert_eq!(store.history(10).await.unwrap().len(), 2);

        // Invalid settings are refused before they are stored.
        let mut invalid = Settings::default();
        invalid.nodes.insert(
            "B".into(),
            NodeConfig {
                partition: None,
                peer: None,
                dimse: None,
                dicomweb: None,
            },
        );
        let err = store.put(&invalid, None, None).await.unwrap_err();
        assert!(err.to_string().contains("exactly one of"), "{err}");
        assert_eq!(store.get(None).await.unwrap().unwrap().revision, 2);
    }
}
