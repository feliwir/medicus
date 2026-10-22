//! `medicus import`: ingests DICOM Part 10 files from a directory tree through the normal ingest
//! pipeline (events carry `origin = import`).

use medicus_core::Partition;
use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use futures::{StreamExt as _, TryStreamExt as _};
use medicus_core::capability::ByteStream;
use medicus_core::capability::index::Duplicate;
use medicus_core::event::Origin;
use medicus_core::{Error, Result};
use serde::Serialize;

use crate::archive::Archive;
use dicom_dictionary_std::uids;

const READ_CHUNK: usize = 256 * 1024;

#[derive(Debug, Default, Serialize)]
pub struct ImportReport {
    pub files: u64,
    pub stored: u64,
    pub replaced: u64,
    pub kept_existing: u64,
    /// Files not imported, by reason (not Part 10, DICOMDIR, ...).
    pub skipped: BTreeMap<String, u64>,
    /// Failures by error kind.
    pub failed: BTreeMap<String, u64>,
    pub bytes: u64,
}

fn files_below(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut dirs = vec![root.to_owned()];
    while let Some(dir) = dirs.pop() {
        if dir.is_file() {
            out.push(dir);
            continue;
        }
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.is_dir() {
                dirs.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Why a file is not imported, if it is not.
fn skip_reason(path: &Path) -> Option<&'static str> {
    let Ok(mut file) = std::fs::File::open(path) else {
        return Some("unreadable");
    };
    let mut preamble = [0u8; 128];
    if file.read_exact(&mut preamble).is_err() {
        return Some("not a DICOM Part 10 file");
    }
    // The meta group (including the DICM prefix) is read by dicom-rs.
    // A missing DICM prefix means "not DICOM"; a broken meta group after it is a corrupt file,
    // which the ingest pipeline reports as a failure.
    match dicom_object::meta::FileMetaTable::from_reader(std::io::BufReader::new(file)) {
        Err(
            dicom_object::meta::Error::NotDicom { .. }
            | dicom_object::meta::Error::ReadMagicCode { .. },
        ) => Some("not a DICOM Part 10 file"),
        Ok(meta) if meta.media_storage_sop_class_uid() == uids::MEDIA_STORAGE_DIRECTORY_STORAGE => {
            Some("DICOMDIR")
        }
        Err(_) | Ok(_) => None,
    }
}

async fn open(path: &Path) -> Result<ByteStream> {
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|e| Error::not_found(format!("{}: {e}", path.display())))?;
    let stream = tokio_util::io::ReaderStream::with_capacity(file, READ_CHUNK)
        .map_err(|e| Error::internal(format!("reading: {e}")));
    Ok(Box::pin(stream))
}

enum Outcome {
    Stored(u64, Duplicate),
    Skipped(&'static str),
    Failed(Error),
}

async fn import_one(archive: &Archive, partition: &Partition, path: PathBuf) -> Outcome {
    let p = path.clone();
    let skip = tokio::task::spawn_blocking(move || skip_reason(&p))
        .await
        .unwrap_or(Some("unreadable"));
    if let Some(reason) = skip {
        return Outcome::Skipped(reason);
    }
    let origin = Origin::Import {
        path: path.display().to_string(),
    };
    let result = match open(&path).await {
        Ok(data) => archive.ingest(partition, data, origin).await,
        Err(e) => Err(e),
    };
    match result {
        Ok(stored) => Outcome::Stored(stored.size, stored.duplicate),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "import failed");
            Outcome::Failed(e)
        }
    }
}

/// Imports all files below `root` into `partition`, `concurrency` at a time.
pub(crate) async fn import(
    archive: &Archive,
    partition: &Partition,
    root: &Path,
    concurrency: usize,
) -> anyhow::Result<ImportReport> {
    let root = root.to_owned();
    let files = tokio::task::spawn_blocking(move || files_below(&root)).await??;
    let mut report = ImportReport {
        files: files.len() as u64,
        ..ImportReport::default()
    };
    let mut outcomes = futures::stream::iter(files)
        .map(|path| import_one(archive, partition, path))
        .buffer_unordered(concurrency.max(1));
    while let Some(outcome) = outcomes.next().await {
        match outcome {
            Outcome::Stored(size, duplicate) => {
                report.bytes += size;
                match duplicate {
                    Duplicate::None => report.stored += 1,
                    Duplicate::Replaced { .. } => report.replaced += 1,
                    Duplicate::KeptExisting => report.kept_existing += 1,
                }
            }
            Outcome::Skipped(reason) => *report.skipped.entry(reason.to_owned()).or_default() += 1,
            Outcome::Failed(e) => *report.failed.entry(e.kind.to_string()).or_default() += 1,
        }
    }
    Ok(report)
}
