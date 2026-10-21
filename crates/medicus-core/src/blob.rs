//! Opaque keys of objects in a `BlobStore`.
//!
//! Keys are never derived from UIDs or patient data: they must not leak PHI into file names or
//! object-store logs, and must be valid on every filesystem (no Windows reserved names, no case
//! collisions, short paths). Generated keys are fanned out over two directory levels.

use std::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const MAX_LEN: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BlobKeyError {
    #[error("blob key is empty")]
    Empty,
    #[error("blob key is longer than {MAX_LEN} bytes")]
    TooLong,
    #[error("blob key contains invalid character {0:?} (allowed: a-z 0-9 . _ - /)")]
    InvalidChar(char),
    #[error("blob key has an empty, '.' or '..' path segment")]
    InvalidSegment,
}

/// A validated, relative, `/`-separated key using only `[a-z0-9._-]` in each segment.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct BlobKey(String);

/// What a blob contains; determines the key suffix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlobKind {
    /// A DICOM Part 10 file.
    Instance,
    /// Derived cache: the concatenated binary headers of a series' instances.
    SeriesMetadata,
}

impl BlobKind {
    fn suffix(self) -> &'static str {
        match self {
            Self::Instance => "dcm",
            Self::SeriesMetadata => "series",
        }
    }
}

impl BlobKey {
    pub fn parse(raw: &str) -> Result<Self, BlobKeyError> {
        if raw.is_empty() {
            return Err(BlobKeyError::Empty);
        }
        if raw.len() > MAX_LEN {
            return Err(BlobKeyError::TooLong);
        }
        if let Some(c) = raw.chars().find(|c| {
            !(c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-' | '/'))
        }) {
            return Err(BlobKeyError::InvalidChar(c));
        }
        if raw
            .split('/')
            .any(|seg| seg.is_empty() || seg == "." || seg == "..")
        {
            return Err(BlobKeyError::InvalidSegment);
        }
        Ok(Self(raw.to_owned()))
    }

    /// A new unique key of the form `ab/cd/<uuid>.<suffix>`.
    ///
    /// The fan-out uses the *random* tail of a `UUIDv7` (the head is a timestamp and would put all
    /// recent objects into the same directory), while the full UUID keeps keys roughly time-ordered.
    pub fn generate(kind: BlobKind) -> Self {
        let id = Uuid::now_v7().simple().to_string();
        let n = id.len();
        Self(format!(
            "{}/{}/{id}.{}",
            &id[n - 2..],
            &id[n - 4..n - 2],
            kind.suffix()
        ))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BlobKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for BlobKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BlobKey({})", self.0)
    }
}

impl TryFrom<String> for BlobKey {
    type Error = BlobKeyError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::parse(&s)
    }
}

impl From<BlobKey> for String {
    fn from(k: BlobKey) -> Self {
        k.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_keys_are_valid_and_fanned_out() {
        let a = BlobKey::generate(BlobKind::Instance);
        let b = BlobKey::generate(BlobKind::Instance);
        assert_ne!(a, b);
        assert_eq!(BlobKey::parse(a.as_str()).unwrap(), a);
        let parts: Vec<_> = a.as_str().split('/').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(std::path::Path::new(parts[2]).extension().unwrap(), "dcm");
        assert!(parts[2][..32].ends_with(&format!("{}{}", parts[1], parts[0])));
    }

    #[test]
    fn rejects_unsafe_keys() {
        assert_eq!(BlobKey::parse(""), Err(BlobKeyError::Empty));
        assert_eq!(
            BlobKey::parse("../etc/passwd"),
            Err(BlobKeyError::InvalidSegment)
        );
        assert_eq!(BlobKey::parse("/abs"), Err(BlobKeyError::InvalidSegment));
        assert_eq!(BlobKey::parse("a//b"), Err(BlobKeyError::InvalidSegment));
        assert_eq!(BlobKey::parse("a\\b"), Err(BlobKeyError::InvalidChar('\\')));
        assert_eq!(BlobKey::parse("Ab"), Err(BlobKeyError::InvalidChar('A')));
        assert_eq!(BlobKey::parse("a:b"), Err(BlobKeyError::InvalidChar(':')));
        assert!(BlobKey::parse("ab/cd/file.series.json").is_ok());
    }
}
