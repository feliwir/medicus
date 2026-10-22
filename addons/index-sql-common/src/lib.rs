//! Database-neutral SQL implementation of [`IndexStore`].
//!
//! A concrete backend (`SQLite`, `PostgreSQL`) only implements [`Backend`]: opening transactions and
//! executing [`Stmt`]s. Schema mapping, query compilation and all write logic live here, so both
//! backends behave identically, which the shared conformance suite verifies.
//!
//! # Event ordering requirement
//! Subscribers read events strictly in `seq` order and remember the last one they processed. A
//! backend must therefore guarantee that an event with a lower `seq` is never committed *after*
//! one with a higher `seq` became visible. `SQLite` does so trivially (one writer at a time);
//! backends with concurrent writers must serialize event insertion (e.g. an advisory lock).

pub mod schema;
pub mod sql;

mod extra;
mod jobs;
mod query;
mod write;

use medicus_core::TagExt as _;
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use medicus_core::capability::index::{
    DeleteOutcome, DicomJsonObject, DuplicatePolicy, ExtraTag, ExtraTagState, ExtraTagStatus,
    InstanceLocation, InstanceRecord, QueryResult, ResourceUid, StoreOutcome,
};
use medicus_core::capability::{IndexStore, Job, JobState};
use medicus_core::event::Event;
use medicus_core::query::Query;
use medicus_core::uid::SopInstanceUid;
use medicus_core::{Error, Result};
use medicus_core::{Tag, VR};

pub use sql::{Backend, Cell, CellKind, Dialect, Stmt, Tx};

use crate::sql::TxExt as _;

fn corrupt(what: &str) -> Error {
    Error::internal(format!("index contains an invalid {what}"))
}

fn to_i64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

/// An [`IndexStore`] on top of any SQL [`Backend`].
#[derive(Debug)]
pub struct SqlIndex<B> {
    backend: B,
    /// Cached copy of the `extra_tags` table; loaded on first use.
    extras: RwLock<Option<Arc<Vec<ExtraTagStatus>>>>,
}

impl<B: Backend> SqlIndex<B> {
    pub fn new(backend: B) -> Self {
        Self {
            backend,
            extras: RwLock::new(None),
        }
    }

    pub fn backend(&self) -> &B {
        &self.backend
    }

    fn cached_extras(&self) -> Option<Arc<Vec<ExtraTagStatus>>> {
        self.extras
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn set_cached_extras(&self, v: Vec<ExtraTagStatus>) -> Arc<Vec<ExtraTagStatus>> {
        let v = Arc::new(v);
        *self
            .extras
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(v.clone());
        v
    }

    async fn load_extras<T: Tx>(tx: &mut T) -> Result<Vec<ExtraTagStatus>> {
        let s = Stmt::raw(
            tx.dialect(),
            "SELECT tag, vr, level, state FROM extra_tags ORDER BY tag",
        );
        let rows = tx.fetch(&s, &[CellKind::Text; 4]).await?;
        rows.iter()
            .map(|r| {
                let text = |i: usize| r.get(i).and_then(Cell::text).unwrap_or_default();
                let tag: Tag = text(0).parse().map_err(|_| corrupt("extra tag"))?;
                let vr: VR = text(1).parse().map_err(|_| corrupt("extra tag VR"))?;
                let level =
                    extra::parse_level(text(2)).ok_or_else(|| corrupt("extra tag level"))?;
                let state = if text(3) == "ready" {
                    ExtraTagState::Ready
                } else {
                    ExtraTagState::Backfilling
                };
                Ok(ExtraTagStatus {
                    tag: ExtraTag { tag, vr, level },
                    state,
                })
            })
            .collect()
    }

    async fn extras(&self) -> Result<Arc<Vec<ExtraTagStatus>>> {
        if let Some(e) = self.cached_extras() {
            return Ok(e);
        }
        let mut tx = self.backend.begin_read().await?;
        let loaded = Self::load_extras(&mut tx).await?;
        Ok(self.set_cached_extras(loaded))
    }
}

#[async_trait]
impl<B: Backend> IndexStore for SqlIndex<B> {
    async fn store_instance(
        &self,
        record: &InstanceRecord,
        policy: DuplicatePolicy,
        event: &Event,
    ) -> Result<StoreOutcome> {
        let extras = self.extras().await?;
        let mut tx = self.backend.begin_write().await?;
        let outcome = write::store_instance(&mut tx, record, policy, event, &extras).await?;
        tx.commit().await?;
        Ok(outcome)
    }

    async fn query(&self, query: &Query) -> Result<QueryResult> {
        let extras = self.extras().await?;
        let compiled = query::compile(self.backend.dialect(), query, &extras)?;
        let mut tx = self.backend.begin_read().await?;
        let mut rows = tx.fetch(&compiled.stmt, &compiled.kinds).await?;
        let has_more = compiled.limit.is_some_and(|l| rows.len() > l as usize);
        if let Some(l) = compiled.limit {
            rows.truncate(l as usize);
        }
        Ok(QueryResult {
            rows: rows
                .into_iter()
                .map(|r| query::row_to_json(&compiled, r, &extras))
                .collect(),
            has_more,
            unfiltered: compiled.unfiltered,
        })
    }

    async fn locate(&self, resource: &ResourceUid) -> Result<Vec<InstanceLocation>> {
        let mut tx = self.backend.begin_read().await?;
        write::locate(&mut tx, resource).await
    }

    async fn delete(&self, resource: &ResourceUid, event: &Event) -> Result<DeleteOutcome> {
        let mut tx = self.backend.begin_write().await?;
        let outcome = write::delete(&mut tx, resource, event).await?;
        tx.commit().await?;
        Ok(outcome)
    }

    async fn append_event(&self, event: &Event) -> Result<u64> {
        let mut tx = self.backend.begin_write().await?;
        let seq = write::append_event(&mut tx, event).await?;
        tx.commit().await?;
        Ok(seq)
    }

    async fn events_after(&self, after: u64, limit: u32) -> Result<Vec<Event>> {
        let mut tx = self.backend.begin_read().await?;
        let mut s = Stmt::new(self.backend.dialect());
        s.push("SELECT seq, body FROM events WHERE seq > ")
            .bind(to_i64(after))
            .push(" ORDER BY seq LIMIT ")
            .bind(i64::from(limit));
        tx.fetch(&s, &[CellKind::Int, CellKind::Text])
            .await?
            .into_iter()
            .map(|row| {
                let seq = row
                    .first()
                    .and_then(Cell::int)
                    .and_then(|s| u64::try_from(s).ok());
                let body = row.get(1).and_then(Cell::text);
                let (Some(seq), Some(body)) = (seq, body) else {
                    return Err(corrupt("event"));
                };
                let mut event: Event =
                    serde_json::from_str(body).map_err(|_| corrupt("event body"))?;
                event.seq = seq;
                Ok(event)
            })
            .collect()
    }

    async fn last_event_seq(&self) -> Result<u64> {
        let mut tx = self.backend.begin_read().await?;
        let s = Stmt::raw(
            self.backend.dialect(),
            "SELECT COALESCE(MAX(seq), 0) FROM events",
        );
        Ok(tx
            .fetch_one_int(&s)
            .await?
            .and_then(|v| u64::try_from(v).ok())
            .unwrap_or(0))
    }

    async fn cursor(&self, subscriber: &str) -> Result<Option<u64>> {
        let mut tx = self.backend.begin_read().await?;
        let mut s = Stmt::new(self.backend.dialect());
        s.push("SELECT seq FROM cursors WHERE subscriber = ")
            .bind(subscriber);
        Ok(tx
            .fetch_one_int(&s)
            .await?
            .and_then(|v| u64::try_from(v).ok()))
    }

    async fn set_cursor(&self, subscriber: &str, seq: u64) -> Result<()> {
        let mut tx = self.backend.begin_write().await?;
        let mut s = Stmt::new(self.backend.dialect());
        s.push("INSERT INTO cursors (subscriber, seq) VALUES ")
            .bind_list([Cell::from(subscriber), Cell::Int(to_i64(seq))])
            .push(" ON CONFLICT (subscriber) DO UPDATE SET seq = excluded.seq");
        tx.execute(&s).await?;
        tx.commit().await
    }

    async fn prune_events(&self, up_to: u64) -> Result<u64> {
        let mut tx = self.backend.begin_write().await?;
        let mut s = Stmt::new(self.backend.dialect());
        s.push("DELETE FROM events WHERE seq <= ")
            .bind(to_i64(up_to));
        let n = tx.execute(&s).await?;
        tx.commit().await?;
        Ok(n)
    }

    async fn stored_bytes(&self) -> Result<BTreeMap<String, u64>> {
        let mut tx = self.backend.begin_read().await?;
        let s = Stmt::raw(
            self.backend.dialect(),
            "SELECT blob_store, CAST(SUM(size) AS BIGINT) FROM instances GROUP BY blob_store",
        );
        let mut out = BTreeMap::new();
        for row in tx.fetch(&s, &[CellKind::Text, CellKind::Int]).await? {
            if let (Some(store), Some(bytes)) = (row[0].text(), row[1].int()) {
                *out.entry(store.to_owned()).or_insert(0) += u64::try_from(bytes).unwrap_or(0);
            }
        }
        Ok(out)
    }

    async fn configure_extra_tags(&self, tags: &[ExtraTag]) -> Result<Vec<ExtraTagStatus>> {
        extra::validate(tags)?;
        let d = self.backend.dialect();
        let mut tx = self.backend.begin_write().await?;
        let existing = Self::load_extras(&mut tx).await?;
        let empty = tx
            .fetch_one_int(&Stmt::raw(d, "SELECT COUNT(*) FROM instances"))
            .await?
            .unwrap_or(0)
            == 0;
        let mut result = Vec::with_capacity(tags.len());
        for t in tags {
            let state = match existing.iter().find(|e| e.tag == *t) {
                Some(e) => e.state,
                None if empty => ExtraTagState::Ready,
                None => ExtraTagState::Backfilling,
            };
            let mut s = Stmt::new(d);
            s.push("INSERT INTO extra_tags (tag, vr, level, state) VALUES ")
                .bind_list([
                    Cell::from(t.tag.json_key()),
                    Cell::from(t.vr.to_string()),
                    Cell::from(extra::level_name(t.level)),
                    Cell::from(extra::state_name(state)),
                ])
                .push(" ON CONFLICT (tag) DO UPDATE SET vr = excluded.vr, level = excluded.level, state = excluded.state");
            tx.execute(&s).await?;
            result.push(ExtraTagStatus { tag: *t, state });
        }
        for e in existing
            .iter()
            .filter(|e| !tags.iter().any(|t| t.tag == e.tag.tag))
        {
            let mut s = Stmt::new(d);
            s.push("DELETE FROM extra_tags WHERE tag = ")
                .bind(e.tag.tag.json_key());
            tx.execute(&s).await?;
        }
        tx.commit().await?;
        self.set_cached_extras(result.clone());
        Ok(result)
    }

    async fn update_extra(
        &self,
        instance: &SopInstanceUid,
        attributes: &DicomJsonObject,
    ) -> Result<()> {
        let extras = self.extras().await?;
        let mut tx = self.backend.begin_write().await?;
        write::update_extra(&mut tx, instance, attributes, &extras).await?;
        tx.commit().await
    }

    async fn mark_extra_ready(&self, tag: Tag) -> Result<()> {
        let mut tx = self.backend.begin_write().await?;
        let mut s = Stmt::new(self.backend.dialect());
        s.push("UPDATE extra_tags SET state = 'ready' WHERE tag = ")
            .bind(tag.json_key());
        if tx.execute(&s).await? == 0 {
            return Err(Error::not_found(format!(
                "{tag} is not a configured extra tag"
            )));
        }
        let extras = Self::load_extras(&mut tx).await?;
        tx.commit().await?;
        self.set_cached_extras(extras);
        Ok(())
    }
    async fn put_job(&self, job: &Job) -> Result<()> {
        jobs::put(&self.backend, job).await
    }

    async fn get_job(&self, id: uuid::Uuid) -> Result<Option<Job>> {
        jobs::get(&self.backend, id).await
    }

    async fn list_jobs(
        &self,
        state: Option<JobState>,
        limit: u32,
        offset: u64,
    ) -> Result<Vec<Job>> {
        jobs::list(&self.backend, state, limit, offset).await
    }

    async fn claim_job(&self, now: time::OffsetDateTime) -> Result<Option<Job>> {
        jobs::claim(&self.backend, now).await
    }
}
