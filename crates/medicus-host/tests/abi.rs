//! ABI test-suite: loads `addons/test-addon` statically and as a shared library and exercises
//! every capability call across the boundary.

// Fake FFI entry points are declared below; helpers may unwrap like tests do.
#![allow(unsafe_code, clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use bytes::Bytes;
use futures::TryStreamExt as _;
use medicus_core::capability::{BlobStore, ByteStream};
use medicus_core::event::{Event, EventKind, Origin};
use medicus_core::{BlobKey, ErrorKind};
use medicus_host::{AddonLoader, LoadError, LoadedAddon, StaticAddon};
use serde_json::json;
use tokio::runtime::Handle;

fn key(s: &str) -> BlobKey {
    BlobKey::parse(s).unwrap()
}

fn data(len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| u8::try_from(i * 7 % 251).unwrap())
        .collect()
}

/// A stream of `bytes` in chunks of `chunk` bytes.
fn stream(bytes: &[u8], chunk: usize) -> ByteStream {
    let chunks: Vec<medicus_core::Result<Bytes>> = bytes
        .chunks(chunk)
        .map(|c| Ok(Bytes::copy_from_slice(c)))
        .collect();
    Box::pin(futures::stream::iter(chunks))
}

async fn read_all(s: ByteStream) -> Vec<u8> {
    let parts: Vec<Bytes> = s.try_collect().await.unwrap();
    parts.concat()
}

fn load_static(config: &serde_json::Value) -> Result<LoadedAddon, LoadError> {
    AddonLoader::new(Handle::current()).load_static(
        &test_addon::MEDICUS_ADDON,
        "test-addon",
        config,
    )
}

fn store(addon: &LoadedAddon) -> Arc<dyn BlobStore> {
    assert_eq!(addon.blob_stores.len(), 1);
    assert_eq!(addon.blob_stores[0].name, "memory");
    addon.blob_stores[0].value.clone()
}

async fn exercise_blob_store(store: &dyn BlobStore) {
    medicus_testkit::blob::suite(store).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn static_blob_store_roundtrip() {
    let addon = load_static(&json!(null)).unwrap();
    assert_eq!(addon.info.name, "test-addon");
    exercise_blob_store(store(&addon).as_ref()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_calls() {
    let addon = load_static(&json!(null)).unwrap();
    let s = store(&addon);
    let tasks: Vec<_> = (0..32)
        .map(|i| {
            let s = s.clone();
            tokio::spawn(async move {
                let k = key(&format!("c/{i}"));
                let payload = data(10_000 + i);
                s.put(&k, stream(&payload, 999)).await.unwrap();
                assert_eq!(read_all(s.get(&k, None).await.unwrap()).await, payload);
            })
        })
        .collect();
    for t in tasks {
        t.await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn panics_are_isolated() {
    let addon = load_static(&json!({ "panic_prefix": "boom" })).unwrap();
    let s = store(&addon);
    let err = s.put(&key("boom/1"), stream(b"x", 1)).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Panicked);
    let err = s.get(&key("boom/1"), None).await.err().unwrap();
    assert_eq!(err.kind, ErrorKind::Panicked);
    // The addon keeps working afterwards.
    s.put(&key("ok/1"), stream(b"fine", 2)).await.unwrap();
    assert_eq!(
        read_all(s.get(&key("ok/1"), None).await.unwrap()).await,
        b"fine"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn event_hooks() {
    let addon = load_static(&json!(null)).unwrap();
    assert_eq!(addon.event_hooks.len(), 1);
    let hook = addon.event_hooks[0].value.clone();
    let filter = hook.filter();
    assert!(filter.kinds.contains(&EventKind::InstanceStored));

    let origin = Origin::Internal {
        component: "test".into(),
    };
    hook.on_event(&Event::new(EventKind::InstanceStored, origin.clone()))
        .await
        .unwrap();
    let err = hook
        .on_event(&Event::new(EventKind::Other("test.fail".into()), origin))
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Unavailable);
    assert_eq!(err.message, "refusing test.fail");
}

#[tokio::test(flavor = "multi_thread")]
async fn init_failure_and_bad_config() {
    assert!(matches!(
        load_static(&json!({ "fail_init": true })),
        Err(LoadError::InitFailed(_))
    ));
    assert!(matches!(
        load_static(&json!({ "unknown": 1 })),
        Err(LoadError::InitFailed(_))
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn abi_mismatch_is_rejected_before_init() {
    unsafe extern "C" fn future_abi() -> u32 {
        0x0001_0000
    }
    unsafe extern "C" fn must_not_run(
        _: *const medicus_abi::ffi::MdHostApi,
    ) -> *const medicus_abi::ffi::MdAddonInfo {
        panic!("init must not be called on ABI mismatch");
    }
    let entry = StaticAddon {
        abi_version: future_abi,
        init: must_not_run,
    };
    let err = AddonLoader::new(Handle::current()).load_static(&entry, "future", &json!(null));
    match err {
        Err(LoadError::AbiMismatch { found, .. }) => assert_eq!(found, "1.0"),
        other => panic!("expected AbiMismatch, got {other:?}"),
    }
}

/// Builds the test addon as a shared library and returns its path.
fn build_cdylib() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace = manifest.parent().unwrap().parent().unwrap();
    let status = Command::new(env!("CARGO"))
        .args(["build", "-p", "test-addon", "--lib"])
        .current_dir(workspace)
        .status()
        .unwrap();
    assert!(status.success(), "building the test addon failed");
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map_or_else(|| workspace.join("target"), PathBuf::from);
    target
        .join("debug")
        .join(libloading::library_filename("test_addon"))
}

#[tokio::test(flavor = "multi_thread")]
async fn dynamic_library() {
    let path = build_cdylib();
    let loader = AddonLoader::new(Handle::current());
    let addon = loader
        .load_library(&path, "test-addon", &json!({ "panic_prefix": "boom" }))
        .unwrap();
    assert_eq!(
        addon.info.target,
        format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
    );
    let s = store(&addon);
    exercise_blob_store(s.as_ref()).await;
    // Panics inside the separately compiled library are isolated as well.
    let err = s.put(&key("boom/x"), stream(b"x", 1)).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Panicked);

    let missing = loader.load_library(&path.with_file_name("does-not-exist.so"), "x", &json!(null));
    assert!(matches!(missing, Err(LoadError::Library { .. })));
}

/// The C example addon (compiled by `addons/example-c/build.rs` with the platform C compiler)
/// passes the same scenarios: the ABI is usable without Rust.
#[tokio::test(flavor = "multi_thread")]
async fn c_addon() {
    let loader = AddonLoader::new(Handle::current());
    let path = std::path::Path::new(example_c::LIBRARY_PATH);
    let addon = loader
        .load_library(path, "example-c", &json!(null))
        .unwrap();
    assert_eq!(addon.info.name, "example-c");
    exercise_blob_store(store(&addon).as_ref()).await;

    assert_eq!(addon.event_hooks.len(), 1);
    let hook = addon.event_hooks[0].value.clone();
    assert!(
        hook.filter().kinds.is_empty(),
        "C hook subscribes to all events"
    );
    let origin = Origin::Internal {
        component: "test".into(),
    };
    hook.on_event(&Event::new(EventKind::StudyStable, origin.clone()))
        .await
        .unwrap();
    let err = hook
        .on_event(&Event::new(EventKind::Other("test.fail".into()), origin))
        .await
        .unwrap_err();
    assert_eq!(
        (err.kind, err.message.as_str()),
        (ErrorKind::Unavailable, "refusing test.fail")
    );
}
