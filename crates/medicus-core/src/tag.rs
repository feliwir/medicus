//! Attribute tags and value representations.
//!
//! medicus uses dicom-rs's [`Tag`] and [`VR`] and its standard dictionary. This module adds what
//! medicus needs on top: the DICOM JSON key format of tags (PS3.18 F.2, `"GGGGEEEE"`), serde
//! helpers for the wire formats used over the addon ABI (dicom-core has no serde support), the
//! VR-dependent matching rules of PS3.4 C.2.2.2, and the table of standard attributes the index
//! knows ([`tags::STANDARD`]).

use std::fmt;

pub use dicom_core::{Tag, VR};

/// DICOM JSON representation of tags.
pub trait TagExt {
    /// The tag as a DICOM JSON key: eight uppercase hex digits, `"GGGGEEEE"`.
    fn json_key(self) -> String;
}

impl TagExt for Tag {
    fn json_key(self) -> String {
        format!("{:04X}{:04X}", self.0, self.1)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid tag {0:?}: expected 8 hex digits (GGGGEEEE) or (GGGG,EEEE)")]
pub struct TagParseError(pub String);

/// Parses `GGGGEEEE` (DICOM JSON / QIDO-RS) and `(GGGG,EEEE)` / `GGGG,EEEE`.
pub fn parse_tag(s: &str) -> Result<Tag, TagParseError> {
    let err = || TagParseError(s.to_owned());
    let inner = s.trim().trim_start_matches('(').trim_end_matches(')');
    let (g, e) = match inner.split_once(',') {
        Some(parts) => parts,
        None if inner.len() == 8 && inner.is_ascii() => inner.split_at(4),
        None => return Err(err()),
    };
    if g.len() != 4 || e.len() != 4 {
        return Err(err());
    }
    let group = u16::from_str_radix(g, 16).map_err(|_| err())?;
    let element = u16::from_str_radix(e, 16).map_err(|_| err())?;
    Ok(Tag(group, element))
}

/// The matching rules of PS3.4 C.2.2.2 that depend on the VR.
pub trait VrExt {
    /// `*` and `?` request Wild Card Matching (C.2.2.2.4).
    fn supports_wildcards(self) -> bool;
    /// `-` requests Range Matching (C.2.2.2.5).
    fn supports_ranges(self) -> bool;
    /// `""` requests Empty Value Matching when negotiated (C.2.2.2.7).
    fn supports_empty_value_matching(self) -> bool;
    /// Eligible for Multiple Value Matching when negotiated (C.2.2.2.8).
    fn supports_multi_value_matching(self) -> bool;
    /// Text VRs whose leading spaces are significant and must not be trimmed.
    fn leading_space_significant(self) -> bool;
}

impl VrExt for VR {
    fn supports_wildcards(self) -> bool {
        use VR::{AE, CS, LO, LT, PN, SH, ST, UC, UR, UT};
        matches!(self, AE | CS | LO | LT | PN | SH | ST | UC | UR | UT)
    }

    fn supports_ranges(self) -> bool {
        matches!(self, VR::DA | VR::TM | VR::DT)
    }

    fn supports_empty_value_matching(self) -> bool {
        use VR::{AE, CS, DA, DT, LO, LT, PN, SH, ST, TM, UC, UR, UT};
        matches!(
            self,
            AE | CS | DA | DT | LO | LT | PN | SH | ST | TM | UC | UR | UT
        )
    }

    fn supports_multi_value_matching(self) -> bool {
        use VR::{AE, AS, AT, CS, LO, PN, SH, UC};
        matches!(self, AE | AS | AT | CS | LO | PN | SH | UC)
    }

    fn leading_space_significant(self) -> bool {
        matches!(self, VR::LT | VR::ST | VR::UT | VR::UR)
    }
}

/// `#[serde(with = "serde_tag")]`: a tag as its DICOM JSON key.
pub mod serde_tag {
    use super::{Tag, TagExt as _, parse_tag};
    use serde::{Deserialize, Deserializer, Serializer, de::Error as _};

    pub fn serialize<S: Serializer>(tag: &Tag, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&tag.json_key())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Tag, D::Error> {
        parse_tag(&String::deserialize(d)?).map_err(D::Error::custom)
    }
}

/// `#[serde(with = "serde_tags")]`: a list of tags as DICOM JSON keys.
pub mod serde_tags {
    use super::{Tag, TagExt as _, parse_tag};
    use serde::{Deserialize, Deserializer, Serializer, de::Error as _, ser::SerializeSeq as _};

    pub fn serialize<S: Serializer>(tags: &[Tag], s: S) -> Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(Some(tags.len()))?;
        for t in tags {
            seq.serialize_element(&t.json_key())?;
        }
        seq.end()
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Tag>, D::Error> {
        Vec::<String>::deserialize(d)?
            .iter()
            .map(|s| parse_tag(s).map_err(D::Error::custom))
            .collect()
    }
}

/// `#[serde(with = "serde_vr")]`: a VR as its two-letter code.
pub mod serde_vr {
    use super::VR;
    use serde::{Deserialize, Deserializer, Serializer, de::Error as _};

    pub fn serialize<S: Serializer>(vr: &VR, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(VR::to_string(*vr))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<VR, D::Error> {
        let s = String::deserialize(d)?;
        s.parse()
            .map_err(|_| D::Error::custom(format!("unknown VR {s:?}")))
    }
}

/// Displays a tag together with its dictionary keyword, e.g. `PatientName (0010,0010)`.
#[derive(Debug, Clone, Copy)]
pub struct Named(pub Tag);

impl fmt::Display for Named {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match tags::keyword(self.0) {
            Some(k) => write!(f, "{k} {}", self.0),
            None => write!(f, "{}", self.0),
        }
    }
}

/// The dicom-rs standard dictionary's tag constants, plus the attributes the index knows.
pub mod tags {
    use dicom_core::dictionary::{DataDictionary as _, DataDictionaryEntry as _, TagRange};
    use dicom_dictionary_std::StandardDataDictionary;
    pub use dicom_dictionary_std::tags::*;

    use super::{Tag, VR};
    use crate::QueryLevel;

    /// A standard attribute the index stores and matches (PS3.4 C.6 query keys).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Attribute {
        pub tag: Tag,
        pub vr: VR,
        pub level: QueryLevel,
    }

    macro_rules! standard {
        ($($name:ident: $vr:ident, $lvl:ident;)*) => {
            /// All standard attributes, in hierarchy order.
            pub const STANDARD: &[Attribute] = &[
                $(Attribute { tag: $name, vr: VR::$vr, level: QueryLevel::$lvl },)*
            ];
        };
    }

    standard! {
        PATIENT_NAME: PN, Patient;
        PATIENT_ID: LO, Patient;
        ISSUER_OF_PATIENT_ID: LO, Patient;
        PATIENT_BIRTH_DATE: DA, Patient;
        PATIENT_SEX: CS, Patient;
        NUMBER_OF_PATIENT_RELATED_STUDIES: IS, Patient;
        STUDY_INSTANCE_UID: UI, Study;
        STUDY_DATE: DA, Study;
        STUDY_TIME: TM, Study;
        ACCESSION_NUMBER: SH, Study;
        STUDY_ID: SH, Study;
        REFERRING_PHYSICIAN_NAME: PN, Study;
        STUDY_DESCRIPTION: LO, Study;
        MODALITIES_IN_STUDY: CS, Study;
        SOP_CLASSES_IN_STUDY: UI, Study;
        NUMBER_OF_STUDY_RELATED_SERIES: IS, Study;
        NUMBER_OF_STUDY_RELATED_INSTANCES: IS, Study;
        INSTITUTION_NAME: LO, Study;
        SERIES_INSTANCE_UID: UI, Series;
        MODALITY: CS, Series;
        SERIES_NUMBER: IS, Series;
        SERIES_DESCRIPTION: LO, Series;
        SERIES_DATE: DA, Series;
        SERIES_TIME: TM, Series;
        BODY_PART_EXAMINED: CS, Series;
        STATION_NAME: SH, Series;
        PERFORMED_PROCEDURE_STEP_START_DATE: DA, Series;
        NUMBER_OF_SERIES_RELATED_INSTANCES: IS, Series;
        SOP_INSTANCE_UID: UI, Instance;
        SOP_CLASS_UID: UI, Instance;
        INSTANCE_NUMBER: IS, Instance;
        CONTENT_DATE: DA, Instance;
        CONTENT_TIME: TM, Instance;
        NUMBER_OF_FRAMES: IS, Instance;
    }

    /// The VR of a standard attribute (usable in constant contexts).
    pub const fn standard_vr(tag: Tag) -> Option<VR> {
        let mut i = 0;
        while i < STANDARD.len() {
            let a = &STANDARD[i];
            if a.tag.0 == tag.0 && a.tag.1 == tag.1 {
                return Some(a.vr);
            }
            i += 1;
        }
        None
    }

    /// Looks up a standard attribute by tag.
    pub fn standard(tag: Tag) -> Option<&'static Attribute> {
        STANDARD.iter().find(|a| a.tag == tag)
    }

    /// Looks up a standard attribute by keyword (as used by QIDO-RS).
    pub fn by_keyword(keyword: &str) -> Option<&'static Attribute> {
        match StandardDataDictionary.by_name(keyword)?.tag_range() {
            TagRange::Single(tag) => standard(tag),
            _ => None,
        }
    }

    /// The dictionary keyword of any tag (e.g. `PatientName`).
    pub fn keyword(tag: Tag) -> Option<&'static str> {
        StandardDataDictionary
            .by_tag(tag)
            .map(dicom_core::dictionary::DataDictionaryEntry::alias)
    }

    /// The unique key of a level.
    pub fn unique_key(level: QueryLevel) -> Tag {
        match level {
            QueryLevel::Patient => PATIENT_ID,
            QueryLevel::Study => STUDY_INSTANCE_UID,
            QueryLevel::Series => SERIES_INSTANCE_UID,
            QueryLevel::Instance => SOP_INSTANCE_UID,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::QueryLevel;
    use dicom_core::dictionary::{DataDictionary as _, DataDictionaryEntry as _};
    use dicom_dictionary_std::StandardDataDictionary;

    #[test]
    fn tag_formats() {
        let t = tags::STUDY_INSTANCE_UID;
        assert_eq!(t.to_string(), "(0020,000D)");
        assert_eq!(t.json_key(), "0020000D");
        assert_eq!(parse_tag("0020000D").unwrap(), t);
        assert_eq!(parse_tag("(0020,000d)").unwrap(), t);
        assert_eq!(parse_tag("0020,000D").unwrap(), t);
        assert!(parse_tag("0020000").is_err());
        assert!(parse_tag("XYZW000D").is_err());
        assert!(parse_tag("ÄÄÄÄ").is_err());
        assert_eq!(
            Named(tags::PATIENT_NAME).to_string(),
            "PatientName (0010,0010)"
        );
    }

    #[test]
    fn serde_wire_formats() {
        #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
        struct W {
            #[serde(with = "serde_tag")]
            t: Tag,
            #[serde(with = "serde_tags")]
            ts: Vec<Tag>,
            #[serde(with = "serde_vr")]
            vr: VR,
        }
        let w = W {
            t: tags::MODALITY,
            ts: vec![tags::PATIENT_ID],
            vr: VR::PN,
        };
        let json = serde_json::to_string(&w).unwrap();
        assert_eq!(json, r#"{"t":"00080060","ts":["00100020"],"vr":"PN"}"#);
        assert_eq!(serde_json::from_str::<W>(&json).unwrap(), w);
        assert!(serde_json::from_str::<W>(r#"{"t":"x","ts":[],"vr":"PN"}"#).is_err());
        assert!(serde_json::from_str::<W>(r#"{"t":"00080060","ts":[],"vr":"QQ"}"#).is_err());
    }

    #[test]
    fn standard_attributes_are_unique_and_match_the_dictionary() {
        for (i, a) in tags::STANDARD.iter().enumerate() {
            assert!(tags::STANDARD[i + 1..].iter().all(|b| b.tag != a.tag));
            let entry = StandardDataDictionary
                .by_tag(a.tag)
                .expect("standard tag in dictionary");
            assert_eq!(entry.vr().relaxed(), a.vr, "{}", Named(a.tag));
            assert_eq!(tags::by_keyword(entry.alias()).unwrap().tag, a.tag);
        }
        assert_eq!(tags::by_keyword("Modality").unwrap().vr, VR::CS);
        assert!(
            tags::by_keyword("SliceThickness").is_none(),
            "not a standard index attribute"
        );
        assert_eq!(
            tags::standard(tags::PATIENT_NAME).unwrap().level,
            QueryLevel::Patient
        );
        assert_eq!(tags::standard_vr(tags::STUDY_DATE), Some(VR::DA));
    }
}
