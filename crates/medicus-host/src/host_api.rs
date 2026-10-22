//! The `MdHostApi` functions provided to every addon.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use medicus_abi::bridge::{CompletionGuard, import_stream};
use medicus_abi::ffi::{
    MD_ABI_VERSION, MD_CAP_BLOB_STORE, MD_CAP_EVENT_HOOK, MD_CAP_INDEX_STORE, MD_CAP_SERVICE,
    MD_INVALID_ARGUMENT, MD_LOG_DEBUG, MD_LOG_ERROR, MD_LOG_INFO, MD_LOG_WARN, MD_OK,
    MD_UNAVAILABLE, MD_UNSUPPORTED, MdBuffer, MdCancel, MdCompletion, MdHostApi, MdReadStream,
    MdStatus, MdStr,
};
use medicus_core::capability::HostServices;
use medicus_core::event::Origin;
use medicus_core::{Error, ErrorKind};
use tokio::runtime::Handle;

/// The host services, bound once the archive is ready (addons are loaded before it exists).
pub(crate) type ServicesSlot = Arc<OnceLock<Arc<dyn HostServices>>>;

/// A capability as registered by the addon, before it is adapted.
#[derive(Debug)]
pub(crate) struct RawRegistration {
    pub kind: u32,
    pub name: String,
    pub vtable: *const c_void,
    pub instance: *mut c_void,
}

// SAFETY: the ABI requires capability instances and vtables to be usable from any thread.
unsafe impl Send for RawRegistration {}

/// Per-addon state behind `MdHostApi::ctx`. Leaked: addons may call the host API (e.g. log)
/// from their own threads at any time until the process exits.
pub(crate) struct HostCtx {
    pub addon: String,
    pub config: Vec<u8>,
    pub registrations: Mutex<Vec<RawRegistration>>,
    /// Set after `medicus_addon_init` returned; later registrations are refused.
    pub sealed: AtomicBool,
    pub services: ServicesSlot,
    /// Runtime on which host service calls run.
    pub rt: Handle,
}

/// Creates the host API for one addon. Both the context and the table live for the rest of the
/// process.
pub(crate) fn new_host_api(
    addon: &str,
    config: &serde_json::Value,
    services: ServicesSlot,
    rt: Handle,
) -> (&'static HostCtx, &'static MdHostApi) {
    let ctx: &'static HostCtx = Box::leak(Box::new(HostCtx {
        addon: addon.to_owned(),
        config: serde_json::to_vec(config).unwrap_or_else(|_| b"null".to_vec()),
        registrations: Mutex::new(Vec::new()),
        sealed: AtomicBool::new(false),
        services,
        rt,
    }));
    let api = Box::leak(Box::new(MdHostApi {
        size: size_of::<MdHostApi>(),
        abi_version: MD_ABI_VERSION,
        ctx: std::ptr::from_ref(ctx).cast_mut().cast(),
        log: host_log,
        config: host_config,
        register_capability: host_register_capability,
        ingest: host_ingest,
    }));
    (ctx, api)
}

/// # Safety
/// `ctx` must be the `ctx` of an API created by [`new_host_api`].
unsafe fn host_ctx<'a>(ctx: *mut c_void) -> &'a HostCtx {
    // SAFETY: guaranteed by the caller; the context is leaked and never freed.
    unsafe { &*ctx.cast_const().cast::<HostCtx>() }
}

unsafe extern "C" fn host_log(ctx: *mut c_void, level: u32, target: MdStr, message: MdStr) {
    // SAFETY: addons pass back the ctx we gave them; strings are valid for this call.
    let (ctx, target, message) = unsafe { (host_ctx(ctx), target.as_bytes(), message.as_bytes()) };
    let target = String::from_utf8_lossy(target);
    let message = String::from_utf8_lossy(message);
    let addon = ctx.addon.as_str();
    match level {
        MD_LOG_ERROR => {
            tracing::error!(target: "medicus::addon", addon, source = %target, "{message}");
        }
        MD_LOG_WARN => {
            tracing::warn!(target: "medicus::addon", addon, source = %target, "{message}");
        }
        MD_LOG_INFO => {
            tracing::info!(target: "medicus::addon", addon, source = %target, "{message}");
        }
        MD_LOG_DEBUG => {
            tracing::debug!(target: "medicus::addon", addon, source = %target, "{message}");
        }
        _ => tracing::trace!(target: "medicus::addon", addon, source = %target, "{message}"),
    }
}

unsafe extern "C" fn host_config(ctx: *mut c_void) -> MdBuffer {
    // SAFETY: addons pass back the ctx we gave them.
    let ctx = unsafe { host_ctx(ctx) };
    MdBuffer::from_vec(ctx.config.clone())
}

unsafe extern "C" fn host_register_capability(
    ctx: *mut c_void,
    kind: u32,
    name: MdStr,
    vtable: *const c_void,
    instance: *mut c_void,
) -> MdStatus {
    // SAFETY: addons pass back the ctx we gave them; `name` is valid for this call.
    let (ctx, name) = unsafe { (host_ctx(ctx), name.to_str()) };
    if ctx.sealed.load(Ordering::Acquire) {
        tracing::warn!(addon = %ctx.addon, "capability registered after initialization; refused");
        return MD_UNAVAILABLE;
    }
    if !matches!(
        kind,
        MD_CAP_BLOB_STORE | MD_CAP_EVENT_HOOK | MD_CAP_INDEX_STORE | MD_CAP_SERVICE
    ) {
        tracing::warn!(addon = %ctx.addon, kind, "unsupported capability kind; refused");
        return MD_UNSUPPORTED;
    }
    let Ok(name) = name else {
        return MD_INVALID_ARGUMENT;
    };
    if vtable.is_null() || name.is_empty() {
        return MD_INVALID_ARGUMENT;
    }
    let Ok(mut regs) = ctx.registrations.lock() else {
        return MD_UNAVAILABLE;
    };
    regs.push(RawRegistration {
        kind,
        name: name.to_owned(),
        vtable,
        instance,
    });
    MD_OK
}

impl std::fmt::Debug for HostCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostCtx")
            .field("addon", &self.addon)
            .field("services_bound", &self.services.get().is_some())
            .finish_non_exhaustive()
    }
}

unsafe extern "C" fn host_ingest(
    ctx: *mut c_void,
    data: MdReadStream,
    origin: MdStr,
    _cancel: MdCancel,
    done: MdCompletion,
) {
    let guard = CompletionGuard::new(done);
    // SAFETY: ownership of `data` is transferred to us; importing first guarantees release.
    let data = unsafe { import_stream(data) };
    // SAFETY: addons pass back the ctx we gave them; `origin` is valid for this call.
    let (ctx, origin) = unsafe { (host_ctx(ctx), origin.as_bytes()) };
    let origin: Origin = match serde_json::from_slice(origin) {
        Ok(o) => o,
        Err(e) => return guard.err(&Error::invalid_argument(format!("invalid origin: {e}"))),
    };
    let Some(services) = ctx.services.get().cloned() else {
        return guard.err(&Error::new(
            ErrorKind::Unavailable,
            "the archive is not ready yet",
        ));
    };
    ctx.rt.spawn(async move {
        let result = services.ingest(data, origin).await.and_then(|ingested| {
            serde_json::to_vec(&ingested)
                .map(MdBuffer::from_vec)
                .map_err(|e| Error::internal(e.to_string()))
        });
        guard.finish(result);
    });
}
