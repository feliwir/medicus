//! `index-sqlite`: the medicus `IndexStore` on `SQLite`.
//!
//! ```toml
//! [addons.index-sqlite]
//! config = { path = "data/index.sqlite", read_connections = 4 }
//! ```
//!
//! Writes go through a single connection (`SQLite` allows one writer at a time anyway), which also
//! keeps the event outbox strictly ordered. Reads use a separate pool and run concurrently with
//! writes thanks to WAL mode.

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use index_sql_common::{Backend, Cell, CellKind, Dialect, SqlIndex, Stmt, Tx};
use medicus_addon_sdk::{Registrar, block_on, static_addon};
use medicus_core::{Error, ErrorKind, Result};
use serde::Deserialize;
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteSynchronous,
};
use sqlx::{AssertSqlSafe, Row as _, Sqlite};

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Database file; created if missing.
    pub path: PathBuf,
    /// Name under which the index is registered.
    #[serde(default = "default_name")]
    pub name: String,
    /// Size of the read connection pool.
    #[serde(default = "default_read_connections")]
    pub read_connections: u32,
}

fn default_name() -> String {
    "sqlite".into()
}

fn default_read_connections() -> u32 {
    4
}

fn db_error(e: &sqlx::Error) -> Error {
    let kind = match e {
        sqlx::Error::Database(d) if d.is_unique_violation() => ErrorKind::AlreadyExists,
        sqlx::Error::PoolTimedOut | sqlx::Error::PoolClosed => ErrorKind::Unavailable,
        sqlx::Error::Database(d) if d.message().contains("database is locked") => {
            ErrorKind::Unavailable
        }
        _ => ErrorKind::Internal,
    };
    Error::new(kind, format!("sqlite: {e}"))
}

#[derive(Debug, Clone)]
pub struct SqliteBackend {
    write: SqlitePool,
    read: SqlitePool,
}

impl SqliteBackend {
    /// Opens (creating and migrating if needed) the database at `cfg.path`.
    pub async fn open(cfg: &Config) -> Result<Self> {
        if let Some(dir) = cfg.path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)
                .map_err(|e| Error::internal(format!("creating {}: {e}", dir.display())))?;
        }
        let options = SqliteConnectOptions::new()
            .filename(&cfg.path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(10));
        let write = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options.clone())
            .await
            .map_err(|e| db_error(&e))?;
        MIGRATOR
            .run(&write)
            .await
            .map_err(|e| Error::internal(format!("migrating {}: {e}", cfg.path.display())))?;
        let read = SqlitePoolOptions::new()
            .max_connections(cfg.read_connections.max(1))
            .connect_with(options.read_only(true))
            .await
            .map_err(|e| db_error(&e))?;
        Ok(Self { write, read })
    }
}

pub struct SqliteTx(sqlx::Transaction<'static, Sqlite>);

impl std::fmt::Debug for SqliteTx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SqliteTx")
    }
}

fn bind<'q>(
    mut q: sqlx::query::Query<'q, Sqlite, <Sqlite as sqlx::Database>::Arguments>,
    args: &'q [Cell],
) -> sqlx::query::Query<'q, Sqlite, <Sqlite as sqlx::Database>::Arguments> {
    for a in args {
        q = match a {
            Cell::Null => q.bind(None::<String>),
            Cell::Text(s) => q.bind(s.as_str()),
            Cell::Int(i) => q.bind(*i),
        };
    }
    q
}

#[async_trait]
impl Tx for SqliteTx {
    fn dialect(&self) -> Dialect {
        Dialect::Sqlite
    }

    async fn execute(&mut self, stmt: &Stmt) -> Result<u64> {
        let q = bind(sqlx::query(AssertSqlSafe(stmt.sql.as_str())), &stmt.args);
        let r = q.execute(&mut *self.0).await.map_err(|e| db_error(&e))?;
        Ok(r.rows_affected())
    }

    async fn fetch(&mut self, stmt: &Stmt, kinds: &[CellKind]) -> Result<Vec<Vec<Cell>>> {
        let q = bind(sqlx::query(AssertSqlSafe(stmt.sql.as_str())), &stmt.args);
        let rows = q.fetch_all(&mut *self.0).await.map_err(|e| db_error(&e))?;
        rows.iter()
            .map(|row| {
                kinds
                    .iter()
                    .enumerate()
                    .map(|(i, k)| match k {
                        CellKind::Text => row.try_get::<Option<String>, _>(i).map(Cell::from),
                        CellKind::Int => row.try_get::<Option<i64>, _>(i).map(Cell::from),
                    })
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(|e| db_error(&e))
            })
            .collect()
    }

    async fn commit(self) -> Result<()> {
        self.0.commit().await.map_err(|e| db_error(&e))
    }
}

#[async_trait]
impl Backend for SqliteBackend {
    type Tx = SqliteTx;

    fn dialect(&self) -> Dialect {
        Dialect::Sqlite
    }

    async fn begin_write(&self) -> Result<SqliteTx> {
        self.write
            .begin()
            .await
            .map(SqliteTx)
            .map_err(|e| db_error(&e))
    }

    async fn begin_read(&self) -> Result<SqliteTx> {
        self.read
            .begin()
            .await
            .map(SqliteTx)
            .map_err(|e| db_error(&e))
    }
}

/// Addon initialization.
pub fn init(reg: &mut Registrar) -> Result<()> {
    let cfg: Config = reg.config()?;
    let backend = block_on(SqliteBackend::open(&cfg))?;
    tracing::info!(name = %cfg.name, path = %cfg.path.display(), "sqlite index ready");
    reg.register_index_store(&cfg.name, SqlIndex::new(backend))
}

/// Entry points for linking this addon into the server binary.
pub static ADDON: medicus_addon_sdk::abi::StaticAddon = static_addon!(
    name = "index-sqlite",
    version = env!("CARGO_PKG_VERSION"),
    init = init
);

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn conformance() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            path: dir.path().join("index.sqlite"),
            name: "t".into(),
            read_connections: 2,
        };
        let index = SqlIndex::new(SqliteBackend::open(&cfg).await.unwrap());
        medicus_testkit::index::suite(&index).await;
    }
}
