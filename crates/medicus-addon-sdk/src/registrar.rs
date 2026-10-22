use std::ffi::c_void;
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use medicus_abi::bridge::{OwnedBuffer, target};
use medicus_abi::ffi::{
    MD_ABI_VERSION, MD_CAP_BLOB_STORE, MD_CAP_EVENT_HOOK, MD_CAP_INDEX_STORE, MD_CAP_SERVICE,
    MD_OK, MdAddonInfo, MdHostApi, MdStr,
};
use medicus_core::Error;
use medicus_core::capability::{BlobStore, EventHook, IndexStore, Service};
use serde::de::DeserializeOwned;

use crate::{exports, logging};

/// The host API, valid for the lifetime of the process (ABI rule).
#[derive(Clone, Copy)]
pub(crate) struct HostRef(&'static MdHostApi);

// SAFETY: the ABI requires all host API functions to be thread-safe.
unsafe impl Send for HostRef {}
// SAFETY: as above.
unsafe impl Sync for HostRef {}

impl HostRef {
    pub(crate) fn api(self) -> &'static MdHostApi {
        self.0
    }
}

/// Passed to the addon's init function to read configuration and register capabilities.
pub struct Registrar {
    host: HostRef,
    name: &'static str,
}

impl fmt::Debug for Registrar {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Registrar")
            .field("addon", &self.name)
            .finish_non_exhaustive()
    }
}

impl Registrar {
    pub fn addon_name(&self) -> &'static str {
        self.name
    }

    /// The addon's configuration (`[addons.<name>.config]`), deserialized into `T`. A missing
    /// configuration deserializes from JSON `null`.
    pub fn config<T: DeserializeOwned>(&self) -> Result<T, Error> {
        let api = self.host.api();
        // SAFETY: valid host API; the returned buffer is owned by us.
        let buf = unsafe { OwnedBuffer::from_raw((api.config)(api.ctx)) };
        let bytes: &[u8] = if buf.is_empty() {
            b"null"
        } else {
            buf.as_slice()
        };
        serde_json::from_slice(bytes)
            .map_err(|e| Error::invalid_argument(format!("invalid configuration: {e}")))
    }

    pub fn register_blob_store(&mut self, name: &str, store: impl BlobStore) -> Result<(), Error> {
        let (vtable, instance) = exports::blob_store(Arc::new(store));
        self.register(
            MD_CAP_BLOB_STORE,
            name,
            vtable.cast(),
            instance,
            exports::destroy_blob_store,
        )
    }

    pub fn register_event_hook(&mut self, name: &str, hook: impl EventHook) -> Result<(), Error> {
        let (vtable, instance) = exports::event_hook(Arc::new(hook));
        self.register(
            MD_CAP_EVENT_HOOK,
            name,
            vtable.cast(),
            instance,
            exports::destroy_event_hook,
        )
    }

    /// A handle to the host's services (e.g. ingest), to keep in the addon's capabilities.
    pub fn host(&self) -> crate::Host {
        crate::Host(self.host)
    }

    pub fn register_service(&mut self, name: &str, service: impl Service) -> Result<(), Error> {
        let (vtable, instance) = exports::service(Arc::new(service));
        self.register(
            MD_CAP_SERVICE,
            name,
            vtable.cast(),
            instance,
            exports::destroy_service,
        )
    }

    pub fn register_index_store(
        &mut self,
        name: &str,
        index: impl IndexStore,
    ) -> Result<(), Error> {
        let (vtable, instance) = exports::index_store(Arc::new(index));
        self.register(
            MD_CAP_INDEX_STORE,
            name,
            vtable.cast(),
            instance,
            exports::destroy_index_store,
        )
    }

    fn register(
        &mut self,
        kind: u32,
        name: &str,
        vtable: *const c_void,
        instance: *mut c_void,
        destroy: unsafe extern "C" fn(*mut c_void),
    ) -> Result<(), Error> {
        let api = self.host.api();
        // SAFETY: valid host API; vtable and instance were just created and are 'static resp.
        // owned by the host from now on.
        let status =
            unsafe { (api.register_capability)(api.ctx, kind, MdStr::new(name), vtable, instance) };
        if status == MD_OK {
            Ok(())
        } else {
            // The host did not take ownership; free the instance ourselves.
            // SAFETY: `instance` was created by `exports` for exactly this destroy function.
            unsafe { destroy(instance) };
            Err(Error::new(
                medicus_abi::bridge::kind_of(status),
                format!("host refused to register capability {name:?}"),
            ))
        }
    }
}

/// Shared implementation of the exported `medicus_addon_init`.
///
/// # Safety
/// `host` must be null or point to a valid `MdHostApi` that lives for the rest of the process.
pub unsafe fn init<E: fmt::Display>(
    host: *const MdHostApi,
    name: &'static str,
    version: &'static str,
    init: fn(&mut Registrar) -> Result<(), E>,
) -> *const MdAddonInfo {
    let run = AssertUnwindSafe(|| {
        // SAFETY: guaranteed by the caller.
        let Some(api) = (unsafe { host.as_ref() }) else {
            return std::ptr::null();
        };
        if api.size < size_of::<MdHostApi>() || !compatible_host(api.abi_version) {
            return std::ptr::null();
        }
        let host = HostRef(api);
        logging::install(host);
        let mut registrar = Registrar { host, name };
        match init(&mut registrar) {
            Ok(()) => info(name, version),
            Err(e) => {
                host.log(
                    medicus_abi::ffi::MD_LOG_ERROR,
                    name,
                    &format!("initialization failed: {e}"),
                );
                std::ptr::null()
            }
        }
    });
    catch_unwind(run).unwrap_or(std::ptr::null())
}

/// The host checks compatibility too; this guards against hosts that skip the check.
fn compatible_host(host_version: u32) -> bool {
    medicus_abi::bridge::abi_compatible(MD_ABI_VERSION, host_version)
}

fn info(name: &'static str, version: &'static str) -> *const MdAddonInfo {
    let target: &'static str = Box::leak(target().into_boxed_str());
    let info = Box::leak(Box::new(MdAddonInfo {
        size: size_of::<MdAddonInfo>(),
        name: MdStr::new(name),
        version: MdStr::new(version),
        target: MdStr::new(target),
    }));
    std::ptr::from_ref(info)
}
