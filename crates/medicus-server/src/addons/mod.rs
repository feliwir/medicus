//! Loading addons and collecting their capabilities by name.
//!
//! Loading happens in two steps (ADR-0004 §2). First the manifests of all candidates are read:
//! the addons built into the binary and the configured libraries. Then the addons to load are
//! resolved — the enabled ones, the required ones, and every provider they (or the archive) need,
//! pulled in implicitly — and initialized in dependency order, providers first.
//!
//! Library addons described by a manifest file ([`file`]) take part in resolution without being
//! loaded; only the libraries of the addons actually selected are opened.
//!
//! The index providers are loaded before everything else ([`load_index`]): the settings, which say
//! which other addons are enabled, are stored in the index (ADR-0004 §3).

mod file;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, bail};
use medicus_core::addon::{AddonManifest, Capability, Requirement};
use medicus_core::capability::{BlobStore, EventHook, IndexStore, RemoteNode, Service};
use medicus_host::{AddonLoader, LoadedAddon, PreparedAddon, StaticAddon};
use serde::Serialize;
use serde_json::json;
use sha2::{Digest as _, Sha256};
use tokio::runtime::Handle;

use crate::config::Config;

/// Addons compiled into this binary (`builtin-*` features).
fn builtins() -> Vec<&'static StaticAddon> {
    vec![
        #[cfg(feature = "builtin-admin-ui")]
        &admin_ui::ADDON,
        #[cfg(feature = "builtin-storage-fs")]
        &storage_fs::ADDON,
        #[cfg(feature = "builtin-index-sqlite")]
        &index_sqlite::ADDON,
        #[cfg(feature = "builtin-index-postgres")]
        &index_postgres::ADDON,
        #[cfg(feature = "builtin-dimse")]
        &dimse::ADDON,
        #[cfg(feature = "builtin-dicomweb")]
        &dicomweb::ADDON,
        #[cfg(feature = "builtin-events-webhook")]
        &events_webhook::ADDON,
    ]
}

/// What the archive itself needs from addons.
const ARCHIVE_REQUIRES: [Capability; 2] = [Capability::IndexStore, Capability::BlobStore];

/// The built-in addon that provides a capability when no enabled addon does.
const DEFAULT_PROVIDERS: [(Capability, &str); 2] = [
    (Capability::IndexStore, "index-sqlite"),
    (Capability::BlobStore, "storage-fs"),
];

/// Why an addon was loaded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "by", rename_all = "snake_case")]
pub(crate) enum LoadedBy {
    /// Enabled in the configuration.
    Config,
    /// The addon is required and always loaded.
    Required,
    /// Pulled in as the provider of a capability another addon requires.
    Dependency { of: String, capability: Capability },
    /// Pulled in as the provider of a capability the archive requires.
    Archive { capability: Capability },
}

/// A loaded addon: its manifest and why it was loaded.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct AddonStatus {
    #[serde(flatten)]
    pub(crate) manifest: AddonManifest,
    pub(crate) loaded_by: LoadedBy,
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
    /// Protocol clients for peers, keyed by protocol.
    pub(crate) remote_nodes: BTreeMap<String, Arc<dyn RemoteNode>>,
    /// HTTP routes, keyed by path prefix.
    pub(crate) http_routes: crate::routes::Routes,
    /// The loaded addons, in the order they were initialized.
    pub(crate) addons: Vec<AddonStatus>,
    /// The manifests of every addon that could be loaded: built in, described by a manifest file,
    /// or configured as a library.
    pub(crate) available: BTreeMap<String, AddonManifest>,
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
            .field(
                "remote_nodes",
                &self.remote_nodes.keys().collect::<Vec<_>>(),
            )
            .field("http_routes", &self.http_routes)
            .field(
                "addons",
                &self
                    .addons
                    .iter()
                    .map(|a| a.manifest.name.as_str())
                    .collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

impl Capabilities {
    fn add(&mut self, addon: LoadedAddon, loaded_by: LoadedBy) -> anyhow::Result<()> {
        let name = addon.manifest.name.clone();
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
        for c in addon.http_routes {
            crate::routes::check_prefix(&c.name).with_context(|| format!("addon {name}"))?;
            if self.http_routes.0.insert(c.name.clone(), c.value).is_some() {
                bail!(
                    "HTTP routes under {:?} are registered twice (second time by {name})",
                    c.name
                );
            }
        }
        for c in addon.remote_nodes {
            if self.remote_nodes.insert(c.name.clone(), c.value).is_some() {
                bail!(
                    "protocol {:?} has two remote node clients (second from {name})",
                    c.name
                );
            }
        }
        self.addons.push(AddonStatus {
            manifest: addon.manifest,
            loaded_by,
        });
        Ok(())
    }
}

/// Resolves which addons to load and in which order.
///
/// - `available`: the manifests of all candidates, by name;
/// - `configured`: the `enabled` flag of each addon the configuration mentions;
/// - `defaults`: the addon that provides a capability when no selected addon does;
/// - `archive`: the capabilities the archive itself requires.
///
/// Returns the addons in initialization order: every addon after the providers it requires.
fn resolve(
    available: &BTreeMap<String, AddonManifest>,
    configured: &BTreeMap<String, bool>,
    defaults: &[(Capability, &str)],
    archive: &[Capability],
) -> anyhow::Result<Vec<(String, LoadedBy)>> {
    let mut selected: BTreeMap<String, LoadedBy> = BTreeMap::new();
    for (name, manifest) in available {
        if manifest.required {
            if configured.get(name) == Some(&false) {
                bail!("addon {name:?} is required and cannot be disabled");
            }
            selected.insert(name.clone(), LoadedBy::Required);
        }
    }
    for (name, _) in configured.iter().filter(|(_, enabled)| **enabled) {
        if !available.contains_key(name) {
            bail!("addon {name:?} is not built into this server; set `library` to load it");
        }
        selected.entry(name.clone()).or_insert(LoadedBy::Config);
    }

    // Satisfy requirements until nothing new is pulled in. `None` stands for the archive.
    let mut pending: Vec<(Option<String>, Requirement)> = archive
        .iter()
        .map(|c| (None, Requirement::from(*c)))
        .collect();
    for name in selected.keys() {
        pending.extend(requirements_of(&available[name], name));
    }
    while let Some((requirer, requirement)) = pending.pop() {
        let who = requirer.as_deref().unwrap_or("the archive");
        let capability = requirement.capability;
        let provider = if let Some(addon) = &requirement.addon {
            let Some(manifest) = available.get(addon) else {
                bail!("{who} requires {capability} from addon {addon:?}, which is not available");
            };
            if !manifest.provides_capability(capability) {
                bail!(
                    "{who} requires {capability} from addon {addon:?}, which does not provide it"
                );
            }
            addon.clone()
        } else {
            if selected
                .keys()
                .any(|n| available[n].provides_capability(capability))
            {
                continue;
            }
            let Some((_, default)) = defaults.iter().find(|(c, _)| *c == capability) else {
                bail!("{who} requires {capability}, but no enabled addon provides it");
            };
            if !available.contains_key(*default) {
                bail!(
                    "{who} requires {capability}, but no enabled addon provides it and the \
                     default provider {default:?} is not built into this server"
                );
            }
            (*default).to_owned()
        };
        if selected.contains_key(&provider) {
            continue;
        }
        if configured.get(&provider) == Some(&false) {
            bail!("{who} requires {capability} from addon {provider:?}, which is disabled");
        }
        let loaded_by = match requirer {
            Some(of) => LoadedBy::Dependency { of, capability },
            None => LoadedBy::Archive { capability },
        };
        pending.extend(requirements_of(&available[&provider], &provider));
        selected.insert(provider, loaded_by);
    }

    let order = initialization_order(available, &selected)?;
    Ok(order
        .into_iter()
        .map(|name| {
            let by = selected.remove(&name).unwrap_or(LoadedBy::Config);
            (name, by)
        })
        .collect())
}

fn requirements_of(
    manifest: &AddonManifest,
    name: &str,
) -> impl Iterator<Item = (Option<String>, Requirement)> {
    manifest
        .requires
        .clone()
        .into_iter()
        .map(move |r| (Some(name.to_owned()), r))
}

/// The selected addons sorted so that each comes after the selected addons that provide what it
/// requires (alphabetical among equals, for a stable order).
fn initialization_order(
    available: &BTreeMap<String, AddonManifest>,
    selected: &BTreeMap<String, LoadedBy>,
) -> anyhow::Result<Vec<String>> {
    let providers = |name: &str| -> BTreeSet<&str> {
        available[name]
            .requires
            .iter()
            .flat_map(|r| {
                selected.keys().map(String::as_str).filter(move |p| {
                    *p != name
                        && available[*p].provides_capability(r.capability)
                        && r.addon.as_deref().is_none_or(|a| a == *p)
                })
            })
            .collect()
    };
    let dependencies: BTreeMap<&str, BTreeSet<&str>> = selected
        .keys()
        .map(|n| (n.as_str(), providers(n)))
        .collect();
    let mut order: Vec<String> = Vec::new();
    let mut done: BTreeSet<&str> = BTreeSet::new();
    while done.len() < dependencies.len() {
        let ready: Vec<&str> = dependencies
            .iter()
            .filter(|(n, deps)| !done.contains(*n) && deps.iter().all(|d| done.contains(d)))
            .map(|(n, _)| *n)
            .collect();
        if ready.is_empty() {
            let cycle = find_cycle(&dependencies, &done);
            bail!("addon dependencies form a cycle: {}", cycle.join(" → "));
        }
        for name in ready {
            done.insert(name);
            order.push(name.to_owned());
        }
    }
    Ok(order)
}

/// A dependency cycle among the addons not yet ordered, as `a → b → a`.
fn find_cycle<'a>(
    dependencies: &BTreeMap<&'a str, BTreeSet<&'a str>>,
    done: &BTreeSet<&'a str>,
) -> Vec<&'a str> {
    let start = dependencies
        .keys()
        .find(|n| !done.contains(*n))
        .copied()
        .unwrap_or_default();
    // Every remaining addon has a remaining dependency, so walking them must revisit one.
    let mut path = vec![start];
    loop {
        let current = path[path.len() - 1];
        let Some(next) = dependencies[current]
            .iter()
            .find(|d| !done.contains(*d))
            .copied()
        else {
            return path;
        };
        if let Some(i) = path.iter().position(|n| *n == next) {
            let mut cycle = path.split_off(i);
            cycle.push(next);
            return cycle;
        }
        path.push(next);
    }
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

/// The configuration of a default provider pulled in without being configured: the local
/// filesystem store and the `SQLite` index below `data_dir`, under the archive's store and index
/// names (zero-configuration start).
fn default_config(cfg: &Config, name: &str) -> serde_json::Value {
    let data = &cfg.server.data_dir;
    match name {
        "storage-fs" => json!({ "root": data.join("blobs"), "name": cfg.archive.storage }),
        "index-sqlite" => json!({ "path": data.join("index.sqlite"), "name": cfg.archive.index }),
        _ => serde_json::Value::Null,
    }
}

/// Where a candidate addon comes from.
enum Source {
    /// Built into the binary; its manifest has been read.
    Builtin(PreparedAddon),
    /// A library described by a manifest file, not loaded yet.
    Described {
        library: PathBuf,
        sha256: Option<String>,
        file: PathBuf,
    },
    /// A configured library without a manifest file, loaded to read its manifest.
    Library(PreparedAddon),
}

/// A candidate addon: its manifest and where it comes from.
struct Candidate {
    manifest: AddonManifest,
    source: Source,
}

/// The candidate addons: those built in, those described by manifest files in the search paths,
/// and the enabled configured libraries. A configured library replaces a built-in addon of the
/// same name; a manifest file does not (set `library` in the configuration to replace one).
fn candidates(loader: &AddonLoader, cfg: &Config) -> anyhow::Result<BTreeMap<String, Candidate>> {
    let mut out = BTreeMap::new();
    for entry in builtins() {
        let prepared = loader.prepare_static(entry)?;
        let manifest = prepared.manifest.clone();
        out.insert(
            manifest.name.clone(),
            Candidate {
                manifest,
                source: Source::Builtin(prepared),
            },
        );
    }
    let target = format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS);
    for (name, found) in file::discover(&cfg.addons.search_paths, &target)? {
        if out.contains_key(&name) {
            tracing::warn!(
                addon = %name,
                file = %found.file.display(),
                "manifest file ignored: the addon is built in (set `library` to load the file's library instead)"
            );
            continue;
        }
        out.insert(
            name,
            Candidate {
                manifest: found.manifest,
                source: Source::Described {
                    library: found.library,
                    sha256: found.sha256,
                    file: found.file,
                },
            },
        );
    }
    for (name, entry) in cfg.addons.entries.iter().filter(|(_, e)| e.enabled) {
        let Some(lib) = &entry.library else {
            if entry.sha256.is_some() {
                bail!("addon {name:?}: `sha256` only applies to libraries");
            }
            continue;
        };
        let path = resolve_library(lib, &cfg.addons.search_paths)?;
        if let Some(expected) = &entry.sha256 {
            verify_sha256(&path, expected)?;
        }
        let prepared = loader
            .prepare_library(&path)
            .with_context(|| format!("loading addon {name:?}"))?;
        if prepared.manifest.name != *name {
            bail!(
                "addon configured as {name:?} calls itself {:?} in its manifest",
                prepared.manifest.name
            );
        }
        let manifest = prepared.manifest.clone();
        out.insert(
            name.clone(),
            Candidate {
                manifest,
                source: Source::Library(prepared),
            },
        );
    }
    Ok(out)
}

/// Loads a selected candidate's library if that has not happened yet, checking it against its
/// manifest file.
fn prepare(loader: &AddonLoader, candidate: Candidate) -> anyhow::Result<PreparedAddon> {
    let (library, sha256, file) = match candidate.source {
        Source::Builtin(prepared) | Source::Library(prepared) => return Ok(prepared),
        Source::Described {
            library,
            sha256,
            file,
        } => (library, sha256, file),
    };
    if let Some(expected) = &sha256 {
        verify_sha256(&library, expected)?;
    }
    let prepared = loader.prepare_library(&library)?;
    if prepared.manifest != candidate.manifest {
        bail!(
            "{} does not match the manifest of {}: the file says {}, the library {}",
            file.display(),
            library.display(),
            serde_json::to_string(&candidate.manifest)?,
            serde_json::to_string(&prepared.manifest)?
        );
    }
    Ok(prepared)
}

/// The `enabled` flag of each addon the configuration mentions.
fn configured(cfg: &Config) -> BTreeMap<String, bool> {
    cfg.addons
        .entries
        .iter()
        .map(|(n, e)| (n.clone(), e.enabled))
        .collect()
}

/// Addons being loaded: the index providers are, the others are still candidates.
pub(crate) struct Loading {
    loader: AddonLoader,
    candidates: BTreeMap<String, Candidate>,
    caps: Capabilities,
}

/// Resolves and loads the addons that provide index stores (and nothing else), so that the
/// settings can be read before the other addons are resolved with [`Loading::finish`].
pub(crate) fn load_index(cfg: &Config, rt: &Handle) -> anyhow::Result<Loading> {
    let loader = AddonLoader::new(rt.clone());
    let candidates = candidates(&loader, cfg)?;
    let providers: BTreeMap<String, AddonManifest> = candidates
        .iter()
        .filter(|(_, c)| c.manifest.provides_capability(Capability::IndexStore))
        .map(|(n, c)| (n.clone(), c.manifest.clone()))
        .collect();
    let mut configured = configured(cfg);
    configured.retain(|name, _| providers.contains_key(name));
    let order = resolve(
        &providers,
        &configured,
        &DEFAULT_PROVIDERS,
        &[Capability::IndexStore],
    )?;
    let mut loading = Loading {
        loader,
        candidates,
        caps: Capabilities::default(),
    };
    loading.init(cfg, order)?;
    Ok(loading)
}

impl Loading {
    /// The index store registered as `name`.
    pub(crate) fn index_store(&self, name: &str) -> anyhow::Result<Arc<dyn IndexStore>> {
        self.caps.index_stores.get(name).cloned().with_context(|| {
            format!(
                "no index store is registered as {name:?} (`archive.index`); registered: {:?}",
                self.caps.index_stores.keys().collect::<Vec<_>>()
            )
        })
    }

    /// The names of the addons loaded so far: the index providers. Their configuration is part of
    /// the bootstrap, not of the settings.
    pub(crate) fn loaded(&self) -> BTreeSet<String> {
        self.caps
            .addons
            .iter()
            .map(|a| a.manifest.name.clone())
            .collect()
    }

    /// Resolves the addons again, now with the settings applied to `cfg`, and loads the ones not
    /// loaded yet.
    pub(crate) fn finish(mut self, cfg: &Config) -> anyhow::Result<Capabilities> {
        let loaded = self.loaded();
        let available: BTreeMap<String, AddonManifest> = self
            .caps
            .addons
            .iter()
            .map(|a| &a.manifest)
            .chain(self.candidates.values().map(|c| &c.manifest))
            .map(|m| (m.name.clone(), m.clone()))
            .collect();
        let order = resolve(
            &available,
            &configured(cfg),
            &DEFAULT_PROVIDERS,
            &ARCHIVE_REQUIRES,
        )?
        .into_iter()
        .filter(|(name, _)| !loaded.contains(name))
        .collect();
        self.caps.available = available;
        self.init(cfg, order)?;
        tracing::info!(caps = ?self.caps, "addons loaded");
        self.caps.loader = Some(self.loader);
        Ok(self.caps)
    }

    /// Initializes the addons in `order`.
    fn init(&mut self, cfg: &Config, order: Vec<(String, LoadedBy)>) -> anyhow::Result<()> {
        for (name, loaded_by) in order {
            let config = match cfg.addons.entries.get(&name) {
                Some(entry) => entry.config.clone(),
                None => default_config(cfg, &name),
            };
            tracing::debug!(addon = %name, ?loaded_by, "initializing addon");
            let candidate = self
                .candidates
                .remove(&name)
                .with_context(|| format!("addon {name:?} disappeared during loading"))?;
            let addon = prepare(&self.loader, candidate)
                .and_then(|prepared| Ok(self.loader.init(&prepared, &config)?))
                .with_context(|| format!("loading addon {name:?}"))?;
            self.caps.add(addon, loaded_by)?;
        }
        Ok(())
    }
}

/// Checks that the addons `cfg` enables can be resolved among `available` (for a settings change,
/// before it is stored): required addons stay enabled, providers are not disabled while needed,
/// and every addon is available. Returns the addons that would be loaded.
pub(crate) fn check_selection(
    available: &BTreeMap<String, AddonManifest>,
    cfg: &Config,
) -> anyhow::Result<Vec<(String, LoadedBy)>> {
    resolve(
        available,
        &configured(cfg),
        &DEFAULT_PROVIDERS,
        &ARCHIVE_REQUIRES,
    )
}

/// For each addon of `selection`, the selected addons (and `archive`) with a requirement only it
/// satisfies: it cannot be disabled while they are enabled.
pub(crate) fn dependants(
    available: &BTreeMap<String, AddonManifest>,
    selection: &[(String, LoadedBy)],
) -> BTreeMap<String, Vec<String>> {
    let selected: Vec<&AddonManifest> = selection
        .iter()
        .filter_map(|(name, _)| available.get(name))
        .collect();
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let archive = ARCHIVE_REQUIRES.map(Requirement::from);
    let needs = std::iter::once(("archive", archive.as_slice())).chain(
        selected
            .iter()
            .map(|m| (m.name.as_str(), m.requires.as_slice())),
    );
    for (who, requirements) in needs {
        for requirement in requirements {
            let providers: Vec<&str> = match &requirement.addon {
                Some(addon) => vec![addon.as_str()],
                None => selected
                    .iter()
                    .filter(|m| m.provides_capability(requirement.capability))
                    .map(|m| m.name.as_str())
                    .collect(),
            };
            if let [only] = providers.as_slice() {
                let list = out.entry((*only).to_owned()).or_default();
                if !list.iter().any(|w| w == who) {
                    list.push(who.to_owned());
                }
            }
        }
    }
    out
}

/// Checks an addon's configuration against the JSON Schema in its manifest, if it has one.
/// `null` stands for "not configured" and is not checked: the addon then uses its defaults or a
/// configuration the core derives.
pub(crate) fn check_config(
    manifest: &AddonManifest,
    config: &serde_json::Value,
) -> anyhow::Result<()> {
    let (Some(schema), false) = (&manifest.config_schema, config.is_null()) else {
        return Ok(());
    };
    let validator = jsonschema::validator_for(schema).map_err(|e| {
        anyhow::anyhow!(
            "addon {:?} has an invalid configuration schema: {e}",
            manifest.name
        )
    })?;
    let errors: Vec<String> = validator
        .iter_errors(config)
        .map(|e| {
            let at = e.instance_path().to_string();
            if at.is_empty() {
                e.to_string()
            } else {
                format!("{at}: {e}")
            }
        })
        .collect();
    if errors.is_empty() {
        Ok(())
    } else {
        bail!(
            "invalid configuration of addon {:?}: {}",
            manifest.name,
            errors.join("; ")
        )
    }
}

/// Resolves and loads the addons configured in `cfg`, without stored settings.
#[cfg(test)]
pub(crate) fn load(cfg: &Config, rt: &Handle) -> anyhow::Result<Capabilities> {
    load_index(cfg, rt)?.finish(cfg)
}

/// The manifest file for the addon library at `library`, to be written as
/// `<name>.addon.toml` into `dir`: it names the library relative to `dir` when the library is in
/// it, else by its absolute path, and pins its SHA-256. Loading the library runs its static
/// initializers (but not the addon's initialization): only use it on trusted libraries.
pub fn manifest_file(library: &Path, dir: &Path) -> anyhow::Result<(String, String)> {
    let loader = AddonLoader::new(Handle::current());
    let prepared = loader
        .prepare_library(library)
        .with_context(|| format!("reading the manifest of {}", library.display()))?;
    let bytes = std::fs::read(library).with_context(|| format!("reading {}", library.display()))?;
    let sha256 = hex::encode(Sha256::digest(&bytes));
    let absolute = std::path::absolute(library)?;
    let named = match (absolute.parent(), std::path::absolute(dir)) {
        (Some(parent), Ok(dir)) if parent == dir => absolute
            .file_name()
            .map_or_else(|| absolute.clone(), PathBuf::from),
        _ => absolute.clone(),
    };
    let name = prepared.manifest.name.clone();
    let file = file::ManifestFile::new(prepared.manifest, named, Some(sha256));
    Ok((format!("{name}{}", file::SUFFIX), file.to_toml()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(name: &str) -> AddonManifest {
        AddonManifest::new(name, "1")
    }

    fn available(manifests: Vec<AddonManifest>) -> BTreeMap<String, AddonManifest> {
        manifests.into_iter().map(|m| (m.name.clone(), m)).collect()
    }

    fn standard() -> BTreeMap<String, AddonManifest> {
        let archive = [Capability::IndexStore, Capability::BlobStore];
        available(vec![
            manifest("index-sqlite").provides([Capability::IndexStore]),
            manifest("storage-fs").provides([Capability::BlobStore]),
            manifest("dimse")
                .provides([Capability::Service])
                .requires(archive),
            manifest("ui").requires([Capability::IndexStore]).required(),
        ])
    }

    fn names(order: &[(String, LoadedBy)]) -> Vec<&str> {
        order.iter().map(|(n, _)| n.as_str()).collect()
    }

    fn configured(entries: &[(&str, bool)]) -> BTreeMap<String, bool> {
        entries.iter().map(|(n, e)| ((*n).to_owned(), *e)).collect()
    }

    #[test]
    fn required_addons_pull_in_their_providers() {
        let order = resolve(&standard(), &BTreeMap::new(), &DEFAULT_PROVIDERS, &[]).unwrap();
        assert_eq!(names(&order), ["index-sqlite", "ui"]);
        assert_eq!(order[1].1, LoadedBy::Required);
        assert_eq!(
            order[0].1,
            LoadedBy::Dependency {
                of: "ui".into(),
                capability: Capability::IndexStore
            }
        );
    }

    #[test]
    fn providers_come_before_the_addons_that_need_them() {
        let order = resolve(
            &standard(),
            &configured(&[("dimse", true)]),
            &DEFAULT_PROVIDERS,
            &ARCHIVE_REQUIRES,
        )
        .unwrap();
        assert_eq!(names(&order), ["index-sqlite", "storage-fs", "dimse", "ui"]);
        // The archive's own requirement was met first by dimse's providers or by itself.
        assert!(
            order[..2].iter().all(|(_, by)| matches!(
                by,
                LoadedBy::Dependency { .. } | LoadedBy::Archive { .. }
            ))
        );
    }

    #[test]
    fn an_enabled_provider_replaces_the_default() {
        let mut all = standard();
        all.insert(
            "storage-s3".into(),
            manifest("storage-s3").provides([Capability::BlobStore]),
        );
        let order = resolve(
            &all,
            &configured(&[("storage-s3", true), ("dimse", true)]),
            &DEFAULT_PROVIDERS,
            &ARCHIVE_REQUIRES,
        )
        .unwrap();
        assert!(names(&order).contains(&"storage-s3"));
        assert!(!names(&order).contains(&"storage-fs"));
    }

    #[test]
    fn configuration_errors_name_the_problem() {
        let err = |configured_: &[(&str, bool)], defaults: &[(Capability, &str)]| {
            resolve(
                &standard(),
                &configured(configured_),
                defaults,
                &ARCHIVE_REQUIRES,
            )
            .unwrap_err()
            .to_string()
        };
        assert!(err(&[("ui", false)], &DEFAULT_PROVIDERS).contains("\"ui\" is required"));
        let disabled = err(
            &[("dimse", true), ("storage-fs", false)],
            &DEFAULT_PROVIDERS,
        );
        assert!(
            disabled.contains("blob_store") && disabled.contains("disabled"),
            "{disabled}"
        );
        let missing = err(&[("dimse", true)], &[]);
        assert!(
            missing.contains("no enabled addon provides it"),
            "{missing}"
        );
        assert!(err(&[("nope", true)], &DEFAULT_PROVIDERS).contains("not built into"));
    }

    #[test]
    fn named_requirements() {
        let mut all = standard();
        all.insert(
            "viewer".into(),
            manifest("viewer").requires([Requirement {
                capability: Capability::Service,
                addon: Some("dimse".into()),
            }]),
        );
        let order = resolve(
            &all,
            &configured(&[("viewer", true)]),
            &DEFAULT_PROVIDERS,
            &[],
        )
        .unwrap();
        let order = names(&order);
        let pos = |n| order.iter().position(|x| *x == n).unwrap();
        assert!(pos("dimse") < pos("viewer") && pos("storage-fs") < pos("dimse"));
    }

    #[test]
    fn cycles_are_reported() {
        let all = available(vec![
            manifest("a")
                .provides([Capability::Service])
                .requires([Capability::EventHook]),
            manifest("b")
                .provides([Capability::EventHook])
                .requires([Capability::Service]),
        ]);
        let err = resolve(&all, &configured(&[("a", true), ("b", true)]), &[], &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("cycle") && err.contains("a → b → a"), "{err}");
    }

    #[test]
    fn zero_config_loads_the_local_defaults() {
        let order = resolve(
            &standard(),
            &BTreeMap::new(),
            &DEFAULT_PROVIDERS,
            &ARCHIVE_REQUIRES,
        )
        .unwrap();
        assert_eq!(names(&order), ["index-sqlite", "storage-fs", "ui"]);
        let cfg = Config::default();
        assert_eq!(default_config(&cfg, "storage-fs")["name"], "local");
        assert_eq!(default_config(&cfg, "index-sqlite")["name"], "sqlite");
    }

    /// The C example addon, described by a generated manifest file in a search path: discovered
    /// without being loaded, loaded once enabled, and refused when the file and the library
    /// disagree.
    #[tokio::test(flavor = "multi_thread")]
    async fn manifest_files_are_discovered_and_checked() {
        let dir = tempfile::tempdir().unwrap();
        let addons = dir.path().join("addons");
        std::fs::create_dir(&addons).unwrap();
        let (name, text) = manifest_file(Path::new(example_c::LIBRARY_PATH), &addons).unwrap();
        assert_eq!(name, "example-c.addon.toml");
        assert!(
            text.contains("blob_store") && text.contains("sha256 = "),
            "{text}"
        );
        let file = addons.join(&name);
        std::fs::write(&file, &text).unwrap();

        let mut cfg = Config::default();
        cfg.server.data_dir = dir.path().join("data");
        cfg.addons.search_paths = vec![addons.clone()];
        let loaded = |cfg: &Config| load(cfg, &Handle::current());
        let names = |caps: &Capabilities| {
            caps.addons
                .iter()
                .map(|a| a.manifest.name.clone())
                .collect::<Vec<_>>()
        };

        // Discovered but not enabled: not loaded.
        let caps = loaded(&cfg).unwrap();
        assert!(!names(&caps).contains(&"example-c".to_owned()));

        // Enabled by name only: loaded from the library the file names.
        cfg.addons.entries.insert(
            "example-c".into(),
            crate::config::AddonEntry {
                enabled: true,
                library: None,
                sha256: None,
                config: serde_json::Value::Null,
            },
        );
        let caps = loaded(&cfg).unwrap();
        assert!(names(&caps).contains(&"example-c".to_owned()));
        assert!(caps.blob_stores.contains_key("memory"));

        // A file that disagrees with the library's own manifest is refused ...
        std::fs::write(
            &file,
            text.replace("version = \"0.1.0\"", "version = \"9.9.9\""),
        )
        .unwrap();
        let err = format!("{:#}", loaded(&cfg).unwrap_err());
        assert!(err.contains("does not match"), "{err}");
        // ... and so is a library that does not match the file's checksum.
        let sha = text
            .lines()
            .find(|l| l.starts_with("sha256"))
            .unwrap()
            .to_owned();
        std::fs::write(
            &file,
            text.replace(&sha, &format!("sha256 = \"{}\"", "0".repeat(64))),
        )
        .unwrap();
        let err = format!("{:#}", loaded(&cfg).unwrap_err());
        assert!(err.contains("SHA-256"), "{err}");
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
