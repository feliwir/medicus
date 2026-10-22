//! Loading addons and collecting their capabilities by name.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, bail};
use medicus_core::capability::{BlobStore, EventHook, IndexStore, Service};
use medicus_host::{AddonInfo, AddonLoader, LoadedAddon, StaticAddon};
use serde_json::json;
use sha2::{Digest as _, Sha256};
use tokio::runtime::Handle;

use crate::config::{AddonEntry, Config};

/// Addons compiled into this binary (`builtin-*` features).
fn builtin(name: &str) -> Option<&'static StaticAddon> {
    match name {
        #[cfg(feature = "builtin-storage-fs")]
        "storage-fs" => Some(&storage_fs::ADDON),
        #[cfg(feature = "builtin-index-sqlite")]
        "index-sqlite" => Some(&index_sqlite::ADDON),
        _ => None,
    }
}

/// The capabilities of all loaded addons, by registered name.
#[derive(Default)]
pub(crate) struct Capabilities {
    pub(crate) index_stores: BTreeMap<String, Arc<dyn IndexStore>>,
    pub(crate) blob_stores: BTreeMap<String, Arc<dyn BlobStore>>,
    /// Event hooks, keyed `<addon>/<hook>`; the key is also the durable subscriber name.
    pub(crate) event_hooks: BTreeMap<String, Arc<dyn EventHook>>,
    /// Background services, keyed `<addon>/<service>`.
    pub(crate) services: BTreeMap<String, Arc<dyn Service>>,
    pub(crate) addons: Vec<AddonInfo>,
    /// The loader, to bind the host services once the archive exists.
    pub(crate) loader: Option<AddonLoader>,
}

impl std::fmt::Debug for Capabilities {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Capabilities")
            .field(
                "index_stores",
                &self.index_stores.keys().collect::<Vec<_>>(),
            )
            .field("blob_stores", &self.blob_stores.keys().collect::<Vec<_>>())
            .field("event_hooks", &self.event_hooks.keys().collect::<Vec<_>>())
            .field("services", &self.services.keys().collect::<Vec<_>>())
            .field("addons", &self.addons)
            .finish_non_exhaustive()
    }
}

impl Capabilities {
    fn add(&mut self, addon: LoadedAddon) -> anyhow::Result<()> {
        let name = addon.info.name.clone();
        for c in addon.index_stores {
            if self.index_stores.insert(c.name.clone(), c.value).is_some() {
                bail!(
                    "index store {:?} is registered twice (second time by {name})",
                    c.name
                );
            }
        }
        for c in addon.blob_stores {
            if self.blob_stores.insert(c.name.clone(), c.value).is_some() {
                bail!(
                    "blob store {:?} is registered twice (second time by {name})",
                    c.name
                );
            }
        }
        for c in addon.event_hooks {
            self.event_hooks
                .insert(format!("{name}/{}", c.name), c.value);
        }
        for c in addon.services {
            self.services.insert(format!("{name}/{}", c.name), c.value);
        }
        self.addons.push(addon.info);
        Ok(())
    }
}

/// The addons to load: the configured ones, or, if none are configured, the built-in local
/// storage and `SQLite` index below `data_dir` (zero-configuration start).
fn effective_entries(cfg: &Config) -> BTreeMap<String, AddonEntry> {
    if !cfg.addons.entries.is_empty() {
        return cfg.addons.entries.clone();
    }
    let data = &cfg.server.data_dir;
    let entry = |config| AddonEntry {
        enabled: true,
        library: None,
        sha256: None,
        config,
    };
    BTreeMap::from([
        (
            "storage-fs".to_owned(),
            entry(json!({ "root": data.join("blobs"), "name": cfg.archive.storage })),
        ),
        (
            "index-sqlite".to_owned(),
            entry(json!({ "path": data.join("index.sqlite"), "name": cfg.archive.index })),
        ),
    ])
}

fn resolve_library(path: &Path, search_paths: &[PathBuf]) -> anyhow::Result<PathBuf> {
    if path.is_absolute() || path.exists() {
        return Ok(path.to_owned());
    }
    search_paths
        .iter()
        .map(|dir| dir.join(path))
        .find(|p| p.exists())
        .with_context(|| {
            format!(
                "addon library {} not found in search paths {search_paths:?}",
                path.display()
            )
        })
}

fn verify_sha256(path: &Path, expected: &str) -> anyhow::Result<()> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let actual = hex::encode(Sha256::digest(&bytes));
    if !actual.eq_ignore_ascii_case(expected.trim()) {
        bail!(
            "addon library {} has SHA-256 {actual}, expected {expected}",
            path.display()
        );
    }
    Ok(())
}

/// Loads all enabled addons.
pub(crate) fn load(cfg: &Config, rt: &Handle) -> anyhow::Result<Capabilities> {
    let loader = AddonLoader::new(rt.clone());
    let mut caps = Capabilities::default();
    for (name, entry) in effective_entries(cfg).iter().filter(|(_, e)| e.enabled) {
        let addon = if let Some(lib) = &entry.library {
            let path = resolve_library(lib, &cfg.addons.search_paths)?;
            if let Some(expected) = &entry.sha256 {
                verify_sha256(&path, expected)?;
            }
            loader.load_library(&path, name, &entry.config)
        } else {
            let Some(statics) = builtin(name) else {
                bail!("addon {name:?} is not built into this server; set `library` to load it");
            };
            if entry.sha256.is_some() {
                bail!("addon {name:?}: `sha256` only applies to libraries");
            }
            loader.load_static(statics, name, &entry.config)
        }
        .with_context(|| format!("loading addon {name:?}"))?;
        caps.add(addon)?;
    }
    tracing::info!(?caps, "addons loaded");
    caps.loader = Some(loader);
    Ok(caps)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_config_uses_builtin_local_addons() {
        let cfg = Config::default();
        let entries = effective_entries(&cfg);
        assert_eq!(
            entries.keys().collect::<Vec<_>>(),
            ["index-sqlite", "storage-fs"]
        );
        assert!(entries.values().all(|e| e.library.is_none()));
        assert_eq!(entries["storage-fs"].config["name"], "local");
    }

    #[test]
    fn sha256_pinning() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("lib.so");
        std::fs::write(&f, b"abc").unwrap();
        // SHA-256("abc")
        let abc = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        verify_sha256(&f, abc).unwrap();
        verify_sha256(&f, &abc.to_uppercase()).unwrap();
        assert!(verify_sha256(&f, "00").is_err());
    }
}
