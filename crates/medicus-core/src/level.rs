//! Levels of the Patient → Study → Series → Instance hierarchy.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QueryLevel {
    Patient,
    Study,
    Series,
    Instance,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown query/retrieve level {0:?}")]
pub struct UnknownLevel(pub String);

impl QueryLevel {
    pub const ALL: [Self; 4] = [Self::Patient, Self::Study, Self::Series, Self::Instance];

    /// The value used in Query/Retrieve Level (0008,0052).
    pub fn as_dicom(self) -> &'static str {
        match self {
            Self::Patient => "PATIENT",
            Self::Study => "STUDY",
            Self::Series => "SERIES",
            Self::Instance => "IMAGE",
        }
    }

    /// Parses a Query/Retrieve Level (0008,0052) value. `INSTANCE` is accepted as an alias of
    /// `IMAGE`; surrounding padding is ignored.
    pub fn from_dicom(raw: &str) -> Result<Self, UnknownLevel> {
        match raw.trim_matches([' ', '\0']) {
            "PATIENT" => Ok(Self::Patient),
            "STUDY" => Ok(Self::Study),
            "SERIES" => Ok(Self::Series),
            "IMAGE" | "INSTANCE" => Ok(Self::Instance),
            other => Err(UnknownLevel(other.to_owned())),
        }
    }

    /// The next level up, if any.
    pub fn parent(self) -> Option<Self> {
        match self {
            Self::Patient => None,
            Self::Study => Some(Self::Patient),
            Self::Series => Some(Self::Study),
            Self::Instance => Some(Self::Series),
        }
    }
}

impl fmt::Display for QueryLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_dicom())
    }
}

impl FromStr for QueryLevel {
    type Err = UnknownLevel;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_dicom(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dicom_roundtrip() {
        for l in QueryLevel::ALL {
            assert_eq!(QueryLevel::from_dicom(l.as_dicom()).unwrap(), l);
        }
        assert_eq!(QueryLevel::from_dicom("STUDY ").unwrap(), QueryLevel::Study);
        assert_eq!(
            QueryLevel::from_dicom("INSTANCE").unwrap(),
            QueryLevel::Instance
        );
        assert!(QueryLevel::from_dicom("study").is_err());
    }

    #[test]
    fn ordering_follows_hierarchy() {
        assert!(QueryLevel::Patient < QueryLevel::Study);
        assert!(QueryLevel::Series < QueryLevel::Instance);
        assert_eq!(QueryLevel::Instance.parent(), Some(QueryLevel::Series));
    }
}
