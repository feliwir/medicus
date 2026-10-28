//! C-FIND: identifiers to queries and query results back to identifiers (PS3.4 C.4.1).

use dicom_core::header::Header as _;
use dicom_core::value::{DataSetSequence, Value};
use dicom_core::{DataElement, PrimitiveValue};
use dicom_dictionary_std::uids;
use dicom_object::InMemDicomObject;
use futures::StreamExt as _;
use medicus_addon_sdk::Host;
use medicus_core::capability::HostServices as _;
use medicus_core::capability::index::DicomJsonObject;
use medicus_core::query::{Key, MatchOptions, Matcher, Query};
use medicus_core::{CallContext, QueryLevel, Tag, VR, tags};
use medicus_dimse::{Matches, Status};

/// Rows fetched from the host per query page.
const PAGE: u32 = 100;

/// The information models served.
pub const FIND_SOP_CLASSES: &[&str] = &[
    uids::PATIENT_ROOT_QUERY_RETRIEVE_INFORMATION_MODEL_FIND,
    uids::STUDY_ROOT_QUERY_RETRIEVE_INFORMATION_MODEL_FIND,
];

/// Attributes that describe the query or response rather than matching.
fn is_control(tag: Tag) -> bool {
    tag.group() == 0x0002
        || tag.element() == 0x0000
        || tag == tags::SPECIFIC_CHARACTER_SET
        || tag == tags::QUERY_RETRIEVE_LEVEL
        || tag == tags::RETRIEVE_AE_TITLE
        || tag == tags::INSTANCE_AVAILABILITY
}

fn mismatch(comment: impl Into<String>) -> Status {
    Status::new(Status::IDENTIFIER_MISMATCH).with_comment(comment)
}

/// A parsed C-FIND request: the query and what each response must contain.
#[derive(Debug)]
pub(crate) struct Find {
    pub(crate) query: Query,
    /// Requested attributes with the VR to use when a match has no value.
    requested: Vec<(Tag, VR)>,
    /// Whether Retrieve AE Title (0008,0054) was requested.
    retrieve_ae: bool,
}

impl Find {
    pub(crate) fn parse(sop_class: &str, identifier: &InMemDicomObject) -> Result<Self, Status> {
        let level = identifier
            .get(tags::QUERY_RETRIEVE_LEVEL)
            .and_then(|e| e.to_str().ok())
            .ok_or_else(|| mismatch("Query/Retrieve Level is missing"))?;
        let level = QueryLevel::from_dicom(&level).map_err(|e| mismatch(e.to_string()))?;
        if level == QueryLevel::Patient
            && sop_class == uids::STUDY_ROOT_QUERY_RETRIEVE_INFORMATION_MODEL_FIND
        {
            return Err(mismatch("the Study Root model has no PATIENT level"));
        }
        let options = MatchOptions::default();
        let mut query = Query::new(level);
        let mut requested = Vec::new();
        for element in identifier {
            let tag = element.tag();
            if is_control(tag) || tag.group() % 2 == 1 {
                continue; // private attributes are not part of the query models
            }
            // Keys of lower levels are not allowed (C.4.1.2.1); ignore them.
            if tags::standard(tag).is_some_and(|a| a.level > level) {
                continue;
            }
            let key = key(element, &options)?;
            requested.push((tag, key.vr));
            query.keys.push(key);
        }
        // The unique key of the query level is always returned.
        let unique = tags::unique_key(level);
        if !requested.iter().any(|(t, _)| *t == unique) {
            let vr = tags::standard_vr(unique).unwrap_or(VR::UI);
            query.keys.push(Key::universal(unique, vr));
            requested.push((unique, vr));
        }
        Ok(Self {
            query,
            requested,
            retrieve_ae: identifier.get(tags::RETRIEVE_AE_TITLE).is_some(),
        })
    }

    /// Streams the identifiers of all matches, page by page.
    pub(crate) fn run(self, host: Host, context: CallContext, ae_title: String) -> Matches {
        let state = (Some(self), 0u64);
        futures::stream::unfold(state, move |(find, offset)| {
            let ae_title = ae_title.clone();
            let context = context.clone();
            async move {
                let find = find?;
                let mut page = find.query.clone();
                page.limit = Some(PAGE);
                page.offset = offset;
                let result = match host.query(&context, &page).await {
                    Ok(result) => result,
                    Err(e) => {
                        let status = if e.kind == medicus_core::ErrorKind::PermissionDenied {
                            Status::from(&e)
                        } else {
                            Status::new(Status::CANNOT_UNDERSTAND).with_comment(e.message)
                        };
                        return Some((vec![Err(status)], (None, 0)));
                    }
                };
                let rows: Vec<_> = result
                    .rows
                    .iter()
                    .map(|row| find.response(row, &ae_title))
                    .collect();
                let next = result.has_more.then_some(find);
                Some((rows, (next, offset + u64::from(PAGE))))
            }
        })
        .flat_map(futures::stream::iter)
        .boxed()
    }

    /// The response identifier for one result row.
    fn response(&self, row: &DicomJsonObject, ae_title: &str) -> Result<InMemDicomObject, Status> {
        let found: InMemDicomObject =
            dicom_json::from_value(serde_json::Value::Object(row.clone())).map_err(|e| {
                Status::new(Status::CANNOT_UNDERSTAND).with_comment(format!("bad result row: {e}"))
            })?;
        let mut out = InMemDicomObject::new_empty();
        // Values come from the index as Unicode.
        if row_is_non_ascii(row) {
            out.put(DataElement::new(
                tags::SPECIFIC_CHARACTER_SET,
                VR::CS,
                PrimitiveValue::from("ISO_IR 192"),
            ));
        }
        out.put(DataElement::new(
            tags::QUERY_RETRIEVE_LEVEL,
            VR::CS,
            PrimitiveValue::from(self.query.level.as_dicom()),
        ));
        if self.retrieve_ae {
            out.put(DataElement::new(
                tags::RETRIEVE_AE_TITLE,
                VR::AE,
                PrimitiveValue::from(ae_title),
            ));
        }
        for &(tag, vr) in &self.requested {
            match found.get(tag) {
                Some(element) => out.put(element.clone()),
                // Requested but without a value: returned empty (C.4.1.1.3.2).
                None if vr == VR::SQ => out.put(DataElement::new(
                    tag,
                    VR::SQ,
                    DataSetSequence::<InMemDicomObject>::empty(),
                )),
                None => out.put(DataElement::new(tag, vr, PrimitiveValue::Empty)),
            };
        }
        Ok(out)
    }
}

fn row_is_non_ascii(row: &DicomJsonObject) -> bool {
    serde_json::to_string(row).is_ok_and(|s| !s.is_ascii())
}

/// A query key from an identifier element.
fn key(element: &dicom_object::mem::InMemElement, options: &MatchOptions) -> Result<Key, Status> {
    let tag = element.tag();
    let vr = match element.vr() {
        // Implicit VR identifiers decode unknown attributes as UN; prefer the dictionary's VR.
        VR::UN => tags::standard_vr(tag).unwrap_or(VR::UN),
        vr => vr,
    };
    if let Value::Sequence(seq) = element.value() {
        let keys = match seq.items().first() {
            Some(item) => item
                .into_iter()
                .filter(|e| !is_control(e.tag()))
                .map(|e| key(e, options))
                .collect::<Result<_, _>>()?,
            None => Vec::new(),
        };
        return Ok(Key {
            tag,
            vr: VR::SQ,
            matcher: Matcher::Sequence { keys },
        });
    }
    let raw = element
        .to_str()
        .map_err(|e| mismatch(format!("{tag}: {e}")))?;
    let raw = raw.trim_end_matches(['\0', ' ']);
    Key::parse(tag, vr, raw, options).map_err(|e| mismatch(format!("{tag}: {e}")))
}
