//! Host side of the addon ABI (ADR-0001).
//!
//! [`AddonLoader`] loads an addon (from a shared library or a statically linked
//! [`StaticAddon`]), performs the version handshake, provides the host API, and returns the
//! registered capabilities as ordinary `Arc<dyn …>` trait objects from
//! [`medicus_core::capability`].

// The host implements the host side of the FFI boundary.
#![allow(unsafe_code)]

mod adapters;
mod host_api;
mod loader;

pub use loader::{AddonLoader, LoadError, LoadedAddon, Named, PreparedAddon, capability_kind};
pub use medicus_abi::StaticAddon;

/// The addon ABI version this host implements, `(major << 16) | minor`.
pub fn abi_version() -> u32 {
    medicus_abi::ffi::MD_ABI_VERSION
}
