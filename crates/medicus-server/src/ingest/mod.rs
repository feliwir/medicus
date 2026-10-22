//! The ingest pipeline (PLAN §3.3). Every way of receiving an instance (admin upload, C-STORE,
//! STOW-RS, ...) ends here, so stored instances and their events are identical regardless of
//! the protocol; only the event's `origin` differs.
//!
//! 1. Buffer the beginning of the stream until dicom-rs can parse the complete header.
//! 2. Extract the attributes to index.
//! 3. Stream the whole file (buffered prefix + rest) into the blob store, without a second copy.
//! 4. Record it in the index together with its `instance.stored` event, in one transaction.
//!    Blobs left unreferenced by a failure or a replaced duplicate are deleted.

pub(crate) mod parse;
#[cfg(test)]
mod real_data;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::{Bytes, BytesMut};
use futures::StreamExt as _;
use medicus_core::blob::BlobKind;
use medicus_core::capability::index::{BlobRef, Duplicate, InstanceRecord};
use medicus_core::capability::{ByteStream, Ingested};
use medicus_core::event::{Event, EventKind, Origin, ResourceRef};
use medicus_core::{BlobKey, Error, ErrorKind, Result};

use crate::archive::Archive;
use parse::ParsedHeader;

/// The first parse attempt is made after this many bytes (or the end of the stream); each
/// further attempt waits for twice as many, so the total parsing work stays linear.
const FIRST_ATTEMPT: usize = 16 * 1024;

impl Archive {
    /// Reads the stream until dicom-rs has parsed the complete header. Returns the buffered
    /// bytes (header and possibly some bulk data), the parsed header, and whether the stream is
    /// already exhausted (a stream must not be polled again after it ended).
    async fn read_header(&self, data: &mut ByteStream) -> Result<(Bytes, ParsedHeader, bool)> {
        let mut buf = BytesMut::new();
        let mut complete = false;
        let mut attempt_at = FIRST_ATTEMPT.min(self.max_header_bytes);
        loop {
            while !complete && buf.len() < attempt_at {
                match data.next().await {
                    Some(chunk) => buf.extend_from_slice(&chunk?),
                    None => complete = true,
                }
            }
            // Parsing is CPU work; keep it off the async worker threads.
            let snapshot = Bytes::copy_from_slice(&buf);
            let extra = self.extra_tags.clone();
            let parsed = tokio::task::spawn_blocking(move || {
                parse::parse_prefix(&snapshot, complete, &extra)
            })
            .await
            .map_err(|e| Error::internal(format!("header parser failed: {e}")))??;
            match parsed {
                Some(header) => return Ok((buf.freeze(), header, complete)),
                None if buf.len() >= self.max_header_bytes => {
                    return Err(Error::new(
                        ErrorKind::ResourceExhausted,
                        format!(
                            "DICOM header exceeds the limit of {} bytes",
                            self.max_header_bytes
                        ),
                    ));
                }
                None => attempt_at = (buf.len() * 2).min(self.max_header_bytes),
            }
        }
    }

    /// Ingests one DICOM Part 10 file.
    pub async fn ingest(&self, mut data: ByteStream, origin: Origin) -> Result<Ingested> {
        self.ensure_capacity()?;
        let (prefix, parsed, complete) = self.read_header(&mut data).await?;
        let rest: ByteStream = if complete {
            Box::pin(futures::stream::empty())
        } else {
            data
        };

        // Stream the complete file: the buffered prefix, then the rest as it arrives.
        let store = self.store(&self.default_store)?;
        let key = BlobKey::generate(BlobKind::Instance);
        let written = Arc::new(AtomicU64::new(0));
        let counter = written.clone();
        let file: ByteStream = Box::pin(
            futures::stream::once(async move { Ok(prefix) })
                .chain(rest)
                .inspect(move |chunk| {
                    if let Ok(c) = chunk {
                        counter.fetch_add(c.len() as u64, Ordering::Relaxed);
                    }
                }),
        );
        let meta = store.put(&key, file).await?;
        debug_assert_eq!(meta.size, written.load(Ordering::Relaxed));

        let record = InstanceRecord {
            study: parsed.study.clone(),
            series: parsed.series.clone(),
            instance: parsed.instance.clone(),
            sop_class: parsed.sop_class.clone(),
            transfer_syntax: parsed.transfer_syntax.clone(),
            attributes: parsed.attributes,
            size: meta.size,
            blob: BlobRef {
                store: self.default_store.clone(),
                key: key.clone(),
            },
            header_length: parsed.header_length,
        };
        let event = Event::new(EventKind::InstanceStored, origin).with_resource(ResourceRef {
            patient_id: parsed.patient_id,
            study: Some(record.study.clone()),
            series: Some(record.series.clone()),
            instance: Some(record.instance.clone()),
            sop_class: Some(record.sop_class.clone()),
            transfer_syntax: Some(record.transfer_syntax.clone()),
            size: Some(record.size),
            blob_store: Some(self.default_store.clone()),
        });
        let outcome = match self
            .index
            .store_instance(&record, self.policy, &event)
            .await
        {
            Ok(o) => o,
            Err(e) => {
                self.delete_blob_best_effort(&self.default_store, &key)
                    .await;
                return Err(e);
            }
        };
        match &outcome.duplicate {
            Duplicate::None => {}
            Duplicate::Replaced { previous } => {
                self.delete_blob_best_effort(&previous.blob.store, &previous.blob.key)
                    .await;
            }
            Duplicate::KeptExisting => {
                self.delete_blob_best_effort(&self.default_store, &key)
                    .await;
            }
        }
        if let Some(seq) = outcome.seq {
            self.events_committed(seq);
        }
        tracing::debug!(instance = %record.instance, size = record.size, seq = ?outcome.seq, "instance stored");
        Ok(Ingested {
            study: record.study,
            series: record.series,
            instance: record.instance,
            sop_class: record.sop_class,
            transfer_syntax: record.transfer_syntax,
            size: record.size,
            seq: outcome.seq,
            duplicate: outcome.duplicate,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use dicom_dictionary_std::tags;
    use medicus_core::capability::index::ResourceUid;
    use medicus_core::query::{Key, MatchOptions, Query};
    use medicus_core::uid::StudyInstanceUid;
    use medicus_core::{QueryLevel, VR};
    use medicus_testkit::dicom::InstanceBuilder;
    use medicus_testkit::{read_all, stream};

    struct Fixture {
        archive: Archive,
        dir: tempfile::TempDir,
    }

    async fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.server.data_dir = dir.path().to_owned();
        let caps = crate::addons::load(&cfg, &tokio::runtime::Handle::current()).unwrap();
        let archive = Archive::new(&caps, &cfg.archive).await.unwrap();
        Fixture { archive, dir }
    }

    fn origin() -> Origin {
        Origin::AdminApi {
            remote_addr: "127.0.0.1:1".into(),
            principal: None,
        }
    }

    /// All blob files currently stored (excluding the temporary directory).
    fn blob_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        fn walk(p: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for e in std::fs::read_dir(p).unwrap().flatten() {
                let path = e.path();
                if path.is_dir() {
                    if path.file_name().is_some_and(|n| n != ".tmp") {
                        walk(&path, out);
                    }
                } else {
                    out.push(path);
                }
            }
        }
        let mut out = Vec::new();
        walk(&dir.join("blobs"), &mut out);
        out
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn deflated_data_sets_are_parsed_while_streaming() {
        let f = fixture().await;
        let file = InstanceBuilder::new("1.2.3", "1.2.3.1", "1.2.3.1.9")
            .transfer_syntax(dicom_dictionary_std::uids::DEFLATED_EXPLICIT_VR_LITTLE_ENDIAN)
            .pixel_data(200_000)
            .with_sequence()
            .build();
        assert!(
            !file.windows(8).any(|w| w == b"Doe^John"),
            "the data set is compressed"
        );
        let stored = f
            .archive
            .ingest(stream(&file, 997), origin())
            .await
            .unwrap();
        assert_eq!(stored.instance.as_str(), "1.2.3.1.9");
        let (_, data) = f.archive.retrieve(&stored.instance).await.unwrap();
        assert_eq!(read_all(data).await, file, "stored byte-identically");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ingest_retrieve_and_event() {
        let f = fixture().await;
        let file = InstanceBuilder::new("1.2.3", "1.2.3.1", "1.2.3.1.1")
            .pixel_data(3 * 1024 * 1024)
            .with_sequence()
            .build();
        // Small chunks: the header is assembled across many of them.
        let stored = f
            .archive
            .ingest(stream(&file, 997), origin())
            .await
            .unwrap();
        assert_eq!(stored.instance.as_str(), "1.2.3.1.1");
        assert_eq!(stored.size, file.len() as u64);
        assert_eq!(stored.duplicate, Duplicate::None);

        let (loc, data) = f.archive.retrieve(&stored.instance).await.unwrap();
        assert_eq!(read_all(data).await, file, "stored byte-identically");
        // The recorded header prefix (a range read, not the whole 3 MiB file) yields the header.
        assert!(
            loc.header_length > 0 && loc.header_length < 64 * 1024,
            "{}",
            loc.header_length
        );
        let store = f.archive.store(&loc.blob.store).unwrap();
        let prefix = read_all(
            store
                .get(&loc.blob.key, Some(0..loc.header_length))
                .await
                .unwrap(),
        )
        .await;
        let header = parse::parse_prefix(&prefix, true, &[]).unwrap().unwrap();
        assert_eq!(header.instance, stored.instance);
        assert!(
            header
                .attributes
                .contains_key(&medicus_core::TagExt::json_key(tags::PATIENT_NAME))
        );

        let q = Query::new(QueryLevel::Study).with_key(
            Key::parse(tags::PATIENT_NAME, VR::PN, "doe*", &MatchOptions::default()).unwrap(),
        );
        let rows = f.archive.index.query(&q).await.unwrap().rows;
        assert_eq!(rows.len(), 1);

        let events = f.archive.index.events_after(0, 10).await.unwrap();
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(
            (e.kind.clone(), Some(e.seq)),
            (EventKind::InstanceStored, stored.seq)
        );
        assert_eq!(e.origin, origin());
        let r = e.resource.as_ref().unwrap();
        assert_eq!(
            (r.size, r.patient_id.as_deref()),
            (Some(file.len() as u64), Some("PID-1"))
        );
        assert_eq!(*f.archive.watch_events().borrow(), stored.seq.unwrap());
        assert_eq!(blob_files(f.dir.path()).len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn overwrite_and_delete_leave_no_orphans() {
        let f = fixture().await;
        let a = InstanceBuilder::new("1.2.3", "1.2.3.1", "1.2.3.1.1").build();
        let b = InstanceBuilder::new("1.2.3", "1.2.3.1", "1.2.3.1.1")
            .attr(tags::PATIENT_NAME, VR::PN, "Doe^Jane")
            .build();
        f.archive.ingest(stream(&a, 4096), origin()).await.unwrap();
        let second = f.archive.ingest(stream(&b, 4096), origin()).await.unwrap();
        assert!(matches!(second.duplicate, Duplicate::Replaced { .. }));
        assert_eq!(blob_files(f.dir.path()).len(), 1, "replaced blob deleted");
        let (_, data) = f.archive.retrieve(&second.instance).await.unwrap();
        assert_eq!(read_all(data).await, b);

        let other = InstanceBuilder::new("1.2.3", "1.2.3.2", "1.2.3.2.1").build();
        f.archive
            .ingest(stream(&other, 4096), origin())
            .await
            .unwrap();
        assert_eq!(blob_files(f.dir.path()).len(), 2);

        let study = ResourceUid::Study(StudyInstanceUid::parse("1.2.3").unwrap());
        let out = f.archive.delete(&study, origin()).await.unwrap();
        assert_eq!(out.removed.len(), 2);
        assert!(
            blob_files(f.dir.path()).is_empty(),
            "deleted study leaves no blobs"
        );
        let kinds: Vec<EventKind> = f
            .archive
            .index
            .events_after(0, 10)
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect();
        assert_eq!(
            kinds,
            [
                EventKind::InstanceStored,
                EventKind::InstanceStored,
                EventKind::InstanceStored,
                EventKind::StudyDeleted
            ]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn invalid_input_stores_nothing() {
        let f = fixture().await;
        let err = f
            .archive
            .ingest(stream(b"definitely not DICOM", 5), origin())
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::InvalidArgument);
        let file = InstanceBuilder::new("1.2.3", "1.2.3.1", "1.2.3.1.1").build();
        let truncated = &file[..file.len() / 3];
        assert_eq!(
            f.archive
                .ingest(stream(truncated, 64), origin())
                .await
                .unwrap_err()
                .kind,
            ErrorKind::InvalidArgument
        );
        let no_series = InstanceBuilder::new("1.2.3", "1.2.3.1", "1.2.3.1.1")
            .attr(tags::SERIES_INSTANCE_UID, VR::UI, "")
            .build();
        assert_eq!(
            f.archive
                .ingest(stream(&no_series, 64), origin())
                .await
                .unwrap_err()
                .kind,
            ErrorKind::InvalidArgument
        );
        assert!(blob_files(f.dir.path()).is_empty());
        assert_eq!(f.archive.index.last_event_seq().await.unwrap(), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oversized_header_is_refused() {
        let mut f = fixture().await;
        f.archive.max_header_bytes = 1024;
        let big = InstanceBuilder::new("1.2.3", "1.2.3.1", "1.2.3.1.1")
            .attr(tags::STUDY_DESCRIPTION, VR::LO, &"x".repeat(60))
            .attr(tags::IMAGE_COMMENTS, VR::LT, &"y".repeat(4000))
            .build();
        let err = f
            .archive
            .ingest(stream(&big, 512), origin())
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::ResourceExhausted);
    }
}
