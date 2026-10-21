//! Compilation of a [`Query`] into SQL, and of result rows into DICOM JSON.
//!
//! The SQL semantics deliberately mirror `medicus_core::query::Matcher::matches` (same value
//! normalization, same sortable date/time forms, same PN case folding), so an index answer equals
//! what the in-memory matcher would return. The shared conformance suite checks this.

use medicus_core::capability::index::DicomJsonObject;
use medicus_core::query::values::{fold, normalize, sortable_lower, sortable_upper};
use medicus_core::query::{Key, MatchOptions, Matcher, OrderKey, Query};
use medicus_core::{Error, QueryLevel, Result, Tag, Vr};
use serde_json::{Value, json};

use crate::schema::{COLUMNS, Column, Storage, column, from_clause, id_expr};
use crate::sql::{Cell, CellKind, Dialect, Stmt};

pub(crate) struct Compiled {
    pub stmt: Stmt,
    pub select: Vec<&'static Column>,
    pub kinds: Vec<CellKind>,
    pub unfiltered: Vec<Tag>,
    pub limit: Option<u32>,
}

/// Columns returned at a level: everything of this and the parent levels; the patient study
/// count only at patient level (it is computed per row).
fn selected(level: QueryLevel) -> Vec<&'static Column> {
    COLUMNS
        .iter()
        .filter(|c| c.level <= level)
        .filter(|c| c.storage != Storage::PatientStudyCount || level == QueryLevel::Patient)
        .collect()
}

fn select_expr(c: &Column, dialect: Dialect) -> String {
    match c.storage {
        Storage::StudyMulti { table } => {
            let agg = match dialect {
                Dialect::Sqlite => "group_concat(x, '\\')",
                Dialect::Postgres => "string_agg(x, '\\' ORDER BY x)",
            };
            format!(
                "(SELECT {agg} FROM (SELECT m.{col} AS x FROM {table} m WHERE m.study_fk = st.id ORDER BY m.{col}) agg)",
                col = c.column
            )
        }
        Storage::PatientStudyCount => {
            "(SELECT COUNT(*) FROM studies s2 WHERE s2.patient_fk = p.id)".to_owned()
        }
        _ => c.expr(),
    }
}

fn kind(c: &Column) -> CellKind {
    match c.storage {
        Storage::Int | Storage::PatientStudyCount => CellKind::Int,
        _ => CellKind::Text,
    }
}

pub(crate) fn compile(dialect: Dialect, q: &Query) -> Result<Compiled> {
    let select = selected(q.level);
    let kinds = select.iter().map(|c| kind(c)).collect();
    let mut stmt = Stmt::new(dialect);
    stmt.push("SELECT ");
    stmt.push(
        &select
            .iter()
            .map(|c| select_expr(c, dialect))
            .collect::<Vec<_>>()
            .join(", "),
    );
    stmt.push(from_clause(q.level));
    stmt.push(" WHERE 1=1");

    let mut unfiltered = Vec::new();
    for key in q.constraints() {
        match column(key.tag) {
            Some(c) if c.level > q.level => {
                return Err(Error::invalid_argument(format!(
                    "attribute {} belongs to a level below the query level {}",
                    key.tag, q.level
                )));
            }
            Some(c) if compilable(&key.matcher) => {
                stmt.push(" AND (");
                condition(&mut stmt, c, key, &q.options);
                stmt.push(")");
            }
            _ => unfiltered.push(key.tag),
        }
    }

    order_by(&mut stmt, q);
    if let Some(limit) = q.limit {
        stmt.push(" LIMIT ").bind(i64::from(limit) + 1);
        stmt.push(" OFFSET ")
            .bind(i64::try_from(q.offset).unwrap_or(i64::MAX));
    } else if q.offset > 0 {
        if dialect == Dialect::Sqlite {
            stmt.push(" LIMIT -1");
        }
        stmt.push(" OFFSET ")
            .bind(i64::try_from(q.offset).unwrap_or(i64::MAX));
    }
    Ok(Compiled {
        stmt,
        select,
        kinds,
        unfiltered,
        limit: q.limit,
    })
}

fn compilable(m: &Matcher) -> bool {
    !matches!(m, Matcher::Sequence { .. })
}

fn order_by(stmt: &mut Stmt, q: &Query) {
    let mut terms = Vec::new();
    for o in &q.order_by {
        let expr = match &o.key {
            OrderKey::Attribute(tag) => column(*tag)
                .filter(|c| c.level <= q.level && !matches!(c.storage, Storage::StudyMulti { .. }))
                .map(|c| match c.storage {
                    Storage::Time { key } => format!("{}.{key}", c.alias),
                    _ => select_expr(c, stmt.dialect()),
                }),
            OrderKey::ReceivedAt => match q.level {
                QueryLevel::Study => Some("st.received_at".into()),
                QueryLevel::Instance => Some("i.received_at".into()),
                QueryLevel::Series => Some("se.updated_at".into()),
                QueryLevel::Patient => None,
            },
            OrderKey::LastAccessedAt => {
                (q.level == QueryLevel::Study).then(|| "st.last_accessed_at".into())
            }
            OrderKey::Size => match q.level {
                QueryLevel::Study => Some("st.size_bytes".into()),
                QueryLevel::Series => Some("se.size_bytes".into()),
                QueryLevel::Instance => Some("i.size".into()),
                QueryLevel::Patient => None,
            },
        };
        if let Some(e) = expr {
            terms.push(format!("{e} {}", if o.descending { "DESC" } else { "ASC" }));
        }
    }
    terms.push(format!("{} ASC", id_expr(q.level)));
    stmt.push(" ORDER BY ").push(&terms.join(", "));
}

/// Value of a key as stored in the column, or `None` if it cannot match any stored value.
fn stored_value(c: &Column, raw: &str, opts: &MatchOptions) -> Option<Cell> {
    let v = normalize(c.vr, raw);
    match c.storage {
        Storage::Text | Storage::StudyMulti { .. } => Some(Cell::Text(v)),
        Storage::PersonName { .. } => Some(Cell::Text(if opts.pn_case_insensitive {
            fold(&v)
        } else {
            v
        })),
        Storage::Date => sortable_lower(Vr::DA, &v).map(Cell::Text),
        Storage::Time { .. } => sortable_lower(Vr::TM, &v).map(Cell::Text),
        Storage::Int | Storage::PatientStudyCount => v
            .trim()
            .trim_start_matches('+')
            .parse::<i64>()
            .ok()
            .map(Cell::Int),
    }
}

/// The column a key is compared against.
fn match_expr(c: &Column, opts: &MatchOptions) -> String {
    match c.storage {
        Storage::PersonName { folded } if opts.pn_case_insensitive => {
            format!("{}.{folded}", c.alias)
        }
        Storage::Time { key } => format!("{}.{key}", c.alias),
        Storage::StudyMulti { .. } => format!("m.{}", c.column),
        Storage::PatientStudyCount => select_expr(c, Dialect::Sqlite),
        _ => c.expr(),
    }
}

/// Appends a boolean SQL condition for one key.
fn condition(stmt: &mut Stmt, c: &Column, key: &Key, opts: &MatchOptions) {
    if let Storage::StudyMulti { table } = c.storage {
        return multi_condition(stmt, c, table, &key.matcher, opts);
    }
    let expr = match c.storage {
        // The computed count needs the dialect-independent subquery.
        Storage::PatientStudyCount => select_expr(c, stmt.dialect()),
        _ => match_expr(c, opts),
    };
    scalar_condition(stmt, c, &expr, &key.matcher, opts);
}

fn eq(stmt: &mut Stmt, c: &Column, expr: &str, raw: &str, opts: &MatchOptions) {
    match stored_value(c, raw, opts) {
        Some(v) => {
            stmt.push(expr).push(" = ").bind(v);
        }
        None => {
            stmt.push("1=0");
        }
    }
}

fn wildcard(stmt: &mut Stmt, c: &Column, expr: &str, pattern: &str, opts: &MatchOptions) {
    let mut p = normalize(c.vr, pattern);
    if matches!(c.storage, Storage::PersonName { .. }) && opts.pn_case_insensitive {
        p = fold(&p);
    }
    match stmt.dialect() {
        // GLOB: `*` and `?` as in DICOM, case-sensitive; only `[` needs escaping.
        Dialect::Sqlite => {
            stmt.push(expr).push(" GLOB ").bind(p.replace('[', "[[]"));
        }
        Dialect::Postgres => {
            let like: String = p
                .chars()
                .flat_map(|ch| match ch {
                    '\\' | '%' | '_' => vec!['\\', ch],
                    '*' => vec!['%'],
                    '?' => vec!['_'],
                    other => vec![other],
                })
                .collect();
            stmt.push(expr)
                .push(" LIKE ")
                .bind(like)
                .push(" ESCAPE '\\'");
        }
    }
}

fn join_conditions<T>(stmt: &mut Stmt, items: &[T], op: &str, mut each: impl FnMut(&mut Stmt, &T)) {
    stmt.push("(");
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            stmt.push(op);
        }
        each(stmt, item);
    }
    if items.is_empty() {
        stmt.push("1=0");
    }
    stmt.push(")");
}

fn scalar_condition(stmt: &mut Stmt, c: &Column, expr: &str, m: &Matcher, opts: &MatchOptions) {
    match m {
        Matcher::Universal | Matcher::Sequence { .. } => {
            stmt.push("1=1");
        }
        Matcher::Empty => {
            let text = matches!(c.storage, Storage::Text | Storage::PersonName { .. });
            if text {
                stmt.push(&format!("({e} IS NULL OR {e} = '')", e = c.expr()));
            } else {
                stmt.push(&format!("{} IS NULL", c.expr()));
            }
        }
        Matcher::Single { value } => eq(stmt, c, expr, value, opts),
        Matcher::UidList { uids } => {
            stmt.push(expr)
                .push(" IN ")
                .bind_list(uids.iter().map(|u| normalize(c.vr, u)));
        }
        Matcher::AnyOf { values } => {
            join_conditions(stmt, values, " OR ", |s, v| eq(s, c, expr, v, opts));
        }
        Matcher::AllOf { values } => {
            join_conditions(stmt, values, " AND ", |s, v| eq(s, c, expr, v, opts));
        }
        Matcher::Wildcard { pattern } => match c.storage {
            Storage::Text | Storage::PersonName { .. } | Storage::StudyMulti { .. } => {
                wildcard(stmt, c, expr, pattern, opts);
            }
            _ => {
                stmt.push("1=0");
            }
        },
        Matcher::Range { from, to } => {
            let vr = match c.storage {
                Storage::Date => Vr::DA,
                Storage::Time { .. } => Vr::TM,
                _ => {
                    stmt.push("1=0");
                    return;
                }
            };
            let lo = from.as_deref().map(|f| sortable_lower(vr, f));
            let hi = to.as_deref().map(|t| sortable_upper(vr, t));
            if matches!(lo, Some(None)) || matches!(hi, Some(None)) {
                stmt.push("1=0");
                return;
            }
            stmt.push("1=1");
            if let Some(Some(lo)) = lo {
                stmt.push(" AND ").push(expr).push(" >= ").bind(lo);
            }
            if let Some(Some(hi)) = hi {
                stmt.push(" AND ").push(expr).push(" <= ").bind(hi);
            }
        }
    }
}

/// Conditions on multi-valued study attributes become `EXISTS` subqueries: the study matches if
/// some value matches (all values for `AllOf`).
fn multi_condition(stmt: &mut Stmt, c: &Column, table: &str, m: &Matcher, opts: &MatchOptions) {
    let exists = |stmt: &mut Stmt, f: &mut dyn FnMut(&mut Stmt)| {
        stmt.push(&format!(
            "EXISTS (SELECT 1 FROM {table} m WHERE m.study_fk = st.id AND "
        ));
        f(stmt);
        stmt.push(")");
    };
    let expr = match_expr(c, opts);
    match m {
        Matcher::Empty => {
            stmt.push(&format!(
                "NOT EXISTS (SELECT 1 FROM {table} m WHERE m.study_fk = st.id)"
            ));
        }
        Matcher::AllOf { values } => join_conditions(stmt, values, " AND ", |s, v| {
            exists(s, &mut |s| eq(s, c, &expr, v, opts));
        }),
        other => exists(stmt, &mut |s| scalar_condition(s, c, &expr, other, opts)),
    }
}

/// Converts a fetched row into DICOM JSON.
pub(crate) fn row_to_json(select: &[&Column], row: Vec<Cell>) -> DicomJsonObject {
    let mut obj = DicomJsonObject::new();
    for (c, cell) in select.iter().zip(row) {
        let vr = c.vr.as_str();
        let element = match cell {
            Cell::Null => json!({ "vr": vr }),
            Cell::Text(s) if s.is_empty() => json!({ "vr": vr }),
            Cell::Text(s) => match c.storage {
                Storage::PersonName { .. } => {
                    let mut pn = serde_json::Map::new();
                    for (group, name) in s.split('=').zip(["Alphabetic", "Ideographic", "Phonetic"])
                    {
                        if !group.is_empty() {
                            pn.insert(name.into(), Value::String(group.into()));
                        }
                    }
                    json!({ "vr": vr, "Value": [pn] })
                }
                Storage::StudyMulti { .. } => {
                    json!({ "vr": vr, "Value": s.split('\\').collect::<Vec<_>>() })
                }
                _ => json!({ "vr": vr, "Value": [s] }),
            },
            Cell::Int(i) => json!({ "vr": vr, "Value": [i] }),
        };
        obj.insert(String::from(c.tag), element);
    }
    obj
}

#[cfg(test)]
mod tests {
    use super::*;
    use medicus_core::tags;

    fn key(tag: Tag, vr: Vr, v: &str) -> Key {
        Key::parse(tag, vr, v, &MatchOptions::default()).unwrap()
    }

    #[test]
    fn compiles_typical_study_query() {
        let q = Query::new(QueryLevel::Study)
            .with_key(key(tags::PATIENT_NAME, Vr::PN, "Doe*"))
            .with_key(key(tags::STUDY_DATE, Vr::DA, "20240101-20240131"))
            .with_key(key(tags::MODALITIES_IN_STUDY, Vr::CS, "CT\\MR"))
            .with_key(key(tags::ACCESSION_NUMBER, Vr::SH, ""));
        let c = compile(Dialect::Postgres, &q).unwrap();
        let sql = &c.stmt.sql;
        assert!(
            sql.contains("p.patient_name_folded LIKE $1 ESCAPE"),
            "{sql}"
        );
        assert!(
            sql.contains("st.study_date >= $2 AND st.study_date <= $3"),
            "{sql}"
        );
        assert!(sql.contains("EXISTS (SELECT 1 FROM study_modalities m WHERE m.study_fk = st.id AND (m.modality = $4 OR m.modality = $5))"), "{sql}");
        assert_eq!(c.stmt.args[0], Cell::from("doe%"));
        assert!(c.unfiltered.is_empty());
    }

    #[test]
    fn lower_level_keys_are_rejected_and_unknown_keys_reported() {
        let q = Query::new(QueryLevel::Study).with_key(key(tags::MODALITY, Vr::CS, "CT"));
        assert!(compile(Dialect::Sqlite, &q).is_err());
        let other = Tag::new(0x0018, 0x0050);
        let q = Query::new(QueryLevel::Instance).with_key(key(other, Vr::DS, "1.5"));
        assert_eq!(
            compile(Dialect::Sqlite, &q).unwrap().unfiltered,
            vec![other]
        );
    }

    #[test]
    fn glob_escaping() {
        let q =
            Query::new(QueryLevel::Study).with_key(key(tags::STUDY_DESCRIPTION, Vr::LO, "[a]*"));
        let c = compile(Dialect::Sqlite, &q).unwrap();
        assert_eq!(c.stmt.args[0], Cell::from("[[]a]*"));
    }
}
