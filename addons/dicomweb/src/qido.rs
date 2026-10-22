//! QIDO-RS (PS3.18 §10.6): searches for studies, series and instances.

use axum::Json;
use axum::extract::{Query as Params, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use dicom_core::dictionary::{DataDictionary as _, DataDictionaryEntry as _, TagRange};
use dicom_dictionary_std::StandardDataDictionary;
use medicus_core::capability::HostServices as _;
use medicus_core::capability::index::DicomJsonObject;
use medicus_core::query::{Key, MatchOptions, Matcher, Query};
use medicus_core::{QueryLevel, Tag, TagExt as _, VR, tags};
use serde_json::{Value, json};

use crate::error::{WebError, WebResult};
use crate::web::{Request, State as Web};

/// Attributes returned by default (PS3.18 Tables 10.6.3-3, -4 and -5), besides the unique keys
/// of higher levels.
const STUDY_DEFAULTS: &[Tag] = &[
    tags::STUDY_DATE,
    tags::STUDY_TIME,
    tags::ACCESSION_NUMBER,
    tags::MODALITIES_IN_STUDY,
    tags::REFERRING_PHYSICIAN_NAME,
    tags::PATIENT_NAME,
    tags::PATIENT_ID,
    tags::PATIENT_BIRTH_DATE,
    tags::PATIENT_SEX,
    tags::STUDY_INSTANCE_UID,
    tags::STUDY_ID,
    tags::NUMBER_OF_STUDY_RELATED_SERIES,
    tags::NUMBER_OF_STUDY_RELATED_INSTANCES,
];
const SERIES_DEFAULTS: &[Tag] = &[
    tags::MODALITY,
    tags::SERIES_DESCRIPTION,
    tags::SERIES_INSTANCE_UID,
    tags::SERIES_NUMBER,
    tags::NUMBER_OF_SERIES_RELATED_INSTANCES,
    tags::PERFORMED_PROCEDURE_STEP_START_DATE,
];
const INSTANCE_DEFAULTS: &[Tag] = &[
    tags::SOP_CLASS_UID,
    tags::SOP_INSTANCE_UID,
    tags::INSTANCE_NUMBER,
    tags::ROWS,
    tags::COLUMNS,
    tags::BITS_ALLOCATED,
    tags::NUMBER_OF_FRAMES,
];

/// Attributes the server computes itself and that are not query keys.
const COMPUTED: &[Tag] = &[
    tags::RETRIEVE_URL,
    tags::INSTANCE_AVAILABILITY,
    tags::TIMEZONE_OFFSET_FROM_UTC,
];

const MAX_LIMIT: u32 = 10_000;

type QueryParams = Params<Vec<(String, String)>>;

pub(crate) async fn studies(
    State(web): State<Web>,
    req: Request,
    Params(p): QueryParams,
) -> WebResult<Response> {
    search(&web, &req, QueryLevel::Study, &p).await
}

pub(crate) async fn series(
    State(web): State<Web>,
    req: Request,
    Params(p): QueryParams,
) -> WebResult<Response> {
    search(&web, &req, QueryLevel::Series, &p).await
}

pub(crate) async fn instances(
    State(web): State<Web>,
    req: Request,
    Params(p): QueryParams,
) -> WebResult<Response> {
    search(&web, &req, QueryLevel::Instance, &p).await
}

/// A tag from a keyword (`PatientName`) or a hexadecimal tag (`00100010`), with its VR.
pub(crate) fn attribute(id: &str) -> WebResult<(Tag, VR)> {
    let entry = match medicus_core::tag::parse_tag(id) {
        Ok(tag) => StandardDataDictionary.by_tag(tag),
        Err(_) => StandardDataDictionary.by_name(id),
    }
    .ok_or_else(|| WebError::bad_request(format!("unknown attribute {id:?}")))?;
    let TagRange::Single(tag) = entry.tag_range() else {
        return Err(WebError::bad_request(format!(
            "{id:?} is not a single attribute"
        )));
    };
    Ok((
        tag,
        tags::standard_vr(tag).unwrap_or_else(|| entry.vr().relaxed()),
    ))
}

/// The attributes returned by default: those of the level, plus those of the higher levels the
/// path does not fix.
fn defaults(level: QueryLevel, req: &Request) -> impl Iterator<Item = &'static Tag> {
    let scoped_study = req.params.contains_key("study");
    let scoped_series = req.params.contains_key("series");
    let lists: &[(&[Tag], bool)] = &[
        (STUDY_DEFAULTS, level == QueryLevel::Study || !scoped_study),
        (
            SERIES_DEFAULTS,
            level >= QueryLevel::Series && (level == QueryLevel::Series || !scoped_series),
        ),
        (INSTANCE_DEFAULTS, level == QueryLevel::Instance),
    ];
    lists
        .iter()
        .filter(|(_, wanted)| *wanted)
        .flat_map(|(list, _)| list.iter())
        .collect::<Vec<_>>()
        .into_iter()
}

/// The query of a search request: the path's UIDs, the matching parameters, and the attributes
/// to return.
fn build(
    level: QueryLevel,
    req: &Request,
    params: &[(String, String)],
) -> WebResult<(Query, Vec<Tag>)> {
    let options = MatchOptions::default();
    let mut query = Query::new(level);
    let mut returned: Vec<Tag> = Vec::new();
    let add_return = |tag: Tag, returned: &mut Vec<Tag>| {
        if !returned.contains(&tag) {
            returned.push(tag);
        }
    };
    for tag in defaults(level, req) {
        add_return(*tag, &mut returned);
    }
    for (name, tag) in [
        ("study", tags::STUDY_INSTANCE_UID),
        ("series", tags::SERIES_INSTANCE_UID),
    ] {
        if let Some(uid) = req.params.get(name) {
            query.keys.push(Key {
                tag,
                vr: VR::UI,
                matcher: Matcher::Single { value: uid.clone() },
            });
            add_return(tag, &mut returned);
        }
    }
    for (name, value) in params {
        match name.as_str() {
            "limit" => {
                let limit: u32 = value
                    .parse()
                    .map_err(|_| WebError::bad_request("invalid limit"))?;
                query.limit = Some(limit.min(MAX_LIMIT));
            }
            "offset" => {
                query.offset = value
                    .parse()
                    .map_err(|_| WebError::bad_request("invalid offset"))?;
            }
            "fuzzymatching" => {} // not supported; exact matching applies (allowed by PS3.18)
            "includefield" => {
                for field in value.split(',').map(str::trim).filter(|f| !f.is_empty()) {
                    if field == "all" {
                        for a in tags::STANDARD.iter().filter(|a| a.level <= level) {
                            add_return(a.tag, &mut returned);
                        }
                    } else {
                        add_return(attribute(field)?.0, &mut returned);
                    }
                }
            }
            id if id.contains('.') => {
                return Err(WebError::bad_request(format!(
                    "sequence matching ({id}) is not supported"
                )));
            }
            id => {
                let (tag, vr) = attribute(id)?;
                // Computed attributes and keys of lower levels do not constrain the search.
                if COMPUTED.contains(&tag) || tags::standard(tag).is_some_and(|a| a.level > level) {
                    continue;
                }
                // UID lists are comma-separated in DICOMweb, backslash-separated in DICOM.
                let raw = if vr == VR::UI {
                    value.replace(',', "\\")
                } else {
                    value.clone()
                };
                let key = Key::parse(tag, vr, &raw, &options)
                    .map_err(|e| WebError::bad_request(format!("{id}: {e}")))?;
                query.keys.retain(|k| k.tag != tag);
                query.keys.push(key);
                add_return(tag, &mut returned);
            }
        }
    }
    // Every returned attribute must be requested from the query service; keys of lower levels
    // than the query level are not allowed there.
    for tag in &returned {
        if !query.keys.iter().any(|k| k.tag == *tag)
            && !COMPUTED.contains(tag)
            && tags::standard(*tag).is_none_or(|a| a.level <= level)
        {
            let vr = tags::standard_vr(*tag).unwrap_or(VR::UN);
            query.keys.push(Key::universal(*tag, vr));
        }
    }
    for unique in [
        tags::STUDY_INSTANCE_UID,
        tags::SERIES_INSTANCE_UID,
        tags::SOP_INSTANCE_UID,
    ]
    .into_iter()
    .filter(|t| tags::standard(*t).is_some_and(|a| a.level <= level))
    {
        if !query.keys.iter().any(|k| k.tag == unique) {
            query.keys.push(Key::universal(unique, VR::UI));
        }
        add_return(unique, &mut returned);
    }
    Ok((query, returned))
}

fn first_string(row: &DicomJsonObject, tag: Tag) -> Option<String> {
    row.get(&tag.json_key())?
        .get("Value")?
        .get(0)?
        .as_str()
        .map(str::to_owned)
}

/// The Retrieve URL of a result row.
pub(crate) fn retrieve_url(root: &str, level: QueryLevel, row: &DicomJsonObject) -> Option<String> {
    let study = first_string(row, tags::STUDY_INSTANCE_UID)?;
    let mut url = format!("{root}/studies/{study}");
    if level >= QueryLevel::Series {
        let series = first_string(row, tags::SERIES_INSTANCE_UID)?;
        url = format!("{url}/series/{series}");
    }
    if level == QueryLevel::Instance {
        let instance = first_string(row, tags::SOP_INSTANCE_UID)?;
        url = format!("{url}/instances/{instance}");
    }
    Some(url)
}

async fn search(
    web: &Web,
    req: &Request,
    level: QueryLevel,
    params: &[(String, String)],
) -> WebResult<Response> {
    let (query, returned) = build(level, req, params)?;
    let result = web.host.query(&req.context, &query).await?;
    let rows: Vec<Value> = result
        .rows
        .iter()
        .map(|row| {
            let mut out = DicomJsonObject::new();
            for tag in &returned {
                let key = tag.json_key();
                if let Some(v) = row.get(&key) {
                    out.insert(key, v.clone());
                }
            }
            if let Some(url) = retrieve_url(&req.root, level, row) {
                out.insert(
                    tags::RETRIEVE_URL.json_key(),
                    json!({ "vr": "UR", "Value": [url] }),
                );
            }
            Value::Object(out)
        })
        .collect();
    let mut response = Json(rows).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/dicom+json"),
    );
    if result.has_more {
        // PS3.18 §8.3.4.4: more matches exist than were returned.
        response.headers_mut().insert(
            header::WARNING,
            header::HeaderValue::from_static(
                "299 medicus: There are additional results that can be requested",
            ),
        );
    }
    Ok(response)
}
