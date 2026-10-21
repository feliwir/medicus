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

pub use loader::{AddonInfo, AddonLoader, LoadError, LoadedAddon, Named};
pub use medicus_abi::StaticAddon;
