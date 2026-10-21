//! Application Entity titles (PS3.5 §6.2, VR AE).

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

pub const MAX_LEN: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AeTitleError {
    #[error("AE title is empty or consists only of spaces")]
    Empty,
    #[error("AE title is longer than {MAX_LEN} characters ({0})")]
    TooLong(usize),
    #[error("AE title contains invalid character {0:?}")]
    InvalidChar(char),
}

/// A validated AE title. Leading and trailing spaces are not significant and are removed.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct AeTitle(String);

impl AeTitle {
    pub fn parse(raw: &str) -> Result<Self, AeTitleError> {
        let s = raw.trim_matches([' ', '\0']);
        if s.is_empty() {
            return Err(AeTitleError::Empty);
        }
        if let Some(c) = s
            .chars()
            .find(|c| !c.is_ascii() || c.is_ascii_control() || *c == '\\')
        {
            return Err(AeTitleError::InvalidChar(c));
        }
        if s.len() > MAX_LEN {
            return Err(AeTitleError::TooLong(s.len()));
        }
        Ok(Self(s.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AeTitle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for AeTitle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AeTitle({})", self.0)
    }
}

impl FromStr for AeTitle {
    type Err = AeTitleError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl TryFrom<String> for AeTitle {
    type Error = AeTitleError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::parse(&s)
    }
}

impl From<AeTitle> for String {
    fn from(a: AeTitle) -> Self {
        a.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_and_validates() {
        assert_eq!(AeTitle::parse("  STORESCP  ").unwrap().as_str(), "STORESCP");
        assert_eq!(AeTitle::parse("ANY-SCP_1").unwrap().as_str(), "ANY-SCP_1");
        assert_eq!(AeTitle::parse("    "), Err(AeTitleError::Empty));
        assert_eq!(AeTitle::parse("A\\B"), Err(AeTitleError::InvalidChar('\\')));
        assert_eq!(AeTitle::parse("A\nB"), Err(AeTitleError::InvalidChar('\n')));
        assert_eq!(
            AeTitle::parse("ABCDEFGHIJKLMNOPQ"),
            Err(AeTitleError::TooLong(17))
        );
        assert!(AeTitle::parse("ABCDEFGHIJKLMNOP").is_ok());
    }
}
