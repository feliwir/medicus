use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use medicus_abi::bridge::{abi_compatible, abi_version_string, target};
use medicus_abi::ffi::{
    MD_ABI_VERSION, MD_CAP_BLOB_STORE, MD_CAP_EVENT_HOOK, MD_CAP_HTTP_ROUTES, MD_CAP_INDEX_STORE,
    MD_CAP_REMOTE_NODE, MD_CAP_SERVICE, MD_OK, MdAbiVersionFn, MdBlobStoreVtable,
    MdEventHookVtable, MdHttpRoutesVtable, MdIndexStoreVtable, MdInitFn, MdManifestFn,
    MdRemoteNodeVtable, MdServiceVtable,
};
use medicus_abi::{SYMBOL_ABI_VERSION, SYMBOL_INIT, SYMBOL_MANIFEST, StaticAddon};
use medicus_core::addon::{AddonManifest, Capability};
use medicus_core::capability::{
    BlobStore, EventHook, HostServices, HttpRoutes, IndexStore, RemoteNode, Service,
};
use tokio::runtime::Handle;

use crate::adapters::{
    FfiBlobStore, FfiEventHook, FfiHttpRoutes, FfiIndexStore, FfiRemoteNode, FfiService,
};
use crate::host_api::{RawRegistration, ServicesSlot, new_host_api};

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("cannot load addon library {path}: {source}")]
    Library {
        path: PathBuf,
        source: libloading::Error,
    },
    #[error("addon library {path} does not export `{symbol}`")]
    MissingSymbol { path: PathBuf, symbol: String },
    #[error("addon {addon} uses ABI {found}, host implements ABI {expected}")]
    AbiMismatch {
        addon: String,
        found: String,
        expected: String,
    },
    #[error("addon {addon} is built for {found}, host runs on {expected}")]
    TargetMismatch {
        addon: String,
        found: String,
        expected: String,
    },
    /// `reason` is the last error the addon logged while initializing.
    #[error("addon {addon} failed to initialize: {}", reason.as_deref().unwrap_or("see its log output"))]
    InitFailed {
        addon: String,
        reason: Option<String>,
    },
    #[error("addon {addon} has an invalid manifest: {reason}")]
    InvalidManifest { addon: String, reason: String },
    #[error("addon configured as {configured:?} calls itself {found:?} in its manifest")]
    NameMismatch { configured: String, found: String },
}

/// The `MD_CAP_*` kind of a capability.
pub fn capability_kind(capability: Capability) -> u32 {
    match capability {
        Capability::BlobStore => MD_CAP_BLOB_STORE,
        Capability::EventHook => MD_CAP_EVENT_HOOK,
        Capability::IndexStore => MD_CAP_INDEX_STORE,
        Capability::Service => MD_CAP_SERVICE,
        Capability::RemoteNode => MD_CAP_REMOTE_NODE,
        Capability::HttpRoutes => MD_CAP_HTTP_ROUTES,
    }
}

/// The size of this host's vtable struct for a capability kind.
fn vtable_size(kind: u32) -> usize {
    match kind {
        MD_CAP_BLOB_STORE => size_of::<MdBlobStoreVtable>(),
        MD_CAP_EVENT_HOOK => size_of::<MdEventHookVtable>(),
        MD_CAP_INDEX_STORE => size_of::<MdIndexStoreVtable>(),
        MD_CAP_SERVICE => size_of::<MdServiceVtable>(),
        MD_CAP_REMOTE_NODE => size_of::<MdRemoteNodeVtable>(),
        MD_CAP_HTTP_ROUTES => size_of::<MdHttpRoutesVtable>(),
        _ => usize::MAX,
    }
}

/// An addon whose manifest has been read and checked, but which is not initialized yet.
#[derive(Debug, Clone)]
pub struct PreparedAddon {
    pub manifest: AddonManifest,
    entry: StaticAddon,
}

/// A registered capability with the name the addon gave it.
#[derive(Debug, Clone)]
pub struct Named<T> {
    pub name: String,
    pub value: T,
}

/// The capabilities an addon registered.
pub struct LoadedAddon {
    pub manifest: AddonManifest,
    pub blob_stores: Vec<Named<Arc<dyn BlobStore>>>,
    pub event_hooks: Vec<Named<Arc<dyn EventHook>>>,
    pub index_stores: Vec<Named<Arc<dyn IndexStore>>>,
    pub services: Vec<Named<Arc<dyn Service>>>,
    /// Protocol clients, named by protocol.
    pub remote_nodes: Vec<Named<Arc<dyn RemoteNode>>>,
    /// HTTP routes, named by their path prefix.
    pub http_routes: Vec<Named<Arc<dyn HttpRoutes>>>,
}

impl std::fmt::Debug for LoadedAddon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fn names<T>(v: &[Named<T>]) -> Vec<&str> {
            v.iter().map(|n| n.name.as_str()).collect()
        }
        f.debug_struct("LoadedAddon")
            .field("addon", &self.manifest.name)
            .field("blob_stores", &names(&self.blob_stores))
            .field("event_hooks", &names(&self.event_hooks))
            .field("index_stores", &names(&self.index_stores))
            .field("services", &names(&self.services))
            .field("remote_nodes", &names(&self.remote_nodes))
            .field("http_routes", &names(&self.http_routes))
            .finish()
    }
}

/// Loads addons. Streams the host hands to addons are served on `rt`.
#[derive(Clone)]
pub struct AddonLoader {
    rt: Handle,
    services: ServicesSlot,
}

impl std::fmt::Debug for AddonLoader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AddonLoader")
            .field("services_bound", &self.services.get().is_some())
            .finish_non_exhaustive()
    }
}

impl AddonLoader {
    pub fn new(rt: Handle) -> Self {
        Self {
            rt,
            services: ServicesSlot::default(),
        }
    }

    /// Binds the host services (once the archive exists). Until then, addon calls to them fail
    /// with `Unavailable`. Returns `false` if services were already bound.
    pub fn bind_services(&self, services: Arc<dyn HostServices>) -> bool {
        self.services.set(services).is_ok()
    }

    /// Loads a shared library and reads its manifest, without initializing the addon.
    ///
    /// Addons are trusted native code (ADR-0001 §8): loading a library runs its static
    /// initializers with the privileges of the server. Only load libraries from trusted,
    /// configured locations. Libraries are never unloaded.
    pub fn prepare_library(&self, path: &Path) -> Result<PreparedAddon, LoadError> {
        // SAFETY: loading a library runs its initializers; trusted by configuration (see above).
        let lib =
            unsafe { libloading::Library::new(path) }.map_err(|source| LoadError::Library {
                path: path.to_owned(),
                source,
            })?;
        let symbol = |sym: &[u8]| LoadError::MissingSymbol {
            path: path.to_owned(),
            symbol: String::from_utf8_lossy(sym).into_owned(),
        };
        // SAFETY: the symbol types are fixed by the ABI (ADR-0001 §2).
        let entry = unsafe {
            let abi_version = *lib
                .get::<MdAbiVersionFn>(SYMBOL_ABI_VERSION)
                .map_err(|_| symbol(SYMBOL_ABI_VERSION))?;
            let manifest = *lib
                .get::<MdManifestFn>(SYMBOL_MANIFEST)
                .map_err(|_| symbol(SYMBOL_MANIFEST))?;
            let init = *lib
                .get::<MdInitFn>(SYMBOL_INIT)
                .map_err(|_| symbol(SYMBOL_INIT))?;
            StaticAddon {
                abi_version,
                manifest,
                init,
            }
        };
        // Never unload: the function pointers and vtables we keep point into the library.
        std::mem::forget(lib);
        let label = path.display().to_string();
        prepare(&entry, &label)
    }

    /// Reads the manifest of an addon linked into the binary.
    pub fn prepare_static(&self, entry: &StaticAddon) -> Result<PreparedAddon, LoadError> {
        prepare(entry, "built-in addon")
    }

    /// Loads, checks and initializes a library addon configured as `name`.
    pub fn load_library(
        &self,
        path: &Path,
        name: &str,
        config: &serde_json::Value,
    ) -> Result<LoadedAddon, LoadError> {
        let prepared = self.prepare_library(path)?;
        check_name(&prepared, name)?;
        self.init(&prepared, config)
    }

    /// Checks and initializes an addon linked into the binary, configured as `name`.
    pub fn load_static(
        &self,
        entry: &StaticAddon,
        name: &str,
        config: &serde_json::Value,
    ) -> Result<LoadedAddon, LoadError> {
        let prepared = self.prepare_static(entry)?;
        check_name(&prepared, name)?;
        self.init(&prepared, config)
    }

    /// Initializes a prepared addon: it registers its capabilities, which are adapted here. It
    /// still goes through the ABI for addons linked into the binary.
    pub fn init(
        &self,
        addon: &PreparedAddon,
        config: &serde_json::Value,
    ) -> Result<LoadedAddon, LoadError> {
        let name = addon.manifest.name.as_str();
        let provides = addon
            .manifest
            .provides
            .iter()
            .map(|c| capability_kind(*c))
            .collect();
        let (ctx, api) = new_host_api(
            name,
            config,
            provides,
            self.services.clone(),
            self.rt.clone(),
        );
        // SAFETY: `api` is valid for the rest of the process.
        let status = unsafe { (addon.entry.init)(api) };
        ctx.sealed.store(true, Ordering::Release);
        let registrations = std::mem::take(
            &mut *ctx
                .registrations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        if status != MD_OK {
            self.discard(&addon.manifest, registrations);
            let reason = ctx
                .init_error
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            return Err(LoadError::InitFailed {
                addon: name.to_owned(),
                reason,
            });
        }
        let mut loaded = LoadedAddon {
            manifest: addon.manifest.clone(),
            blob_stores: Vec::new(),
            event_hooks: Vec::new(),
            index_stores: Vec::new(),
            services: Vec::new(),
            remote_nodes: Vec::new(),
            http_routes: Vec::new(),
        };
        for reg in registrations {
            self.adapt(&mut loaded, reg);
        }
        tracing::info!(
            addon = %loaded.manifest.name,
            version = %loaded.manifest.version,
            blob_stores = loaded.blob_stores.len(),
            event_hooks = loaded.event_hooks.len(),
            "addon loaded"
        );
        Ok(loaded)
    }

    fn adapt(&self, loaded: &mut LoadedAddon, reg: RawRegistration) {
        let addon = loaded.manifest.name.clone();
        // SAFETY: every vtable starts with its size (ABI rule).
        let size = unsafe { *reg.vtable.cast::<usize>() };
        if size < vtable_size(reg.kind) {
            tracing::error!(%addon, name = %reg.name, kind = reg.kind, "capability vtable too small; ignored");
            return;
        }
        match reg.kind {
            MD_CAP_BLOB_STORE => {
                // SAFETY: the addon registered this pointer for this kind, and its size was
                // checked; vtables are 'static because libraries are never unloaded.
                let vtable = unsafe { &*reg.vtable.cast::<MdBlobStoreVtable>() };
                let store = FfiBlobStore {
                    addon,
                    vtable,
                    instance: reg.instance,
                    rt: self.rt.clone(),
                };
                loaded.blob_stores.push(Named {
                    name: reg.name,
                    value: Arc::new(store),
                });
            }
            MD_CAP_EVENT_HOOK => {
                // SAFETY: the addon registered this pointer for this kind, and its size was
                // checked; vtables are 'static because libraries are never unloaded.
                let vtable = unsafe { &*reg.vtable.cast::<MdEventHookVtable>() };
                let filter = FfiEventHook::query_filter(vtable, reg.instance);
                let hook = FfiEventHook {
                    addon,
                    vtable,
                    instance: reg.instance,
                    filter,
                };
                loaded.event_hooks.push(Named {
                    name: reg.name,
                    value: Arc::new(hook),
                });
            }
            MD_CAP_INDEX_STORE => {
                // SAFETY: the addon registered this pointer for this kind, and its size was
                // checked; vtables are 'static because libraries are never unloaded.
                let vtable = unsafe { &*reg.vtable.cast::<MdIndexStoreVtable>() };
                let index = FfiIndexStore {
                    addon,
                    vtable,
                    instance: reg.instance,
                };
                loaded.index_stores.push(Named {
                    name: reg.name,
                    value: Arc::new(index),
                });
            }
            MD_CAP_SERVICE => {
                // SAFETY: the addon registered this pointer for this kind, and its size was
                // checked; vtables are 'static because libraries are never unloaded.
                let vtable = unsafe { &*reg.vtable.cast::<MdServiceVtable>() };
                let service = FfiService {
                    addon,
                    vtable,
                    instance: reg.instance,
                };
                loaded.services.push(Named {
                    name: reg.name,
                    value: Arc::new(service),
                });
            }
            MD_CAP_REMOTE_NODE => {
                // SAFETY: the addon registered this pointer for this kind, and its size was
                // checked; vtables are 'static because libraries are never unloaded.
                let vtable = unsafe { &*reg.vtable.cast::<MdRemoteNodeVtable>() };
                let node = FfiRemoteNode {
                    addon,
                    vtable,
                    instance: reg.instance,
                };
                loaded.remote_nodes.push(Named {
                    name: reg.name,
                    value: Arc::new(node),
                });
            }
            MD_CAP_HTTP_ROUTES => {
                // SAFETY: the addon registered this pointer for this kind, and its size was
                // checked; vtables are 'static because libraries are never unloaded.
                let vtable = unsafe { &*reg.vtable.cast::<MdHttpRoutesVtable>() };
                let routes = FfiHttpRoutes {
                    addon,
                    vtable,
                    instance: reg.instance,
                };
                loaded.http_routes.push(Named {
                    name: reg.name,
                    value: Arc::new(routes),
                });
            }
            // Unknown kinds were refused at registration time.
            _ => {}
        }
    }

    /// Destroys capabilities registered by an addon whose initialization failed.
    fn discard(&self, manifest: &AddonManifest, registrations: Vec<RawRegistration>) {
        let mut tmp = LoadedAddon {
            manifest: manifest.clone(),
            blob_stores: Vec::new(),
            event_hooks: Vec::new(),
            index_stores: Vec::new(),
            services: Vec::new(),
            remote_nodes: Vec::new(),
            http_routes: Vec::new(),
        };
        for reg in registrations {
            self.adapt(&mut tmp, reg);
        }
        // Dropping the adapters calls each capability's `destroy`.
    }
}

fn check_name(addon: &PreparedAddon, configured: &str) -> Result<(), LoadError> {
    if addon.manifest.name == configured {
        Ok(())
    } else {
        Err(LoadError::NameMismatch {
            configured: configured.to_owned(),
            found: addon.manifest.name.clone(),
        })
    }
}

/// Checks the ABI version and reads the manifest; `label` names the addon in errors until its
/// manifest has been read.
fn prepare(entry: &StaticAddon, label: &str) -> Result<PreparedAddon, LoadError> {
    // SAFETY: entry points of a trusted addon, signature fixed by the ABI.
    let version = unsafe { (entry.abi_version)() };
    if !abi_compatible(version, MD_ABI_VERSION) {
        return Err(LoadError::AbiMismatch {
            addon: label.to_owned(),
            found: abi_version_string(version),
            expected: abi_version_string(MD_ABI_VERSION),
        });
    }
    let invalid = |reason: String| LoadError::InvalidManifest {
        addon: label.to_owned(),
        reason,
    };
    // SAFETY: the manifest is valid UTF-8 JSON for the process lifetime (ABI rule); it is read
    // here as bytes and validated.
    let raw = unsafe { (entry.manifest)() };
    // SAFETY: as above.
    let json = unsafe { raw.to_str() }.map_err(|_| invalid("not valid UTF-8".to_owned()))?;
    let manifest: AddonManifest = serde_json::from_str(json).map_err(|e| invalid(e.to_string()))?;
    if manifest.name.is_empty() {
        return Err(invalid("empty name".to_owned()));
    }
    let host_target = target();
    if manifest.target != host_target {
        return Err(LoadError::TargetMismatch {
            addon: manifest.name,
            found: manifest.target,
            expected: host_target,
        });
    }
    Ok(PreparedAddon {
        manifest,
        entry: *entry,
    })
}
