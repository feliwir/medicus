//! STOW-RS (PS3.18 §10.5): stores the instances of a `multipart/related` request.
//!
//! Each part is streamed into the host's ingest as it arrives, so request size is not limited
//! by memory. The response lists stored and failed instances (PS3.18 Table 10.5.3-1).

use axum::Json;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures::StreamExt as _;
use medicus_core::capability::{ByteStream, HostServices as _, Ingested};
use medicus_core::{Error, ErrorKind, Permission, TagExt as _, tags};
use serde_json::{Value, json};

use crate::error::{WebError, WebResult};
use crate::multipart;
use crate::web::{Request, State as Web};

/// Failure Reason (0008,1197) values (PS3.18 Table 10.5.3-2 and PS3.4 B.2.3).
const OUT_OF_RESOURCES: u16 = 0xA700;
const CANNOT_UNDERSTAND: u16 = 0xC000;
const STUDY_MISMATCH: u16 = 0xA900;
const NOT_AUTHORIZED: u16 = 0x0124;

fn failure_reason(e: &Error) -> u16 {
    match e.kind {
        ErrorKind::ResourceExhausted | ErrorKind::Unavailable => OUT_OF_RESOURCES,
        ErrorKind::PermissionDenied => NOT_AUTHORIZED,
        _ => CANNOT_UNDERSTAND,
    }
}

fn uid(value: &str) -> Value {
    json!({ "vr": "UI", "Value": [value] })
}

pub(crate) async fn store(
    State(web): State<Web>,
    req: Request,
    headers: HeaderMap,
    body: Body,
) -> WebResult<Response> {
    // Refuse before reading a byte if the caller may not store into the node at all.
    web.host.authorize(&req.context, Permission::Store).await?;
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let boundary = multipart::boundary(content_type).ok_or_else(|| {
        WebError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "expected multipart/related with a boundary",
        )
    })?;
    let expected_study = req.params.get("study").cloned();
    let mut parts = multer::Multipart::new(body.into_data_stream(), boundary);
    let mut stored: Vec<Ingested> = Vec::new();
    let mut failed: Vec<Value> = Vec::new();
    while let Some(field) = parts
        .next_field()
        .await
        .map_err(|e| WebError::bad_request(format!("malformed multipart body: {e}")))?
    {
        let media = field.content_type().map_or_else(
            || "application/dicom".to_owned(),
            |m| m.essence_str().to_owned(),
        );
        if media != "application/dicom" {
            return Err(WebError::new(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                format!("part of type {media}: only application/dicom is supported"),
            ));
        }
        let data: ByteStream = Box::pin(field.map(|chunk| {
            chunk.map_err(|e| Error::invalid_argument(format!("reading the request: {e}")))
        }));
        match web.host.ingest(&req.context, data).await {
            Ok(ingested) => {
                if expected_study
                    .as_deref()
                    .is_some_and(|s| s != ingested.study.as_str())
                {
                    // Stored already: the request's study only scopes the response here.
                    tracing::warn!(instance = %ingested.instance, "STOW-RS instance belongs to another study than the request path");
                    failed.push(json!({
                        tags::REFERENCED_SOP_CLASS_UID.json_key(): uid(ingested.sop_class.as_str()),
                        tags::REFERENCED_SOP_INSTANCE_UID.json_key(): uid(ingested.instance.as_str()),
                        tags::FAILURE_REASON.json_key(): { "vr": "US", "Value": [STUDY_MISMATCH] },
                    }));
                } else {
                    stored.push(ingested);
                }
            }
            Err(e) => {
                tracing::info!(error = %e, "STOW-RS instance not stored");
                failed.push(json!({
                    tags::FAILURE_REASON.json_key(): { "vr": "US", "Value": [failure_reason(&e)] },
                }));
            }
        }
    }
    Ok(response(&req.root, &stored, &failed))
}

/// The STOW-RS response: status and the Store Instances Response Module (PS3.18 Annex I).
fn response(root: &str, stored: &[Ingested], failed: &[Value]) -> Response {
    let referenced: Vec<Value> = stored
        .iter()
        .map(|i| {
            let url = format!(
                "{}/studies/{}/series/{}/instances/{}",
                root,
                i.study.as_str(),
                i.series.as_str(),
                i.instance.as_str()
            );
            json!({
                tags::REFERENCED_SOP_CLASS_UID.json_key(): uid(i.sop_class.as_str()),
                tags::REFERENCED_SOP_INSTANCE_UID.json_key(): uid(i.instance.as_str()),
                tags::RETRIEVE_URL.json_key(): { "vr": "UR", "Value": [url] },
            })
        })
        .collect();
    let mut response = serde_json::Map::new();
    let studies: std::collections::BTreeSet<&str> =
        stored.iter().map(|i| i.study.as_str()).collect();
    if let [study] = studies.into_iter().collect::<Vec<_>>()[..] {
        response.insert(
            tags::RETRIEVE_URL.json_key(),
            json!({ "vr": "UR", "Value": [format!("{}/studies/{study}", root)] }),
        );
    }
    if !referenced.is_empty() {
        response.insert(
            tags::REFERENCED_SOP_SEQUENCE.json_key(),
            json!({ "vr": "SQ", "Value": referenced }),
        );
    }
    if !failed.is_empty() {
        response.insert(
            tags::FAILED_SOP_SEQUENCE.json_key(),
            json!({ "vr": "SQ", "Value": failed }),
        );
    }
    let status = match (stored.is_empty(), failed.is_empty()) {
        (_, true) => StatusCode::OK,
        (false, false) => StatusCode::ACCEPTED,
        (true, false) => StatusCode::CONFLICT,
    };
    let mut response = (status, Json(Value::Object(response))).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/dicom+json"),
    );
    response
}
