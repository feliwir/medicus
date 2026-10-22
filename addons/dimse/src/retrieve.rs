//! C-MOVE and C-GET: which instances an identifier selects (PS3.4 C.4.2.2.1).

use dicom_dictionary_std::uids;
use dicom_object::InMemDicomObject;
use medicus_addon_sdk::Host;
use medicus_core::capability::index::{InstanceLocation, ResourceUid};
use medicus_core::capability::{Endpoint, HostServices as _};
use medicus_core::query::{Attributes, Key, Matcher, Query};
use medicus_core::uid::{SeriesInstanceUid, SopInstanceUid, StudyInstanceUid};
use medicus_core::{Error, ErrorKind, QueryLevel, VR, tags};
use medicus_dimse::{Destination, Instance, Status};

/// The information models served.
pub(crate) const RETRIEVE_SOP_CLASSES: &[&str] = &[
    uids::PATIENT_ROOT_QUERY_RETRIEVE_INFORMATION_MODEL_MOVE,
    uids::STUDY_ROOT_QUERY_RETRIEVE_INFORMATION_MODEL_MOVE,
    uids::PATIENT_ROOT_QUERY_RETRIEVE_INFORMATION_MODEL_GET,
    uids::STUDY_ROOT_QUERY_RETRIEVE_INFORMATION_MODEL_GET,
];

/// Studies of a patient fetched per query page.
const PAGE: u32 = 500;

fn mismatch(comment: impl Into<String>) -> Status {
    Status::new(Status::IDENTIFIER_MISMATCH).with_comment(comment)
}

fn failure(e: &Error) -> Status {
    Status::new(Status::UNABLE_TO_PERFORM_SUBOPERATIONS).with_comment(e.message.clone())
}

/// The values of the unique key `tag` (a single value or a list of UIDs).
fn unique_values(
    identifier: &InMemDicomObject,
    tag: medicus_core::Tag,
) -> Result<Vec<String>, Status> {
    let raw = identifier
        .get(tag)
        .and_then(|e| e.to_str().ok())
        .ok_or_else(|| mismatch(format!("unique key {tag} is missing")))?;
    let values: Vec<String> = raw
        .split('\\')
        .map(|v| v.trim_matches(['\0', ' ']).to_owned())
        .filter(|v| !v.is_empty())
        .collect();
    if values.is_empty() {
        return Err(mismatch(format!("unique key {tag} is empty")));
    }
    Ok(values)
}

fn parse<T>(
    raw: &str,
    parse: impl Fn(&str) -> Result<T, medicus_core::uid::UidError>,
) -> Result<T, Status> {
    parse(raw).map_err(|e| mismatch(format!("{raw:?}: {e}")))
}

/// The stored instances an identifier selects, in index order.
pub(crate) async fn instances(
    host: &Host,
    sop_class: &str,
    identifier: &InMemDicomObject,
) -> Result<Vec<Instance>, Status> {
    let level = identifier
        .get(tags::QUERY_RETRIEVE_LEVEL)
        .and_then(|e| e.to_str().ok())
        .ok_or_else(|| mismatch("Query/Retrieve Level is missing"))?;
    let level = QueryLevel::from_dicom(&level).map_err(|e| mismatch(e.to_string()))?;
    let study_root = [
        uids::STUDY_ROOT_QUERY_RETRIEVE_INFORMATION_MODEL_MOVE,
        uids::STUDY_ROOT_QUERY_RETRIEVE_INFORMATION_MODEL_GET,
    ]
    .contains(&sop_class);
    if level == QueryLevel::Patient && study_root {
        return Err(mismatch("the Study Root model has no PATIENT level"));
    }
    let keys = unique_values(identifier, tags::unique_key(level))?;
    let mut resources = Vec::new();
    for key in &keys {
        match level {
            QueryLevel::Patient => resources.extend(patient_studies(host, key).await?),
            QueryLevel::Study => {
                resources.push(ResourceUid::Study(parse(key, StudyInstanceUid::parse)?));
            }
            QueryLevel::Series => {
                resources.push(ResourceUid::Series(parse(key, SeriesInstanceUid::parse)?));
            }
            QueryLevel::Instance => {
                resources.push(ResourceUid::Instance(parse(key, SopInstanceUid::parse)?));
            }
        }
    }
    let mut out = Vec::new();
    for resource in &resources {
        match host.locate(resource).await {
            Ok(found) => out.extend(found.iter().map(instance)),
            // A UID that matches nothing selects nothing.
            Err(e) if e.kind == ErrorKind::NotFound => {}
            Err(e) => return Err(failure(&e)),
        }
    }
    Ok(out)
}

fn instance(location: &InstanceLocation) -> Instance {
    Instance {
        sop_class: location.sop_class.as_str().to_owned(),
        sop_instance: location.instance.as_str().to_owned(),
        transfer_syntax: location.transfer_syntax.as_str().to_owned(),
    }
}

/// The studies of the patient with Patient ID `patient_id`.
async fn patient_studies(host: &Host, patient_id: &str) -> Result<Vec<ResourceUid>, Status> {
    let mut query = Query::new(QueryLevel::Study)
        .with_key(Key {
            tag: tags::PATIENT_ID,
            vr: VR::LO,
            matcher: Matcher::Single {
                value: patient_id.to_owned(),
            },
        })
        .with_key(Key::universal(tags::STUDY_INSTANCE_UID, VR::UI));
    query.limit = Some(PAGE);
    let mut studies = Vec::new();
    loop {
        let page = host.query(&query).await.map_err(|e| failure(&e))?;
        for row in &page.rows {
            let Some(uid) = Attributes::values(row, tags::STUDY_INSTANCE_UID)
                .and_then(|v| v.into_iter().next())
            else {
                continue;
            };
            studies.push(ResourceUid::Study(parse(&uid, StudyInstanceUid::parse)?));
        }
        if !page.has_more {
            return Ok(studies);
        }
        query.offset += u64::from(PAGE);
    }
}

/// The configured DIMSE peer with AE title `ae_title`.
pub(crate) async fn destination(host: &Host, ae_title: &str) -> Option<Destination> {
    let peers = match host.peers().await {
        Ok(peers) => peers,
        Err(e) => {
            tracing::warn!(error = %e, "cannot read the configured peers");
            return None;
        }
    };
    peers.into_iter().find_map(|peer| match peer.endpoint {
        Endpoint::Dimse {
            ae_title: peer_ae,
            host,
            port,
        } if peer_ae.as_str() == ae_title => Some(Destination {
            ae_title: peer_ae.as_str().to_owned(),
            address: format!("{host}:{port}"),
        }),
        Endpoint::Dimse { .. } => None,
    })
}
