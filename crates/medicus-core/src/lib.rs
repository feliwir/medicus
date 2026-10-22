//! Domain model of the medicus PACS.
//!
//! This crate is deliberately free of I/O. It builds on the dicom-rs types and defines the
//! vocabulary shared by the core server and all addons:
//!
//! - validated identifiers ([`uid`], [`AeTitle`], [`BlobKey`], [`Partition`]),
//! - DICOM attribute addressing ([`Tag`], [`VR`]) and the standard query keys ([`tags`]),
//! - the backend-neutral [`query`] model implementing PS3.4 C.2.2.2 matching,
//! - the versioned [`event`] envelope.
//!
//! Every type that crosses the addon boundary has a stable serde representation.

pub mod access;
pub mod ae;
pub mod blob;
pub mod capability;
pub mod error;
pub mod event;
pub mod level;
pub mod partition;
pub mod query;
pub mod tag;
pub mod uid;

pub use access::{CallContext, NodeInfo, Permission};
pub use ae::AeTitle;
pub use blob::BlobKey;
pub use error::{Error, ErrorKind, Result};
pub use level::QueryLevel;
pub use partition::Partition;
pub use tag::{Tag, TagExt, VR, VrExt, tags};
