//! Backend-neutral query model.
//!
//! A [`Query`] is built by a protocol front-end (C-FIND identifier, QIDO-RS parameters) and
//! executed by the index backend, which compiles indexed keys to SQL. Keys the backend cannot
//! evaluate are post-filtered by the core with [`Query::matches`] against the stored metadata, so
//! every backend produces identical results.

mod datetime;
mod dicom_json;
mod matcher;

use std::borrow::Cow;

use serde::{Deserialize, Serialize};

pub use dicom_json::DicomJson;
pub use matcher::Matcher;

/// Value normalization shared by the in-memory matcher and index backends, so that SQL
/// evaluation agrees with [`Query::matches`].
pub mod values {
    use crate::Vr;

    /// Removes insignificant padding (and trailing empty PN components), as the matcher does.
    pub fn normalize(vr: Vr, raw: &str) -> String {
        super::matcher::normalize(vr, raw)
    }

    /// Canonical, lexically sortable form of a DA or TM value (lower end of its interval), e.g.
    /// for an index column. `None` for other VRs or invalid values.
    pub fn sortable_lower(vr: Vr, raw: &str) -> Option<String> {
        super::datetime::sortable(vr, raw, false)
    }

    /// Like [`sortable_lower`], for the upper end of the value's interval (the TM `"10"` covers
    /// up to 10:59:59.999999). Used for the end of a range.
    pub fn sortable_upper(vr: Vr, raw: &str) -> Option<String> {
        super::datetime::sortable(vr, raw, true)
    }

    /// Case folding used for case-insensitive PN matching.
    pub fn fold(s: &str) -> String {
        s.to_lowercase()
    }
}

use crate::{QueryLevel, Tag, Vr};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QueryError {
    #[error("invalid key value {value:?} for VR {vr}")]
    InvalidValue { vr: Vr, value: String },
    #[error("range {value:?} is empty (start is after end)")]
    EmptyRange { value: String },
    #[error("sequence keys must be built from items, not from a string value")]
    SequenceNotString,
}

/// Matching behaviour that depends on association negotiation or configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MatchOptions {
    /// Extended Negotiation of Empty Value Matching succeeded (PS3.4 C.2.2.2.7).
    pub empty_value_matching: bool,
    /// Extended Negotiation of Multiple Value Matching succeeded (PS3.4 C.2.2.2.8).
    pub multi_value_matching: bool,
    /// Match PN case-insensitively (permitted by PS3.4 C.2.2.2.1.1). Default: `true`.
    pub pn_case_insensitive: bool,
}

impl Default for MatchOptions {
    fn default() -> Self {
        Self {
            empty_value_matching: false,
            multi_value_matching: false,
            pn_case_insensitive: true,
        }
    }
}

/// One key attribute of a query: what to match and/or return.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Key {
    pub tag: Tag,
    pub vr: Vr,
    pub matcher: Matcher,
}

impl Key {
    /// Builds a key from its encoded value (see [`Matcher::parse`]).
    pub fn parse(tag: Tag, vr: Vr, raw: &str, options: &MatchOptions) -> Result<Self, QueryError> {
        Ok(Self {
            tag,
            vr,
            matcher: Matcher::parse(vr, raw, options)?,
        })
    }

    /// A key that only requests the attribute to be returned.
    pub fn universal(tag: Tag, vr: Vr) -> Self {
        Self {
            tag,
            vr,
            matcher: Matcher::Universal,
        }
    }

    pub fn matches(&self, entity: &dyn Attributes, options: &MatchOptions) -> bool {
        self.matcher.matches(self, entity, options)
    }
}

/// Sort criteria. Attribute ordering compares the stored string values; the other keys are
/// bookkeeping fields maintained by the index (used e.g. by eviction policies).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderKey {
    Attribute(Tag),
    ReceivedAt,
    LastAccessedAt,
    Size,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderBy {
    pub key: OrderKey,
    #[serde(default)]
    pub descending: bool,
}

/// A query at one level of the hierarchy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Query {
    pub level: QueryLevel,
    pub keys: Vec<Key>,
    #[serde(default)]
    pub options: MatchOptions,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub order_by: Vec<OrderBy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    #[serde(default)]
    pub offset: u64,
}

impl Query {
    pub fn new(level: QueryLevel) -> Self {
        Self {
            level,
            keys: Vec::new(),
            options: MatchOptions::default(),
            order_by: Vec::new(),
            limit: None,
            offset: 0,
        }
    }

    #[must_use]
    pub fn with_key(mut self, key: Key) -> Self {
        self.keys.push(key);
        self
    }

    /// `true` if `entity` satisfies every key.
    pub fn matches(&self, entity: &dyn Attributes) -> bool {
        self.keys.iter().all(|k| k.matches(entity, &self.options))
    }

    /// Keys that actually constrain the result (i.e. not universal).
    pub fn constraints(&self) -> impl Iterator<Item = &Key> {
        self.keys.iter().filter(|k| !k.matcher.is_universal())
    }

    /// Attributes requested for return.
    pub fn return_tags(&self) -> impl Iterator<Item = Tag> + '_ {
        self.keys.iter().map(|k| k.tag)
    }
}

/// Read access to the attributes of an entity, used to evaluate matchers.
///
/// Values are already decoded to Unicode; character set handling happens at the protocol edge.
pub trait Attributes {
    /// The values of `tag`, or `None` if the attribute is absent. A present but empty attribute is
    /// `Some(vec![])`.
    fn values(&self, tag: Tag) -> Option<Vec<Cow<'_, str>>>;

    /// The items of sequence `tag` (empty if absent or not a sequence).
    fn items(&self, tag: Tag) -> Vec<&dyn Attributes>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tags;
    use serde_json::json;

    fn entity() -> serde_json::Value {
        json!({
            "00100010": { "vr": "PN", "Value": [{ "Alphabetic": "Doe^John" }] },
            "00100020": { "vr": "LO", "Value": ["PID-1"] },
            "00080020": { "vr": "DA", "Value": ["20240315"] },
            "00080030": { "vr": "TM", "Value": ["101500"] },
            "00080061": { "vr": "CS", "Value": ["CT", "SR"] },
            "00201208": { "vr": "IS", "Value": [42] },
            "00081030": { "vr": "LO" },
            "00081032": { "vr": "SQ", "Value": [
                { "00080100": { "vr": "SH", "Value": ["ABC"] }, "00080102": { "vr": "SH", "Value": ["L"] } },
                { "00080100": { "vr": "SH", "Value": ["XYZ"] }, "00080102": { "vr": "SH", "Value": ["99"] } }
            ]}
        })
    }

    fn q(keys: &[(Tag, Vr, &str)]) -> Query {
        let o = MatchOptions::default();
        keys.iter()
            .fold(Query::new(QueryLevel::Study), |q, (t, vr, v)| {
                q.with_key(Key::parse(*t, *vr, v, &o).unwrap())
            })
    }

    fn check(query: &Query) -> bool {
        let e = entity();
        query.matches(&DicomJson::new(e.as_object().unwrap()))
    }

    #[test]
    fn matches_against_dicom_json() {
        assert!(check(&q(&[(tags::PATIENT_NAME, Vr::PN, "doe^john")])));
        assert!(check(&q(&[(tags::PATIENT_NAME, Vr::PN, "DOE*")])));
        assert!(!check(&q(&[(tags::PATIENT_NAME, Vr::PN, "Smith*")])));
        assert!(check(&q(&[(tags::PATIENT_ID, Vr::LO, "PID-1")])));
        assert!(!check(&q(&[(tags::PATIENT_ID, Vr::LO, "pid-1")])));
        assert!(check(&q(&[(
            tags::STUDY_DATE,
            Vr::DA,
            "20240301-20240331"
        )])));
        assert!(check(&q(&[(tags::STUDY_DATE, Vr::DA, "-20240315")])));
        assert!(!check(&q(&[(tags::STUDY_DATE, Vr::DA, "20240316-")])));
        assert!(check(&q(&[(tags::STUDY_TIME, Vr::TM, "10-11")])));
        assert!(check(&q(&[(tags::STUDY_TIME, Vr::TM, "1015")])));
        assert!(check(&q(&[(tags::MODALITIES_IN_STUDY, Vr::CS, "SR")])));
        assert!(check(&q(&[(tags::MODALITIES_IN_STUDY, Vr::CS, "MR\\CT")])));
        assert!(check(&q(&[(
            tags::NUMBER_OF_STUDY_RELATED_INSTANCES,
            Vr::IS,
            "+42"
        )])));
        // universal keys always match, also for absent attributes
        assert!(check(&q(&[(tags::ACCESSION_NUMBER, Vr::SH, "")])));
        // a constraint on an absent / empty attribute does not match
        assert!(!check(&q(&[(tags::ACCESSION_NUMBER, Vr::SH, "A1")])));
        assert!(!check(&q(&[(tags::STUDY_DESCRIPTION, Vr::LO, "x*")])));
    }

    #[test]
    fn empty_value_matching() {
        let o = MatchOptions {
            empty_value_matching: true,
            ..MatchOptions::default()
        };
        let mut query = Query::new(QueryLevel::Study);
        query.options = o.clone();
        query
            .keys
            .push(Key::parse(tags::STUDY_DESCRIPTION, Vr::LO, "\"\"", &o).unwrap());
        query
            .keys
            .push(Key::parse(tags::ACCESSION_NUMBER, Vr::SH, "\"\"", &o).unwrap());
        assert!(check(&query));
    }

    #[test]
    fn sequence_matching_is_per_item() {
        let item = |code: &str, scheme: &str| {
            let o = MatchOptions::default();
            Query::new(QueryLevel::Study).with_key(Key {
                tag: Tag::new(0x0008, 0x1032),
                vr: Vr::SQ,
                matcher: Matcher::Sequence {
                    keys: vec![
                        Key::parse(Tag::new(0x0008, 0x0100), Vr::SH, code, &o).unwrap(),
                        Key::parse(Tag::new(0x0008, 0x0102), Vr::SH, scheme, &o).unwrap(),
                    ],
                },
            })
        };
        assert!(check(&item("ABC", "L")));
        assert!(check(&item("XYZ", "")));
        // values from different items must not combine
        assert!(!check(&item("ABC", "99")));
    }

    #[test]
    fn serde_roundtrip() {
        let query = q(&[
            (tags::PATIENT_NAME, Vr::PN, "Doe*"),
            (tags::STUDY_DATE, Vr::DA, "20240101-"),
        ]);
        let s = serde_json::to_string(&query).unwrap();
        assert!(s.contains("\"type\":\"wildcard\""));
        let back: Query = serde_json::from_str(&s).unwrap();
        assert_eq!(back, query);
    }
}
