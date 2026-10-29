//! The first-party storage and index addons, linked statically, pass their conformance suites
//! *through the ABI* (every call crosses the C vtables and, for the index, the JSON protocol).
#![allow(clippy::unwrap_used)]

use medicus_host::AddonLoader;
use serde_json::json;
use tokio::runtime::Handle;

#[tokio::test(flavor = "multi_thread")]
async fn storage_fs_over_abi() {
    let dir = tempfile::tempdir().unwrap();
    let config = json!({ "root": dir.path(), "name": "local", "fsync": false });
    let addon = AddonLoader::new(Handle::current())
        .load_static(&storage_fs::ADDON, "storage-fs", &config)
        .unwrap();
    assert_eq!(addon.blob_stores[0].name, "local");
    medicus_testkit::blob::suite(addon.blob_stores[0].value.as_ref()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn index_sqlite_over_abi() {
    let dir = tempfile::tempdir().unwrap();
    let config = json!({ "path": dir.path().join("index.sqlite") });
    let addon = AddonLoader::new(Handle::current())
        .load_static(&index_sqlite::ADDON, "index-sqlite", &config)
        .unwrap();
    assert_eq!(addon.index_stores[0].name, "sqlite");
    medicus_testkit::index::suite(addon.index_stores[0].value.as_ref()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_config_fails_cleanly() {
    let err = AddonLoader::new(Handle::current())
        .load_static(&index_sqlite::ADDON, "index-sqlite", &json!({ "pth": "x" }))
        .unwrap_err();
    assert!(
        matches!(err, medicus_host::LoadError::InitFailed { .. }),
        "{err}"
    );
    // The addon's reason reaches the host.
    assert!(err.to_string().contains("pth"), "{err}");
}
