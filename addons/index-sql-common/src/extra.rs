//! Operator-configured extra indexed tags, kept as binary JSON in an `extra` column of each level's
//! table, in two forms:
//!
//! ```json
//! { "d": { "00180050": { "vr": "DS", "Value": [1.5] } },   // DICOM JSON, returned in results
//!   "m": { "00180050": ["1.5"] } }                         // match forms, used by queries
//! ```
//!
//! Match forms are the normalization the query side applies to keys too (PN case-folded, DA/TM
//! sortable, IS/DS numerically canonical), so SQL comparisons agree with the in-memory matcher;
//! raw DICOM JSON values could not be compared correctly in SQL (e.g. Unicode case folding, TM
//! matched by meaning). The DICOM JSON form lets queries return extra tags without reading
//! instance headers.

use medicus_core::TagExt as _;
use medicus_core::capability::index::{DicomJsonObject, ExtraTag, ExtraTagState, ExtraTagStatus};
use medicus_core::query::values::{fold, normalize, sortable_lower};
use medicus_core::query::{Attributes, MatchOptions, Matcher};
use medicus_core::{Error, QueryLevel, Result, Tag, VR, tags};

use crate::sql::{Dialect, Stmt};

/// VRs that can be indexed as extra tags (textual or numeric values).
pub(crate) fn supported_vr(vr: VR) -> bool {
    !matches!(
        vr,
        VR::SQ | VR::OB | VR::OD | VR::OF | VR::OL | VR::OV | VR::OW | VR::UN
    )
}

pub(crate) fn validate(tags_: &[ExtraTag]) -> Result<()> {
    for (i, t) in tags_.iter().enumerate() {
        if tags::standard(t.tag).is_some() {
            return Err(Error::invalid_argument(format!(
                "{} is a standard attribute and always indexed",
                t.tag
            )));
        }
        if !supported_vr(t.vr) {
            return Err(Error::invalid_argument(format!(
                "{} has VR {}, which cannot be indexed",
                t.tag, t.vr
            )));
        }
        if tags_[..i].iter().any(|o| o.tag == t.tag) {
            return Err(Error::invalid_argument(format!(
                "{} is configured twice",
                t.tag
            )));
        }
    }
    Ok(())
}

/// The match form of one value, or `None` if it cannot match anything (e.g. an invalid date).
pub(crate) fn match_form(vr: VR, raw: &str) -> Option<String> {
    let v = normalize(vr, raw);
    match vr {
        VR::PN => Some(fold(&v)),
        VR::DA | VR::TM => sortable_lower(vr, &v),
        VR::IS => Some(
            v.trim()
                .trim_start_matches('+')
                .parse::<i64>()
                .map_or(v.clone(), |n| n.to_string()),
        ),
        VR::DS => Some(v.trim().parse::<f64>().map_or(v.clone(), |n| n.to_string())),
        _ => Some(v),
    }
}

/// The `extra` JSON of one level for `attributes`, or `None` if no extra tag is configured there.
pub(crate) fn level_json(
    extras: &[ExtraTagStatus],
    level: QueryLevel,
    attributes: &DicomJsonObject,
) -> Option<String> {
    let mut dicom = serde_json::Map::new();
    let mut matching = serde_json::Map::new();
    let mut any = false;
    for e in extras.iter().filter(|e| e.tag.level == level) {
        any = true;
        let key = e.tag.tag.json_key();
        if let Some(values) = Attributes::values(attributes, e.tag.tag) {
            let forms: Vec<serde_json::Value> = values
                .iter()
                .filter_map(|v| match_form(e.tag.vr, v))
                .filter(|v| !v.is_empty())
                .map(serde_json::Value::String)
                .collect();
            matching.insert(key.clone(), serde_json::Value::Array(forms));
        }
        if let Some(element) = attributes.get(&key) {
            dicom.insert(key, element.clone());
        }
    }
    any.then(|| serde_json::json!({ "d": dicom, "m": matching }).to_string())
}

pub(crate) fn alias(level: QueryLevel) -> &'static str {
    match level {
        QueryLevel::Patient => "p",
        QueryLevel::Study => "st",
        QueryLevel::Series => "se",
        QueryLevel::Instance => "i",
    }
}

pub(crate) fn table(level: QueryLevel) -> &'static str {
    match level {
        QueryLevel::Patient => "patients",
        QueryLevel::Study => "studies",
        QueryLevel::Series => "series",
        QueryLevel::Instance => "instances",
    }
}

pub(crate) fn level_name(level: QueryLevel) -> &'static str {
    match level {
        QueryLevel::Patient => "patient",
        QueryLevel::Study => "study",
        QueryLevel::Series => "series",
        QueryLevel::Instance => "instance",
    }
}

pub(crate) fn parse_level(s: &str) -> Option<QueryLevel> {
    QueryLevel::ALL.into_iter().find(|l| level_name(*l) == s)
}

pub(crate) fn state_name(s: ExtraTagState) -> &'static str {
    match s {
        ExtraTagState::Ready => "ready",
        ExtraTagState::Backfilling => "backfilling",
    }
}

/// Whether the index can evaluate `m` on an extra tag (otherwise the key is reported unfiltered).
pub(crate) fn compilable(t: ExtraTag, m: &Matcher, opts: &MatchOptions) -> bool {
    match m {
        Matcher::Sequence { .. } => false,
        _ if t.vr == VR::DT => false,
        // Values are stored case-folded.
        _ if t.vr == VR::PN && !opts.pn_case_insensitive => false,
        Matcher::Range { .. } => matches!(t.vr, VR::DA | VR::TM),
        _ => true,
    }
}

/// `EXISTS (SELECT 1 FROM <elements of extra[tag]> je WHERE <cond on je.value>)`.
fn exists(stmt: &mut Stmt, t: ExtraTag, cond: &mut dyn FnMut(&mut Stmt)) {
    let col = format!("{}.extra", alias(t.level));
    // The tag key is 8 hex digits, safe to embed.
    let key = t.tag.json_key();
    match stmt.dialect() {
        Dialect::Sqlite => stmt.push(&format!(
            "EXISTS (SELECT 1 FROM json_each({col}, '$.m.\"{key}\"') je WHERE "
        )),
        Dialect::Postgres => stmt.push(&format!(
            "EXISTS (SELECT 1 FROM jsonb_array_elements_text({col} -> 'm' -> '{key}') AS je(value) WHERE "
        )),
    };
    cond(stmt);
    stmt.push(")");
}

fn eq(stmt: &mut Stmt, t: ExtraTag, raw: &str) {
    match match_form(t.vr, raw) {
        Some(v) => {
            stmt.push("je.value = ").bind(v);
        }
        None => {
            stmt.push("1=0");
        }
    }
}

/// Appends the condition for one compilable key on an extra tag.
pub(crate) fn condition(stmt: &mut Stmt, t: ExtraTag, m: &Matcher, opts: &MatchOptions) {
    match m {
        Matcher::Universal | Matcher::Sequence { .. } => {
            stmt.push("1=1");
        }
        Matcher::Empty => {
            stmt.push("NOT ");
            exists(stmt, t, &mut |s| {
                s.push("1=1");
            });
        }
        Matcher::Single { value } => exists(stmt, t, &mut |s| eq(s, t, value)),
        Matcher::UidList { uids } => exists(stmt, t, &mut |s| {
            s.push("je.value IN ")
                .bind_list(uids.iter().map(|u| normalize(t.vr, u)));
        }),
        Matcher::AnyOf { values } => exists(stmt, t, &mut |s| {
            s.push("(");
            for (i, v) in values.iter().enumerate() {
                if i > 0 {
                    s.push(" OR ");
                }
                eq(s, t, v);
            }
            s.push(")");
        }),
        Matcher::AllOf { values } => {
            stmt.push("(1=1");
            for v in values {
                stmt.push(" AND ");
                exists(stmt, t, &mut |s| eq(s, t, v));
            }
            stmt.push(")");
        }
        Matcher::Wildcard { pattern } => {
            let mut p = normalize(t.vr, pattern);
            if t.vr == VR::PN && opts.pn_case_insensitive {
                p = fold(&p);
            }
            exists(stmt, t, &mut |s| {
                crate::query::wildcard_on(s, "je.value", &p);
            });
        }
        Matcher::Range { from, to } => {
            let lo = from.as_deref().map(|f| sortable_lower(t.vr, f));
            let hi = to
                .as_deref()
                .map(|v| medicus_core::query::values::sortable_upper(t.vr, v));
            if matches!(lo, Some(None)) || matches!(hi, Some(None)) {
                stmt.push("1=0");
                return;
            }
            exists(stmt, t, &mut |s| {
                s.push("1=1");
                if let Some(Some(lo)) = &lo {
                    s.push(" AND je.value >= ").bind(lo.clone());
                }
                if let Some(Some(hi)) = &hi {
                    s.push(" AND je.value <= ").bind(hi.clone());
                }
            });
        }
    }
}

/// Expression selecting a level's `extra` column as JSON text.
pub(crate) fn select_expr(level: QueryLevel, dialect: Dialect) -> String {
    match dialect {
        Dialect::Sqlite => format!("json({}.extra)", alias(level)),
        Dialect::Postgres => format!("{}.extra::text", alias(level)),
    }
}

/// Merges the DICOM JSON part of an `extra` value into a result row, for tags still configured.
pub(crate) fn merge_into(row: &mut DicomJsonObject, extra_json: &str, extras: &[ExtraTagStatus]) {
    let Ok(serde_json::Value::Object(mut v)) = serde_json::from_str(extra_json) else {
        return;
    };
    let Some(serde_json::Value::Object(dicom)) = v.remove("d") else {
        return;
    };
    for (key, element) in dicom {
        if extras.iter().any(|e| e.tag.tag.json_key() == key) {
            row.insert(key, element);
        }
    }
}

/// The configured extra tag for `tag`, if any.
pub(crate) fn find(extras: &[ExtraTagStatus], tag: Tag) -> Option<&ExtraTagStatus> {
    extras.iter().find(|e| e.tag.tag == tag)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn match_forms_agree_with_matcher_semantics() {
        assert_eq!(
            match_form(VR::DS, "1.50").unwrap(),
            match_form(VR::DS, " 1.5").unwrap()
        );
        assert_eq!(match_form(VR::IS, "+007").unwrap(), "7");
        assert_eq!(match_form(VR::PN, "Doe^John^^").unwrap(), "doe^john");
        assert_eq!(match_form(VR::DA, "2024.01.02").unwrap(), "20240102");
        assert!(match_form(VR::DA, "garbage").is_none());
    }

    #[test]
    fn validation() {
        let t = |tag, vr| ExtraTag {
            tag,
            vr,
            level: QueryLevel::Instance,
        };
        assert!(validate(&[t(Tag(0x0018, 0x0050), VR::DS)]).is_ok());
        assert!(validate(&[t(tags::MODALITY, VR::CS)]).is_err());
        assert!(validate(&[t(Tag(0x0040, 0x0275), VR::SQ)]).is_err());
        let dup = t(Tag(0x0018, 0x0050), VR::DS);
        assert!(validate(&[dup, dup]).is_err());
    }
}
