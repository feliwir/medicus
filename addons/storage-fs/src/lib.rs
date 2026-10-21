//! `storage-fs`: a `BlobStore` on the local filesystem.
//!
//! ```toml
//! [addons.storage-fs]
//! config = { root = "/var/lib/medicus/blobs", name = "local", fsync = true }
//! ```
//!
//! Blob keys map to relative paths below `root` (keys are validated and fanned out, see
//! `BlobKey`). Writes go to a temporary file in `root/.tmp` and are renamed into place, so readers
//! never see partial files and a crash leaves at most an orphaned temporary file.

use std::io::{ErrorKind as IoErrorKind, SeekFrom};
use std::ops::Range;
use std::path::{Path, PathBuf};

use futures::{StreamExt as _, TryStreamExt as _};
use medicus_addon_sdk::{Registrar, async_trait, static_addon};
use medicus_core::capability::{BlobMeta, BlobStore, ByteStream, StorageUsage};
use medicus_core::{BlobKey, Error, ErrorKind, Result};
use serde::Deserialize;
use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _, AsyncWriteExt as _};

/// Read buffer size of streams returned by `get`.
const READ_CHUNK: usize = 256 * 1024;
const TMP_DIR: &str = ".tmp";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Directory holding the blobs.
    pub root: PathBuf,
    /// Name under which the store is registered (referenced by `[storage]` in the server config).
    #[serde(default = "default_name")]
    pub name: String,
    /// `fsync` files before they become visible. Disable only for tests or throwaway data.
    #[serde(default = "default_true")]
    pub fsync: bool,
}

fn default_name() -> String {
    "local".into()
}

fn default_true() -> bool {
    true
}

fn io_error(context: &str, e: &std::io::Error) -> Error {
    let kind = match e.kind() {
        IoErrorKind::NotFound => ErrorKind::NotFound,
        IoErrorKind::StorageFull | IoErrorKind::QuotaExceeded => ErrorKind::ResourceExhausted,
        IoErrorKind::PermissionDenied => ErrorKind::Unavailable,
        _ => ErrorKind::Internal,
    };
    Error::new(kind, format!("{context}: {e}"))
}

#[derive(Debug)]
pub struct FsStore {
    root: PathBuf,
    tmp: PathBuf,
    fsync: bool,
}

impl FsStore {
    /// Opens (and creates, if needed) a store rooted at `root`.
    pub fn open(root: &Path, fsync: bool) -> Result<Self> {
        let tmp = root.join(TMP_DIR);
        std::fs::create_dir_all(&tmp)
            .map_err(|e| io_error(&format!("creating {}", tmp.display()), &e))?;
        Ok(Self {
            root: root.to_owned(),
            tmp,
            fsync,
        })
    }

    fn path(&self, key: &BlobKey) -> PathBuf {
        // Keys are validated: relative, `/`-separated, no `.`/`..` segments.
        key.as_str()
            .split('/')
            .fold(self.root.clone(), |p, seg| p.join(seg))
    }

    async fn write_tmp(&self, tmp: &Path, mut data: ByteStream) -> Result<u64> {
        let mut file = tokio::fs::File::create(tmp)
            .await
            .map_err(|e| io_error("creating temporary file", &e))?;
        let mut size = 0u64;
        while let Some(chunk) = data.next().await {
            let chunk = chunk?;
            file.write_all(&chunk)
                .await
                .map_err(|e| io_error("writing", &e))?;
            size += chunk.len() as u64;
        }
        file.flush().await.map_err(|e| io_error("writing", &e))?;
        if self.fsync {
            file.sync_all().await.map_err(|e| io_error("syncing", &e))?;
        }
        Ok(size)
    }
}

#[async_trait]
impl BlobStore for FsStore {
    async fn put(&self, key: &BlobKey, data: ByteStream) -> Result<BlobMeta> {
        let target = self.path(key);
        let tmp = self
            .tmp
            .join(format!("{}.part", uuid::Uuid::now_v7().simple()));
        let result = async {
            let size = self.write_tmp(&tmp, data).await?;
            if let Some(parent) = target.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| io_error("creating directory", &e))?;
            }
            // Atomic replace on all platforms (MoveFileEx with REPLACE_EXISTING on Windows).
            tokio::fs::rename(&tmp, &target)
                .await
                .map_err(|e| io_error("renaming into place", &e))?;
            Ok(BlobMeta { size })
        }
        .await;
        if result.is_err() {
            let _ = tokio::fs::remove_file(&tmp).await;
        }
        result
    }

    async fn get(&self, key: &BlobKey, range: Option<Range<u64>>) -> Result<ByteStream> {
        let path = self.path(key);
        let mut file = tokio::fs::File::open(&path)
            .await
            .map_err(|e| io_error(key.as_str(), &e))?;
        let len = file
            .metadata()
            .await
            .map_err(|e| io_error(key.as_str(), &e))?
            .len();
        let r = range.unwrap_or(0..len);
        if r.start > r.end || r.end > len {
            return Err(Error::invalid_argument(format!(
                "range {r:?} outside 0..{len} of {key}"
            )));
        }
        if r.start > 0 {
            file.seek(SeekFrom::Start(r.start))
                .await
                .map_err(|e| io_error("seeking", &e))?;
        }
        let reader = file.take(r.end - r.start);
        let stream = tokio_util::io::ReaderStream::with_capacity(reader, READ_CHUNK)
            .map_err(|e| io_error("reading", &e));
        Ok(Box::pin(stream))
    }

    async fn stat(&self, key: &BlobKey) -> Result<Option<BlobMeta>> {
        match tokio::fs::metadata(self.path(key)).await {
            Ok(m) => Ok(Some(BlobMeta { size: m.len() })),
            Err(e) if e.kind() == IoErrorKind::NotFound => Ok(None),
            Err(e) => Err(io_error(key.as_str(), &e)),
        }
    }

    async fn delete(&self, key: &BlobKey) -> Result<()> {
        match tokio::fs::remove_file(self.path(key)).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == IoErrorKind::NotFound => Ok(()),
            Err(e) => Err(io_error(key.as_str(), &e)),
        }
    }

    async fn usage(&self) -> Result<Option<StorageUsage>> {
        let root = self.root.clone();
        let stats = tokio::task::spawn_blocking(move || fs4::statvfs(&root))
            .await
            .map_err(|e| Error::internal(e.to_string()))?
            .map_err(|e| io_error("querying free space", &e))?;
        let (capacity, available) = (stats.total_space(), stats.available_space());
        Ok(Some(StorageUsage {
            capacity,
            used: capacity.saturating_sub(available),
            available,
        }))
    }
}

/// Addon initialization: one store per addon instance.
pub fn init(reg: &mut Registrar) -> Result<()> {
    let cfg: Config = reg.config()?;
    let store = FsStore::open(&cfg.root, cfg.fsync)?;
    tracing::info!(name = %cfg.name, root = %cfg.root.display(), fsync = cfg.fsync, "filesystem blob store ready");
    reg.register_blob_store(&cfg.name, store)
}

/// Entry points for linking this addon into the server binary.
pub static ADDON: medicus_addon_sdk::abi::StaticAddon = static_addon!(
    name = "storage-fs",
    version = env!("CARGO_PKG_VERSION"),
    init = init
);

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn conformance() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsStore::open(dir.path(), false).unwrap();
        medicus_testkit::blob::suite(&store).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn writes_are_atomic_and_leave_no_temporaries() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsStore::open(dir.path(), true).unwrap();
        let key = BlobKey::parse("ab/cd/x.dcm").unwrap();
        // A failing stream must not leave a partial blob or a temporary file behind.
        let failing: ByteStream = Box::pin(futures::stream::iter(vec![
            Ok(bytes::Bytes::from_static(b"partial")),
            Err(Error::internal("source failed")),
        ]));
        assert!(store.put(&key, failing).await.is_err());
        assert!(store.stat(&key).await.unwrap().is_none());
        assert_eq!(
            std::fs::read_dir(dir.path().join(TMP_DIR)).unwrap().count(),
            0
        );
        store
            .put(&key, medicus_testkit::stream(b"complete", 3))
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("ab/cd/x.dcm")).unwrap(),
            b"complete"
        );
    }
}
