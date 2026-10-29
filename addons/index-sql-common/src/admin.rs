//! Settings revisions, users and credentials: each a JSON body plus the columns needed to find,
//! order and prune them.

use medicus_core::capability::{Credential, SettingsRevision, User};
use medicus_core::{Error, ErrorKind, Result};
use serde::Serialize;
use serde::de::DeserializeOwned;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::sql::{Backend, Cell, CellKind, Stmt, Tx, TxExt as _};
use crate::to_i64;

fn millis(t: OffsetDateTime) -> i64 {
    i64::try_from(t.unix_timestamp_nanos() / 1_000_000).unwrap_or(i64::MAX)
}

fn body<T: Serialize>(value: &T) -> Result<String> {
    serde_json::to_string(value).map_err(|e| Error::internal(e.to_string()))
}

fn decode<T: DeserializeOwned>(what: &str, rows: Vec<Vec<Cell>>) -> Result<Vec<T>> {
    rows.into_iter()
        .map(|row| {
            let body = row.first().and_then(Cell::text).unwrap_or_default();
            serde_json::from_str(body)
                .map_err(|e| Error::internal(format!("corrupt {what} row: {e}")))
        })
        .collect()
}

async fn fetch<B: Backend, T: DeserializeOwned>(
    backend: &B,
    what: &str,
    stmt: &Stmt,
) -> Result<Vec<T>> {
    let mut tx = backend.begin_read().await?;
    decode(what, tx.fetch(stmt, &[CellKind::Text]).await?)
}

// ------------------------------------------------------------------------------------------------
// Settings

const SETTINGS: &str = "SELECT body FROM settings";

pub(crate) async fn settings_current<B: Backend>(backend: &B) -> Result<Option<SettingsRevision>> {
    let s = Stmt::raw(
        backend.dialect(),
        "SELECT body FROM settings ORDER BY revision DESC LIMIT 1",
    );
    Ok(fetch(backend, "settings", &s).await?.pop())
}

pub(crate) async fn settings_revision<B: Backend>(
    backend: &B,
    revision: u64,
) -> Result<Option<SettingsRevision>> {
    let mut s = Stmt::new(backend.dialect());
    s.push(SETTINGS)
        .push(" WHERE revision = ")
        .bind(to_i64(revision));
    Ok(fetch(backend, "settings", &s).await?.pop())
}

pub(crate) async fn settings_history<B: Backend>(
    backend: &B,
    limit: u32,
    offset: u64,
) -> Result<Vec<SettingsRevision>> {
    let mut s = Stmt::new(backend.dialect());
    s.push(SETTINGS)
        .push(" ORDER BY revision DESC LIMIT ")
        .bind(i64::from(limit))
        .push(" OFFSET ")
        .bind(to_i64(offset));
    fetch(backend, "settings", &s).await
}

pub(crate) async fn put_settings<B: Backend>(
    backend: &B,
    revision: &SettingsRevision,
) -> Result<()> {
    let mut tx = backend.begin_write().await?;
    let latest = tx
        .fetch_one_int(&Stmt::raw(
            backend.dialect(),
            "SELECT COALESCE(MAX(revision), 0) FROM settings",
        ))
        .await?
        .unwrap_or(0);
    let conflict = || {
        Error::conflict(format!(
            "settings revision {} does not follow the latest revision {latest}",
            revision.revision
        ))
    };
    if revision.revision != u64::try_from(latest).unwrap_or(0) + 1 {
        return Err(conflict());
    }
    let mut s = Stmt::new(backend.dialect());
    s.push("INSERT INTO settings (revision, created_at, body) VALUES ")
        .bind_list([
            Cell::Int(to_i64(revision.revision)),
            Cell::Int(millis(revision.created_at)),
            Cell::from(body(revision)?),
        ]);
    // A concurrent writer (on backends that do not serialize writes) inserts the same key.
    match tx.execute(&s).await {
        Err(e) if e.kind == ErrorKind::AlreadyExists => return Err(conflict()),
        r => r?,
    };
    tx.commit().await
}

// ------------------------------------------------------------------------------------------------
// Users

pub(crate) async fn create_user<B: Backend>(backend: &B, user: &User) -> Result<()> {
    let mut tx = backend.begin_write().await?;
    let mut s = Stmt::new(backend.dialect());
    s.push("INSERT INTO users (name, body) VALUES ")
        .bind_list([Cell::from(user.name.as_str()), Cell::from(body(user)?)]);
    match tx.execute(&s).await {
        Err(e) if e.kind == ErrorKind::AlreadyExists => {
            return Err(Error::new(
                ErrorKind::AlreadyExists,
                format!("user {:?} already exists", user.name),
            ));
        }
        r => r?,
    };
    tx.commit().await
}

fn no_user(name: &str) -> Error {
    Error::not_found(format!("no user {name:?}"))
}

pub(crate) async fn update_user<B: Backend>(backend: &B, user: &User) -> Result<()> {
    let mut tx = backend.begin_write().await?;
    let mut s = Stmt::new(backend.dialect());
    s.push("UPDATE users SET body = ")
        .bind(body(user)?)
        .push(" WHERE name = ")
        .bind(user.name.as_str());
    if tx.execute(&s).await? == 0 {
        return Err(no_user(&user.name));
    }
    tx.commit().await
}

pub(crate) async fn get_user<B: Backend>(backend: &B, name: &str) -> Result<Option<User>> {
    let mut s = Stmt::new(backend.dialect());
    s.push("SELECT body FROM users WHERE name = ").bind(name);
    Ok(fetch(backend, "user", &s).await?.pop())
}

pub(crate) async fn list_users<B: Backend>(backend: &B) -> Result<Vec<User>> {
    let s = Stmt::raw(backend.dialect(), "SELECT body FROM users ORDER BY name");
    fetch(backend, "user", &s).await
}

pub(crate) async fn delete_user<B: Backend>(backend: &B, name: &str) -> Result<()> {
    let mut tx = backend.begin_write().await?;
    // Explicitly, rather than relying on the foreign key's cascade being enabled.
    let mut s = Stmt::new(backend.dialect());
    s.push("DELETE FROM credentials WHERE user_name = ")
        .bind(name);
    tx.execute(&s).await?;
    let mut s = Stmt::new(backend.dialect());
    s.push("DELETE FROM users WHERE name = ").bind(name);
    if tx.execute(&s).await? == 0 {
        return Err(no_user(name));
    }
    tx.commit().await
}

// ------------------------------------------------------------------------------------------------
// Credentials

pub(crate) async fn put_credential<B: Backend>(backend: &B, credential: &Credential) -> Result<()> {
    let mut tx = backend.begin_write().await?;
    let mut s = Stmt::new(backend.dialect());
    s.push("SELECT COUNT(*) FROM users WHERE name = ")
        .bind(credential.user.as_str());
    if tx.fetch_one_int(&s).await?.unwrap_or(0) == 0 {
        return Err(no_user(&credential.user));
    }
    let mut s = Stmt::new(backend.dialect());
    s.push(
        "INSERT INTO credentials (id, user_name, secret_hash, created_at, expires_at, body) \
         VALUES ",
    )
    .bind_list([
        Cell::from(credential.id.to_string()),
        Cell::from(credential.user.as_str()),
        Cell::from(credential.secret_hash.as_str()),
        Cell::Int(millis(credential.created_at)),
        Cell::from(credential.expires_at.map(millis)),
        Cell::from(body(credential)?),
    ])
    .push(
        " ON CONFLICT (id) DO UPDATE SET user_name = excluded.user_name, \
         secret_hash = excluded.secret_hash, expires_at = excluded.expires_at, \
         body = excluded.body",
    );
    tx.execute(&s).await?;
    tx.commit().await
}

pub(crate) async fn find_credential<B: Backend>(
    backend: &B,
    secret_hash: &str,
) -> Result<Option<Credential>> {
    let mut s = Stmt::new(backend.dialect());
    s.push("SELECT body FROM credentials WHERE secret_hash = ")
        .bind(secret_hash);
    Ok(fetch(backend, "credential", &s).await?.pop())
}

pub(crate) async fn list_credentials<B: Backend>(
    backend: &B,
    user: &str,
) -> Result<Vec<Credential>> {
    let mut s = Stmt::new(backend.dialect());
    s.push("SELECT body FROM credentials WHERE user_name = ")
        .bind(user)
        .push(" ORDER BY created_at, id");
    fetch(backend, "credential", &s).await
}

pub(crate) async fn delete_credential<B: Backend>(backend: &B, id: Uuid) -> Result<()> {
    let mut tx = backend.begin_write().await?;
    let mut s = Stmt::new(backend.dialect());
    s.push("DELETE FROM credentials WHERE id = ")
        .bind(id.to_string());
    if tx.execute(&s).await? == 0 {
        return Err(Error::not_found(format!("no credential {id}")));
    }
    tx.commit().await
}

pub(crate) async fn prune_credentials<B: Backend>(backend: &B, now: OffsetDateTime) -> Result<u64> {
    let mut tx = backend.begin_write().await?;
    let mut s = Stmt::new(backend.dialect());
    s.push("DELETE FROM credentials WHERE expires_at <= ")
        .bind(millis(now));
    let n = tx.execute(&s).await?;
    tx.commit().await?;
    Ok(n)
}
