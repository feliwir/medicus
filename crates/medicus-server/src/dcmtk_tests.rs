//! Interoperability with DCMTK: the `dimse` addon against `echoscu` and `storescu`.
//!
//! The DCMTK tools are taken from `MEDICUS_DCMTK_BIN` (required to exist if set), or from
//! `refs/dcmtk-install/bin` in the workspace. Without them the tests are skipped (with a note on
//! stderr).

#![cfg(feature = "builtin-dimse")]

use std::path::{Path, PathBuf};
use std::process::Command;

use dicom_dictionary_std::uids;
use medicus_core::event::{EventKind, Origin};
use medicus_testkit::dicom::InstanceBuilder;
use serde_json::json;

use crate::Node;
use crate::config::{AddonEntry, Config};

fn dcmtk_bin() -> Option<PathBuf> {
    let has_tools = |dir: &Path| dir.join("storescu").exists() || dir.join("storescu.exe").exists();
    if let Some(dir) = std::env::var_os("MEDICUS_DCMTK_BIN").map(PathBuf::from) {
        // Explicitly configured (as in CI): missing tools are an error, not a skip.
        assert!(
            has_tools(&dir),
            "MEDICUS_DCMTK_BIN={} has no storescu",
            dir.display()
        );
        return Some(dir);
    }
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../refs/dcmtk-install/bin");
    if has_tools(&dir) {
        Some(dir)
    } else {
        eprintln!("DCMTK not found (set MEDICUS_DCMTK_BIN); skipping");
        None
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

async fn node_with_dimse(data_dir: &Path, port: u16, extra: serde_json::Value) -> Node {
    let mut cfg = Config::default();
    cfg.server.data_dir = data_dir.to_owned();
    let mut config = json!({ "ae_title": "MEDICUS", "listen": format!("127.0.0.1:{port}") });
    if let (Some(config), Some(extra)) = (config.as_object_mut(), extra.as_object()) {
        config.extend(extra.clone());
    }
    cfg.addons.entries.insert(
        "dimse".into(),
        AddonEntry {
            enabled: true,
            library: None,
            sha256: None,
            config,
        },
    );
    let node = Node::open(cfg).await.unwrap();
    node.start_services().await.unwrap();
    node
}

/// Runs a DCMTK tool off the async runtime and returns whether it succeeded, printing its output
/// on failure.
async fn run(bin: &Path, tool: &str, args: Vec<String>) -> bool {
    let program = bin.join(tool);
    let output = tokio::task::spawn_blocking(move || Command::new(program).args(args).output())
        .await
        .unwrap()
        .unwrap();
    if !output.status.success() {
        eprintln!(
            "{tool} failed ({}):\n{}{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    output.status.success()
}

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_owned()).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn echoscu_and_storescu() {
    let Some(bin) = dcmtk_bin() else { return };
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let node = node_with_dimse(&dir.path().join("data"), port, json!({})).await;
    let port_arg = port.to_string();

    assert!(
        run(
            &bin,
            "echoscu",
            args(&[
                "-aet",
                "DCMTKSCU",
                "-aec",
                "MEDICUS",
                "127.0.0.1",
                &port_arg
            ])
        )
        .await
    );

    // Three instances in two transfer syntaxes, one large enough to span many PDUs.
    let files = dir.path().join("files");
    std::fs::create_dir(&files).unwrap();
    let instances = [
        InstanceBuilder::new("1.2.3", "1.2.3.1", "1.2.3.1.1"),
        InstanceBuilder::new("1.2.3", "1.2.3.1", "1.2.3.1.2")
            .transfer_syntax(uids::IMPLICIT_VR_LITTLE_ENDIAN),
        InstanceBuilder::new("1.2.3", "1.2.3.2", "1.2.3.2.1").pixel_data(3_000_000),
    ];
    for (i, b) in instances.iter().enumerate() {
        std::fs::write(files.join(format!("{i}.dcm")), b.build()).unwrap();
    }
    let mut store = args(&["-aet", "DCMTKSCU", "-aec", "MEDICUS", "+sd", "+r"]);
    store.extend([
        "127.0.0.1".to_owned(),
        port_arg.clone(),
        files.display().to_string(),
    ]);
    assert!(run(&bin, "storescu", store).await);

    let events = node.archive.index.events_after(0, 100).await.unwrap();
    let stored: Vec<_> = events
        .iter()
        .filter(|e| e.kind == EventKind::InstanceStored)
        .collect();
    assert_eq!(stored.len(), 3);
    for event in stored {
        let Origin::Dimse {
            calling_ae,
            called_ae,
            association,
            ..
        } = &event.origin
        else {
            panic!("unexpected origin {:?}", event.origin);
        };
        assert_eq!(calling_ae.as_str(), "DCMTKSCU");
        assert_eq!(called_ae.as_str(), "MEDICUS");
        assert!(association.is_some());
    }

    // The stored files carry the data sets exactly as DCMTK sent them; check one end to end.
    let sop: medicus_core::uid::SopInstanceUid = "1.2.3.2.1".parse().unwrap();
    let (_, stream) = node.archive.retrieve(&sop).await.unwrap();
    let stored = medicus_testkit::read_all(stream).await;
    let obj = dicom_object::from_reader(&stored[128..]).unwrap();
    assert_eq!(obj.meta().media_storage_sop_instance_uid(), "1.2.3.2.1");
    assert_eq!(
        obj.meta().source_application_entity_title(),
        Some("DCMTKSCU")
    );
    assert_eq!(
        obj.element(dicom_dictionary_std::tags::PIXEL_DATA)
            .unwrap()
            .to_bytes()
            .unwrap()
            .len(),
        3_000_000
    );
    node.stop_services().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_calling_ae_is_rejected() {
    let Some(bin) = dcmtk_bin() else { return };
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let node = node_with_dimse(
        dir.path(),
        port,
        json!({ "allowed_calling": ["MODALITY1"] }),
    )
    .await;
    let port_arg = port.to_string();
    assert!(
        !run(
            &bin,
            "echoscu",
            args(&[
                "-aet",
                "INTRUDER",
                "-aec",
                "MEDICUS",
                "127.0.0.1",
                &port_arg
            ])
        )
        .await
    );
    assert!(
        run(
            &bin,
            "echoscu",
            args(&[
                "-aet",
                "MODALITY1",
                "-aec",
                "MEDICUS",
                "127.0.0.1",
                &port_arg
            ])
        )
        .await
    );
    node.stop_services().await;
}
