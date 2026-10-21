use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use medicus_abi::bridge::{abi_compatible, abi_version_string, target};
use medicus_abi::ffi::{
    MD_ABI_VERSION, MD_CAP_BLOB_STORE, MD_CAP_EVENT_HOOK, MdAbiVersionFn, MdAddonInfo,
    MdBlobStoreVtable, MdEventHookVtable, MdInitFn,
};
use medicus_abi::{SYMBOL_ABI_VERSION, SYMBOL_INIT, StaticAddon};
use medicus_core::capability::{BlobStore, EventHook};
use tokio::runtime::Handle;

use crate::adapters::{FfiBlobStore, FfiEventHook};
use crate::host_api::{RawRegistration, new_host_api};

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
    #[error("addon {0} failed to initialize (see its log output)")]
    InitFailed(String),
    #[error("addon {addon} returned invalid information: {reason}")]
    InvalidInfo { addon: String, reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddonInfo {
    pub name: String,
    pub version: String,
    pub target: String,
}

/// A registered capability with the name the addon gave it.
#[derive(Debug, Clone)]
pub struct Named<T> {
    pub name: String,
    pub value: T,
}

/// The capabilities an addon registered.
pub struct LoadedAddon {
    pub info: AddonInfo,
    pub blob_stores: Vec<Named<Arc<dyn BlobStore>>>,
    pub event_hooks: Vec<Named<Arc<dyn EventHook>>>,
}

impl std::fmt::Debug for LoadedAddon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fn names<T>(v: &[Named<T>]) -> Vec<&str> {
            v.iter().map(|n| n.name.as_str()).collect()
        }
        f.debug_struct("LoadedAddon")
            .field("info", &self.info)
            .field("blob_stores", &names(&self.blob_stores))
            .field("event_hooks", &names(&self.event_hooks))
            .finish()
    }
}

/// Loads addons. Streams the host hands to addons are served on `rt`.
#[derive(Debug, Clone)]
pub struct AddonLoader {
    rt: Handle,
}

impl AddonLoader {
    pub fn new(rt: Handle) -> Self {
        Self { rt }
    }

    /// Loads an addon from a shared library.
    ///
    /// Addons are trusted native code (ADR-0001 §8): loading one runs its initialization code
    /// with the privileges of the server. Only load libraries from trusted, configured locations.
    /// Libraries are never unloaded.
    pub fn load_library(
        &self,
        path: &Path,
        name: &str,
        config: &serde_json::Value,
    ) -> Result<LoadedAddon, LoadError> {
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
            let init = *lib
                .get::<MdInitFn>(SYMBOL_INIT)
                .map_err(|_| symbol(SYMBOL_INIT))?;
            StaticAddon { abi_version, init }
        };
        // Never unload: the function pointers and vtables we keep point into the library.
        std::mem::forget(lib);
        self.load_static(&entry, name, config)
    }

    /// Initializes an addon that is linked into the binary. It still goes through the ABI.
    pub fn load_static(
        &self,
        entry: &StaticAddon,
        name: &str,
        config: &serde_json::Value,
    ) -> Result<LoadedAddon, LoadError> {
        // SAFETY: entry points of a trusted addon, signature fixed by the ABI.
        let version = unsafe { (entry.abi_version)() };
        if !abi_compatible(version, MD_ABI_VERSION) {
            return Err(LoadError::AbiMismatch {
                addon: name.to_owned(),
                found: abi_version_string(version),
                expected: abi_version_string(MD_ABI_VERSION),
            });
        }
        let (ctx, api) = new_host_api(name, config);
        // SAFETY: `api` is valid for the rest of the process.
        let info_ptr = unsafe { (entry.init)(api) };
        ctx.sealed.store(true, Ordering::Release);
        let registrations = std::mem::take(
            &mut *ctx
                .registrations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        // SAFETY: a non-null result points to an MdAddonInfo valid for the process lifetime.
        let Some(raw_info) = (unsafe { info_ptr.as_ref() }) else {
            self.discard(name, registrations);
            return Err(LoadError::InitFailed(name.to_owned()));
        };
        let info = match read_info(name, raw_info) {
            Ok(info) => info,
            Err(e) => {
                self.discard(name, registrations);
                return Err(e);
            }
        };
        let mut loaded = LoadedAddon {
            info,
            blob_stores: Vec::new(),
            event_hooks: Vec::new(),
        };
        for reg in registrations {
            self.adapt(&mut loaded, reg);
        }
        tracing::info!(
            addon = %loaded.info.name,
            version = %loaded.info.version,
            blob_stores = loaded.blob_stores.len(),
            event_hooks = loaded.event_hooks.len(),
            "addon loaded"
        );
        Ok(loaded)
    }

    fn adapt(&self, loaded: &mut LoadedAddon, reg: RawRegistration) {
        let addon = loaded.info.name.clone();
        match reg.kind {
            MD_CAP_BLOB_STORE => {
                // SAFETY: the addon registered this pointer for MD_CAP_BLOB_STORE; vtables are
                // 'static because libraries are never unloaded.
                let vtable = unsafe { &*reg.vtable.cast::<MdBlobStoreVtable>() };
                if vtable.size < size_of::<MdBlobStoreVtable>() {
                    tracing::error!(%addon, name = %reg.name, "blob store vtable too small; ignored");
                    return;
                }
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
                // SAFETY: as above, for MD_CAP_EVENT_HOOK.
                let vtable = unsafe { &*reg.vtable.cast::<MdEventHookVtable>() };
                if vtable.size < size_of::<MdEventHookVtable>() {
                    tracing::error!(%addon, name = %reg.name, "event hook vtable too small; ignored");
                    return;
                }
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
            // Unknown kinds were refused at registration time.
            _ => {}
        }
    }

    /// Destroys capabilities registered by an addon whose initialization failed.
    fn discard(&self, name: &str, registrations: Vec<RawRegistration>) {
        let info = AddonInfo {
            name: name.to_owned(),
            version: String::new(),
            target: String::new(),
        };
        let mut tmp = LoadedAddon {
            info,
            blob_stores: Vec::new(),
            event_hooks: Vec::new(),
        };
        for reg in registrations {
            self.adapt(&mut tmp, reg);
        }
        // Dropping the adapters calls each capability's `destroy`.
    }
}

fn read_info(name: &str, raw: &MdAddonInfo) -> Result<AddonInfo, LoadError> {
    let invalid = |reason: &str| LoadError::InvalidInfo {
        addon: name.to_owned(),
        reason: reason.to_owned(),
    };
    if raw.size < size_of::<MdAddonInfo>() {
        return Err(invalid("info struct too small"));
    }
    // SAFETY: strings in MdAddonInfo are valid for the process lifetime (ABI rule).
    let (n, v, t) = unsafe { (raw.name.to_str(), raw.version.to_str(), raw.target.to_str()) };
    let (Ok(n), Ok(v), Ok(t)) = (n, v, t) else {
        return Err(invalid("strings are not valid UTF-8"));
    };
    let host_target = target();
    if t != host_target {
        return Err(LoadError::TargetMismatch {
            addon: name.to_owned(),
            found: t.to_owned(),
            expected: host_target,
        });
    }
    Ok(AddonInfo {
        name: n.to_owned(),
        version: v.to_owned(),
        target: t.to_owned(),
    })
}
