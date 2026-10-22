//! SDK for writing medicus addons in Rust.
//!
//! An addon implements capability traits from [`medicus_core::capability`] and registers them in
//! its init function; [`export_addon!`] generates the C entry points:
//!
//! ```ignore
//! use medicus_addon_sdk::{Registrar, export_addon};
//!
//! fn init(reg: &mut Registrar) -> Result<(), medicus_addon_sdk::medicus_core::Error> {
//!     let cfg: MyConfig = reg.config()?;
//!     reg.register_blob_store("default", MyStore::new(cfg))?;
//!     Ok(())
//! }
//!
//! export_addon!(name = "storage-example", version = env!("CARGO_PKG_VERSION"), init = init);
//! ```
//!
//! What the SDK takes care of:
//! - the ABI version handshake and the conversion between traits and C vtables,
//! - an addon-owned tokio runtime ([`runtime`]) on which all calls run,
//! - forwarding `tracing` events to the host's log,
//! - turning panics into `MD_PANICKED` errors instead of unwinding into the host.

// The SDK implements the addon side of the FFI boundary.
#![allow(unsafe_code)]

mod exports;
mod host;
mod logging;
mod registrar;
mod runtime;

pub use async_trait::async_trait;
pub use host::Host;
pub use medicus_abi as abi;
pub use medicus_core;
pub use registrar::Registrar;
pub use runtime::{block_on, runtime};
pub use tracing;

#[doc(hidden)]
pub mod __private {
    pub use crate::registrar::init;
}

/// Creates a [`StaticAddon`](medicus_abi::StaticAddon) for linking an addon into a host binary,
/// **without** exporting any symbols. Several addons can therefore be linked into one binary.
///
/// Use this in an addon's library crate; a separate small `cdylib` crate calls
/// [`export_addon!`] with the same `init` function to produce the loadable library.
#[macro_export]
macro_rules! static_addon {
    (name = $name:expr, version = $version:expr, init = $init:path $(,)?) => {{
        // Item names are not hygienic in `macro_rules!`; prefixed so they cannot shadow `$init`.
        #[allow(unsafe_code)]
        unsafe extern "C" fn __medicus_static_abi_version() -> u32 {
            $crate::abi::ffi::MD_ABI_VERSION
        }
        #[allow(unsafe_code)]
        unsafe extern "C" fn __medicus_static_init(
            host: *const $crate::abi::ffi::MdHostApi,
        ) -> *const $crate::abi::ffi::MdAddonInfo {
            // SAFETY: called by the host with a valid, process-lifetime `MdHostApi`.
            unsafe { $crate::__private::init(host, $name, $version, $init) }
        }
        $crate::abi::StaticAddon {
            abi_version: __medicus_static_abi_version,
            init: __medicus_static_init,
        }
    }};
}

/// Exports the addon's C entry points (`medicus_addon_abi_version`, `medicus_addon_init`) and a
/// `MEDICUS_ADDON` static for linking the addon into a host statically.
///
/// `init` is a `fn(&mut Registrar) -> Result<(), E>` with `E: Display`.
#[macro_export]
macro_rules! export_addon {
    (name = $name:expr, version = $version:expr, init = $init:path $(,)?) => {
        /// Entry points of this addon, for linking it into a host statically.
        pub static MEDICUS_ADDON: $crate::abi::StaticAddon = $crate::abi::StaticAddon {
            abi_version: medicus_addon_abi_version,
            init: medicus_addon_init,
        };

        #[allow(unsafe_code)]
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn medicus_addon_abi_version() -> u32 {
            $crate::abi::ffi::MD_ABI_VERSION
        }

        /// # Safety
        /// Called by the host with a valid `MdHostApi` that lives for the rest of the process.
        #[allow(unsafe_code)]
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn medicus_addon_init(
            host: *const $crate::abi::ffi::MdHostApi,
        ) -> *const $crate::abi::ffi::MdAddonInfo {
            // SAFETY: forwarded from the host, see above.
            unsafe { $crate::__private::init(host, $name, $version, $init) }
        }
    };
}
