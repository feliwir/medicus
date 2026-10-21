//! Attribute tags and value representations.
//!
//! These are small, dependency-free definitions with a stable serde format: tags serialize like
//! the keys of DICOM JSON (PS3.18 F.2), i.e. as eight uppercase hex digits `"GGGGEEEE"`.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// A DICOM attribute tag `(gggg,eeee)`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Tag {
    pub group: u16,
    pub element: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid tag {0:?}: expected 8 hex digits (GGGGEEEE) or (GGGG,EEEE)")]
pub struct TagParseError(pub String);

impl Tag {
    pub const fn new(group: u16, element: u16) -> Self {
        Self { group, element }
    }

    /// Private tags have an odd group number.
    pub const fn is_private(self) -> bool {
        self.group % 2 == 1
    }
}

impl fmt::Display for Tag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "({:04X},{:04X})", self.group, self.element)
    }
}

impl fmt::Debug for Tag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl FromStr for Tag {
    type Err = TagParseError;

    /// Accepts `GGGGEEEE` (DICOM JSON / QIDO) and `(GGGG,EEEE)` / `GGGG,EEEE`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || TagParseError(s.to_owned());
        let inner = s.trim().trim_start_matches('(').trim_end_matches(')');
        let (g, e) = match inner.split_once(',') {
            Some((g, e)) => (g, e),
            None if inner.len() == 8 => inner.split_at(4),
            None => return Err(err()),
        };
        if g.len() != 4 || e.len() != 4 {
            return Err(err());
        }
        let group = u16::from_str_radix(g, 16).map_err(|_| err())?;
        let element = u16::from_str_radix(e, 16).map_err(|_| err())?;
        Ok(Self { group, element })
    }
}

impl TryFrom<String> for Tag {
    type Error = TagParseError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<Tag> for String {
    fn from(t: Tag) -> Self {
        format!("{:04X}{:04X}", t.group, t.element)
    }
}

/// Value Representation (PS3.5 §6.2).
#[allow(clippy::upper_case_acronyms)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Vr {
    AE,
    AS,
    AT,
    CS,
    DA,
    DS,
    DT,
    FD,
    FL,
    IS,
    LO,
    LT,
    OB,
    OD,
    OF,
    OL,
    OV,
    OW,
    PN,
    SH,
    SL,
    SQ,
    SS,
    ST,
    SV,
    TM,
    UC,
    UI,
    UL,
    UN,
    UR,
    US,
    UT,
    UV,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown VR {0:?}")]
pub struct UnknownVr(pub String);

impl Vr {
    /// VRs for which `*` and `?` request Wild Card Matching (PS3.4 C.2.2.2.4).
    pub fn supports_wildcards(self) -> bool {
        use Vr::{AE, CS, LO, LT, PN, SH, ST, UC, UR, UT};
        matches!(self, AE | CS | LO | LT | PN | SH | ST | UC | UR | UT)
    }

    /// VRs for which `"-"` requests Range Matching (PS3.4 C.2.2.2.5).
    pub fn supports_ranges(self) -> bool {
        matches!(self, Vr::DA | Vr::TM | Vr::DT)
    }

    /// VRs for which `""` requests Empty Value Matching when negotiated (PS3.4 C.2.2.2.7).
    pub fn supports_empty_value_matching(self) -> bool {
        use Vr::{AE, CS, DA, DT, LO, LT, PN, SH, ST, TM, UC, UR, UT};
        matches!(
            self,
            AE | CS | DA | DT | LO | LT | PN | SH | ST | TM | UC | UR | UT
        )
    }

    /// VRs eligible for Multiple Value Matching when negotiated (PS3.4 C.2.2.2.8).
    pub fn supports_multi_value_matching(self) -> bool {
        use Vr::{AE, AS, AT, CS, LO, PN, SH, UC};
        matches!(self, AE | AS | AT | CS | LO | PN | SH | UC)
    }

    /// Text VRs whose leading spaces are significant and must not be trimmed.
    pub fn leading_space_significant(self) -> bool {
        matches!(self, Vr::LT | Vr::ST | Vr::UT | Vr::UR)
    }

    pub fn as_str(self) -> &'static str {
        // Debug output is exactly the two-letter code.
        VR_NAMES[self as usize]
    }
}

const VR_NAMES: [&str; 34] = [
    "AE", "AS", "AT", "CS", "DA", "DS", "DT", "FD", "FL", "IS", "LO", "LT", "OB", "OD", "OF", "OL",
    "OV", "OW", "PN", "SH", "SL", "SQ", "SS", "ST", "SV", "TM", "UC", "UI", "UL", "UN", "UR", "US",
    "UT", "UV",
];

const VR_ALL: [Vr; 34] = {
    #[allow(clippy::enum_glob_use)]
    use Vr::*;
    [
        AE, AS, AT, CS, DA, DS, DT, FD, FL, IS, LO, LT, OB, OD, OF, OL, OV, OW, PN, SH, SL, SQ, SS,
        ST, SV, TM, UC, UI, UL, UN, UR, US, UT, UV,
    ]
};

impl fmt::Display for Vr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Vr {
    type Err = UnknownVr;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        VR_NAMES
            .iter()
            .position(|n| *n == s)
            .map(|i| VR_ALL[i])
            .ok_or_else(|| UnknownVr(s.to_owned()))
    }
}

/// Well-known attributes used by the core (query keys of PS3.4 C.6 and identifiers).
///
/// Each entry carries its VR and the hierarchy level it belongs to, so that front-ends and
/// index backends agree on how standard keys are matched and stored.
pub mod tags {
    use super::{Tag, Vr};
    use crate::QueryLevel;

    /// A standard attribute known to the core.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Attribute {
        pub tag: Tag,
        pub keyword: &'static str,
        pub vr: Vr,
        pub level: QueryLevel,
    }

    macro_rules! attrs {
        ($($name:ident = ($g:literal, $e:literal), $kw:literal, $vr:ident, $lvl:ident;)*) => {
            $(pub const $name: Tag = Tag::new($g, $e);)*

            /// All standard attributes, in hierarchy order.
            pub const STANDARD: &[Attribute] = &[
                $(Attribute { tag: $name, keyword: $kw, vr: Vr::$vr, level: QueryLevel::$lvl },)*
            ];
        };
    }

    attrs! {
        // Patient
        PATIENT_NAME = (0x0010, 0x0010), "PatientName", PN, Patient;
        PATIENT_ID = (0x0010, 0x0020), "PatientID", LO, Patient;
        ISSUER_OF_PATIENT_ID = (0x0010, 0x0021), "IssuerOfPatientID", LO, Patient;
        PATIENT_BIRTH_DATE = (0x0010, 0x0030), "PatientBirthDate", DA, Patient;
        PATIENT_SEX = (0x0010, 0x0040), "PatientSex", CS, Patient;
        NUMBER_OF_PATIENT_RELATED_STUDIES = (0x0020, 0x1200), "NumberOfPatientRelatedStudies", IS, Patient;
        // Study
        STUDY_INSTANCE_UID = (0x0020, 0x000D), "StudyInstanceUID", UI, Study;
        STUDY_DATE = (0x0008, 0x0020), "StudyDate", DA, Study;
        STUDY_TIME = (0x0008, 0x0030), "StudyTime", TM, Study;
        ACCESSION_NUMBER = (0x0008, 0x0050), "AccessionNumber", SH, Study;
        STUDY_ID = (0x0020, 0x0010), "StudyID", SH, Study;
        REFERRING_PHYSICIAN_NAME = (0x0008, 0x0090), "ReferringPhysicianName", PN, Study;
        STUDY_DESCRIPTION = (0x0008, 0x1030), "StudyDescription", LO, Study;
        MODALITIES_IN_STUDY = (0x0008, 0x0061), "ModalitiesInStudy", CS, Study;
        SOP_CLASSES_IN_STUDY = (0x0008, 0x0062), "SOPClassesInStudy", UI, Study;
        NUMBER_OF_STUDY_RELATED_SERIES = (0x0020, 0x1206), "NumberOfStudyRelatedSeries", IS, Study;
        NUMBER_OF_STUDY_RELATED_INSTANCES = (0x0020, 0x1208), "NumberOfStudyRelatedInstances", IS, Study;
        INSTITUTION_NAME = (0x0008, 0x0080), "InstitutionName", LO, Study;
        // Series
        SERIES_INSTANCE_UID = (0x0020, 0x000E), "SeriesInstanceUID", UI, Series;
        MODALITY = (0x0008, 0x0060), "Modality", CS, Series;
        SERIES_NUMBER = (0x0020, 0x0011), "SeriesNumber", IS, Series;
        SERIES_DESCRIPTION = (0x0008, 0x103E), "SeriesDescription", LO, Series;
        SERIES_DATE = (0x0008, 0x0021), "SeriesDate", DA, Series;
        SERIES_TIME = (0x0008, 0x0031), "SeriesTime", TM, Series;
        BODY_PART_EXAMINED = (0x0018, 0x0015), "BodyPartExamined", CS, Series;
        STATION_NAME = (0x0008, 0x1010), "StationName", SH, Series;
        PERFORMED_PROCEDURE_STEP_START_DATE = (0x0040, 0x0244), "PerformedProcedureStepStartDate", DA, Series;
        NUMBER_OF_SERIES_RELATED_INSTANCES = (0x0020, 0x1209), "NumberOfSeriesRelatedInstances", IS, Series;
        // Instance
        SOP_INSTANCE_UID = (0x0008, 0x0018), "SOPInstanceUID", UI, Instance;
        SOP_CLASS_UID = (0x0008, 0x0016), "SOPClassUID", UI, Instance;
        INSTANCE_NUMBER = (0x0020, 0x0013), "InstanceNumber", IS, Instance;
        CONTENT_DATE = (0x0008, 0x0023), "ContentDate", DA, Instance;
        CONTENT_TIME = (0x0008, 0x0033), "ContentTime", TM, Instance;
        NUMBER_OF_FRAMES = (0x0028, 0x0008), "NumberOfFrames", IS, Instance;
    }

    /// Query/Retrieve control attributes (not stored per level).
    pub const QUERY_RETRIEVE_LEVEL: Tag = Tag::new(0x0008, 0x0052);
    pub const SPECIFIC_CHARACTER_SET: Tag = Tag::new(0x0008, 0x0005);
    pub const TIMEZONE_OFFSET_FROM_UTC: Tag = Tag::new(0x0008, 0x0201);
    pub const RETRIEVE_AE_TITLE: Tag = Tag::new(0x0008, 0x0054);
    pub const INSTANCE_AVAILABILITY: Tag = Tag::new(0x0008, 0x0056);
    pub const TRANSFER_SYNTAX_UID: Tag = Tag::new(0x0002, 0x0010);
    pub const PIXEL_DATA: Tag = Tag::new(0x7FE0, 0x0010);

    /// Looks up a standard attribute by tag.
    pub fn standard(tag: Tag) -> Option<&'static Attribute> {
        STANDARD.iter().find(|a| a.tag == tag)
    }

    /// Looks up a standard attribute by keyword (as used by QIDO-RS).
    pub fn by_keyword(keyword: &str) -> Option<&'static Attribute> {
        STANDARD.iter().find(|a| a.keyword == keyword)
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

    #[test]
    fn tag_formats() {
        let t = tags::STUDY_INSTANCE_UID;
        assert_eq!(t.to_string(), "(0020,000D)");
        assert_eq!(String::from(t), "0020000D");
        assert_eq!("0020000D".parse::<Tag>().unwrap(), t);
        assert_eq!("(0020,000d)".parse::<Tag>().unwrap(), t);
        assert_eq!("0020,000D".parse::<Tag>().unwrap(), t);
        assert!("0020000".parse::<Tag>().is_err());
        assert!("XYZW000D".parse::<Tag>().is_err());
        assert_eq!(serde_json::to_string(&t).unwrap(), "\"0020000D\"");
    }

    #[test]
    fn vr_names_are_consistent() {
        for (i, vr) in VR_ALL.iter().enumerate() {
            assert_eq!(format!("{vr:?}"), VR_NAMES[i]);
            assert_eq!(vr.as_str().parse::<Vr>().unwrap(), *vr);
        }
    }

    #[test]
    fn standard_attributes_are_unique() {
        for (i, a) in tags::STANDARD.iter().enumerate() {
            assert!(
                tags::STANDARD[i + 1..]
                    .iter()
                    .all(|b| b.tag != a.tag && b.keyword != a.keyword)
            );
        }
        assert_eq!(tags::by_keyword("Modality").unwrap().vr, Vr::CS);
        assert_eq!(
            tags::standard(tags::PATIENT_NAME).unwrap().level,
            QueryLevel::Patient
        );
    }
}
