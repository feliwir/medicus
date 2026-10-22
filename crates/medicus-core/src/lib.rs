//! Domain model of the medicus PACS.
//!
//! This crate is deliberately free of I/O and of any DICOM parsing library. It defines the
//! vocabulary shared by the core server and all addons:
//!
//! - validated identifiers ([`uid`], [`AeTitle`], [`BlobKey`]),
//! - DICOM attribute addressing ([`Tag`], [`VR`]) and the standard query keys ([`tags`]),
//! - the backend-neutral [`query`] model implementing PS3.4 C.2.2.2 matching,
//! - the versioned [`event`] envelope.
//!
//! Every type that crosses the addon boundary has a stable serde representation.

pub mod ae;
pub mod blob;
pub mod capability;
pub mod error;
pub mod event;
pub mod level;
pub mod query;
pub mod tag;
pub mod uid;

pub use ae::AeTitle;
pub use blob::BlobKey;
pub use error::{Error, ErrorKind, Result};
pub use level::QueryLevel;
pub use tag::{Tag, TagExt, VR, VrExt, tags};
