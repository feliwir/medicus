//! Capability traits: what addons provide and the core consumes.
//!
//! Addons implement these traits through the SDK, which exports them as C vtables; the host
//! adapts the vtables back into the same traits. Core code therefore only ever sees
//! `Arc<dyn BlobStore>` etc. and cannot tell an in-process implementation from an addon.

use std::ops::Range;
use std::pin::Pin;

use async_trait::async_trait;
use bytes::Bytes;
use futures_core::Stream;
use serde::{Deserialize, Serialize};

use crate::BlobKey;
use crate::error::Result;
use crate::event::{Event, EventKind};

/// A stream of byte chunks, used for blob contents and HTTP bodies.
pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes>> + Send + 'static>>;

/// Kinds of capabilities, as registered over the ABI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityKind {
    BlobStore,
    EventHook,
}

/// Metadata of a stored blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobMeta {
    pub size: u64,
}

/// Space accounting of a store (PLAN §3.10). All values in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageUsage {
    pub capacity: u64,
    pub used: u64,
    pub available: u64,
}

/// Storage of opaque blobs (DICOM files, metadata documents) under [`BlobKey`]s.
#[async_trait]
pub trait BlobStore: Send + Sync + 'static {
    /// Stores `data` under `key`, replacing any existing blob atomically. Returns the size written.
    async fn put(&self, key: &BlobKey, data: ByteStream) -> Result<BlobMeta>;

    /// Streams the blob, or the byte `range` of it.
    async fn get(&self, key: &BlobKey, range: Option<Range<u64>>) -> Result<ByteStream>;

    /// `None` if the blob does not exist.
    async fn stat(&self, key: &BlobKey) -> Result<Option<BlobMeta>>;

    /// Deletes the blob. Deleting a missing blob is not an error (idempotent).
    async fn delete(&self, key: &BlobKey) -> Result<()>;

    /// Physical capacity information, if the store has a physical limit.
    async fn usage(&self) -> Result<Option<StorageUsage>> {
        Ok(None)
    }
}

/// Which events a hook wants to receive.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventFilter {
    /// Empty = all kinds.
    #[serde(default)]
    pub kinds: Vec<EventKind>,
}

impl EventFilter {
    pub fn accepts(&self, event: &Event) -> bool {
        self.kinds.is_empty() || self.kinds.contains(&event.kind)
    }
}

/// Reacts to events after they happened (PLAN §3.7). Returning an error triggers redelivery.
#[async_trait]
pub trait EventHook: Send + Sync + 'static {
    fn filter(&self) -> EventFilter {
        EventFilter::default()
    }

    async fn on_event(&self, event: &Event) -> Result<()>;
}
