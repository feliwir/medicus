//! Parsing the header of an incoming instance with dicom-rs.
//!
//! The ingest pipeline buffers the beginning of a stream and asks dicom-rs to parse it with
//! `read_until(PixelData)`. The bytes are offered through [`PrefixReader`], which reports
//! [`NeedMore`] at the end of the buffer while the stream is still open. dicom-rs ends a data set
//! gracefully only on a genuine end of input, so:
//! - the parse fails with `NeedMore` → buffer more and try again;
//! - the parse succeeds → dicom-rs reached the first bulk data element within the buffered bytes
//!   (or the stream ended), and the header is complete.
//!
//! The number of bytes dicom-rs consumed is recorded as the instance's header length: a prefix
//! of the file that contains the complete header (it may extend a little into the bulk data, as
//! dicom-rs reads ahead), so a range read of it yields the metadata.

use std::io::Read;

use dicom_dictionary_std::tags;
use dicom_object::file::ReadPreamble;
use dicom_object::{DefaultDicomObject, InMemDicomObject, OpenFileOptions};
use medicus_core::capability::index::{DicomJsonObject, ExtraTag};
use medicus_core::uid::{
    SeriesInstanceUid, SopClassUid, SopInstanceUid, StudyInstanceUid, TransferSyntaxUid, Uid,
};
use medicus_core::{Error, Result, Tag, TagExt as _};

/// The identifying UIDs and the attributes to index of one instance.
#[derive(Debug, Clone)]
pub(crate) struct ParsedHeader {
    pub(crate) study: StudyInstanceUid,
    pub(crate) series: SeriesInstanceUid,
    pub(crate) instance: SopInstanceUid,
    pub(crate) sop_class: SopClassUid,
    pub(crate) transfer_syntax: TransferSyntaxUid,
    pub(crate) patient_id: Option<String>,
    /// Standard and configured extra attributes as DICOM JSON.
    pub(crate) attributes: DicomJsonObject,
    /// Leading bytes of the file that contain the complete header.
    pub(crate) header_length: u64,
}

/// Marker error: the buffered bytes are exhausted but the stream continues.
#[derive(Debug)]
pub(crate) struct NeedMore;

impl std::fmt::Display for NeedMore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("more input needed")
    }
}

impl std::error::Error for NeedMore {}

/// Offers buffered bytes to dicom-rs and counts how many it consumed.
struct PrefixReader<'a> {
    data: &'a [u8],
    pos: usize,
    /// Whether `data` is the whole stream.
    complete: bool,
}

impl Read for PrefixReader<'_> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let rest = &self.data[self.pos..];
        if rest.is_empty() {
            return if self.complete {
                Ok(0)
            } else {
                Err(std::io::Error::other(NeedMore))
            };
        }
        let n = rest.len().min(out.len());
        out[..n].copy_from_slice(&rest[..n]);
        self.pos += n;
        Ok(n)
    }
}

fn is_need_more(e: &(dyn std::error::Error + 'static)) -> bool {
    let mut source = Some(e);
    while let Some(err) = source {
        if err.downcast_ref::<NeedMore>().is_some() {
            return true;
        }
        if let Some(io) = err.downcast_ref::<std::io::Error>()
            && matches!(io.get_ref(), Some(inner) if inner.is::<NeedMore>())
        {
            return true;
        }
        source = err.source();
    }
    false
}

fn invalid(msg: impl Into<String>) -> Error {
    Error::invalid_argument(msg)
}

/// Parses the header from the buffered beginning of a Part 10 file. `Ok(None)`: more bytes are
/// needed (only if `complete` is false).
pub(crate) fn parse_prefix(
    data: &[u8],
    complete: bool,
    extra: &[ExtraTag],
) -> Result<Option<ParsedHeader>> {
    let mut reader = PrefixReader {
        data,
        pos: 0,
        complete,
    };
    let parsed = OpenFileOptions::new()
        .read_preamble(ReadPreamble::Always)
        .read_until(tags::PIXEL_DATA)
        .from_reader(&mut reader);
    match parsed {
        Ok(file) => extract(&file, reader.pos as u64, extra).map(Some),
        Err(e) if !complete && is_need_more(&e) => Ok(None),
        Err(e) => Err(invalid(format!("cannot parse DICOM file: {e}"))),
    }
}

/// Parses a stored header (a range read of `0..header_length` of a Part 10 file).
pub(crate) fn parse_header(data: &[u8]) -> Result<DefaultDicomObject> {
    OpenFileOptions::new()
        .read_preamble(ReadPreamble::Always)
        .read_until(tags::PIXEL_DATA)
        .from_reader(data)
        .map_err(|e| Error::internal(format!("cannot parse stored header: {e}")))
}

/// The attributes `tags` of `ds` (those present) as DICOM JSON.
pub(crate) fn to_json(
    ds: &InMemDicomObject,
    tags: impl IntoIterator<Item = Tag>,
) -> Result<DicomJsonObject> {
    let mut out = DicomJsonObject::new();
    for tag in tags {
        let Ok(element) = ds.element(tag) else {
            continue;
        };
        let mut json = dicom_json::to_value(element).map_err(|e| invalid(format!("{tag}: {e}")))?;
        conform(&mut json);
        out.insert(tag.json_key(), json);
    }
    Ok(out)
}

/// The whole data set `ds` as DICOM JSON.
pub(crate) fn dataset_to_json(ds: &InMemDicomObject) -> Result<DicomJsonObject> {
    match dicom_json::to_value(ds) {
        Ok(serde_json::Value::Object(mut map)) => {
            map.values_mut().for_each(conform);
            Ok(map)
        }
        Ok(_) => Err(Error::internal("data set is not a JSON object")),
        Err(e) => Err(Error::internal(format!("cannot convert data set: {e}"))),
    }
}

/// Brings one DICOM JSON attribute (and the items of a sequence) in line with PS3.18 F.2: AT
/// values as eight uppercase hexadecimal digits (dicom-json writes `(gggg,eeee)`), IS and DS
/// values as JSON numbers where they parse (others stay strings, as F.2.3 permits), and no
/// `Value` at all for an empty attribute (F.2.5; dicom-json writes `"Value": []`).
fn conform(attribute: &mut serde_json::Value) {
    use serde_json::Value;
    let vr = attribute
        .get("vr")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if attribute
        .get("Value")
        .and_then(Value::as_array)
        .is_some_and(Vec::is_empty)
        && let Some(object) = attribute.as_object_mut()
    {
        object.remove("Value");
    }
    let Some(Value::Array(values)) = attribute.get_mut("Value") else {
        return;
    };
    for value in values {
        match (vr.as_str(), &*value) {
            ("SQ", Value::Object(item)) => {
                let mut item = item.clone();
                item.values_mut().for_each(conform);
                *value = Value::Object(item);
            }
            ("AT", Value::String(s)) => {
                let hex: String = s.chars().filter(char::is_ascii_hexdigit).collect();
                if hex.len() == 8 {
                    *value = Value::String(hex.to_ascii_uppercase());
                }
            }
            ("IS", Value::String(s)) => {
                if let Ok(n) = s.trim().trim_start_matches('+').parse::<i64>() {
                    *value = Value::from(n);
                }
            }
            ("DS", Value::String(s)) => {
                if let Some(n) = s
                    .trim()
                    .parse::<f64>()
                    .ok()
                    .and_then(serde_json::Number::from_f64)
                {
                    *value = Value::Number(n);
                }
            }
            _ => {}
        }
    }
}

fn string(ds: &InMemDicomObject, tag: Tag) -> Option<String> {
    let s = ds.element(tag).ok()?.to_str().ok()?;
    let s = s.trim_end_matches(['\0', ' ']).trim_start();
    (!s.is_empty()).then(|| s.to_owned())
}

fn required_uid(ds: &InMemDicomObject, tag: Tag, what: &str) -> Result<Uid> {
    let raw = string(ds, tag).ok_or_else(|| invalid(format!("{what} {tag} is missing")))?;
    Uid::parse(&raw).map_err(|e| invalid(format!("{what} {tag}: {e}")))
}

/// Extracts identifiers and the attributes to index (standard ones plus `extra`).
fn extract(
    file: &DefaultDicomObject,
    header_length: u64,
    extra: &[ExtraTag],
) -> Result<ParsedHeader> {
    let meta = file.meta();
    let transfer_syntax = TransferSyntaxUid::parse(meta.transfer_syntax())
        .map_err(|e| invalid(format!("Transfer Syntax UID: {e}")))?;
    let ds: &InMemDicomObject = file;

    let study = required_uid(ds, tags::STUDY_INSTANCE_UID, "Study Instance UID")?.into();
    let series = required_uid(ds, tags::SERIES_INSTANCE_UID, "Series Instance UID")?.into();
    let instance: SopInstanceUid =
        required_uid(ds, tags::SOP_INSTANCE_UID, "SOP Instance UID")?.into();
    // SOP Class UID is mandatory in the data set; fall back to the meta group for robustness.
    let sop_class = match string(ds, tags::SOP_CLASS_UID) {
        Some(raw) => Uid::parse(&raw),
        None => Uid::parse(meta.media_storage_sop_class_uid()),
    }
    .map_err(|e| invalid(format!("SOP Class UID: {e}")))?
    .into();
    if meta.media_storage_sop_instance_uid() != instance.as_str() {
        tracing::warn!(%instance, "Media Storage SOP Instance UID differs from SOP Instance UID");
    }

    let wanted = medicus_core::tags::STANDARD
        .iter()
        .map(|a| a.tag)
        .chain(extra.iter().map(|e| e.tag));
    let attributes = to_json(ds, wanted)?;
    Ok(ParsedHeader {
        study,
        series,
        instance,
        sop_class,
        transfer_syntax,
        patient_id: string(ds, tags::PATIENT_ID),
        attributes,
        header_length,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_follows_ps3_18_f_2_3() {
        use serde_json::json;
        let mut at = json!({ "vr": "AT", "Value": ["(0020,9056)"] });
        conform(&mut at);
        assert_eq!(at, json!({ "vr": "AT", "Value": ["00209056"] }));
        let mut numbers = json!({ "vr": "DS", "Value": [" 0.5", "1e3", "garbage"] });
        conform(&mut numbers);
        assert_eq!(
            numbers,
            json!({ "vr": "DS", "Value": [0.5, 1000.0, "garbage"] })
        );
        let mut empty = json!({ "vr": "LO", "Value": [] });
        conform(&mut empty);
        assert_eq!(empty, json!({ "vr": "LO" }));
        let mut seq =
            json!({ "vr": "SQ", "Value": [{ "00200013": { "vr": "IS", "Value": ["+7"] } }] });
        conform(&mut seq);
        assert_eq!(seq["Value"][0]["00200013"]["Value"][0], json!(7));
    }
    use dicom_core::VR;
    use dicom_dictionary_std::uids;
    use medicus_core::QueryLevel;
    use medicus_core::query::Attributes;
    use medicus_testkit::dicom::InstanceBuilder;

    fn values(p: &ParsedHeader, tag: Tag) -> Vec<String> {
        Attributes::values(&p.attributes, tag)
            .unwrap_or_default()
            .into_iter()
            .map(Into::into)
            .collect()
    }

    /// Feeds `file` in growing prefixes until dicom-rs has the whole header.
    fn parse_incrementally(file: &[u8], extra: &[ExtraTag]) -> (ParsedHeader, usize) {
        for len in (1..=file.len()).step_by(37) {
            if let Some(p) = parse_prefix(&file[..len], false, extra).unwrap() {
                return (p, len);
            }
        }
        (
            parse_prefix(file, true, extra).unwrap().unwrap(),
            file.len(),
        )
    }

    #[allow(deprecated)] // Explicit VR Big Endian is retired but still received
    const ENCODINGS: [&str; 3] = [
        uids::EXPLICIT_VR_LITTLE_ENDIAN,
        uids::IMPLICIT_VR_LITTLE_ENDIAN,
        uids::EXPLICIT_VR_BIG_ENDIAN,
    ];

    #[test]
    fn header_is_complete_before_the_bulk_data_in_every_encoding() {
        for ts in ENCODINGS {
            let file = InstanceBuilder::new("1.2.3", "1.2.3.4", "1.2.3.4.5")
                .transfer_syntax(ts)
                .with_sequence()
                .pixel_data(100_000)
                .build();
            let (p, needed) = parse_incrementally(&file, &[]);
            // The header was recognized long before the end of the pixel data.
            assert!(
                needed < file.len() / 2,
                "{ts}: needed {needed} of {}",
                file.len()
            );
            assert!(usize::try_from(p.header_length).unwrap() <= needed);
            assert_eq!(p.instance.as_str(), "1.2.3.4.5", "{ts}");
            assert_eq!(p.transfer_syntax.as_str(), ts);
            assert_eq!(p.sop_class.as_str(), uids::CT_IMAGE_STORAGE);
            assert_eq!(p.patient_id.as_deref(), Some("PID-1"));
            assert_eq!(values(&p, tags::PATIENT_NAME), ["Doe^John"], "{ts}");
            assert!(p.attributes.get(&tags::PIXEL_DATA.json_key()).is_none());

            // The recorded prefix alone reproduces the same header.
            let again = parse_prefix(
                &file[..usize::try_from(p.header_length).unwrap()],
                true,
                &[],
            )
            .unwrap()
            .unwrap();
            assert_eq!(again.attributes, p.attributes, "{ts}");
        }
    }

    #[test]
    fn file_without_pixel_data_needs_the_end_of_the_stream() {
        let file = InstanceBuilder::new("1.2.3", "1.2.3.4", "1.2.3.4.5")
            .pixel_data(0)
            .build();
        assert!(
            parse_prefix(&file, false, &[]).unwrap().is_none(),
            "could continue"
        );
        let p = parse_prefix(&file, true, &[]).unwrap().unwrap();
        assert_eq!(p.header_length, file.len() as u64);
    }

    #[test]
    fn extracts_only_standard_and_extra_attributes() {
        let file = InstanceBuilder::new("1.2.3", "1.2.3.4", "1.2.3.4.5")
            .attr(tags::SLICE_THICKNESS, VR::DS, "1.25")
            .attr(tags::IMAGE_TYPE, VR::CS, "ORIGINAL\\PRIMARY")
            .attr(tags::MANUFACTURER, VR::LO, "ACME")
            .build();
        let extra = [
            ExtraTag {
                tag: tags::SLICE_THICKNESS,
                vr: VR::DS,
                level: QueryLevel::Instance,
            },
            ExtraTag {
                tag: tags::IMAGE_TYPE,
                vr: VR::CS,
                level: QueryLevel::Instance,
            },
        ];
        let (p, _) = parse_incrementally(&file, &extra);
        assert_eq!(values(&p, tags::SLICE_THICKNESS), ["1.25"]);
        assert_eq!(values(&p, tags::IMAGE_TYPE), ["ORIGINAL", "PRIMARY"]);
        assert!(
            p.attributes.get(&tags::MANUFACTURER.json_key()).is_none(),
            "not configured"
        );
    }

    #[test]
    fn invalid_input_is_rejected() {
        assert!(parse_prefix(b"not dicom at all", true, &[]).is_err());
        let file = InstanceBuilder::new("1.2.3", "1.2.3.4", "1.2.3.4.5")
            .attr(tags::SERIES_INSTANCE_UID, VR::UI, "")
            .build();
        let err = parse_prefix(&file, true, &[]).unwrap_err();
        assert!(err.message.contains("Series Instance UID"), "{err}");
    }
}
