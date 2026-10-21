//! DICOM Unique Identifiers (PS3.5 §9).
//!
//! Parsing is *liberal in what it accepts* where real-world data commonly deviates from the
//! standard (trailing NUL/space padding, leading zeros in components), and strict where a
//! deviation would make the value ambiguous (characters other than digits and `.`, empty
//! components, more than 64 characters). [`Uid::is_strictly_conformant`] reports full conformance.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Maximum length of a UID (PS3.5 §9.1).
pub const MAX_LEN: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UidError {
    #[error("UID is empty")]
    Empty,
    #[error("UID is longer than {MAX_LEN} characters ({0})")]
    TooLong(usize),
    #[error("UID contains invalid character {0:?}")]
    InvalidChar(char),
    #[error("UID contains an empty component")]
    EmptyComponent,
}

/// A validated DICOM UID of any kind.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Uid(String);

impl Uid {
    /// Parses a UID, stripping trailing NUL and space padding.
    pub fn parse(raw: &str) -> Result<Self, UidError> {
        let s = raw.trim_end_matches(['\0', ' ']);
        if s.is_empty() {
            return Err(UidError::Empty);
        }
        if s.len() > MAX_LEN {
            return Err(UidError::TooLong(s.len()));
        }
        if let Some(c) = s.chars().find(|c| !c.is_ascii_digit() && *c != '.') {
            return Err(UidError::InvalidChar(c));
        }
        if s.split('.').any(str::is_empty) {
            return Err(UidError::EmptyComponent);
        }
        Ok(Self(s.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `true` if no component has a leading zero (the only rule [`Uid::parse`] does not enforce).
    pub fn is_strictly_conformant(&self) -> bool {
        self.0.split('.').all(|c| c == "0" || !c.starts_with('0'))
    }
}

impl fmt::Display for Uid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for Uid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Uid({})", self.0)
    }
}

impl FromStr for Uid {
    type Err = UidError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl TryFrom<String> for Uid {
    type Error = UidError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::parse(&s)
    }
}

impl From<Uid> for String {
    fn from(u: Uid) -> Self {
        u.0
    }
}

impl AsRef<str> for Uid {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Declares a role-specific UID newtype, so that e.g. a series UID cannot be passed where a
/// study UID is expected.
macro_rules! uid_newtype {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(Uid);

        impl $name {
            pub fn parse(raw: &str) -> Result<Self, UidError> {
                Uid::parse(raw).map(Self)
            }
            pub fn as_str(&self) -> &str {
                self.0.as_str()
            }
            pub fn as_uid(&self) -> &Uid {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }

        impl FromStr for $name {
            type Err = UidError;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Self::parse(s)
            }
        }

        impl From<Uid> for $name {
            fn from(u: Uid) -> Self {
                Self(u)
            }
        }

        impl From<$name> for Uid {
            fn from(u: $name) -> Self {
                u.0
            }
        }
    };
}

uid_newtype!(
    /// Study Instance UID (0020,000D).
    StudyInstanceUid
);
uid_newtype!(
    /// Series Instance UID (0020,000E).
    SeriesInstanceUid
);
uid_newtype!(
    /// SOP Instance UID (0008,0018).
    SopInstanceUid
);
uid_newtype!(
    /// SOP Class UID (0008,0016).
    SopClassUid
);
uid_newtype!(
    /// Transfer Syntax UID (0002,0010).
    TransferSyntaxUid
);

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn accepts_typical_uids() {
        for s in ["1.2.840.10008.1.2.1", "1.2.3", "2.25.123456789", "0.0"] {
            assert_eq!(Uid::parse(s).unwrap().as_str(), s);
        }
    }

    #[test]
    fn strips_padding() {
        assert_eq!(Uid::parse("1.2.3\0").unwrap().as_str(), "1.2.3");
        assert_eq!(Uid::parse("1.2.3 ").unwrap().as_str(), "1.2.3");
    }

    #[test]
    fn rejects_invalid() {
        assert_eq!(Uid::parse(""), Err(UidError::Empty));
        assert_eq!(Uid::parse("\0"), Err(UidError::Empty));
        assert_eq!(Uid::parse("1..2"), Err(UidError::EmptyComponent));
        assert_eq!(Uid::parse(".1.2"), Err(UidError::EmptyComponent));
        assert_eq!(Uid::parse("1.2."), Err(UidError::EmptyComponent));
        assert_eq!(Uid::parse("1.2.a"), Err(UidError::InvalidChar('a')));
        assert_eq!(Uid::parse(" 1.2"), Err(UidError::InvalidChar(' ')));
        let long = format!("1.{}", "2".repeat(63));
        assert_eq!(Uid::parse(&long), Err(UidError::TooLong(65)));
    }

    #[test]
    fn leading_zero_is_accepted_but_flagged() {
        let u = Uid::parse("1.02.3").unwrap();
        assert!(!u.is_strictly_conformant());
        assert!(Uid::parse("1.0.3").unwrap().is_strictly_conformant());
    }

    #[test]
    fn serde_roundtrip_validates() {
        let u: StudyInstanceUid = serde_json::from_str("\"1.2.3\"").unwrap();
        assert_eq!(serde_json::to_string(&u).unwrap(), "\"1.2.3\"");
        assert!(serde_json::from_str::<StudyInstanceUid>("\"1.x\"").is_err());
    }

    proptest! {
        #[test]
        fn valid_uids_roundtrip(parts in prop::collection::vec(0u32..100_000, 1..8)) {
            let s = parts.iter().map(u32::to_string).collect::<Vec<_>>().join(".");
            prop_assume!(s.len() <= MAX_LEN);
            let u = Uid::parse(&s).unwrap();
            prop_assert_eq!(u.as_str(), s.as_str());
            prop_assert!(u.is_strictly_conformant());
        }

        #[test]
        fn parse_never_panics(s in ".*") {
            let _ = Uid::parse(&s);
        }
    }
}
