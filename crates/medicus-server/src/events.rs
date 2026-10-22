//! Delivery of outbox events to event hooks (PLAN §3.7).
//!
//! Every hook is a durable subscriber named `<addon>/<hook>` whose cursor (the last delivered
//! sequence number) lives in the index. Delivery is in order and at least once: the cursor
//! advances only after the hook accepted an event, and a failing hook is retried with
//! exponential backoff without skipping, so it never sees events out of order. A hook that is new
//! to an existing archive starts at the current end of the outbox instead of replaying history.

use std::sync::Arc;
use std::time::Duration;

use medicus_core::capability::{EventFilter, EventHook};
use medicus_core::event::Event;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::archive::Archive;

/// Events fetched from the outbox per round trip.
const BATCH: u32 = 100;
const MIN_BACKOFF: Duration = Duration::from_millis(100);
const MAX_BACKOFF: Duration = Duration::from_mins(1);

/// Starts one delivery task per hook. The tasks stop when `shutdown` is cancelled.
pub(crate) fn spawn(
    archive: &Arc<Archive>,
    hooks: impl IntoIterator<Item = (String, Arc<dyn EventHook>)>,
    shutdown: &CancellationToken,
) -> JoinSet<()> {
    let mut tasks = JoinSet::new();
    for (name, hook) in hooks {
        let subscriber = Subscriber {
            name,
            filter: hook.filter(),
            hook,
            archive: archive.clone(),
        };
        tasks.spawn(subscriber.run(shutdown.clone()));
    }
    tasks
}

struct Subscriber {
    name: String,
    hook: Arc<dyn EventHook>,
    filter: EventFilter,
    archive: Arc<Archive>,
}

impl Subscriber {
    async fn run(self, shutdown: CancellationToken) {
        tracing::info!(subscriber = %self.name, "event delivery started");
        tokio::select! {
            () = shutdown.cancelled() => {}
            () = self.deliver_forever() => {}
        }
        tracing::info!(subscriber = %self.name, "event delivery stopped");
    }

    /// The subscriber's cursor; a new subscriber starts at the current end of the outbox.
    async fn initial_cursor(&self) -> u64 {
        let mut backoff = MIN_BACKOFF;
        loop {
            let result = async {
                let index = &self.archive.index;
                if let Some(seq) = index.cursor(&self.name).await? {
                    return Ok(seq);
                }
                let last = index.last_event_seq().await?;
                index.set_cursor(&self.name, last).await?;
                Ok::<_, medicus_core::Error>(last)
            }
            .await;
            match result {
                Ok(seq) => return seq,
                Err(e) => {
                    tracing::warn!(subscriber = %self.name, error = %e, "cannot read cursor; retrying");
                    backoff = sleep_backoff(backoff).await;
                }
            }
        }
    }

    async fn deliver_forever(&self) {
        let mut cursor = self.initial_cursor().await;
        let mut committed = self.archive.watch_events();
        loop {
            // Mark the current value as seen *before* reading, so a commit that happens while we
            // read is not missed.
            committed.borrow_and_update();
            let events = match self.archive.index.events_after(cursor, BATCH).await {
                Ok(events) => events,
                Err(e) => {
                    tracing::warn!(subscriber = %self.name, error = %e, "cannot read events; retrying");
                    sleep_backoff(MIN_BACKOFF).await;
                    continue;
                }
            };
            if events.is_empty() {
                if committed.changed().await.is_err() {
                    return; // the archive is gone
                }
                continue;
            }
            for event in events {
                if self.filter.accepts(&event) {
                    self.deliver(&event).await;
                }
                cursor = event.seq;
                self.save_cursor(cursor).await;
            }
        }
    }

    /// Delivers one event, retrying until the hook accepts it.
    async fn deliver(&self, event: &Event) {
        let mut backoff = MIN_BACKOFF;
        let mut attempt = 1u32;
        loop {
            match self.hook.on_event(event).await {
                Ok(()) => return,
                Err(e) => {
                    tracing::warn!(
                        subscriber = %self.name,
                        seq = event.seq,
                        kind = %event.kind,
                        attempt,
                        error = %e,
                        "event hook failed; retrying"
                    );
                    attempt += 1;
                    backoff = sleep_backoff(backoff).await;
                }
            }
        }
    }

    async fn save_cursor(&self, seq: u64) {
        let mut backoff = MIN_BACKOFF;
        while let Err(e) = self.archive.index.set_cursor(&self.name, seq).await {
            tracing::warn!(subscriber = %self.name, error = %e, "cannot save cursor; retrying");
            backoff = sleep_backoff(backoff).await;
        }
    }
}

/// Sleeps for `backoff` and returns the next, doubled backoff (capped).
async fn sleep_backoff(backoff: Duration) -> Duration {
    tokio::time::sleep(backoff).await;
    (backoff * 2).min(MAX_BACKOFF)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use medicus_core::event::{EventKind, Origin};
    use medicus_core::{Error, ErrorKind};
    use medicus_testkit::dicom::InstanceBuilder;
    use medicus_testkit::stream;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Records delivered events; fails the first `fail_first` deliveries.
    #[derive(Default)]
    struct Recorder {
        seen: Mutex<Vec<(u64, EventKind)>>,
        fail_first: AtomicU32,
        kinds: Vec<EventKind>,
    }

    #[async_trait::async_trait]
    impl EventHook for Recorder {
        fn filter(&self) -> EventFilter {
            EventFilter {
                kinds: self.kinds.clone(),
            }
        }

        async fn on_event(&self, event: &Event) -> medicus_core::Result<()> {
            if self.fail_first.load(Ordering::SeqCst) > 0 {
                self.fail_first.fetch_sub(1, Ordering::SeqCst);
                return Err(Error::new(ErrorKind::Unavailable, "not yet"));
            }
            self.seen
                .lock()
                .unwrap()
                .push((event.seq, event.kind.clone()));
            Ok(())
        }
    }

    async fn archive(dir: &std::path::Path) -> Arc<Archive> {
        let mut cfg = Config::default();
        cfg.server.data_dir = dir.to_owned();
        let caps = crate::addons::load(&cfg, &tokio::runtime::Handle::current()).unwrap();
        Arc::new(Archive::new(&caps, &cfg.archive).await.unwrap())
    }

    fn origin() -> Origin {
        Origin::Internal {
            component: "test".into(),
        }
    }

    async fn ingest(archive: &Archive, sop: &str) {
        let file = InstanceBuilder::new("1.2.3", "1.2.3.1", sop).build();
        archive.ingest(stream(&file, 4096), origin()).await.unwrap();
    }

    async fn wait_for(rec: &Recorder, n: usize) -> Vec<(u64, EventKind)> {
        for _ in 0..1000 {
            let seen = rec.seen.lock().unwrap().clone();
            if seen.len() >= n {
                return seen;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "only {} of {n} events delivered",
            rec.seen.lock().unwrap().len()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn delivers_in_order_retries_and_resumes_from_the_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let archive = archive(dir.path()).await;
        // History before the hook exists is not replayed.
        ingest(&archive, "1.2.3.1.1").await;

        let rec = Arc::new(Recorder {
            fail_first: AtomicU32::new(2),
            ..Recorder::default()
        });
        let shutdown = CancellationToken::new();
        let mut tasks = spawn(
            &archive,
            [("test/rec".to_owned(), rec.clone() as Arc<dyn EventHook>)],
            &shutdown,
        );
        // Wait until the new subscriber has stored its starting cursor.
        for _ in 0..1000 {
            if archive.index.cursor("test/rec").await.unwrap().is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        ingest(&archive, "1.2.3.1.2").await;
        ingest(&archive, "1.2.3.1.3").await;
        let seen = wait_for(&rec, 2).await;
        assert_eq!(
            seen.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
            [2, 3],
            "in order, after 2 failures"
        );
        assert_eq!(archive.index.cursor("test/rec").await.unwrap(), Some(3));

        // Restart: delivery resumes after the stored cursor, nothing is delivered twice.
        shutdown.cancel();
        while tasks.join_next().await.is_some() {}
        ingest(&archive, "1.2.3.1.4").await;
        let shutdown = CancellationToken::new();
        let mut tasks = spawn(
            &archive,
            [("test/rec".to_owned(), rec.clone() as Arc<dyn EventHook>)],
            &shutdown,
        );
        let seen = wait_for(&rec, 3).await;
        assert_eq!(seen.iter().map(|(s, _)| *s).collect::<Vec<_>>(), [2, 3, 4]);
        shutdown.cancel();
        while tasks.join_next().await.is_some() {}
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn filters_by_kind_but_advances_the_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let archive = archive(dir.path()).await;
        let rec = Arc::new(Recorder {
            kinds: vec![EventKind::StudyDeleted],
            ..Recorder::default()
        });
        let shutdown = CancellationToken::new();
        let mut tasks = spawn(
            &archive,
            [("test/f".to_owned(), rec.clone() as Arc<dyn EventHook>)],
            &shutdown,
        );
        for _ in 0..1000 {
            if archive.index.cursor("test/f").await.unwrap().is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        ingest(&archive, "1.2.3.1.1").await;
        let study = medicus_core::capability::index::ResourceUid::Study(
            medicus_core::uid::StudyInstanceUid::parse("1.2.3").unwrap(),
        );
        archive.delete(&study, origin()).await.unwrap();
        let seen = wait_for(&rec, 1).await;
        assert_eq!(seen, [(2, EventKind::StudyDeleted)]);
        for _ in 0..1000 {
            if archive.index.cursor("test/f").await.unwrap() == Some(2) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            archive.index.cursor("test/f").await.unwrap(),
            Some(2),
            "filtered events are skipped, not stuck"
        );
        shutdown.cancel();
        while tasks.join_next().await.is_some() {}
    }
}
