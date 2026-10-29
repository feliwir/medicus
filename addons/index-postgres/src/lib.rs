//! `index-postgres`: the medicus `IndexStore` on `PostgreSQL`.
//!
//! ```toml
//! [addons.index-postgres]
//! config = { url = "postgres://medicus:secret@db.example/medicus", max_connections = 16 }
//! ```
//!
//! Writers run concurrently (READ COMMITTED); the shared code serializes only the outbox insert
//! with an advisory lock, which keeps events committing in `seq` order. Reads run in
//! REPEATABLE READ, READ ONLY transactions, so a paged query sees one snapshot.

use std::time::Duration;

use async_trait::async_trait;
use index_sql_common::{Backend, Cell, CellKind, Dialect, SqlIndex, Stmt, Tx};
use medicus_addon_sdk::{AddonManifest, Capability, Registrar, block_on, static_addon};
use medicus_core::{Error, ErrorKind, Result};
use serde::Deserialize;
use sqlx::encode::IsNull;
use sqlx::error::BoxDynError;
use sqlx::postgres::types::Oid;
use sqlx::postgres::{PgArgumentBuffer, PgConnectOptions, PgPool, PgPoolOptions, PgTypeInfo};
use sqlx::{AssertSqlSafe, Postgres, Row as _};

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Connection URL (`postgres://user:password@host:port/database?sslmode=...`). The database
    /// must exist; the schema is created and migrated on start.
    pub url: String,
    /// Name under which the index is registered.
    #[serde(default = "default_name")]
    pub name: String,
    /// Size of the connection pool, shared by reads and writes.
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,
}

fn default_name() -> String {
    "postgres".into()
}

fn default_max_connections() -> u32 {
    16
}

fn db_error(e: &sqlx::Error) -> Error {
    let kind = match e {
        sqlx::Error::Database(d) if d.is_unique_violation() => ErrorKind::AlreadyExists,
        // serialization_failure, deadlock_detected, and the server shutting down.
        sqlx::Error::Database(d)
            if matches!(
                d.code().as_deref(),
                Some("40001" | "40P01" | "57P01" | "57P03")
            ) =>
        {
            ErrorKind::Unavailable
        }
        sqlx::Error::PoolTimedOut | sqlx::Error::PoolClosed | sqlx::Error::Io(_) => {
            ErrorKind::Unavailable
        }
        _ => ErrorKind::Internal,
    };
    Error::new(kind, format!("postgres: {e}"))
}

#[derive(Debug, Clone)]
pub struct PostgresBackend {
    pool: PgPool,
}

impl PostgresBackend {
    /// Connects to `cfg.url` and migrates the schema.
    pub async fn open(cfg: &Config) -> Result<Self> {
        let options: PgConnectOptions = cfg
            .url
            .parse()
            .map_err(|e| Error::invalid_argument(format!("postgres url: {e}")))?;
        let pool = PgPoolOptions::new()
            .max_connections(cfg.max_connections.max(1))
            .acquire_timeout(Duration::from_secs(10))
            .connect_with(options)
            .await
            .map_err(|e| db_error(&e))?;
        MIGRATOR
            .run(&pool)
            .await
            .map_err(|e| Error::internal(format!("migrating postgres index: {e}")))?;
        Ok(Self { pool })
    }
}

pub struct PostgresTx(sqlx::Transaction<'static, Postgres>);

impl std::fmt::Debug for PostgresTx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PostgresTx")
    }
}

/// A NULL of unspecified type: `PostgreSQL` infers it from the context (`Cell::Null` carries no
/// type, and a NULL bound as TEXT is refused by an integer column).
struct UntypedNull;

impl sqlx::Type<Postgres> for UntypedNull {
    fn type_info() -> PgTypeInfo {
        PgTypeInfo::with_oid(Oid(0))
    }
}

impl sqlx::Encode<'_, Postgres> for UntypedNull {
    fn encode_by_ref(&self, _: &mut PgArgumentBuffer) -> std::result::Result<IsNull, BoxDynError> {
        Ok(IsNull::Yes)
    }
}

fn bind<'q>(
    mut q: sqlx::query::Query<'q, Postgres, <Postgres as sqlx::Database>::Arguments>,
    args: &'q [Cell],
) -> sqlx::query::Query<'q, Postgres, <Postgres as sqlx::Database>::Arguments> {
    for a in args {
        q = match a {
            Cell::Null => q.bind(UntypedNull),
            Cell::Text(s) | Cell::Json(s) => q.bind(s.as_str()),
            Cell::Int(i) => q.bind(*i),
        };
    }
    q
}

#[async_trait]
impl Tx for PostgresTx {
    fn dialect(&self) -> Dialect {
        Dialect::Postgres
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
impl Backend for PostgresBackend {
    type Tx = PostgresTx;

    fn dialect(&self) -> Dialect {
        Dialect::Postgres
    }

    async fn begin_write(&self) -> Result<PostgresTx> {
        self.pool
            .begin()
            .await
            .map(PostgresTx)
            .map_err(|e| db_error(&e))
    }

    async fn begin_read(&self) -> Result<PostgresTx> {
        self.pool
            .begin_with("BEGIN ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .await
            .map(PostgresTx)
            .map_err(|e| db_error(&e))
    }
}

/// Addon initialization.
pub fn init(reg: &mut Registrar) -> Result<()> {
    let cfg: Config = reg.config()?;
    let backend = block_on(PostgresBackend::open(&cfg))?;
    // Not the URL: it may contain the password.
    tracing::info!(name = %cfg.name, "postgres index ready");
    reg.register_index_store(&cfg.name, SqlIndex::new(backend))
}

/// What this addon provides and needs (ADR-0004 §2).
pub fn manifest() -> AddonManifest {
    AddonManifest::new("index-postgres", env!("CARGO_PKG_VERSION"))
        .provides([Capability::IndexStore])
        .config_schema(medicus_addon_sdk::config_schema::<Config>())
}

/// Entry points for linking this addon into the server binary.
pub static ADDON: medicus_addon_sdk::abi::StaticAddon =
    static_addon!(manifest = manifest(), init = init);

/// The tests need a server: set `INDEX_POSTGRES_TEST_URL` to a role that may create databases
/// (e.g. `postgres://postgres:postgres@localhost:5432/postgres`; not `MEDICUS_*`, which the server
/// reads as configuration). Each test runs in a database of its own. Without the variable the
/// tests pass without running, so `cargo test` works anywhere; CI sets it.
#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::Executor as _;

    /// A fresh, empty database; dropped again by [`TestDb::drop_db`].
    struct TestDb {
        admin: PgPool,
        name: String,
        cfg: Config,
    }

    impl TestDb {
        async fn create() -> Option<Self> {
            let Ok(url) = std::env::var("INDEX_POSTGRES_TEST_URL") else {
                eprintln!("INDEX_POSTGRES_TEST_URL is not set; skipping");
                return None;
            };
            let admin = PgPool::connect(&url).await.unwrap();
            let name = format!("medicus_test_{}", uuid::Uuid::now_v7().simple());
            admin
                .execute(AssertSqlSafe(format!("CREATE DATABASE {name}")))
                .await
                .unwrap();
            // Same server and role, other database: replace the URL's path.
            let (server, _) = url.split('?').next().unwrap().rsplit_once('/').unwrap();
            let cfg = Config {
                url: format!("{server}/{name}"),
                name: "t".into(),
                max_connections: 8,
            };
            Some(Self { admin, name, cfg })
        }

        async fn open(&self) -> SqlIndex<PostgresBackend> {
            SqlIndex::new(PostgresBackend::open(&self.cfg).await.unwrap())
        }

        async fn drop_db(self, index: SqlIndex<PostgresBackend>) {
            index.backend().pool.close().await;
            self.admin
                .execute(AssertSqlSafe(format!(
                    "DROP DATABASE {} WITH (FORCE)",
                    self.name
                )))
                .await
                .unwrap();
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn conformance() {
        let Some(db) = TestDb::create().await else {
            return;
        };
        let index = db.open().await;
        medicus_testkit::index::suite(&index).await;
        db.drop_db(index).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_writers() {
        let Some(db) = TestDb::create().await else {
            return;
        };
        let index = db.open().await;
        medicus_testkit::index::concurrent_writers(&index).await;
        db.drop_db(index).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn extra_tags_on_empty_index() {
        let Some(db) = TestDb::create().await else {
            return;
        };
        let index = db.open().await;
        medicus_testkit::index::extra_tags_on_empty_index(&index).await;

        // Extra values are stored as jsonb, not as JSON text.
        let mut tx = index.backend().begin_read().await.unwrap();
        let s = Stmt::raw(
            Dialect::Postgres,
            "SELECT pg_typeof(extra)::text, extra::text FROM instances",
        );
        let rows = tx
            .fetch(&s, &[CellKind::Text, CellKind::Text])
            .await
            .unwrap();
        assert_eq!(rows[0][0], Cell::from("jsonb"));
        let stored: serde_json::Value = serde_json::from_str(rows[0][1].text().unwrap()).unwrap();
        assert_eq!(
            stored,
            serde_json::json!({ "d": { "00180050": { "vr": "DS", "Value": [2.5] } }, "m": { "00180050": ["2.5"] } })
        );
        drop(tx);
        db.drop_db(index).await;
    }

    /// The event ordering requirement, deterministically: while one writer has inserted an event
    /// but not committed, another append must wait. Otherwise a subscriber could see the later
    /// `seq` first and skip the earlier one for good.
    #[tokio::test(flavor = "multi_thread")]
    async fn events_commit_in_seq_order() {
        use medicus_core::capability::IndexStore as _;
        use medicus_core::event::{Event, EventKind, Origin};

        let Some(db) = TestDb::create().await else {
            return;
        };
        let index = std::sync::Arc::new(db.open().await);
        let event = || {
            Event::new(
                EventKind::InstanceStored,
                Origin::Internal {
                    component: "test".into(),
                },
            )
        };

        // A writer as the shared code runs it, paused between its event insert and commit.
        let mut slow = index.backend().begin_write().await.unwrap();
        slow.execute(&Stmt::raw(
            Dialect::Postgres,
            &format!(
                "SELECT pg_advisory_xact_lock({})",
                index_sql_common::EVENTS_LOCK_KEY
            ),
        ))
        .await
        .unwrap();
        let mut insert = Stmt::new(Dialect::Postgres);
        insert
            .push("INSERT INTO events (id, kind, time, body) VALUES ")
            .bind_list([
                Cell::from("slow"),
                Cell::from("instance.stored"),
                Cell::Int(0),
                Cell::from(serde_json::to_string(&event()).unwrap()),
            ]);
        slow.execute(&insert).await.unwrap();

        let fast = tokio::spawn({
            let index = index.clone();
            async move { index.append_event(&event()).await }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !fast.is_finished(),
            "an event was appended while an earlier one was uncommitted"
        );
        slow.commit().await.unwrap();
        let fast_seq = fast.await.unwrap().unwrap();

        let seqs: Vec<u64> = index
            .events_after(0, 10)
            .await
            .unwrap()
            .iter()
            .map(|e| e.seq)
            .collect();
        assert_eq!(seqs.len(), 2);
        assert!(seqs[0] < seqs[1] && seqs[1] == fast_seq, "{seqs:?}");
        db.drop_db(std::sync::Arc::into_inner(index).unwrap()).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn migrations_are_idempotent() {
        let Some(db) = TestDb::create().await else {
            return;
        };
        drop(db.open().await);
        let index = db.open().await;
        medicus_testkit::index::extra_tags_on_empty_index(&index).await;
        db.drop_db(index).await;
    }
}
