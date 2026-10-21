//! Matching of a single key attribute (PS3.4 C.2.2.2).

use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use super::datetime::{self, Interval};
use super::{Attributes, Key, MatchOptions, QueryError};
use crate::Vr;

/// How a key attribute is matched. Built with [`Matcher::parse`] from the encoded key value of a
/// C-FIND identifier or QIDO-RS query parameter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Matcher {
    /// Zero-length key value: everything matches; the attribute is only requested for return.
    Universal,
    /// Exact match (case-insensitive for PN if configured; by meaning for DA/TM/DT, numeric for
    /// DS/IS).
    Single { value: String },
    /// List of UID Matching (VR UI with several values).
    UidList { uids: Vec<String> },
    /// `*` / `?` Wild Card Matching.
    Wildcard { pattern: String },
    /// Range Matching on DA, TM or DT; at least one bound is present.
    Range {
        from: Option<String>,
        to: Option<String>,
    },
    /// Empty Value Matching (`""`, if negotiated).
    Empty,
    /// Multiple Value Matching (if negotiated): every key value must be present.
    AllOf { values: Vec<String> },
    /// Backslash-separated values without Multiple Value Matching negotiated: matches if any value
    /// matches. This is a widespread non-standard extension (e.g. `ModalitiesInStudy=CT\MR`) and is
    /// documented as such in the Conformance Statement.
    AnyOf { values: Vec<String> },
    /// Sequence Matching: some item must match all item keys. No keys = universal.
    Sequence { keys: Vec<Key> },
}

impl Matcher {
    /// Classifies an encoded key value according to PS3.4 C.2.2.2.
    ///
    /// Sequence keys cannot be expressed as a string; build [`Matcher::Sequence`] directly.
    pub fn parse(vr: Vr, raw: &str, options: &MatchOptions) -> Result<Self, QueryError> {
        if vr == Vr::SQ {
            return Err(QueryError::SequenceNotString);
        }
        let value = normalize(vr, raw);
        if value.is_empty() {
            return Ok(Self::Universal);
        }
        if options.empty_value_matching && value == "\"\"" && vr.supports_empty_value_matching() {
            return Ok(Self::Empty);
        }
        if vr == Vr::UI {
            let uids: Vec<String> = value.split('\\').map(|u| normalize(vr, u)).collect();
            if uids.iter().any(String::is_empty) {
                return Err(QueryError::InvalidValue {
                    vr,
                    value: value.clone(),
                });
            }
            return Ok(match <[String; 1]>::try_from(uids) {
                Ok([single]) => Self::Single { value: single },
                Err(uids) => Self::UidList { uids },
            });
        }
        if vr.supports_ranges() && value.contains('-') {
            return parse_range(vr, &value);
        }
        if vr.supports_wildcards() && value.contains(['*', '?']) {
            if value.chars().all(|c| c == '*') {
                return Ok(Self::Universal);
            }
            return Ok(Self::Wildcard { pattern: value });
        }
        if value.contains('\\') {
            let values: Vec<String> = value.split('\\').map(|v| normalize(vr, v)).collect();
            if options.multi_value_matching && vr.supports_multi_value_matching() {
                return Ok(Self::AllOf { values });
            }
            return Ok(Self::AnyOf { values });
        }
        validate_single(vr, &value)?;
        Ok(Self::Single { value })
    }

    /// `true` if this matcher constrains nothing (attribute is only a return key).
    pub fn is_universal(&self) -> bool {
        match self {
            Self::Universal => true,
            Self::Sequence { keys } => keys.iter().all(|k| k.matcher.is_universal()),
            _ => false,
        }
    }

    /// Evaluates the matcher against the entity's attribute `tag` in `entity`.
    pub(super) fn matches(&self, key: &Key, entity: &dyn Attributes, opt: &MatchOptions) -> bool {
        let vr = key.vr;
        if let Self::Sequence { keys } = self {
            if keys.is_empty() {
                return true;
            }
            return entity
                .items(key.tag)
                .iter()
                .any(|item| keys.iter().all(|k| k.matches(*item, opt)));
        }
        let raw = entity.values(key.tag);
        let values: Vec<String> = raw
            .iter()
            .flatten()
            .map(|v| normalize(vr, v))
            .filter(|v| !v.is_empty())
            .collect();
        match self {
            Self::Universal => true,
            Self::Empty => values.is_empty(),
            Self::Single { value } => values.iter().any(|v| single_eq(vr, value, v, opt)),
            Self::UidList { uids } => values.iter().any(|v| uids.contains(v)),
            Self::AnyOf { values: keys } => keys
                .iter()
                .any(|k| values.iter().any(|v| single_eq(vr, k, v, opt))),
            Self::AllOf { values: keys } => keys
                .iter()
                .all(|k| values.iter().any(|v| single_eq(vr, k, v, opt))),
            Self::Wildcard { pattern } => {
                let fold = vr == Vr::PN && opt.pn_case_insensitive;
                values.iter().any(|v| wildcard(pattern, v, fold))
            }
            Self::Range { from, to } => {
                let lo = from.as_deref().and_then(|f| interval(vr, f)).map(|i| i.lo);
                let hi = to.as_deref().and_then(|t| interval(vr, t)).map(|i| i.hi);
                values
                    .iter()
                    .filter_map(|v| interval(vr, v))
                    .any(|v| lo.is_none_or(|lo| v.lo >= lo) && hi.is_none_or(|hi| v.lo <= hi))
            }
            Self::Sequence { .. } => unreachable!("handled above"),
        }
    }
}

/// Removes insignificant padding: trailing spaces (and NUL for UI); leading spaces except for text
/// VRs where they are significant. PN values additionally drop trailing empty components.
fn normalize(vr: Vr, raw: &str) -> String {
    let s = raw.trim_end_matches([' ', '\0']);
    let s = if vr.leading_space_significant() {
        s
    } else {
        s.trim_start_matches(' ')
    };
    if vr == Vr::PN {
        return s
            .split('=')
            .map(|group| group.trim_end_matches(['^', ' ']))
            .collect::<Vec<_>>()
            .join("=")
            .trim_end_matches('=')
            .to_owned();
    }
    s.to_owned()
}

fn interval(vr: Vr, v: &str) -> Option<Interval> {
    match vr {
        Vr::DA => datetime::date(v),
        Vr::TM => datetime::time(v),
        Vr::DT => datetime::datetime(v),
        _ => None,
    }
}

fn validate_single(vr: Vr, v: &str) -> Result<(), QueryError> {
    let ok = match vr {
        Vr::DA | Vr::TM | Vr::DT => interval(vr, v).is_some(),
        Vr::IS => v.trim().parse::<i64>().is_ok(),
        Vr::DS => v.trim().parse::<f64>().is_ok(),
        _ => true,
    };
    if ok {
        Ok(())
    } else {
        Err(QueryError::InvalidValue {
            vr,
            value: v.to_owned(),
        })
    }
}

fn parse_range(vr: Vr, value: &str) -> Result<Matcher, QueryError> {
    let invalid = || QueryError::InvalidValue {
        vr,
        value: value.to_owned(),
    };
    let bound = |s: &str| -> Result<Option<String>, ()> {
        let s = s.trim();
        if s.is_empty() {
            Ok(None)
        } else if interval(vr, s).is_some() {
            Ok(Some(s.to_owned()))
        } else {
            Err(())
        }
    };
    // For DT a '-' may also start a UTC offset, so try every split position and take the first
    // one where both sides are valid and the range is not empty. (In "20240101120000-0500" the
    // right side "0500" is a valid DT on its own — the year 500 — giving an empty range.)
    let mut empty_range = false;
    for (i, _) in value.match_indices('-') {
        if let (Ok(from), Ok(to)) = (bound(&value[..i]), bound(&value[i + 1..])) {
            if from.is_none() && to.is_none() {
                return Err(invalid());
            }
            if let (Some(f), Some(t)) = (&from, &to) {
                let (fi, ti) = (
                    interval(vr, f).ok_or_else(invalid)?,
                    interval(vr, t).ok_or_else(invalid)?,
                );
                if fi.lo > ti.hi {
                    empty_range = true;
                    continue;
                }
            }
            return Ok(Matcher::Range { from, to });
        }
    }
    // Strictly, PS3.4 C.2.2.2.1.3 Note 3 says a negative UTC offset cannot be used with Single
    // Value Matching. We accept it leniently, e.g. "20240101120000-0500".
    if vr == Vr::DT && interval(vr, value).is_some() {
        return Ok(Matcher::Single {
            value: value.to_owned(),
        });
    }
    if empty_range {
        return Err(QueryError::EmptyRange {
            value: value.to_owned(),
        });
    }
    Err(invalid())
}

fn single_eq(vr: Vr, key: &str, value: &str, opt: &MatchOptions) -> bool {
    match vr {
        Vr::DA | Vr::TM | Vr::DT => match (interval(vr, key), interval(vr, value)) {
            (Some(k), Some(v)) => k.lo == v.lo,
            _ => false,
        },
        Vr::IS => match (key.trim().parse::<i64>(), value.trim().parse::<i64>()) {
            (Ok(k), Ok(v)) => k == v,
            _ => key == value,
        },
        #[allow(clippy::float_cmp)] // exact numeric identity is the intended semantics
        Vr::DS => match (key.trim().parse::<f64>(), value.trim().parse::<f64>()) {
            (Ok(k), Ok(v)) => k == v,
            _ => key == value,
        },
        Vr::PN if opt.pn_case_insensitive => fold(key) == fold(value),
        _ => key == value,
    }
}

fn fold(s: &str) -> Cow<'_, str> {
    if s.chars().any(char::is_uppercase) {
        Cow::Owned(s.to_lowercase())
    } else {
        Cow::Borrowed(s)
    }
}

/// `*` matches any sequence (including empty), `?` exactly one character.
fn wildcard(pattern: &str, value: &str, case_fold: bool) -> bool {
    let (p, v) = if case_fold {
        (fold(pattern), fold(value))
    } else {
        (pattern.into(), value.into())
    };
    let p: Vec<char> = p.chars().collect();
    let v: Vec<char> = v.chars().collect();
    // Iterative matcher with single-star backtracking: O(len(p) * len(v)) worst case.
    let (mut pi, mut vi) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while vi < v.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == v[vi]) {
            pi += 1;
            vi += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, vi));
            pi += 1;
        } else if let Some((sp, sv)) = star {
            pi = sp + 1;
            vi = sv + 1;
            star = Some((sp, sv + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn parse(vr: Vr, s: &str) -> Matcher {
        Matcher::parse(vr, s, &MatchOptions::default()).unwrap()
    }

    #[test]
    fn classification() {
        assert_eq!(parse(Vr::PN, ""), Matcher::Universal);
        assert_eq!(parse(Vr::PN, "  "), Matcher::Universal);
        assert_eq!(parse(Vr::PN, "*"), Matcher::Universal);
        assert_eq!(
            parse(Vr::PN, "Doe*"),
            Matcher::Wildcard {
                pattern: "Doe*".into()
            }
        );
        assert_eq!(
            parse(Vr::UI, "1.2\\1.3"),
            Matcher::UidList {
                uids: vec!["1.2".into(), "1.3".into()]
            }
        );
        assert_eq!(
            parse(Vr::UI, "1.2\0"),
            Matcher::Single {
                value: "1.2".into()
            }
        );
        assert_eq!(
            parse(Vr::DA, "20240101-"),
            Matcher::Range {
                from: Some("20240101".into()),
                to: None
            }
        );
        assert_eq!(
            parse(Vr::DA, "-20240101"),
            Matcher::Range {
                from: None,
                to: Some("20240101".into())
            }
        );
        assert_eq!(
            parse(Vr::CS, "CT\\MR"),
            Matcher::AnyOf {
                values: vec!["CT".into(), "MR".into()]
            }
        );
        // wildcards are not special for UI or DA
        assert!(Matcher::parse(Vr::DA, "2024*", &MatchOptions::default()).is_err());
        // empty value matching only when negotiated
        assert_eq!(
            parse(Vr::LO, "\"\""),
            Matcher::Single {
                value: "\"\"".into()
            }
        );
        let opts = MatchOptions {
            empty_value_matching: true,
            ..MatchOptions::default()
        };
        assert_eq!(
            Matcher::parse(Vr::LO, "\"\"", &opts).unwrap(),
            Matcher::Empty
        );
        let opts = MatchOptions {
            multi_value_matching: true,
            ..MatchOptions::default()
        };
        assert_eq!(
            Matcher::parse(Vr::CS, "CT\\MR", &opts).unwrap(),
            Matcher::AllOf {
                values: vec!["CT".into(), "MR".into()]
            }
        );
    }

    #[test]
    fn invalid_ranges() {
        let o = MatchOptions::default();
        assert!(Matcher::parse(Vr::DA, "-", &o).is_err());
        assert!(Matcher::parse(Vr::DA, "20240102-20240101", &o).is_err());
        assert!(Matcher::parse(Vr::DA, "2024-01-01", &o).is_err());
    }

    #[test]
    fn datetime_ranges_with_offsets() {
        assert_eq!(
            parse(Vr::DT, "20240101120000-0500-20240102"),
            Matcher::Range {
                from: Some("20240101120000-0500".into()),
                to: Some("20240102".into())
            }
        );
        assert_eq!(
            parse(Vr::DT, "20240101120000-0500"),
            Matcher::Single {
                value: "20240101120000-0500".into()
            }
        );
        assert_eq!(
            parse(Vr::DT, "2024-2025"),
            Matcher::Range {
                from: Some("2024".into()),
                to: Some("2025".into())
            }
        );
    }

    #[test]
    fn wildcard_semantics() {
        assert!(wildcard("Doe*", "Doe^John", false));
        assert!(wildcard("*^John", "Doe^John", false));
        assert!(wildcard("D?e*", "Doe", false));
        assert!(!wildcard("D?e", "De", false));
        assert!(wildcard("*", "", false));
        assert!(wildcard("a*b*c", "aXXbYYc", false));
        assert!(!wildcard("a*b*c", "aXXbYY", false));
        assert!(!wildcard("doe*", "Doe", false));
        assert!(wildcard("doe*", "Doe", true));
    }

    #[test]
    fn pn_normalization() {
        assert_eq!(normalize(Vr::PN, "Doe^John^^^"), "Doe^John");
        assert_eq!(normalize(Vr::PN, "Doe^John^^=^^"), "Doe^John");
        assert_eq!(normalize(Vr::LT, "  text  "), "  text");
        assert_eq!(normalize(Vr::LO, "  text  "), "text");
    }

    proptest! {
        #[test]
        fn literal_patterns_match_themselves(s in "[A-Za-z0-9^ ]{0,20}") {
            prop_assert!(wildcard(&s, &s, false));
            prop_assert!(wildcard("*", &s, false));
            let mut prefix = s.clone();
            prefix.push('*');
            prop_assert!(wildcard(&prefix, &s, false));
        }

        #[test]
        fn parse_never_panics(raw in ".{0,40}", vr_idx in 0usize..34) {
            let vr: Vr = ["AE","AS","AT","CS","DA","DS","DT","FD","FL","IS","LO","LT","OB","OD","OF","OL","OV","OW","PN","SH","SL","SQ","SS","ST","SV","TM","UC","UI","UL","UN","UR","US","UT","UV"][vr_idx].parse().unwrap();
            let _ = Matcher::parse(vr, &raw, &MatchOptions::default());
        }
    }
}
