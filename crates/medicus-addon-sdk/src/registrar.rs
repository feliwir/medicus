use std::ffi::c_void;
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use std::sync::OnceLock;

use medicus_abi::bridge::OwnedBuffer;
use medicus_abi::ffi::{
    MD_ABI_VERSION, MD_CAP_BLOB_STORE, MD_CAP_EVENT_HOOK, MD_CAP_HTTP_ROUTES, MD_CAP_INDEX_STORE,
    MD_CAP_REMOTE_NODE, MD_CAP_SERVICE, MD_INTERNAL, MD_INVALID_ARGUMENT, MD_OK, MD_UNSUPPORTED,
    MdHostApi, MdStatus, MdStr,
};
use medicus_core::Error;
use medicus_core::addon::AddonManifest;
use medicus_core::capability::{BlobStore, EventHook, HttpRoutes, IndexStore, RemoteNode, Service};
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
    /// configuration deserializes from JSON `null` (so `Option<_>` is `None`), or else from `{}`
    /// (so a struct takes its defaults).
    pub fn config<T: DeserializeOwned>(&self) -> Result<T, Error> {
        let api = self.host.api();
        // SAFETY: valid host API; the returned buffer is owned by us.
        let buf = unsafe { OwnedBuffer::from_raw((api.config)(api.ctx)) };
        let bytes: &[u8] = if buf.is_empty() {
            b"null"
        } else {
            buf.as_slice()
        };
        parse_config(bytes)
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

    /// Registers a protocol client for peers; `protocol` is the peers' `protocol` (e.g. `dimse`).
    pub fn register_remote_node(
        &mut self,
        protocol: &str,
        node: impl RemoteNode,
    ) -> Result<(), Error> {
        let (vtable, instance) = exports::remote_node(Arc::new(node));
        self.register(
            MD_CAP_REMOTE_NODE,
            protocol,
            vtable.cast(),
            instance,
            exports::destroy_remote_node,
        )
    }

    /// Registers HTTP routes under `prefix` (e.g. `/` or `/viewer`), which the core mounts into
    /// its HTTP server.
    pub fn register_http_routes(
        &mut self,
        prefix: &str,
        routes: impl HttpRoutes,
    ) -> Result<(), Error> {
        let (vtable, instance) = exports::http_routes(Arc::new(routes));
        self.register(
            MD_CAP_HTTP_ROUTES,
            prefix,
            vtable.cast(),
            instance,
            exports::destroy_http_routes,
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

/// The manifest of an addon and its JSON, built once on first use and kept for the process
/// lifetime (the host reads the JSON through `medicus_addon_manifest`).
#[derive(Debug)]
pub struct ManifestCell(OnceLock<(AddonManifest, String)>);

impl ManifestCell {
    #[allow(clippy::new_without_default)] // used in statics
    pub const fn new() -> Self {
        Self(OnceLock::new())
    }

    pub fn get(&'static self, build: impl FnOnce() -> AddonManifest) -> &'static AddonManifest {
        &self.get_or_init(build).0
    }

    /// The manifest as JSON (shared implementation of the exported `medicus_addon_manifest`).
    pub fn json(&'static self, build: impl FnOnce() -> AddonManifest) -> MdStr {
        MdStr::new(&self.get_or_init(build).1)
    }

    fn get_or_init(
        &'static self,
        build: impl FnOnce() -> AddonManifest,
    ) -> &'static (AddonManifest, String) {
        self.0.get_or_init(|| {
            let manifest = build();
            // A manifest of plain strings and enums always serializes.
            let json = serde_json::to_string(&manifest).unwrap_or_default();
            (manifest, json)
        })
    }
}

/// Shared implementation of the exported `medicus_addon_init`.
///
/// # Safety
/// `host` must be null or point to a valid `MdHostApi` that lives for the rest of the process.
pub unsafe fn init<E: fmt::Display>(
    host: *const MdHostApi,
    manifest: &'static AddonManifest,
    init: fn(&mut Registrar) -> Result<(), E>,
) -> MdStatus {
    let name = manifest.name.as_str();
    let run = AssertUnwindSafe(|| {
        // SAFETY: guaranteed by the caller.
        let Some(api) = (unsafe { host.as_ref() }) else {
            return MD_INVALID_ARGUMENT;
        };
        if api.size < size_of::<MdHostApi>() || !compatible_host(api.abi_version) {
            return MD_UNSUPPORTED;
        }
        let host = HostRef(api);
        logging::install(host);
        let mut registrar = Registrar { host, name };
        match init(&mut registrar) {
            Ok(()) => MD_OK,
            Err(e) => {
                host.log(
                    medicus_abi::ffi::MD_LOG_ERROR,
                    name,
                    &format!("initialization failed: {e}"),
                );
                MD_INTERNAL
            }
        }
    });
    catch_unwind(run).unwrap_or(medicus_abi::ffi::MD_PANICKED)
}

/// The host checks compatibility too; this guards against hosts that skip the check.
fn compatible_host(host_version: u32) -> bool {
    medicus_abi::bridge::abi_compatible(MD_ABI_VERSION, host_version)
}

/// Deserializes a configuration; `null` (not configured) falls back to `{}` if `T` does not
/// accept it.
fn parse_config<T: DeserializeOwned>(bytes: &[u8]) -> serde_json::Result<T> {
    match serde_json::from_slice(bytes) {
        Err(_) if bytes == b"null" => serde_json::from_slice(b"{}"),
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::parse_config;

    #[derive(Debug, PartialEq, Deserialize)]
    struct Defaults {
        #[serde(default)]
        port: u16,
    }

    #[derive(Debug, Deserialize)]
    struct Required {
        #[allow(dead_code)]
        root: String,
    }

    #[test]
    fn null_config_takes_struct_defaults() {
        assert_eq!(
            parse_config::<Defaults>(b"null").unwrap(),
            Defaults { port: 0 }
        );
    }

    #[test]
    fn null_config_is_none() {
        assert_eq!(parse_config::<Option<Defaults>>(b"null").unwrap(), None);
    }

    #[test]
    fn null_config_reports_missing_fields() {
        let e = parse_config::<Required>(b"null").unwrap_err();
        assert!(e.to_string().contains("missing field `root`"), "{e}");
    }
}
