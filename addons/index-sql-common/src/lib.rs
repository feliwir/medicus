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

mod query;
mod write;

use std::collections::BTreeMap;

use async_trait::async_trait;
use medicus_core::capability::IndexStore;
use medicus_core::capability::index::{
    DeleteOutcome, DuplicatePolicy, InstanceLocation, InstanceRecord, QueryResult, ResourceUid,
    StoreOutcome,
};
use medicus_core::event::Event;
use medicus_core::query::Query;
use medicus_core::{Error, Result};

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
}

impl<B: Backend> SqlIndex<B> {
    pub fn new(backend: B) -> Self {
        Self { backend }
    }

    pub fn backend(&self) -> &B {
        &self.backend
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
        let mut tx = self.backend.begin_write().await?;
        let outcome = write::store_instance(&mut tx, record, policy, event).await?;
        tx.commit().await?;
        Ok(outcome)
    }

    async fn query(&self, query: &Query) -> Result<QueryResult> {
        let compiled = query::compile(self.backend.dialect(), query)?;
        let mut tx = self.backend.begin_read().await?;
        let mut rows = tx.fetch(&compiled.stmt, &compiled.kinds).await?;
        let has_more = compiled.limit.is_some_and(|l| rows.len() > l as usize);
        if let Some(l) = compiled.limit {
            rows.truncate(l as usize);
        }
        Ok(QueryResult {
            rows: rows
                .into_iter()
                .map(|r| query::row_to_json(&compiled.select, r))
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
}
