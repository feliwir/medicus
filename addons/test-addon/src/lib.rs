//! Test addon for the ABI test-suite.
//!
//! Configuration:
//! - `panic_prefix`: blob operations on keys starting with this prefix panic.
//! - `fail_init`: initialization fails.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Mutex;

use bytes::{Bytes, BytesMut};
use futures::StreamExt as _;
use medicus_addon_sdk::{Registrar, async_trait, export_addon};
use medicus_core::BlobKey;
use medicus_core::capability::{
    BlobMeta, BlobStore, ByteStream, EventFilter, EventHook, StorageUsage,
};
use medicus_core::event::{Event, EventKind};
use medicus_core::{Error, ErrorKind, Result};
use serde::Deserialize;

/// Chunk size of streams returned by `get`, small enough to exercise multi-chunk transfers.
const CHUNK: usize = 64 * 1024;

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Config {
    panic_prefix: Option<String>,
    fail_init: bool,
}

#[derive(Debug, Default)]
struct MemoryStore {
    blobs: Mutex<HashMap<BlobKey, Bytes>>,
    panic_prefix: Option<String>,
}

impl MemoryStore {
    fn check_panic(&self, key: &BlobKey) {
        if let Some(p) = &self.panic_prefix {
            assert!(
                !key.as_str().starts_with(p.as_str()),
                "injected panic for {key}"
            );
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<BlobKey, Bytes>> {
        self.blobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[async_trait]
impl BlobStore for MemoryStore {
    async fn put(&self, key: &BlobKey, mut data: ByteStream) -> Result<BlobMeta> {
        self.check_panic(key);
        let mut buf = BytesMut::new();
        while let Some(chunk) = data.next().await {
            buf.extend_from_slice(&chunk?);
        }
        let size = buf.len() as u64;
        self.lock().insert(key.clone(), buf.freeze());
        Ok(BlobMeta { size })
    }

    async fn get(&self, key: &BlobKey, range: Option<Range<u64>>) -> Result<ByteStream> {
        self.check_panic(key);
        let blob = self
            .lock()
            .get(key)
            .cloned()
            .ok_or_else(|| Error::not_found(key.to_string()))?;
        let len = blob.len() as u64;
        let r = range.unwrap_or(0..len);
        if r.start > r.end || r.end > len {
            return Err(Error::invalid_argument(format!(
                "range {r:?} outside 0..{len}"
            )));
        }
        let (start, end) = (
            usize::try_from(r.start).unwrap_or(usize::MAX),
            usize::try_from(r.end).unwrap_or(usize::MAX),
        );
        let slice = blob.slice(start..end);
        let chunks: Vec<Result<Bytes>> = (0..slice.len())
            .step_by(CHUNK)
            .map(|i| Ok(slice.slice(i..(i + CHUNK).min(slice.len()))))
            .collect();
        Ok(Box::pin(futures::stream::iter(chunks)))
    }

    async fn stat(&self, key: &BlobKey) -> Result<Option<BlobMeta>> {
        Ok(self.lock().get(key).map(|b| BlobMeta {
            size: b.len() as u64,
        }))
    }

    async fn delete(&self, key: &BlobKey) -> Result<()> {
        self.lock().remove(key);
        Ok(())
    }

    async fn usage(&self) -> Result<Option<StorageUsage>> {
        let used: u64 = self.lock().values().map(|b| b.len() as u64).sum();
        Ok(Some(StorageUsage {
            capacity: 1 << 30,
            used,
            available: (1 << 30) - used,
        }))
    }
}

/// Accepts `instance.stored` and `test.*` events; `test.fail` is refused (to test redelivery).
#[derive(Debug)]
struct TestHook;

#[async_trait]
impl EventHook for TestHook {
    fn filter(&self) -> EventFilter {
        EventFilter {
            kinds: vec![
                EventKind::InstanceStored,
                EventKind::Other("test.fail".into()),
            ],
        }
    }

    async fn on_event(&self, event: &Event) -> Result<()> {
        tracing::info!(kind = %event.kind, seq = event.seq, "event received");
        if event.kind.as_str() == "test.fail" {
            return Err(Error::new(ErrorKind::Unavailable, "refusing test.fail"));
        }
        Ok(())
    }
}

fn init(reg: &mut Registrar) -> Result<()> {
    let cfg: Option<Config> = reg.config()?;
    let cfg = cfg.unwrap_or_default();
    if cfg.fail_init {
        return Err(Error::internal("fail_init requested"));
    }
    reg.register_blob_store(
        "memory",
        MemoryStore {
            panic_prefix: cfg.panic_prefix,
            ..MemoryStore::default()
        },
    )?;
    reg.register_event_hook("test-hook", TestHook)?;
    tracing::info!("test addon initialized");
    Ok(())
}

export_addon!(
    name = "test-addon",
    version = env!("CARGO_PKG_VERSION"),
    init = init
);
