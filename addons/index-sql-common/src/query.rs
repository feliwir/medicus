//! Compilation of a [`Query`] into SQL, and of result rows into DICOM JSON.
//!
//! The SQL semantics deliberately mirror `medicus_core::query::Matcher::matches` (same value
//! normalization, same sortable date/time forms, same PN case folding), so an index answer equals
//! what the in-memory matcher would return. The shared conformance suite checks this.

use medicus_core::capability::index::{DicomJsonObject, ExtraTagState, ExtraTagStatus};
use medicus_core::query::values::{fold, normalize, sortable_lower, sortable_upper};
use medicus_core::query::{Key, MatchOptions, Matcher, OrderKey, Query};
use medicus_core::{Error, QueryLevel, Result, Tag, Vr};
use serde_json::{Value, json};

use crate::extra;
use crate::schema::{
    COLUMNS, Column, Storage, column, from_clause, id_expr, is_computed, size_expr,
};
use crate::sql::{Cell, CellKind, Dialect, Stmt};

pub(crate) struct Compiled {
    pub stmt: Stmt,
    pub select: Vec<&'static Column>,
    // The levels' `extra` JSON (if any) is selected after `select`, see `row_to_json`.
    pub kinds: Vec<CellKind>,
    pub unfiltered: Vec<Tag>,
    pub limit: Option<u32>,
}

/// Columns returned at a level: the stored attributes of this and the parent levels, and the
/// computed ones (counts, aggregates) of this level only, since they cost a subquery per row.
fn selected(level: QueryLevel) -> Vec<&'static Column> {
    COLUMNS
        .iter()
        .filter(|c| c.level <= level)
        .filter(|c| !is_computed(c) || c.level == level)
        .collect()
}

fn select_expr(c: &Column, dialect: Dialect) -> String {
    match c.storage {
        Storage::StudyMulti { from } => {
            let agg = match dialect {
                Dialect::Sqlite => "group_concat(x, '\\')",
                Dialect::Postgres => "string_agg(x, '\\' ORDER BY x)",
            };
            format!(
                "(SELECT {agg} FROM (SELECT DISTINCT m.{col} AS x FROM {from} AND m.{col} IS NOT NULL \
                 AND m.{col} <> '' ORDER BY x) agg)",
                col = c.column
            )
        }
        Storage::Count { expr } => expr.to_owned(),
        _ => c.expr(),
    }
}

fn kind(c: &Column) -> CellKind {
    match c.storage {
        Storage::Int | Storage::Count { .. } => CellKind::Int,
        _ => CellKind::Text,
    }
}

pub(crate) fn compile(dialect: Dialect, q: &Query, extras: &[ExtraTagStatus]) -> Result<Compiled> {
    let select = selected(q.level);
    let extra_levels: Vec<QueryLevel> = QueryLevel::ALL
        .into_iter()
        .filter(|l| *l <= q.level && extras.iter().any(|e| e.tag.level == *l))
        .collect();
    let mut kinds: Vec<CellKind> = select.iter().map(|c| kind(c)).collect();
    kinds.extend(extra_levels.iter().map(|_| CellKind::Text));
    let mut stmt = Stmt::new(dialect);
    stmt.push("SELECT ");
    let mut exprs: Vec<String> = select.iter().map(|c| select_expr(c, dialect)).collect();
    exprs.extend(extra_levels.iter().map(|l| extra::select_expr(*l, dialect)));
    stmt.push(&exprs.join(", "));
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
            Some(_) => unfiltered.push(key.tag),
            None => match extra::find(extras, key.tag) {
                Some(e) if e.tag.level > q.level => {
                    return Err(Error::invalid_argument(format!(
                        "attribute {} belongs to a level below the query level {}",
                        key.tag, q.level
                    )));
                }
                Some(e)
                    if e.state == ExtraTagState::Ready
                        && extra::compilable(e.tag, &key.matcher, &q.options) =>
                {
                    stmt.push(" AND (");
                    extra::condition(&mut stmt, e.tag, &key.matcher, &q.options);
                    stmt.push(")");
                }
                _ => unfiltered.push(key.tag),
            },
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
            OrderKey::Size => size_expr(q.level).map(str::to_owned),
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
        Storage::Int | Storage::Count { .. } => v
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
        Storage::Count { expr } => expr.to_owned(),
        _ => c.expr(),
    }
}

/// Appends a boolean SQL condition for one key.
fn condition(stmt: &mut Stmt, c: &Column, key: &Key, opts: &MatchOptions) {
    if let Storage::StudyMulti { from } = c.storage {
        return multi_condition(stmt, c, from, &key.matcher, opts);
    }
    scalar_condition(stmt, c, &match_expr(c, opts), &key.matcher, opts);
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
    wildcard_on(stmt, expr, &p);
}

/// `expr` matches the DICOM wildcard `pattern` (`*`, `?`), case-sensitively. `pattern` must
/// already be normalized (and folded, for case-insensitive PN matching).
pub(crate) fn wildcard_on(stmt: &mut Stmt, expr: &str, pattern: &str) {
    let p = pattern;
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
fn multi_condition(stmt: &mut Stmt, c: &Column, from: &str, m: &Matcher, opts: &MatchOptions) {
    let exists = |stmt: &mut Stmt, f: &mut dyn FnMut(&mut Stmt)| {
        stmt.push(&format!("EXISTS (SELECT 1 FROM {from} AND "));
        f(stmt);
        stmt.push(")");
    };
    let expr = match_expr(c, opts);
    match m {
        Matcher::Empty => {
            stmt.push(&format!(
                "NOT EXISTS (SELECT 1 FROM {from} AND {expr} IS NOT NULL AND {expr} <> '')"
            ));
        }
        Matcher::AllOf { values } => join_conditions(stmt, values, " AND ", |s, v| {
            exists(s, &mut |s| eq(s, c, &expr, v, opts));
        }),
        other => exists(stmt, &mut |s| scalar_condition(s, c, &expr, other, opts)),
    }
}

/// Converts a fetched row into DICOM JSON, including the extra tags of the selected levels.
pub(crate) fn row_to_json(
    compiled: &Compiled,
    mut row: Vec<Cell>,
    extras: &[ExtraTagStatus],
) -> DicomJsonObject {
    let extra_cells = row.split_off(compiled.select.len().min(row.len()));
    let mut obj = DicomJsonObject::new();
    for cell in &extra_cells {
        if let Some(json) = cell.text() {
            extra::merge_into(&mut obj, json, extras);
        }
    }
    for (c, cell) in compiled.select.iter().zip(row) {
        let vr = c.vr.as_str();
        // JSON cells are only ever bound, never fetched; treat them as text for completeness.
        let cell = match cell {
            Cell::Json(s) => Cell::Text(s),
            other => other,
        };
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
            Cell::Json(_) => unreachable!("converted above"),
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
        let c = compile(Dialect::Postgres, &q, &[]).unwrap();
        let sql = &c.stmt.sql;
        assert!(
            sql.contains("p.patient_name_folded LIKE $1 ESCAPE"),
            "{sql}"
        );
        assert!(
            sql.contains("st.study_date >= $2 AND st.study_date <= $3"),
            "{sql}"
        );
        assert!(sql.contains("EXISTS (SELECT 1 FROM series m WHERE m.study_fk = st.id AND (m.modality = $4 OR m.modality = $5))"), "{sql}");
        assert_eq!(c.stmt.args[0], Cell::from("doe%"));
        assert!(c.unfiltered.is_empty());
    }

    #[test]
    fn lower_level_keys_are_rejected_and_unknown_keys_reported() {
        let q = Query::new(QueryLevel::Study).with_key(key(tags::MODALITY, Vr::CS, "CT"));
        assert!(compile(Dialect::Sqlite, &q, &[]).is_err());
        let other = Tag::new(0x0018, 0x0050);
        let q = Query::new(QueryLevel::Instance).with_key(key(other, Vr::DS, "1.5"));
        assert_eq!(
            compile(Dialect::Sqlite, &q, &[]).unwrap().unfiltered,
            vec![other]
        );
    }

    #[test]
    fn glob_escaping() {
        let q =
            Query::new(QueryLevel::Study).with_key(key(tags::STUDY_DESCRIPTION, Vr::LO, "[a]*"));
        let c = compile(Dialect::Sqlite, &q, &[]).unwrap();
        assert_eq!(c.stmt.args[0], Cell::from("[[]a]*"));
    }
}
