//! WADO-RS (PS3.18 §10.4): instances, metadata and frames, as stored (no transcoding).
//!
//! A request for a transfer syntax other than the stored one is answered with 406.

use axum::Json;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use dicom_dictionary_std::tags;
use dicom_encoding::adapters::PixelDataObject as _;
use futures::StreamExt as _;
use medicus_core::capability::index::{InstanceLocation, ResourceUid};
use medicus_core::capability::{ByteStream, HostServices as _};
use medicus_core::uid::{SeriesInstanceUid, SopInstanceUid, StudyInstanceUid};
use serde_json::Value;

use crate::error::{WebError, WebResult};
use crate::multipart::{self, Part};
use crate::web::{Request, State as Web};

/// Largest instance whose frames are extracted or rendered (the whole file is read into memory).
const MAX_DECODED_FILE: usize = 2 << 30;

fn uid<T>(
    raw: &str,
    parse: impl Fn(&str) -> Result<T, medicus_core::uid::UidError>,
) -> WebResult<T> {
    parse(raw).map_err(|e| WebError::bad_request(format!("{raw:?}: {e}")))
}

/// The resource a WADO-RS path addresses.
fn resource(req: &Request) -> WebResult<ResourceUid> {
    if let Some(instance) = req.params.get("instance") {
        return Ok(ResourceUid::Instance(uid(instance, SopInstanceUid::parse)?));
    }
    if let Some(series) = req.params.get("series") {
        return Ok(ResourceUid::Series(uid(series, SeriesInstanceUid::parse)?));
    }
    Ok(ResourceUid::Study(uid(
        req.param("study")?,
        StudyInstanceUid::parse,
    )?))
}

/// The stored instances of the path's resource; 404 if there are none.
pub(crate) async fn locate(web: &Web, req: &Request) -> WebResult<Vec<InstanceLocation>> {
    let resource = resource(req)?;
    let mut found = web.host.locate(&req.context, &resource).await?;
    // The path's higher-level UIDs must match too.
    if let Some(study) = req.params.get("study") {
        found.retain(|l| l.study.as_str() == study);
    }
    if let Some(series) = req.params.get("series") {
        found.retain(|l| l.series.as_str() == series);
    }
    if found.is_empty() {
        return Err(WebError::new(StatusCode::NOT_FOUND, "no such resource"));
    }
    Ok(found)
}

/// One acceptable representation from an Accept header: media type and its parameters.
#[derive(Debug, Clone)]
pub(crate) struct Acceptable {
    pub(crate) media: String,
    /// The `type` parameter of `multipart/related`.
    part_type: Option<String>,
    transfer_syntax: Option<String>,
}

pub(crate) fn accepted(headers: &HeaderMap) -> Vec<Acceptable> {
    let raw = headers
        .get_all(header::ACCEPT)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect::<Vec<_>>()
        .join(",");
    if raw.trim().is_empty() {
        return Vec::new();
    }
    // Commas inside quoted parameter values are not used by DICOMweb media types.
    raw.split(',')
        .filter_map(|item| {
            let mut pieces = item.split(';').map(str::trim);
            let media = pieces.next()?.to_ascii_lowercase();
            let mut acceptable = Acceptable {
                media,
                part_type: None,
                transfer_syntax: None,
            };
            for p in pieces {
                let Some((k, v)) = p.split_once('=') else {
                    continue;
                };
                let v = v.trim().trim_matches('"').to_owned();
                match k.trim().to_ascii_lowercase().as_str() {
                    "type" => acceptable.part_type = Some(v.to_ascii_lowercase()),
                    "transfer-syntax" => acceptable.transfer_syntax = Some(v),
                    _ => {}
                }
            }
            Some(acceptable)
        })
        .collect()
}

/// Whether an Accept header admits `multipart/related` parts of `part_type` in `transfer_syntax`.
fn admits(headers: &HeaderMap, part_type: &str, transfer_syntax: &str) -> bool {
    let list = accepted(headers);
    list.is_empty()
        || list.iter().any(|a| {
            // `transfer-syntax=*` asks for the data as stored, whatever its media type (this is
            // how viewers such as OHIF ask for frames).
            let any_syntax = a.transfer_syntax.as_deref() == Some("*");
            let media_ok = a.media == "*/*"
                || a.media == "multipart/*"
                || (a.media == "multipart/related"
                    && (any_syntax
                        || a.part_type
                            .as_deref()
                            .is_none_or(|t| t == part_type || t == "*/*")));
            let ts_ok = a
                .transfer_syntax
                .as_deref()
                .is_none_or(|ts| ts == "*" || ts == transfer_syntax);
            media_ok && ts_ok
        })
}

fn multipart_response(
    part_type: &str,
    parts: impl futures::Stream<Item = medicus_core::Result<Part>> + Send + 'static,
) -> Response {
    let boundary = multipart::new_boundary();
    let content_type = format!("multipart/related; type=\"{part_type}\"; boundary={boundary}");
    let body = Body::from_stream(multipart::body(boundary, parts));
    ([(header::CONTENT_TYPE, content_type)], body).into_response()
}

/// `GET .../studies/{study}[/series/{series}[/instances/{instance}]]`: the instances as stored.
pub(crate) async fn retrieve(
    State(web): State<Web>,
    req: Request,
    headers: HeaderMap,
) -> WebResult<Response> {
    let found = locate(&web, &req).await?;
    for location in &found {
        if !admits(
            &headers,
            "application/dicom",
            location.transfer_syntax.as_str(),
        ) {
            return Err(WebError::not_acceptable(format!(
                "instance {} is stored as {}; transcoding is not supported",
                location.instance,
                location.transfer_syntax.as_str()
            )));
        }
    }
    let host = web.host;
    let context = req.context.clone();
    let parts = futures::stream::iter(found).then(move |location| {
        let context = context.clone();
        async move {
            let body = host.open_instance(&context, &location.instance).await?;
            Ok(Part {
                content_type: format!(
                    "application/dicom; transfer-syntax={}",
                    location.transfer_syntax.as_str()
                ),
                body,
            })
        }
    });
    Ok(multipart_response("application/dicom", parts))
}

/// `GET .../metadata`: the instances' metadata as a DICOM JSON array.
pub(crate) async fn metadata(State(web): State<Web>, req: Request) -> WebResult<Response> {
    let found = locate(&web, &req).await?;
    let mut out = Vec::with_capacity(found.len());
    for location in &found {
        let metadata = web.host.metadata(&req.context, &location.instance).await?;
        out.push(Value::Object(metadata));
    }
    let mut response = Json(out).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/dicom+json"),
    );
    Ok(response)
}

/// The media type of frames encoded in `transfer_syntax` (PS3.18 Table 8.7.3-2).
pub(crate) fn frame_media_type(transfer_syntax: &str) -> &'static str {
    match transfer_syntax {
        "1.2.840.10008.1.2.4.50"
        | "1.2.840.10008.1.2.4.51"
        | "1.2.840.10008.1.2.4.57"
        | "1.2.840.10008.1.2.4.70" => "image/jpeg",
        "1.2.840.10008.1.2.4.80" | "1.2.840.10008.1.2.4.81" => "image/jls",
        "1.2.840.10008.1.2.4.90" | "1.2.840.10008.1.2.4.91" => "image/jp2",
        "1.2.840.10008.1.2.4.92" | "1.2.840.10008.1.2.4.93" => "image/jpx",
        "1.2.840.10008.1.2.4.201" | "1.2.840.10008.1.2.4.202" | "1.2.840.10008.1.2.4.203" => {
            "image/jphc"
        }
        "1.2.840.10008.1.2.5" => "image/x-dicom-rle",
        "1.2.840.10008.1.2.4.100"
        | "1.2.840.10008.1.2.4.101"
        | "1.2.840.10008.1.2.4.102"
        | "1.2.840.10008.1.2.4.103"
        | "1.2.840.10008.1.2.4.104"
        | "1.2.840.10008.1.2.4.105"
        | "1.2.840.10008.1.2.4.106" => "video/mpeg",
        _ => "application/octet-stream",
    }
}

/// The 1-based frame numbers of a `frames/{list}` path.
pub(crate) fn frame_numbers(list: &str) -> WebResult<Vec<usize>> {
    list.split(',')
        .map(|n| match n.trim().parse::<usize>() {
            Ok(n) if n >= 1 => Ok(n),
            _ => Err(WebError::bad_request(format!("invalid frame number {n:?}"))),
        })
        .collect()
}

/// Why requested frames cannot be returned.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FrameError {
    /// The instance has no pixel data (e.g. a presentation state).
    NoPixelData,
    /// The frame number is beyond the instance's frames.
    NoSuchFrame(usize),
    /// The file or its pixel data cannot be read.
    Unreadable(String),
}

impl From<FrameError> for WebError {
    fn from(e: FrameError) -> Self {
        match e {
            FrameError::NoPixelData => {
                WebError::new(StatusCode::NOT_FOUND, "the instance has no pixel data")
            }
            FrameError::NoSuchFrame(n) => {
                WebError::new(StatusCode::NOT_FOUND, format!("frame {n} does not exist"))
            }
            FrameError::Unreadable(e) => WebError::new(StatusCode::UNPROCESSABLE_ENTITY, e),
        }
    }
}

/// A stored instance's whole Part 10 file, for decoding.
pub(crate) async fn read_instance(
    web: &Web,
    req: &Request,
    location: &InstanceLocation,
) -> WebResult<Vec<u8>> {
    let mut file = Vec::new();
    let mut stream: ByteStream = web
        .host
        .open_instance(&req.context, &location.instance)
        .await?;
    while let Some(chunk) = stream.next().await {
        file.extend_from_slice(&chunk?);
        if file.len() > MAX_DECODED_FILE {
            return Err(WebError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "instance too large to decode",
            ));
        }
    }
    Ok(file)
}

/// Frames `numbers` (1-based) of a stored Part 10 file, as stored, via dicom-rs
/// ([`PixelDataObject::frame_pixel_data`]): native frames by frame size, encapsulated frames by
/// fragment or Basic Offset Table.
pub(crate) fn extract_frames(file: &[u8], numbers: &[usize]) -> Result<Vec<Bytes>, FrameError> {
    let obj = dicom_object::OpenFileOptions::new()
        .from_reader(file)
        .map_err(|e| FrameError::Unreadable(e.to_string()))?;
    if obj.get(tags::PIXEL_DATA).is_none() {
        return Err(FrameError::NoPixelData);
    }
    let frames = obj.number_of_frames().unwrap_or(1) as usize;
    numbers
        .iter()
        .map(|&n| {
            if n > frames {
                return Err(FrameError::NoSuchFrame(n));
            }
            let index = u32::try_from(n - 1).map_err(|_| FrameError::NoSuchFrame(n))?;
            obj.frame_pixel_data(index)
                .map(|data| Bytes::from(data.into_owned()))
                .ok_or_else(|| {
                    FrameError::Unreadable(format!("frame {n} cannot be read from the pixel data"))
                })
        })
        .collect()
}

/// `GET .../instances/{instance}/frames/{list}`: the requested frames, as stored.
pub(crate) async fn frames(
    State(web): State<Web>,
    req: Request,
    headers: HeaderMap,
) -> WebResult<Response> {
    let numbers = frame_numbers(req.param("frames")?)?;
    let found = locate(&web, &req).await?;
    let location = &found[0];
    let ts = location.transfer_syntax.as_str().to_owned();
    let media = frame_media_type(&ts);
    if !admits(&headers, media, &ts) {
        return Err(WebError::not_acceptable(format!(
            "frames are stored as {media} ({ts}); transcoding is not supported"
        )));
    }
    let file = read_instance(&web, &req, location).await?;
    let wanted = numbers.clone();
    let extracted = tokio::task::spawn_blocking(move || extract_frames(&file, &wanted))
        .await
        .map_err(|e| WebError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    let mut parts = Vec::with_capacity(extracted.len());
    for frame in extracted {
        let content_type = if media == "application/octet-stream" {
            media.to_owned()
        } else {
            format!("{media}; transfer-syntax={ts}")
        };
        let body: ByteStream = Box::pin(futures::stream::once(async move { Ok(frame) }));
        parts.push(Ok(Part { content_type, body }));
    }
    Ok(multipart_response(media, futures::stream::iter(parts)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accept_negotiation() {
        let mut h = HeaderMap::new();
        assert!(admits(&h, "application/dicom", "1.2.840.10008.1.2.1"));
        h.insert(
            header::ACCEPT,
            "multipart/related; type=\"application/dicom\"; transfer-syntax=1.2.840.10008.1.2.1"
                .parse()
                .unwrap(),
        );
        assert!(admits(&h, "application/dicom", "1.2.840.10008.1.2.1"));
        assert!(!admits(&h, "application/dicom", "1.2.840.10008.1.2.4.50"));
        h.insert(
            header::ACCEPT,
            "multipart/related; type=\"application/dicom\"; transfer-syntax=*"
                .parse()
                .unwrap(),
        );
        assert!(admits(&h, "application/dicom", "1.2.840.10008.1.2.4.50"));
        h.insert(
            header::ACCEPT,
            "multipart/related; type=\"image/jpeg\"".parse().unwrap(),
        );
        assert!(!admits(
            &h,
            "application/octet-stream",
            "1.2.840.10008.1.2.1"
        ));
        // OHIF: octet-stream "in any transfer syntax" accepts compressed frames as stored.
        h.insert(
            header::ACCEPT,
            "multipart/related; type=\"application/octet-stream\"; transfer-syntax=*"
                .parse()
                .unwrap(),
        );
        assert!(admits(&h, "image/jpeg", "1.2.840.10008.1.2.4.50"));
    }

    fn file(ts: &str, elements: Vec<dicom_object::mem::InMemElement>) -> Vec<u8> {
        use dicom_core::{DataElement, PrimitiveValue, VR};
        let mut all = vec![
            DataElement::new(
                tags::SOP_CLASS_UID,
                VR::UI,
                PrimitiveValue::from("1.2.840.10008.5.1.4.1.1.7"),
            ),
            DataElement::new(
                tags::SOP_INSTANCE_UID,
                VR::UI,
                PrimitiveValue::from("1.2.3"),
            ),
            DataElement::new(tags::ROWS, VR::US, PrimitiveValue::from(2_u16)),
            DataElement::new(tags::COLUMNS, VR::US, PrimitiveValue::from(2_u16)),
            DataElement::new(tags::BITS_ALLOCATED, VR::US, PrimitiveValue::from(8_u16)),
            DataElement::new(tags::SAMPLES_PER_PIXEL, VR::US, PrimitiveValue::from(1_u16)),
            DataElement::new(tags::NUMBER_OF_FRAMES, VR::IS, PrimitiveValue::from("3")),
            DataElement::new(
                tags::PHOTOMETRIC_INTERPRETATION,
                VR::CS,
                PrimitiveValue::from("MONOCHROME2"),
            ),
        ];
        all.extend(elements);
        let obj = dicom_object::InMemDicomObject::from_element_iter(all)
            .with_meta(dicom_object::meta::FileMetaTableBuilder::new().transfer_syntax(ts))
            .unwrap();
        let mut out = Vec::new();
        obj.write_all(&mut out).unwrap();
        out
    }

    #[test]
    fn native_frames_are_split_by_frame_size() {
        use dicom_core::{DataElement, PrimitiveValue, VR};
        let pixels: Vec<u8> = (0..12).collect();
        let f = file(
            "1.2.840.10008.1.2.1",
            vec![DataElement::new(
                tags::PIXEL_DATA,
                VR::OB,
                PrimitiveValue::from(pixels),
            )],
        );
        let frames = extract_frames(&f, &[3, 1]).unwrap();
        assert_eq!(&frames[0][..], &[8, 9, 10, 11]);
        assert_eq!(&frames[1][..], &[0, 1, 2, 3]);
        assert_eq!(extract_frames(&f, &[4]), Err(FrameError::NoSuchFrame(4)));
    }

    #[test]
    fn encapsulated_frames_are_fragments() {
        use dicom_core::value::PixelFragmentSequence;
        use dicom_core::{DataElement, VR};
        let fragments = vec![vec![1_u8, 2], vec![3, 4], vec![5, 6]];
        let seq = PixelFragmentSequence::new(Vec::<u32>::new(), fragments);
        let f = file(
            "1.2.840.10008.1.2.4.50",
            vec![DataElement::new(tags::PIXEL_DATA, VR::OB, seq)],
        );
        let frames = extract_frames(&f, &[1, 2, 3]).unwrap();
        assert_eq!(
            frames.iter().map(|f| f.to_vec()).collect::<Vec<_>>(),
            [vec![1, 2], vec![3, 4], vec![5, 6]]
        );
        assert_eq!(frame_media_type("1.2.840.10008.1.2.4.50"), "image/jpeg");
    }

    #[test]
    fn frame_lists() {
        assert_eq!(frame_numbers("1,3, 2").unwrap(), [1, 3, 2]);
        assert!(frame_numbers("0").is_err());
        assert!(frame_numbers("x").is_err());
    }
}
