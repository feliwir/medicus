//! `medicus import`: ingests DICOM Part 10 files from a directory tree through the normal ingest
//! pipeline (events carry `origin = import`).

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

const READ_CHUNK: usize = 256 * 1024;

#[derive(Debug, Default, Serialize)]
pub(crate) struct ImportReport {
    pub(crate) files: u64,
    pub(crate) stored: u64,
    pub(crate) replaced: u64,
    pub(crate) kept_existing: u64,
    /// Files without the `DICM` prefix of a Part 10 file.
    pub(crate) skipped: u64,
    /// Failures by error kind.
    pub(crate) failed: BTreeMap<String, u64>,
    pub(crate) bytes: u64,
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

fn is_part10(path: &Path) -> bool {
    let mut head = [0u8; 132];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut head))
        .is_ok()
        && &head[128..] == b"DICM"
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
    Skipped,
    Failed(Error),
}

async fn import_one(archive: &Archive, path: PathBuf) -> Outcome {
    let p = path.clone();
    if !tokio::task::spawn_blocking(move || is_part10(&p))
        .await
        .unwrap_or(false)
    {
        return Outcome::Skipped;
    }
    let origin = Origin::Import {
        path: path.display().to_string(),
    };
    let result = match open(&path).await {
        Ok(data) => archive.ingest(data, origin).await,
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

/// Imports all files below `root`, `concurrency` at a time.
pub(crate) async fn import(
    archive: &Archive,
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
        .map(|path| import_one(archive, path))
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
            Outcome::Skipped => report.skipped += 1,
            Outcome::Failed(e) => *report.failed.entry(e.kind.to_string()).or_default() += 1,
        }
    }
    Ok(report)
}
