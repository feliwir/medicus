//! The archive service: the protocol-neutral operations every front-end (admin API, DIMSE,
//! `DICOMweb`) uses. It owns the configured index and blob stores.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context as _, bail};
use medicus_core::capability::index::{
    DeleteOutcome, DuplicatePolicy, ExtraTag, ExtraTagState, InstanceLocation, ResourceUid,
};
use medicus_core::capability::{BlobStore, ByteStream, IndexStore};
use medicus_core::event::{Event, EventKind, Origin, ResourceRef};
use medicus_core::uid::SopInstanceUid;
use medicus_core::{Error, Partition, Result};
use tokio::sync::watch;

use crate::addons::Capabilities;
use crate::config::{ArchiveConfig, CapacityConfig};

pub struct Archive {
    pub(crate) index: Arc<dyn IndexStore>,
    stores: BTreeMap<String, Arc<dyn BlobStore>>,
    pub(crate) default_store: String,
    /// Partitions writing to their own store instead of the default one.
    /// Changes with the settings, while ingests run.
    partition_stores: arc_swap::ArcSwap<BTreeMap<Partition, String>>,
    pub(crate) policy: DuplicatePolicy,
    pub(crate) max_header_bytes: usize,
    pub(crate) extra_tags: Arc<[ExtraTag]>,
    /// The highest event sequence number committed so far; subscribers wait on changes.
    last_seq: watch::Sender<u64>,
    pub(crate) capacity: CapacityConfig,
    /// Set while the default store is critically full; ingest is refused meanwhile.
    pub(crate) storage_full: std::sync::atomic::AtomicBool,
}

impl std::fmt::Debug for Archive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Archive")
            .field("stores", &self.stores.keys().collect::<Vec<_>>())
            .field("default_store", &self.default_store)
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

impl Archive {
    /// Wires the configured index and stores and declares the extra tags to the index.
    pub(crate) async fn new(caps: &Capabilities, cfg: &ArchiveConfig) -> anyhow::Result<Self> {
        let Some(index) = caps.index_stores.get(&cfg.index) else {
            bail!(
                "index {:?} is not provided by any addon (available: {:?})",
                cfg.index,
                caps.index_stores.keys().collect::<Vec<_>>()
            );
        };
        if !caps.blob_stores.contains_key(&cfg.storage) {
            bail!(
                "blob store {:?} is not provided by any addon (available: {:?})",
                cfg.storage,
                caps.blob_stores.keys().collect::<Vec<_>>()
            );
        }
        let extra_tags: Vec<ExtraTag> = cfg
            .extra_tags
            .iter()
            .map(|t| t.resolve().map_err(anyhow::Error::msg))
            .collect::<anyhow::Result<_>>()?;
        let status = index
            .configure_extra_tags(&extra_tags)
            .await
            .context("configuring extra tags")?;
        for s in status
            .iter()
            .filter(|s| s.state == ExtraTagState::Backfilling)
        {
            tracing::warn!(
                tag = %medicus_core::tag::Named(s.tag.tag),
                "extra tag added to a non-empty index; queries on it are post-filtered until it is backfilled"
            );
        }
        let last = index
            .last_event_seq()
            .await
            .context("reading the event outbox")?;
        Ok(Self {
            index: index.clone(),
            stores: caps.blob_stores.clone(),
            default_store: cfg.storage.clone(),
            partition_stores: arc_swap::ArcSwap::default(),
            policy: cfg.duplicate_policy,
            max_header_bytes: cfg.max_header_bytes,
            extra_tags: extra_tags.into(),
            last_seq: watch::Sender::new(last),
            capacity: cfg.capacity.clone(),
            storage_full: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// An archive whose index is `index`, without calling it: for degraded mode, where every
    /// call fails. The configuration is still checked.
    pub(crate) fn degraded(
        caps: &Capabilities,
        cfg: &ArchiveConfig,
        index: Arc<dyn IndexStore>,
    ) -> anyhow::Result<Self> {
        if !caps.blob_stores.contains_key(&cfg.storage) {
            bail!(
                "blob store {:?} is not provided by any addon (available: {:?})",
                cfg.storage,
                caps.blob_stores.keys().collect::<Vec<_>>()
            );
        }
        let extra_tags: Vec<ExtraTag> = cfg
            .extra_tags
            .iter()
            .map(|t| t.resolve().map_err(anyhow::Error::msg))
            .collect::<anyhow::Result<_>>()?;
        Ok(Self {
            index,
            stores: caps.blob_stores.clone(),
            default_store: cfg.storage.clone(),
            partition_stores: arc_swap::ArcSwap::default(),
            policy: cfg.duplicate_policy,
            max_header_bytes: cfg.max_header_bytes,
            extra_tags: extra_tags.into(),
            last_seq: watch::Sender::new(0),
            capacity: cfg.capacity.clone(),
            storage_full: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Checks that the blob stores the partitions name exist.
    pub(crate) fn check_partition_stores(
        &self,
        stores: &BTreeMap<Partition, String>,
    ) -> anyhow::Result<()> {
        for (partition, store) in stores {
            if !self.stores.contains_key(store) {
                anyhow::bail!("partition {partition}: blob store {store:?} is not configured");
            }
        }
        Ok(())
    }

    /// Routes the partitions' new instances to their own blob stores, from now on.
    pub(crate) fn set_partition_stores(
        &self,
        stores: BTreeMap<Partition, String>,
    ) -> anyhow::Result<()> {
        self.check_partition_stores(&stores)?;
        self.partition_stores.store(Arc::new(stores));
        Ok(())
    }

    /// The blob store `partition` writes new instances to.
    pub(crate) fn store_for(&self, partition: &Partition) -> String {
        self.partition_stores
            .load()
            .get(partition)
            .unwrap_or(&self.default_store)
            .clone()
    }

    pub(crate) fn store(&self, name: &str) -> Result<&Arc<dyn BlobStore>> {
        self.stores
            .get(name)
            .ok_or_else(|| Error::internal(format!("blob store {name:?} is not configured")))
    }

    /// Receives the highest committed event sequence number whenever it changes.
    pub(crate) fn watch_events(&self) -> watch::Receiver<u64> {
        self.last_seq.subscribe()
    }

    pub(crate) fn events_committed(&self, seq: u64) {
        self.last_seq.send_if_modified(|last| {
            let changed = seq > *last;
            *last = (*last).max(seq);
            changed
        });
    }

    /// Opens an instance's Part 10 file.
    pub(crate) async fn retrieve(
        &self,
        partition: &Partition,
        instance: &SopInstanceUid,
    ) -> Result<(InstanceLocation, ByteStream)> {
        let location = self
            .index
            .locate(partition, &ResourceUid::Instance(instance.clone()))
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| Error::not_found(format!("instance {instance} does not exist")))?;
        let stream = self
            .store(&location.blob.store)?
            .get(&location.blob.key, None)
            .await?;
        Ok((location, stream))
    }

    /// Deletes a resource from the index (with its event) and then its blobs.
    // Not reachable through any API yet: deletion will come from eviction (M6b).
    #[cfg_attr(not(test), expect(dead_code))]
    pub(crate) async fn delete(
        &self,
        partition: &Partition,
        resource: &ResourceUid,
        origin: Origin,
    ) -> Result<DeleteOutcome> {
        let (kind, reference) = match resource {
            ResourceUid::Study(uid) => (
                EventKind::StudyDeleted,
                ResourceRef {
                    study: Some(uid.clone()),
                    ..ResourceRef::default()
                },
            ),
            ResourceUid::Series(uid) => (
                EventKind::SeriesDeleted,
                ResourceRef {
                    series: Some(uid.clone()),
                    ..ResourceRef::default()
                },
            ),
            ResourceUid::Instance(uid) => (
                EventKind::InstanceDeleted,
                ResourceRef {
                    instance: Some(uid.clone()),
                    ..ResourceRef::default()
                },
            ),
        };
        let event = Event::new(kind, origin)
            .with_partition(partition.clone())
            .with_resource(reference);
        let outcome = self.index.delete(partition, resource, &event).await?;
        self.events_committed(outcome.seq);
        for removed in &outcome.removed {
            self.delete_blob_best_effort(&removed.blob.store, &removed.blob.key)
                .await;
        }
        Ok(outcome)
    }

    /// Deletes a blob that is no longer referenced. Failures leave an orphan, which the orphan
    /// GC (M7) removes later; they never fail the operation that made the blob unreferenced.
    pub(crate) async fn delete_blob_best_effort(&self, store: &str, key: &medicus_core::BlobKey) {
        let result = match self.store(store) {
            Ok(s) => s.delete(key).await,
            Err(e) => Err(e),
        };
        if let Err(e) = result {
            tracing::warn!(%store, %key, error = %e, "could not delete unreferenced blob");
        }
    }
}
