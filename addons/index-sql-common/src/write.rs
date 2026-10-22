//! Transactional write paths: storing and removing instances, events, cursors.

use medicus_core::capability::index::{
    BlobRef, DeleteOutcome, DicomJsonObject, Duplicate, DuplicatePolicy, ExtraTagStatus,
    InstanceLocation, InstanceRecord, ResourceUid, StoreOutcome,
};
use medicus_core::event::Event;
use medicus_core::query::Attributes;
use medicus_core::query::values::{fold, normalize, sortable_lower};
use medicus_core::uid::Uid;
use medicus_core::{BlobKey, Error, ErrorKind, Partition, QueryLevel, Result, Tag, VR, tags};
use time::OffsetDateTime;

use crate::extra;
use crate::schema::{COLUMNS, Storage, is_computed};
use crate::sql::{Cell, CellKind, Stmt, Tx, TxExt as _};

pub(crate) fn micros(t: OffsetDateTime) -> i64 {
    i64::try_from(t.unix_timestamp_nanos() / 1000).unwrap_or(i64::MAX)
}

/// The first value of `tag` in the record's attributes, normalized for storage.
fn first_value(record: &InstanceRecord, tag: Tag, vr: VR) -> Option<String> {
    let values = Attributes::values(&record.attributes, tag)?;
    let v = normalize(vr, values.first()?);
    (!v.is_empty()).then_some(v)
}

/// `(column, value)` pairs to store for the attributes of `level`.
fn level_values(
    record: &InstanceRecord,
    level: QueryLevel,
    extras: &[ExtraTagStatus],
) -> Vec<(String, Cell)> {
    let mut out = Vec::new();
    for c in COLUMNS
        .iter()
        .filter(|c| c.level == level && !is_computed(c))
    {
        let v = first_value(record, c.tag, c.vr);
        match c.storage {
            Storage::Text => out.push((c.column.to_owned(), v.into())),
            Storage::PersonName { folded } => {
                out.push((folded.to_owned(), v.as_deref().map(fold).into()));
                out.push((c.column.to_owned(), v.into()));
            }
            Storage::Date => {
                out.push((
                    c.column.to_owned(),
                    v.and_then(|v| sortable_lower(VR::DA, &v)).into(),
                ));
            }
            Storage::Time { key } => {
                out.push((
                    key.to_owned(),
                    v.as_deref().and_then(|v| sortable_lower(VR::TM, v)).into(),
                ));
                out.push((c.column.to_owned(), v.into()));
            }
            Storage::Int => out.push((
                c.column.to_owned(),
                v.and_then(|v| v.trim_start_matches('+').parse::<i64>().ok())
                    .into(),
            )),
            Storage::StudyMulti { .. } | Storage::Count { .. } => {}
        }
    }
    if let Some(json) = extra::level_json(extras, level, &record.attributes) {
        out.push(("extra".to_owned(), Cell::Json(json)));
    }
    out
}

/// `INSERT INTO table (fixed.., cols..) VALUES (..) ON CONFLICT (conflict) DO UPDATE SET cols..
/// RETURNING id`.
fn upsert(
    stmt: &mut Stmt,
    table: &str,
    conflict: &str,
    fixed: &[(&str, Cell)],
    updated: &[(String, Cell)],
    returning: &str,
) {
    let names: Vec<&str> = fixed
        .iter()
        .map(|(n, _)| *n)
        .chain(updated.iter().map(|(n, _)| n.as_str()))
        .collect();
    stmt.push(&format!(
        "INSERT INTO {table} ({}) VALUES ",
        names.join(", ")
    ));
    stmt.bind_list(
        fixed
            .iter()
            .map(|(_, v)| v.clone())
            .chain(updated.iter().map(|(_, v)| v.clone())),
    );
    stmt.push(&format!(" ON CONFLICT ({conflict}) DO UPDATE SET "));
    let sets: Vec<String> = updated
        .iter()
        .map(|(n, _)| format!("{n} = excluded.{n}"))
        .collect();
    stmt.push(&if sets.is_empty() {
        format!("{conflict} = excluded.{conflict}")
    } else {
        sets.join(", ")
    });
    stmt.push(&format!(" RETURNING {returning}"));
}

struct ExistingInstance {
    id: i64,
    location: InstanceLocation,
}

const LOCATION_COLUMNS: &str = "i.id, st.study_uid, se.series_uid, i.sop_uid, i.sop_class, i.transfer_syntax, \
     i.size, i.blob_store, i.blob_key, i.header_length, i.partition";
const LOCATION_KINDS: &[CellKind] = &[
    CellKind::Int,
    CellKind::Text,
    CellKind::Text,
    CellKind::Text,
    CellKind::Text,
    CellKind::Text,
    CellKind::Int,
    CellKind::Text,
    CellKind::Text,
    CellKind::Int,
    CellKind::Text,
];
const LOCATION_FROM: &str = " FROM instances i JOIN series se ON se.id = i.series_fk JOIN studies st ON st.id = se.study_fk";

fn corrupt(what: &str) -> Error {
    Error::internal(format!("index contains an invalid {what}"))
}

fn decode_location(row: &[Cell]) -> Result<ExistingInstance> {
    let text = |i: usize| row.get(i).and_then(Cell::text).map(str::to_owned);
    let int = |i: usize| row.get(i).and_then(Cell::int);
    let uid = |i: usize| {
        text(i)
            .and_then(|s| Uid::parse(&s).ok())
            .ok_or_else(|| corrupt("UID"))
    };
    let blob = |store: usize, key: usize| -> Result<Option<BlobRef>> {
        match (text(store), text(key)) {
            (Some(store), Some(key)) => Ok(Some(BlobRef {
                store,
                key: BlobKey::parse(&key).map_err(|_| corrupt("blob key"))?,
            })),
            _ => Ok(None),
        }
    };
    let size = |i: usize| int(i).and_then(|v| u64::try_from(v).ok()).unwrap_or(0);
    Ok(ExistingInstance {
        id: int(0).ok_or_else(|| corrupt("id"))?,
        location: InstanceLocation {
            partition: text(10)
                .and_then(|p| Partition::parse(&p).ok())
                .ok_or_else(|| corrupt("partition"))?,
            study: uid(1)?.into(),
            series: uid(2)?.into(),
            instance: uid(3)?.into(),
            sop_class: uid(4)?.into(),
            transfer_syntax: uid(5)?.into(),
            size: size(6),
            blob: blob(7, 8)?.ok_or_else(|| corrupt("blob reference"))?,
            header_length: size(9),
        },
    })
}

pub(crate) async fn locate<T: Tx>(
    tx: &mut T,
    partition: &Partition,
    resource: &ResourceUid,
) -> Result<Vec<InstanceLocation>> {
    Ok(locate_rows(tx, partition, resource)
        .await?
        .into_iter()
        .map(|e| e.location)
        .collect())
}

async fn locate_rows<T: Tx>(
    tx: &mut T,
    partition: &Partition,
    resource: &ResourceUid,
) -> Result<Vec<ExistingInstance>> {
    let mut stmt = Stmt::new(tx.dialect());
    stmt.push("SELECT ")
        .push(LOCATION_COLUMNS)
        .push(LOCATION_FROM)
        .push(" WHERE i.partition = ")
        .bind(partition.as_str());
    match resource {
        ResourceUid::Study(uid) => stmt.push(" AND st.study_uid = ").bind(uid.as_str()),
        ResourceUid::Series(uid) => stmt.push(" AND se.series_uid = ").bind(uid.as_str()),
        ResourceUid::Instance(uid) => stmt.push(" AND i.sop_uid = ").bind(uid.as_str()),
    };
    stmt.push(" ORDER BY se.id, i.id");
    let rows = tx.fetch(&stmt, LOCATION_KINDS).await?;
    rows.iter().map(|r| decode_location(r)).collect()
}

/// Removes one instance row and deletes parents that became empty. Counts and aggregates are
/// computed on read, so there is nothing else to maintain.
async fn remove_instance<T: Tx>(tx: &mut T, existing: &ExistingInstance) -> Result<()> {
    let d = tx.dialect();
    let mut s = Stmt::new(d);
    s.push(
        "SELECT se.id, se.study_fk, st.patient_fk FROM instances i JOIN series se ON se.id = i.series_fk \
         JOIN studies st ON st.id = se.study_fk WHERE i.id = ",
    )
    .bind(existing.id);
    let row = tx
        .fetch(&s, &[CellKind::Int, CellKind::Int, CellKind::Int])
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| Error::not_found("instance vanished during removal"))?;
    let id = |i: usize| row.get(i).and_then(Cell::int).ok_or_else(|| corrupt("id"));
    let (series_id, study_id, patient_id) = (id(0)?, id(1)?, id(2)?);

    let mut s = Stmt::new(d);
    s.push("DELETE FROM instances WHERE id = ")
        .bind(existing.id);
    tx.execute(&s).await?;
    let mut s = Stmt::new(d);
    s.push("DELETE FROM series WHERE id = ")
        .bind(series_id)
        .push(" AND NOT EXISTS (SELECT 1 FROM instances WHERE series_fk = ")
        .bind(series_id)
        .push(")");
    tx.execute(&s).await?;
    let mut s = Stmt::new(d);
    s.push("DELETE FROM studies WHERE id = ")
        .bind(study_id)
        .push(" AND NOT EXISTS (SELECT 1 FROM series WHERE study_fk = ")
        .bind(study_id)
        .push(")");
    if tx.execute(&s).await? > 0 {
        let mut s = Stmt::new(d);
        s.push("DELETE FROM patients WHERE id = ")
            .bind(patient_id)
            .push(" AND NOT EXISTS (SELECT 1 FROM studies WHERE patient_fk = ")
            .bind(patient_id)
            .push(")");
        tx.execute(&s).await?;
    }
    Ok(())
}

pub(crate) async fn append_event<T: Tx>(tx: &mut T, event: &Event) -> Result<u64> {
    let mut body = event.clone();
    body.seq = 0;
    let json = serde_json::to_string(&body).map_err(|e| Error::internal(e.to_string()))?;
    let mut s = Stmt::new(tx.dialect());
    s.push("INSERT INTO events (id, kind, partition, time, body) VALUES ")
        .bind_list([
            Cell::from(event.id.to_string()),
            Cell::from(event.kind.as_str()),
            event.partition.as_ref().map(Partition::as_str).into(),
            Cell::from(micros(event.time)),
            Cell::from(json),
        ])
        .push(" RETURNING seq");
    let seq = tx
        .fetch_one_int(&s)
        .await?
        .ok_or_else(|| Error::internal("no sequence number returned"))?;
    u64::try_from(seq).map_err(|_| corrupt("sequence number"))
}

/// Outcome of applying the duplicate policy.
enum PolicyDecision {
    Proceed(Duplicate),
    /// Keep the existing instance; nothing changes.
    Keep,
}

/// Applies the duplicate policy *before* the hierarchy is upserted, so that removing a replaced
/// instance cannot delete the series or study the new instance is about to be inserted into.
async fn apply_duplicate_policy<T: Tx>(
    tx: &mut T,
    record: &InstanceRecord,
    policy: DuplicatePolicy,
) -> Result<PolicyDecision> {
    let existing = locate_rows(
        tx,
        &record.partition,
        &ResourceUid::Instance(record.instance.clone()),
    )
    .await?
    .into_iter()
    .next();
    match (existing, policy) {
        (None, _) => Ok(PolicyDecision::Proceed(Duplicate::None)),
        (Some(_), DuplicatePolicy::Reject) => Err(Error::new(
            ErrorKind::AlreadyExists,
            format!("instance {} already exists", record.instance),
        )),
        (Some(_), DuplicatePolicy::KeepExisting) => Ok(PolicyDecision::Keep),
        (Some(old), DuplicatePolicy::Overwrite) => {
            remove_instance(tx, &old).await?;
            Ok(PolicyDecision::Proceed(Duplicate::Replaced {
                previous: Box::new(old.location),
            }))
        }
    }
}

/// Upserts patient, study and series (last writer wins for attribute values); returns the series
/// id.
async fn upsert_hierarchy<T: Tx>(
    tx: &mut T,
    record: &InstanceRecord,
    extras: &[ExtraTagStatus],
    now: i64,
) -> Result<i64> {
    let d = tx.dialect();
    let patient_id = first_value(record, tags::PATIENT_ID, VR::LO).unwrap_or_default();
    let issuer = first_value(record, tags::ISSUER_OF_PATIENT_ID, VR::LO).unwrap_or_default();
    let mut patient_cols = level_values(record, QueryLevel::Patient, extras);
    patient_cols.retain(|(n, _)| n != "patient_id" && n != "issuer");
    let mut s = Stmt::new(d);
    let partition = record.partition.as_str();
    let key = [
        ("partition", partition.into()),
        ("patient_id", patient_id.into()),
        ("issuer", issuer.into()),
    ];
    upsert(
        &mut s,
        "patients",
        "partition, patient_id, issuer",
        &key,
        &patient_cols,
        "id",
    );
    let patient_fk = tx
        .fetch_one_int(&s)
        .await?
        .ok_or_else(|| corrupt("patient id"))?;

    let mut study_cols = level_values(record, QueryLevel::Study, extras);
    study_cols.retain(|(n, _)| n != "study_uid");
    study_cols.push(("updated_at".into(), now.into()));
    let mut s = Stmt::new(d);
    let key = [
        ("partition", partition.into()),
        ("study_uid", record.study.as_str().into()),
        ("patient_fk", patient_fk.into()),
        ("received_at", now.into()),
    ];
    upsert(
        &mut s,
        "studies",
        "partition, study_uid",
        &key,
        &study_cols,
        "id",
    );
    let study_fk = tx
        .fetch_one_int(&s)
        .await?
        .ok_or_else(|| corrupt("study id"))?;

    let mut series_cols = level_values(record, QueryLevel::Series, extras);
    series_cols.retain(|(n, _)| n != "series_uid");
    series_cols.push(("updated_at".into(), now.into()));
    let mut s = Stmt::new(d);
    let key = [
        ("partition", partition.into()),
        ("series_uid", record.series.as_str().into()),
        ("study_fk", study_fk.into()),
    ];
    upsert(
        &mut s,
        "series",
        "partition, series_uid",
        &key,
        &series_cols,
        "id, study_fk",
    );
    let row = tx
        .fetch(&s, &[CellKind::Int, CellKind::Int])
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| corrupt("series id"))?;
    let series_fk = row[0].int().ok_or_else(|| corrupt("series id"))?;
    if row[1].int() != Some(study_fk) {
        return Err(Error::invalid_argument(format!(
            "series {} already belongs to a different study than {}",
            record.series, record.study
        )));
    }
    Ok(series_fk)
}

async fn insert_instance<T: Tx>(
    tx: &mut T,
    record: &InstanceRecord,
    extras: &[ExtraTagStatus],
    series_fk: i64,
    now: i64,
) -> Result<()> {
    let mut cols = level_values(record, QueryLevel::Instance, extras);
    cols.retain(|(n, _)| n != "sop_uid" && n != "sop_class");
    let mut fixed: Vec<(&str, Cell)> = vec![
        ("partition", record.partition.as_str().into()),
        ("series_fk", series_fk.into()),
        ("sop_uid", record.instance.as_str().into()),
        ("sop_class", record.sop_class.as_str().into()),
        ("transfer_syntax", record.transfer_syntax.as_str().into()),
        ("size", to_i64(record.size).into()),
        ("blob_store", record.blob.store.as_str().into()),
        ("blob_key", record.blob.key.as_str().into()),
        ("header_length", to_i64(record.header_length).into()),
        ("received_at", now.into()),
    ];
    fixed.extend(cols.iter().map(|(n, v)| (n.as_str(), v.clone())));
    let names: Vec<&str> = fixed.iter().map(|(n, _)| *n).collect();
    let mut s = Stmt::new(tx.dialect());
    s.push(&format!(
        "INSERT INTO instances ({}) VALUES ",
        names.join(", ")
    ));
    s.bind_list(fixed.into_iter().map(|(_, v)| v));
    tx.execute(&s).await?;
    Ok(())
}

fn to_i64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

pub(crate) async fn store_instance<T: Tx>(
    tx: &mut T,
    record: &InstanceRecord,
    policy: DuplicatePolicy,
    event: &Event,
    extras: &[ExtraTagStatus],
) -> Result<StoreOutcome> {
    let now = micros(event.time);
    let duplicate = match apply_duplicate_policy(tx, record, policy).await? {
        PolicyDecision::Proceed(d) => d,
        PolicyDecision::Keep => {
            return Ok(StoreOutcome {
                seq: None,
                duplicate: Duplicate::KeptExisting,
            });
        }
    };
    let series_fk = upsert_hierarchy(tx, record, extras, now).await?;
    insert_instance(tx, record, extras, series_fk, now).await?;
    // The event is part of the same transaction (transactional outbox).
    let seq = append_event(tx, event).await?;
    Ok(StoreOutcome {
        seq: Some(seq),
        duplicate,
    })
}

pub(crate) async fn delete<T: Tx>(
    tx: &mut T,
    partition: &Partition,
    resource: &ResourceUid,
    event: &Event,
) -> Result<DeleteOutcome> {
    let rows = locate_rows(tx, partition, resource).await?;
    if rows.is_empty() {
        return Err(Error::not_found(format!("{resource:?} does not exist")));
    }
    for row in &rows {
        remove_instance(tx, row).await?;
    }
    let seq = append_event(tx, event).await?;
    Ok(DeleteOutcome {
        seq,
        removed: rows.into_iter().map(|r| r.location).collect(),
    })
}

/// Rewrites the `extra` JSON of an instance and its parents (backfill).
pub(crate) async fn update_extra<T: Tx>(
    tx: &mut T,
    partition: &Partition,
    instance: &medicus_core::uid::SopInstanceUid,
    attributes: &DicomJsonObject,
    extras: &[ExtraTagStatus],
) -> Result<()> {
    let d = tx.dialect();
    let mut s = Stmt::new(d);
    s.push(
        "SELECT i.id, se.id, st.id, st.patient_fk FROM instances i JOIN series se ON se.id = i.series_fk \
         JOIN studies st ON st.id = se.study_fk WHERE i.partition = ",
    )
    .bind(partition.as_str())
    .push(" AND i.sop_uid = ")
    .bind(instance.as_str());
    let kinds = [CellKind::Int, CellKind::Int, CellKind::Int, CellKind::Int];
    let row = tx
        .fetch(&s, &kinds)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| Error::not_found(format!("instance {instance} does not exist")))?;
    let ids = [
        QueryLevel::Instance,
        QueryLevel::Series,
        QueryLevel::Study,
        QueryLevel::Patient,
    ];
    for (level, id) in ids.into_iter().zip(row) {
        let Some(json) = extra::level_json(extras, level, attributes) else {
            continue;
        };
        let mut s = Stmt::new(d);
        s.push(&format!("UPDATE {} SET extra = ", extra::table(level)))
            .bind(Cell::Json(json))
            .push(" WHERE id = ")
            .bind(id);
        tx.execute(&s).await?;
    }
    Ok(())
}
