//! The medicus addon ABI (see `docs/adr/0001-addon-abi.md`).
//!
//! - [`ffi`] is the raw C contract. `include/medicus_addon.h` is generated from it, and it is
//!   the only thing host and addons share at runtime.
//! - [`bridge`] holds safe Rust helpers on top of it: owned buffers, completion futures,
//!   completion guards, and stream import/export. Both the host and the Rust SDK use them, so the
//!   delicate ownership logic exists exactly once.

// This crate implements the FFI boundary; `unsafe` is confined to `ffi` and `bridge`, and every
// unsafe block documents the invariant it relies on.
#![allow(unsafe_code)]

pub mod bridge;
pub mod ffi;

/// Name of the exported ABI version symbol.
pub const SYMBOL_ABI_VERSION: &[u8] = b"medicus_addon_abi_version";
/// Name of the exported manifest symbol.
pub const SYMBOL_MANIFEST: &[u8] = b"medicus_addon_manifest";
/// Name of the exported init symbol.
pub const SYMBOL_INIT: &[u8] = b"medicus_addon_init";

/// Entry points of an addon linked into the host binary instead of loaded from a library.
#[derive(Debug, Clone, Copy)]
pub struct StaticAddon {
    pub abi_version: ffi::MdAbiVersionFn,
    pub manifest: ffi::MdManifestFn,
    pub init: ffi::MdInitFn,
}
