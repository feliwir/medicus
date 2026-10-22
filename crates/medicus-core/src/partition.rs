//! Partitions: the unit of data separation (multi-tenancy, ADR-0003).
//!
//! Every patient, study, series, instance, blob, event and job belongs to exactly one partition.
//! UIDs are unique per partition only, and nothing is shared or queried across partitions.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

pub const MAX_LEN: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PartitionError {
    #[error("partition name is empty")]
    Empty,
    #[error("partition name is longer than {MAX_LEN} characters ({0})")]
    TooLong(usize),
    #[error(
        "partition name {0:?} must be lowercase ASCII letters, digits, '-' or '_', starting with a letter or digit"
    )]
    Invalid(String),
}

/// A validated partition name: `[a-z0-9][a-z0-9_-]*`, at most 64 characters. Names are safe to
/// use in blob keys and paths.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Partition(String);

impl Partition {
    /// The partition of a zero-configuration archive.
    pub const DEFAULT: &'static str = "default";

    pub fn parse(raw: &str) -> Result<Self, PartitionError> {
        if raw.is_empty() {
            return Err(PartitionError::Empty);
        }
        if raw.len() > MAX_LEN {
            return Err(PartitionError::TooLong(raw.len()));
        }
        let valid = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_';
        let first_ok = raw
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
        if !first_ok || !raw.chars().all(valid) {
            return Err(PartitionError::Invalid(raw.to_owned()));
        }
        Ok(Self(raw.to_owned()))
    }

    /// The `default` partition.
    pub fn default_partition() -> Self {
        Self(Self::DEFAULT.to_owned())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Partition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for Partition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Partition({})", self.0)
    }
}

impl FromStr for Partition {
    type Err = PartitionError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl TryFrom<String> for Partition {
    type Error = PartitionError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::parse(&s)
    }
}

impl From<Partition> for String {
    fn from(p: Partition) -> Self {
        p.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        for ok in ["default", "tenant-a", "t_1", "0research"] {
            assert_eq!(Partition::parse(ok).unwrap().as_str(), ok);
        }
        for bad in ["", "Tenant", "-a", "a/b", "a b", "ä", &"a".repeat(65)] {
            assert!(Partition::parse(bad).is_err(), "{bad:?}");
        }
        assert_eq!(Partition::default_partition().as_str(), Partition::DEFAULT);
    }
}
