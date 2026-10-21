//! `BlobStore` conformance suite.

use medicus_core::capability::BlobStore;
use medicus_core::{BlobKey, ErrorKind};

use crate::{payload, read_all, stream};

fn key(s: &str) -> BlobKey {
    BlobKey::parse(s).unwrap()
}

/// Runs all scenarios. Uses keys below `conformance/`; the store should start without them.
pub async fn suite(store: &dyn BlobStore) {
    roundtrip_large(store).await;
    ranges(store).await;
    overwrite(store).await;
    missing_and_delete(store).await;
    empty_blob(store).await;
    usage(store).await;
}

/// 5 MiB in 100 KiB chunks: many chunks in both directions.
async fn roundtrip_large(store: &dyn BlobStore) {
    let data = payload(5 * 1024 * 1024 + 17);
    let k = key("conformance/large.dcm");
    let meta = store.put(&k, stream(&data, 100 * 1024)).await.unwrap();
    assert_eq!(meta.size, data.len() as u64);
    assert_eq!(read_all(store.get(&k, None).await.unwrap()).await, data);
    assert_eq!(
        store.stat(&k).await.unwrap().unwrap().size,
        data.len() as u64
    );
    store.delete(&k).await.unwrap();
}

async fn ranges(store: &dyn BlobStore) {
    let data = payload(300_000);
    let k = key("conformance/ranges.dcm");
    store.put(&k, stream(&data, 7_000)).await.unwrap();
    let r = read_all(store.get(&k, Some(1000..200_000)).await.unwrap()).await;
    assert_eq!(r, data[1000..200_000]);
    let tail = read_all(store.get(&k, Some(299_990..300_000)).await.unwrap()).await;
    assert_eq!(tail, data[299_990..]);
    assert!(
        read_all(store.get(&k, Some(5..5)).await.unwrap())
            .await
            .is_empty()
    );
    #[allow(clippy::reversed_empty_ranges)] // deliberately invalid
    let reversed = store.get(&k, Some(10..5)).await.err().unwrap();
    assert_eq!(reversed.kind, ErrorKind::InvalidArgument);
    let beyond = store.get(&k, Some(0..300_001)).await.err().unwrap();
    assert_eq!(beyond.kind, ErrorKind::InvalidArgument);
    store.delete(&k).await.unwrap();
}

async fn overwrite(store: &dyn BlobStore) {
    let k = key("conformance/overwrite.dcm");
    store.put(&k, stream(&payload(1000), 100)).await.unwrap();
    store.put(&k, stream(b"short", 2)).await.unwrap();
    assert_eq!(read_all(store.get(&k, None).await.unwrap()).await, b"short");
    assert_eq!(store.stat(&k).await.unwrap().unwrap().size, 5);
    store.delete(&k).await.unwrap();
}

async fn missing_and_delete(store: &dyn BlobStore) {
    let k = key("conformance/missing/blob.dcm");
    assert_eq!(
        store.get(&k, None).await.err().unwrap().kind,
        ErrorKind::NotFound
    );
    assert!(store.stat(&k).await.unwrap().is_none());
    store.delete(&k).await.unwrap(); // idempotent
}

async fn empty_blob(store: &dyn BlobStore) {
    let k = key("conformance/empty");
    assert_eq!(store.put(&k, stream(&[], 1)).await.unwrap().size, 0);
    assert!(
        read_all(store.get(&k, None).await.unwrap())
            .await
            .is_empty()
    );
    assert_eq!(store.stat(&k).await.unwrap().unwrap().size, 0);
    store.delete(&k).await.unwrap();
}

/// `None` (no physical limit) is legitimate; if reported, numbers must be consistent.
async fn usage(store: &dyn BlobStore) {
    if let Some(u) = store.usage().await.unwrap() {
        assert!(u.used <= u.capacity && u.available <= u.capacity, "{u:?}");
    }
}
