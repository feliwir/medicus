//! The `dicomweb` addon over real HTTP: STOW-RS, QIDO-RS and WADO-RS, nodes and permissions.

#![cfg(feature = "builtin-dicomweb")]

use std::path::Path;

use bytes::Bytes;
use dicom_dictionary_std::tags;
use medicus_core::TagExt as _;
use medicus_core::event::{EventKind, Origin};
use medicus_testkit::dicom::InstanceBuilder;
use serde_json::{Value, json};

use crate::Node;
use crate::config::{AddonEntry, Config};

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A node with the `dicomweb` addon on `port`, plus the nodes, roles and principals of `toml`.
async fn node(data_dir: &Path, port: u16, toml: &str, addon: Value) -> Node {
    use figment::providers::{Format, Serialized, Toml};
    let mut cfg: Config = figment::Figment::from(Serialized::defaults(Config::default()))
        .merge(Toml::string(toml))
        .extract()
        .unwrap();
    cfg.server.data_dir = data_dir.to_owned();
    let mut config = json!({ "listen": format!("127.0.0.1:{port}") });
    if let (Some(c), Some(extra)) = (config.as_object_mut(), addon.as_object()) {
        c.extend(extra.clone());
    }
    cfg.addons.entries.insert(
        "dicomweb".into(),
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

/// A `multipart/related; type="application/dicom"` body of `files`.
fn stow_body(files: &[Vec<u8>]) -> (String, Vec<u8>) {
    let boundary = "test-boundary-42";
    let mut body = Vec::new();
    for file in files {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Type: application/dicom\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(file);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    (
        format!("multipart/related; type=\"application/dicom\"; boundary={boundary}"),
        body,
    )
}

/// The parts of a `multipart/related` response.
async fn parts(response: reqwest::Response) -> Vec<(String, Bytes)> {
    let content_type = response.headers()["content-type"]
        .to_str()
        .unwrap()
        .to_owned();
    let mime: mime::Mime = content_type.parse().unwrap();
    let boundary = mime.get_param("boundary").unwrap().as_str().to_owned();
    let mut multipart = multer::Multipart::new(response.bytes_stream(), boundary);
    let mut out = Vec::new();
    while let Some(field) = multipart.next_field().await.unwrap() {
        let ct = field
            .content_type()
            .map(ToString::to_string)
            .unwrap_or_default();
        out.push((ct, field.bytes().await.unwrap()));
    }
    out
}

fn values(v: &Value, tag: dicom_core::Tag) -> Vec<String> {
    v[tag.json_key()]["Value"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|x| match x {
                    Value::String(s) => s.clone(),
                    Value::Object(o) => o["Alphabetic"].as_str().unwrap_or_default().to_owned(),
                    other => other.to_string(),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Stores three instances (two studies) through STOW-RS; returns the files.
async fn stow_three(client: &reqwest::Client, base: &str) -> Vec<Vec<u8>> {
    let files: Vec<Vec<u8>> = [
        ("2.1", "2.1.1", "2.1.1.1", "Doe^John", "CT"),
        ("2.1", "2.1.1", "2.1.1.2", "Doe^John", "CT"),
        ("2.2", "2.2.1", "2.2.1.1", "Roe^Jane", "MR"),
    ]
    .iter()
    .map(|(st, se, sop, name, modality)| {
        InstanceBuilder::new(st, se, sop)
            .attr(tags::PATIENT_NAME, dicom_core::VR::PN, name)
            .attr(tags::PATIENT_ID, dicom_core::VR::LO, &name.to_lowercase())
            .attr(tags::MODALITY, dicom_core::VR::CS, modality)
            .pixel_data(100_000)
            .build()
    })
    .collect();

    // STOW-RS
    let (content_type, body) = stow_body(&files);
    let response = client
        .post(format!("{base}/studies"))
        .header("content-type", content_type)
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let stored: Value = response.json().await.unwrap();
    assert_eq!(
        stored[tags::REFERENCED_SOP_SEQUENCE.json_key()]["Value"]
            .as_array()
            .unwrap()
            .len(),
        3
    );

    files
}

#[tokio::test(flavor = "multi_thread")]
async fn stow_qido_wado() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let node = node(dir.path(), port, "", json!({})).await;
    let base = format!("http://127.0.0.1:{port}/rs");
    let client = reqwest::Client::new();

    let files = stow_three(&client, &base).await;

    // QIDO-RS at every level, with matching.
    let get = |path: String| {
        let client = client.clone();
        async move {
            let r = client.get(path).send().await.unwrap();
            assert_eq!(r.status(), 200);
            assert_eq!(r.headers()["content-type"], "application/dicom+json");
            r.json::<Value>().await.unwrap()
        }
    };
    let studies = get(format!("{base}/studies")).await;
    assert_eq!(studies.as_array().unwrap().len(), 2);
    let doe = get(format!("{base}/studies?PatientName=Doe*")).await;
    assert_eq!(values(&doe[0], tags::STUDY_INSTANCE_UID), ["2.1"]);
    assert_eq!(values(&doe[0], tags::MODALITIES_IN_STUDY), ["CT"]);
    assert_eq!(
        values(&doe[0], tags::NUMBER_OF_STUDY_RELATED_INSTANCES),
        ["2"]
    );
    assert_eq!(
        values(&doe[0], tags::RETRIEVE_URL),
        [format!("{base}/studies/2.1")]
    );
    // Series-level keys do not constrain a study search (like C-FIND).
    let ignored = get(format!("{base}/studies?Modality=MR")).await;
    assert_eq!(ignored.as_array().unwrap().len(), 2);
    let series = get(format!("{base}/studies/2.1/series")).await;
    assert_eq!(values(&series[0], tags::SERIES_INSTANCE_UID), ["2.1.1"]);
    let instances = get(format!("{base}/studies/2.1/series/2.1.1/instances?limit=1")).await;
    assert_eq!(instances.as_array().unwrap().len(), 1);
    let all = get(format!(
        "{base}/instances?Modality=MR&includefield=PatientName"
    ))
    .await;
    assert_eq!(values(&all[0], tags::SOP_INSTANCE_UID), ["2.2.1.1"]);
    assert_eq!(values(&all[0], tags::PATIENT_NAME), ["Roe^Jane"]);
    // The same through the node's own path.
    let by_node = get(format!("http://127.0.0.1:{port}/nodes/medicus/rs/studies")).await;
    assert_eq!(by_node.as_array().unwrap().len(), 2);

    // WADO-RS: instances as stored, byte for byte.
    let response = client
        .get(format!("{base}/studies/2.1"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let retrieved = parts(response).await;
    assert_eq!(retrieved.len(), 2);
    assert!(retrieved[0].0.starts_with("application/dicom"));
    let mut bodies: Vec<_> = retrieved.iter().map(|(_, b)| b.to_vec()).collect();
    bodies.sort();
    let mut expected = files[..2].to_vec();
    expected.sort();
    assert_eq!(bodies, expected);

    // Another transfer syntax than the stored one: 406 (no transcoding).
    let response = client
        .get(format!("{base}/studies/2.2"))
        .header(
            "accept",
            "multipart/related; type=\"application/dicom\"; transfer-syntax=1.2.840.10008.1.2.4.50",
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 406);

    // Metadata from the stored headers.
    let metadata = get(format!("{base}/studies/2.2/series/2.2.1/metadata")).await;
    assert_eq!(values(&metadata[0], tags::PATIENT_NAME), ["Roe^Jane"]);
    assert!(metadata[0].get(tags::PIXEL_DATA.json_key()).is_none());

    let missing = client
        .get(format!("{base}/studies/9.9"))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 404);

    // STOW-RS goes through the same ingest as DIMSE: same events, DICOMweb origin.
    let events = node.archive.index.events_after(0, 100).await.unwrap();
    let stored: Vec<_> = events
        .iter()
        .filter(|e| e.kind == EventKind::InstanceStored)
        .collect();
    assert_eq!(stored.len(), 3);
    assert!(stored.iter().all(
        |e| matches!(e.origin, Origin::Dicomweb { .. }) && e.node.as_deref() == Some("MEDICUS")
    ));
    node.stop_services().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn rendered_resources() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let node = node(dir.path(), port, "", json!({})).await;
    let base = format!("http://127.0.0.1:{port}/rs");
    let client = reqwest::Client::new();

    let file = InstanceBuilder::new("3.1", "3.1.1", "3.1.1.1")
        .image(16, 32)
        .build();
    let (content_type, body) = stow_body(&[file]);
    let response = client
        .post(format!("{base}/studies"))
        .header("content-type", content_type)
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);

    let get = |path: &str, accept: &str| {
        let request = client.get(format!("{base}{path}")).header("accept", accept);
        async move { request.send().await.unwrap() }
    };
    let instance = "/studies/3.1/series/3.1.1/instances/3.1.1.1";

    // JPEG by default, at every level, and for thumbnails.
    for path in [
        "/studies/3.1/rendered".to_owned(),
        "/studies/3.1/series/3.1.1/thumbnail".to_owned(),
        format!("{instance}/rendered"),
        format!("{instance}/frames/1/rendered"),
    ] {
        let response = get(&path, "*/*").await;
        assert_eq!(response.status(), 200, "{path}");
        assert_eq!(response.headers()["content-type"], "image/jpeg");
        assert_eq!(&response.bytes().await.unwrap()[..2], [0xFF, 0xD8]);
    }

    // PNG scaled to the viewport: the IHDR chunk holds width and height.
    let response = get(
        &format!("{instance}/rendered?viewport=64,64&window=125,250,linear&annotation=patient"),
        "image/png",
    )
    .await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "image/png");
    assert!(response.headers().contains_key("warning"));
    let png = response.bytes().await.unwrap();
    assert_eq!(&png[1..4], b"PNG");
    assert_eq!(&png[16..24], [0, 0, 0, 64, 0, 0, 0, 64]);

    assert_eq!(
        get(&format!("{instance}/rendered"), "video/mp4")
            .await
            .status(),
        406
    );
    assert_eq!(
        get(&format!("{instance}/frames/2/rendered"), "*/*")
            .await
            .status(),
        404
    );
    assert_eq!(
        get(&format!("{instance}/rendered?quality=0"), "*/*")
            .await
            .status(),
        400
    );
    node.stop_services().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn nodes_and_permissions() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let config = r#"
        [partitions.tenant-a]
        [partitions.tenant-b]
        [nodes.A]
        partition = "tenant-a"
        dicomweb = { path = "a" }
        [nodes.B]
        partition = "tenant-b"
        dicomweb = { path = "b" }
        [roles.uploader]
        grants = [{ nodes = ["A"], permissions = ["store"] }]
        [roles.viewer]
        grants = [{ nodes = ["A", "B"], permissions = ["query", "retrieve"] }]
        [principals.robot]
        roles = ["uploader"]
        [principals.alice]
        roles = ["viewer"]
    "#;
    let node = node(dir.path(), port, config, json!({ "user_header": "x-user" })).await;
    let client = reqwest::Client::new();
    let url = |path: &str| format!("http://127.0.0.1:{port}/nodes/{path}");
    let file = InstanceBuilder::new("3.1", "3.1.1", "3.1.1.1").build();
    let (content_type, body) = stow_body(&[file]);
    let stow = |user: &'static str, node: &'static str| {
        client
            .post(url(&format!("{node}/rs/studies")))
            .header("content-type", content_type.clone())
            .header("x-user", user)
            .body(body.clone())
            .send()
    };
    assert_eq!(
        stow("alice", "a").await.unwrap().status(),
        403,
        "viewers may not store"
    );
    assert_eq!(
        stow("robot", "b").await.unwrap().status(),
        403,
        "robot may store into A only"
    );
    assert_eq!(stow("robot", "a").await.unwrap().status(), 200);

    let search = |user: &'static str, node: &'static str| {
        client
            .get(url(&format!("{node}/rs/studies")))
            .header("x-user", user)
            .send()
    };
    let in_a: Value = search("alice", "a").await.unwrap().json().await.unwrap();
    assert_eq!(in_a.as_array().unwrap().len(), 1);
    let in_b: Value = search("alice", "b").await.unwrap().json().await.unwrap();
    assert!(
        in_b.as_array().unwrap().is_empty(),
        "partitions are separate"
    );
    assert_eq!(search("robot", "a").await.unwrap().status(), 403);
    // Anonymous callers have no roles.
    let anonymous = client.get(url("a/rs/studies")).send().await.unwrap();
    assert_eq!(anonymous.status(), 403);
    // No default node with two nodes, and unknown nodes are 404.
    let response = client
        .get(format!("http://127.0.0.1:{port}/rs/studies"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    assert_eq!(
        client
            .get(url("zzz/rs/studies"))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    node.stop_services().await;
}

/// STOW-RS and C-STORE yield equivalent `instance.stored` events, delivered to a webhook.
#[cfg(all(feature = "builtin-dimse", feature = "builtin-events-webhook"))]
#[tokio::test(flavor = "multi_thread")]
async fn stow_and_c_store_produce_equivalent_events() {
    use std::sync::{Arc, Mutex};
    let received: Arc<Mutex<Vec<Value>>> = Arc::default();
    let record = received.clone();
    let app = axum::Router::new().route(
        "/hook",
        axum::routing::post(move |axum::Json(event): axum::Json<Value>| {
            let record = record.clone();
            async move {
                record.lock().unwrap().push(event);
                axum::http::StatusCode::NO_CONTENT
            }
        }),
    );
    let hook_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hook_url = format!("http://{}/hook", hook_listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(hook_listener, app).await.unwrap() });

    let dir = tempfile::tempdir().unwrap();
    let (web_port, dimse_port) = (free_port(), free_port());
    let toml = format!(
        "[addons.dimse]\nconfig = {{ listen = \"127.0.0.1:{dimse_port}\" }}\n\
         [addons.events-webhook]\nconfig = {{ hooks = [{{ name = \"test\", url = \"{hook_url}\", kinds = [\"instance.stored\"] }}] }}\n"
    );
    let node = node(dir.path(), web_port, &toml, json!({})).await;
    let stop = tokio_util::sync::CancellationToken::new();
    let delivery = crate::events::spawn(&node.archive, node.caps.event_hooks.clone(), &stop);

    // Let the new subscriber start at the current end of the outbox before storing.
    for _ in 0..500 {
        if node
            .archive
            .index
            .cursor("events-webhook/test")
            .await
            .unwrap()
            .is_some()
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let via_web = InstanceBuilder::new("4.1", "4.1.1", "4.1.1.1").build();
    let (content_type, body) = stow_body(&[via_web]);
    let response = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{web_port}/rs/studies"))
        .header("content-type", content_type)
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);

    let via_dimse = InstanceBuilder::new("4.1", "4.1.1", "4.1.1.2").build();
    let options =
        medicus_dimse::ScuOptions::new("MODALITY", "MEDICUS", format!("127.0.0.1:{dimse_port}"));
    let instance = medicus_dimse::Instance {
        sop_class: dicom_dictionary_std::uids::CT_IMAGE_STORAGE.into(),
        sop_instance: "4.1.1.2".into(),
        transfer_syntax: dicom_dictionary_std::uids::EXPLICIT_VR_LITTLE_ENDIAN.into(),
    };
    let mut scu = medicus_dimse::StoreScu::connect(&options, [&instance])
        .await
        .unwrap();
    let status = scu
        .store(&instance, medicus_testkit::stream(&via_dimse, 4096), None)
        .await
        .unwrap();
    assert!(status.is_success());
    scu.release().await.unwrap();

    for _ in 0..500 {
        if received.lock().unwrap().len() >= 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let events = received.lock().unwrap().clone();
    assert_eq!(events.len(), 2, "{events:?}");
    let [web, dimse] = [&events[0], &events[1]];
    // Same kind, partition, node and resource shape; only the origin differs.
    for key in ["kind", "partition", "node"] {
        assert_eq!(web[key], dimse[key], "{key}");
    }
    assert_eq!(web["origin"]["type"], "dicomweb");
    assert_eq!(dimse["origin"]["type"], "dimse");
    assert_eq!(dimse["origin"]["calling_ae"], "MODALITY");
    let keys = |e: &Value| {
        let mut k: Vec<String> = e["resource"].as_object().unwrap().keys().cloned().collect();
        k.sort();
        k
    };
    assert_eq!(keys(web), keys(dimse));
    stop.cancel();
    let mut delivery = delivery;
    while delivery.join_next().await.is_some() {}
    node.stop_services().await;
}
