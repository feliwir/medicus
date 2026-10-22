//! The critical-capacity guard (PLAN §3.10): refuses ingest while the default store is full.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use medicus_core::event::{Event, EventKind, Origin, ResourceRef};
use medicus_core::{Error, ErrorKind, Result};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::archive::Archive;

/// How full the default store is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Fill {
    pub(crate) used: u64,
    pub(crate) limit: u64,
}

impl Fill {
    #[allow(clippy::cast_precision_loss)] // a percentage does not need 64-bit precision
    pub(crate) fn percent(self) -> f64 {
        if self.limit == 0 {
            100.0
        } else {
            self.used as f64 * 100.0 / self.limit as f64
        }
    }
}

impl Archive {
    /// Refuses ingest while storage is critically full.
    pub(crate) fn ensure_capacity(&self) -> Result<()> {
        if self.storage_full.load(Ordering::Acquire) {
            return Err(Error::new(
                ErrorKind::ResourceExhausted,
                format!(
                    "blob store {:?} is full; new instances are refused",
                    self.default_store
                ),
            ));
        }
        Ok(())
    }

    /// The fill level of the default store: the fuller of physical usage (if the store reports
    /// it) and the configured quota (measured by the bytes the index references).
    pub(crate) async fn fill(&self) -> Result<Option<Fill>> {
        let mut fills = Vec::new();
        if let Some(u) = self.store(&self.default_store)?.usage().await? {
            fills.push(Fill {
                used: u.used,
                limit: u.capacity,
            });
        }
        if let Some(quota) = self.capacity.quota_bytes {
            let used = self
                .index
                .stored_bytes()
                .await?
                .iter()
                .filter(|s| s.store == self.default_store)
                .map(|s| s.bytes)
                .sum();
            fills.push(Fill { used, limit: quota });
        }
        Ok(fills
            .into_iter()
            .max_by(|a, b| a.percent().total_cmp(&b.percent())))
    }

    /// Measures the fill level once and switches between normal and full (with hysteresis),
    /// publishing `storage.full` / `storage.recovered` on transitions.
    pub(crate) async fn check_capacity(&self) -> Result<Option<Fill>> {
        let Some(fill) = self.fill().await? else {
            return Ok(None);
        };
        let was_full = self.storage_full.load(Ordering::Acquire);
        let pct = fill.percent();
        let kind = if !was_full && pct >= self.capacity.critical_percent {
            Some(EventKind::StorageFull)
        } else if was_full && pct < self.capacity.resume_percent {
            Some(EventKind::StorageRecovered)
        } else {
            None
        };
        if let Some(kind) = kind {
            let now_full = kind == EventKind::StorageFull;
            self.storage_full.store(now_full, Ordering::Release);
            if now_full {
                tracing::error!(store = %self.default_store, percent = pct, "storage critically full; refusing new instances");
            } else {
                tracing::info!(store = %self.default_store, percent = pct, "storage recovered; accepting new instances");
            }
            let event = Event::new(
                kind,
                Origin::Internal {
                    component: "capacity".into(),
                },
            )
            .with_resource(ResourceRef {
                blob_store: Some(self.default_store.clone()),
                ..ResourceRef::default()
            })
            .with_detail(json!({ "used": fill.used, "limit": fill.limit, "percent": pct }));
            let seq = self.index.append_event(&event).await?;
            self.events_committed(seq);
        }
        Ok(Some(fill))
    }
}

/// Checks the capacity periodically until `shutdown`.
pub(crate) async fn monitor(archive: Arc<Archive>, shutdown: CancellationToken) {
    let mut tick = tokio::time::interval(Duration::from_secs(
        archive.capacity.check_interval_secs.max(1),
    ));
    loop {
        tokio::select! {
            () = shutdown.cancelled() => return,
            _ = tick.tick() => {
                if let Err(e) = archive.check_capacity().await {
                    tracing::warn!(error = %e, "capacity check failed");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use medicus_core::capability::index::ResourceUid;
    use medicus_core::uid::StudyInstanceUid;
    use medicus_testkit::dicom::InstanceBuilder;
    use medicus_testkit::stream;

    #[tokio::test(flavor = "multi_thread")]
    async fn refuses_ingest_when_full_and_recovers_with_hysteresis() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.server.data_dir = dir.path().to_owned();
        // A quota of 100 KB, measured by the index; the physical disk is far from full.
        cfg.archive.capacity.quota_bytes = Some(100_000);
        cfg.archive.capacity.critical_percent = 90.0;
        cfg.archive.capacity.resume_percent = 50.0;
        let caps = crate::addons::load(&cfg, &tokio::runtime::Handle::current()).unwrap();
        let archive = Archive::new(&caps, &cfg.archive).await.unwrap();
        let origin = || Origin::Internal {
            component: "test".into(),
        };
        let file = |series: &str, sop: &str| {
            InstanceBuilder::new("1.2.3", series, sop)
                .pixel_data(30_000)
                .build()
        };

        for (series, sop) in [
            ("1.2.3.1", "1.2.3.1.1"),
            ("1.2.3.1", "1.2.3.1.2"),
            ("1.2.3.2", "1.2.3.2.1"),
        ] {
            archive
                .ingest(
                    &medicus_core::Partition::default_partition(),
                    stream(&file(series, sop), 4096),
                    origin(),
                )
                .await
                .unwrap();
            archive.check_capacity().await.unwrap();
        }
        assert!(archive.check_capacity().await.unwrap().unwrap().percent() >= 90.0);
        let err = archive
            .ingest(
                &medicus_core::Partition::default_partition(),
                stream(&file("1.2.3.3", "1.2.3.3.1"), 4096),
                origin(),
            )
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::ResourceExhausted);

        // Hysteresis: dropping below the critical level is not enough...
        let second_series = ResourceUid::Series("1.2.3.2".parse().unwrap());
        archive
            .delete(
                &medicus_core::Partition::default_partition(),
                &second_series,
                origin(),
            )
            .await
            .unwrap();
        let level = archive.check_capacity().await.unwrap().unwrap();
        assert!((50.0..90.0).contains(&level.percent()), "{level:?}");
        assert!(archive.ensure_capacity().is_err());
        // ...only below the resume level.
        let study = ResourceUid::Study(StudyInstanceUid::parse("1.2.3").unwrap());
        archive
            .delete(
                &medicus_core::Partition::default_partition(),
                &study,
                origin(),
            )
            .await
            .unwrap();
        archive.check_capacity().await.unwrap();
        archive.ensure_capacity().unwrap();

        let kinds: Vec<EventKind> = archive
            .index
            .events_after(0, 100)
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect();
        assert_eq!(
            kinds
                .iter()
                .filter(|k| **k == EventKind::StorageFull)
                .count(),
            1
        );
        assert_eq!(
            kinds
                .iter()
                .filter(|k| **k == EventKind::StorageRecovered)
                .count(),
            1
        );
    }
}
