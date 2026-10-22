//! Persisted jobs: the job as a JSON body plus the columns needed to schedule and list them.

use medicus_core::capability::{Job, JobState};
use medicus_core::{Error, Result};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::sql::{Backend, Cell, CellKind, Dialect, Stmt, Tx};

fn millis(t: OffsetDateTime) -> i64 {
    i64::try_from(t.unix_timestamp_nanos() / 1_000_000).unwrap_or(i64::MAX)
}

fn decode(rows: Vec<Vec<Cell>>) -> Result<Vec<Job>> {
    rows.into_iter()
        .map(|row| {
            let body = row.first().and_then(Cell::text).unwrap_or_default();
            serde_json::from_str(body).map_err(|e| Error::internal(format!("corrupt job row: {e}")))
        })
        .collect()
}

async fn write<T: Tx>(tx: &mut T, job: &Job) -> Result<()> {
    let body = serde_json::to_string(job).map_err(|e| Error::internal(e.to_string()))?;
    let mut s = Stmt::new(tx.dialect());
    s.push("INSERT INTO jobs (id, state, created_at, not_before, body) VALUES ")
        .bind_list([
            Cell::from(job.id.to_string()),
            Cell::from(job.state.as_str()),
            Cell::Int(millis(job.created_at)),
            Cell::Int(millis(job.not_before)),
            Cell::from(body),
        ])
        .push(
            " ON CONFLICT (id) DO UPDATE SET state = excluded.state, \
             not_before = excluded.not_before, body = excluded.body",
        );
    tx.execute(&s).await?;
    Ok(())
}

pub(crate) async fn put<B: Backend>(backend: &B, job: &Job) -> Result<()> {
    let mut tx = backend.begin_write().await?;
    write(&mut tx, job).await?;
    tx.commit().await
}

pub(crate) async fn get<B: Backend>(backend: &B, id: Uuid) -> Result<Option<Job>> {
    let mut tx = backend.begin_read().await?;
    let mut s = Stmt::new(backend.dialect());
    s.push("SELECT body FROM jobs WHERE id = ")
        .bind(id.to_string());
    Ok(decode(tx.fetch(&s, &[CellKind::Text]).await?)?.pop())
}

pub(crate) async fn list<B: Backend>(
    backend: &B,
    state: Option<JobState>,
    limit: u32,
    offset: u64,
) -> Result<Vec<Job>> {
    let mut tx = backend.begin_read().await?;
    let mut s = Stmt::new(backend.dialect());
    s.push("SELECT body FROM jobs");
    if let Some(state) = state {
        s.push(" WHERE state = ").bind(state.as_str());
    }
    s.push(" ORDER BY created_at DESC, id DESC LIMIT ")
        .bind(i64::from(limit))
        .push(" OFFSET ")
        .bind(i64::try_from(offset).unwrap_or(i64::MAX));
    decode(tx.fetch(&s, &[CellKind::Text]).await?)
}

pub(crate) async fn claim<B: Backend>(backend: &B, now: OffsetDateTime) -> Result<Option<Job>> {
    let mut tx = backend.begin_write().await?;
    let mut s = Stmt::new(backend.dialect());
    s.push("SELECT body FROM jobs WHERE state = ")
        .bind(JobState::Queued.as_str())
        .push(" AND not_before <= ")
        .bind(millis(now))
        .push(" ORDER BY not_before, created_at LIMIT 1");
    if backend.dialect() == Dialect::Postgres {
        // SQLite serializes writers; PostgreSQL needs the row lock.
        s.push(" FOR UPDATE SKIP LOCKED");
    }
    let Some(mut job) = decode(tx.fetch(&s, &[CellKind::Text]).await?)?.pop() else {
        return Ok(None);
    };
    job.state = JobState::Running;
    job.attempts += 1;
    job.updated_at = now;
    write(&mut tx, &job).await?;
    tx.commit().await?;
    Ok(Some(job))
}
