//! The admin API's write side (ADR-0004 §3–5): settings, addons, users and their tokens, restart.
//!
//! Settings changes go through [`crate::Node::apply_settings`], so they are validated, stored as a
//! new revision and put into effect exactly like any other change. Revisions double as entity
//! tags: `GET /settings` answers with `ETag: "<revision>"`, and replacing the whole document needs
//! `If-Match` with it. Item endpoints (`/settings/nodes/{name}`, ...) apply to the latest revision
//! unless `If-Match` names one.

use std::collections::BTreeMap;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use medicus_core::addon::AddonManifest;
use medicus_core::capability::{Credential, User};
use medicus_core::{Error, ErrorKind, Permission};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::access::ADMIN_ROLE;
use crate::addons::LoadedBy;
use crate::api::{ApiError, ApiResult, AppState, Auth, UserView};
use crate::settings::{AddonSettings, Settings};

/// The settings sections that are maps of named items, editable one by one.
const SECTIONS: [&str; 5] = ["nodes", "partitions", "peers", "roles", "principals"];

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/settings", get(get_settings).put(put_settings))
        .route("/settings/history", get(history))
        .route("/settings/revisions/{revision}", get(get_revision))
        .route("/settings/revisions/{revision}/rollback", post(rollback))
        .route("/settings/{section}", get(get_section))
        .route(
            "/settings/{section}/{name}",
            put(put_item).delete(delete_item),
        )
        .route("/addons", get(list_addons))
        .route("/addons/{name}", put(put_addon))
        .route("/users", get(list_users).post(create_user))
        .route(
            "/users/{name}",
            get(get_user).patch(update_user).delete(delete_user),
        )
        .route("/auth/tokens", get(list_tokens).post(create_token))
        .route("/auth/tokens/{id}", axum::routing::delete(delete_token))
        .route("/auth/password", put(change_password))
        .route("/system/restart", post(restart))
}

fn invalid(message: impl std::fmt::Display) -> ApiError {
    Error::invalid_argument(message.to_string()).into()
}

// ------------------------------------------------------------------------------------------------
// Settings

fn etag(revision: u64) -> HeaderValue {
    HeaderValue::from_str(&format!("\"{revision}\"")).unwrap_or(HeaderValue::from_static("\"0\""))
}

/// The revision named by `If-Match` (`"3"`, `3` or `W/"3"`).
fn if_match(headers: &HeaderMap) -> ApiResult<Option<u64>> {
    let Some(value) = headers.get(header::IF_MATCH) else {
        return Ok(None);
    };
    value
        .to_str()
        .ok()
        .map(|v| v.trim().trim_start_matches("W/").trim_matches('"'))
        .and_then(|v| v.parse().ok())
        .map(Some)
        .ok_or_else(|| invalid("If-Match must name a settings revision, e.g. \"3\""))
}

/// Applies `settings` as the revision after `expected`, by the request's user.
async fn apply(
    s: &AppState,
    auth: &Auth,
    expected: u64,
    settings: Settings,
    comment: Option<String>,
) -> ApiResult<Response> {
    match s
        .node
        .apply_settings(expected, settings, Some(auth.user.name.clone()), comment)
        .await
    {
        Ok(applied) => {
            Ok(([(header::ETAG, etag(applied.revision))], Json(applied)).into_response())
        }
        Err(e) if e.kind == ErrorKind::Conflict => Err(ApiError::status(
            StatusCode::PRECONDITION_FAILED,
            "conflict",
            &e.message,
        )),
        Err(e) => Err(e.into()),
    }
}

#[derive(Debug, Default, Deserialize)]
struct CommentQuery {
    /// Why the settings are changed; recorded with the revision.
    comment: Option<String>,
}

/// `GET /api/v1/settings`: the settings in effect, their revision and what waits for a restart.
async fn get_settings(State(s): State<AppState>, auth: Auth) -> ApiResult<Response> {
    auth.require(&s, Permission::AdminSystem)?;
    let (revision, settings, restart_required) = s.node.settings_status().await;
    let body = json!({
        "revision": revision,
        "settings": settings,
        "restart_required": restart_required,
    });
    Ok(([(header::ETAG, etag(revision))], Json(body)).into_response())
}

/// `PUT /api/v1/settings?comment=`: replaces the whole document; needs `If-Match`.
async fn put_settings(
    State(s): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    Query(q): Query<CommentQuery>,
    Json(settings): Json<Settings>,
) -> ApiResult<Response> {
    auth.require(&s, Permission::AdminSystem)?;
    let Some(expected) = if_match(&headers)? else {
        return Err(ApiError::status(
            StatusCode::PRECONDITION_REQUIRED,
            "if_match_required",
            "replacing the settings needs If-Match with the revision they are based on",
        ));
    };
    apply(&s, &auth, expected, settings, q.comment).await
}

#[derive(Debug, Deserialize)]
struct Page {
    #[serde(default = "default_limit")]
    limit: u32,
    #[serde(default)]
    offset: u64,
}

fn default_limit() -> u32 {
    50
}

#[derive(Debug, Serialize)]
struct RevisionSummary {
    revision: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    author: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    comment: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    created_at: time::OffsetDateTime,
}

/// `GET /api/v1/settings/history?limit=&offset=`: revisions, newest first, without documents.
async fn history(
    State(s): State<AppState>,
    auth: Auth,
    Query(page): Query<Page>,
) -> ApiResult<Json<Vec<RevisionSummary>>> {
    auth.require(&s, Permission::AdminSystem)?;
    let revisions = s
        .archive
        .index
        .settings_history(page.limit.min(500), page.offset)
        .await?;
    Ok(Json(
        revisions
            .into_iter()
            .map(|r| RevisionSummary {
                revision: r.revision,
                author: r.author,
                comment: r.comment,
                created_at: r.created_at,
            })
            .collect(),
    ))
}

async fn stored_revision(
    s: &AppState,
    revision: u64,
) -> ApiResult<medicus_core::capability::SettingsRevision> {
    s.archive
        .index
        .settings_revision(revision)
        .await?
        .ok_or_else(|| Error::not_found(format!("there is no settings revision {revision}")).into())
}

/// `GET /api/v1/settings/revisions/{revision}`: one revision with its document.
async fn get_revision(
    State(s): State<AppState>,
    auth: Auth,
    Path(revision): Path<u64>,
) -> ApiResult<Json<Value>> {
    auth.require(&s, Permission::AdminSystem)?;
    let r = stored_revision(&s, revision).await?;
    Ok(Json(json!({
        "revision": r.revision,
        "author": r.author,
        "comment": r.comment,
        "created_at": r.created_at.format(&time::format_description::well_known::Rfc3339).ok(),
        "settings": r.document,
    })))
}

/// `POST /api/v1/settings/revisions/{revision}/rollback`: stores that revision's document as a
/// new revision (on top of the latest, or of `If-Match`).
async fn rollback(
    State(s): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    Path(revision): Path<u64>,
) -> ApiResult<Response> {
    auth.require(&s, Permission::AdminSystem)?;
    let old = stored_revision(&s, revision).await?;
    let settings: Settings = serde_json::from_value(old.document)
        .map_err(|e| invalid(format!("revision {revision} cannot be restored: {e}")))?;
    let (latest, _) = s.node.settings().await;
    let expected = if_match(&headers)?.unwrap_or(latest);
    let comment = Some(format!("rollback to revision {revision}"));
    apply(&s, &auth, expected, settings, comment).await
}

fn section(name: &str) -> ApiResult<&'static str> {
    SECTIONS
        .iter()
        .find(|s| **s == name)
        .copied()
        .ok_or_else(|| {
            Error::not_found(format!(
                "no settings section {name:?}; the item sections are {SECTIONS:?}"
            ))
            .into()
        })
}

/// The settings as JSON, with `section` as an object.
fn document(settings: &Settings) -> ApiResult<Value> {
    serde_json::to_value(settings).map_err(|e| Error::internal(e.to_string()).into())
}

/// `GET /api/v1/settings/{section}`: the items of a section, by name.
async fn get_section(
    State(s): State<AppState>,
    auth: Auth,
    Path(name): Path<String>,
) -> ApiResult<Response> {
    auth.require(&s, Permission::AdminSystem)?;
    let section = section(&name)?;
    let (revision, settings) = s.node.settings().await;
    let mut doc = document(&settings)?;
    let items = doc.get_mut(section).map(Value::take).unwrap_or_default();
    Ok(([(header::ETAG, etag(revision))], Json(items)).into_response())
}

/// Changes one item of a section and applies the result.
async fn change_item(
    s: &AppState,
    auth: &Auth,
    headers: &HeaderMap,
    section: &str,
    name: &str,
    item: Option<Value>,
) -> ApiResult<Response> {
    auth.require(s, Permission::AdminSystem)?;
    let section = self::section(section)?;
    let (revision, settings) = s.node.settings().await;
    let expected = if_match(headers)?.unwrap_or(revision);
    let mut doc = document(&settings)?;
    let Some(items) = doc.get_mut(section).and_then(Value::as_object_mut) else {
        return Err(Error::internal(format!("settings section {section} is not a map")).into());
    };
    let comment = if let Some(item) = item {
        let verb = if items.contains_key(name) {
            "changed"
        } else {
            "added"
        };
        items.insert(name.to_owned(), item);
        format!("{section}: {verb} {name}")
    } else {
        if items.remove(name).is_none() {
            return Err(Error::not_found(format!("{section} has no {name:?}")).into());
        }
        format!("{section}: removed {name}")
    };
    let settings: Settings =
        serde_json::from_value(doc).map_err(|e| invalid(format!("invalid {section} item: {e}")))?;
    apply(s, auth, expected, settings, Some(comment)).await
}

/// `PUT /api/v1/settings/{section}/{name}`: adds or replaces an item.
async fn put_item(
    State(s): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    Path((section, name)): Path<(String, String)>,
    Json(item): Json<Value>,
) -> ApiResult<Response> {
    change_item(&s, &auth, &headers, &section, &name, Some(item)).await
}

/// `DELETE /api/v1/settings/{section}/{name}`: removes an item.
async fn delete_item(
    State(s): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    Path((section, name)): Path<(String, String)>,
) -> ApiResult<Response> {
    change_item(&s, &auth, &headers, &section, &name, None).await
}

// ------------------------------------------------------------------------------------------------
// Addons

/// An available addon, as the admin UI shows it.
#[derive(Debug, Serialize)]
struct AddonView {
    #[serde(flatten)]
    manifest: AddonManifest,
    /// Loaded in the running server, and why.
    #[serde(skip_serializing_if = "Option::is_none")]
    loaded_by: Option<LoadedBy>,
    /// What the settings say; `None` if they do not mention the addon.
    #[serde(skip_serializing_if = "Option::is_none")]
    enabled: Option<bool>,
    /// The addon's configuration in the settings.
    config: Value,
    /// Configured in the configuration file (the index), not in the settings.
    bootstrap: bool,
    /// Loaded by the stored settings, i.e. after the next restart.
    selected: bool,
    /// The addons (and `archive`) that need it with the stored settings; it cannot be disabled
    /// while they are enabled.
    required_by: Vec<String>,
}

/// `GET /api/v1/addons`: every available addon with its manifest (including the configuration's
/// JSON Schema), its status and its dependants.
async fn list_addons(State(s): State<AppState>, auth: Auth) -> ApiResult<Json<Vec<AddonView>>> {
    auth.require(&s, Permission::AdminSystem)?;
    let node = &s.node;
    let (_, settings) = node.settings().await;
    let cfg = settings
        .apply(node.bootstrap.clone(), &node.index_addons)
        .map_err(|e| Error::internal(format!("{e:#}")))?;
    let selection = crate::addons::check_selection(&node.caps.available, &cfg).unwrap_or_default();
    let dependants = crate::addons::dependants(&node.caps.available, &selection);
    let loaded: BTreeMap<&str, &LoadedBy> = node
        .caps
        .addons
        .iter()
        .map(|a| (a.manifest.name.as_str(), &a.loaded_by))
        .collect();
    Ok(Json(
        node.caps
            .available
            .values()
            .map(|manifest| {
                let name = manifest.name.as_str();
                let entry = settings.addons.get(name);
                AddonView {
                    loaded_by: loaded.get(name).map(|l| (*l).clone()),
                    enabled: entry.map(|e| e.enabled),
                    config: entry.map(|e| e.config.clone()).unwrap_or_default(),
                    bootstrap: node.index_addons.contains(name),
                    selected: selection.iter().any(|(n, _)| n == name),
                    required_by: dependants.get(name).cloned().unwrap_or_default(),
                    manifest: manifest.clone(),
                }
            })
            .collect(),
    ))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AddonChange {
    enabled: Option<bool>,
    /// The new configuration; `null` returns to the defaults.
    #[serde(default, deserialize_with = "present")]
    config: Option<Value>,
}

/// Distinguishes an absent field (`None`) from `null` (`Some(Null)`).
fn present<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(d).map(Some)
}

/// `PUT /api/v1/addons/{name}`: enables or disables an addon or changes its configuration. Takes
/// effect at the next restart; refused if the result cannot be resolved (e.g. disabling an addon
/// others need) or the configuration does not match the addon's schema.
async fn put_addon(
    State(s): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(change): Json<AddonChange>,
) -> ApiResult<Response> {
    auth.require(&s, Permission::AdminSystem)?;
    let node = &s.node;
    if !node.caps.available.contains_key(&name) {
        return Err(Error::not_found(format!("no addon {name:?} is available")).into());
    }
    if node.index_addons.contains(&name) {
        return Err(invalid(format!(
            "addon {name:?} provides the index and is configured in the configuration file"
        )));
    }
    let (revision, mut settings) = node.settings().await;
    let expected = if_match(&headers)?.unwrap_or(revision);
    // An addon pulled in as a dependency counts as enabled until said otherwise.
    let running = node.caps.addons.iter().any(|a| a.manifest.name == name);
    let entry = settings
        .addons
        .entry(name.clone())
        .or_insert_with(|| AddonSettings {
            enabled: running,
            config: Value::Null,
        });
    let mut what = Vec::new();
    if let Some(enabled) = change.enabled {
        entry.enabled = enabled;
        what.push(if enabled { "enabled" } else { "disabled" });
    }
    if let Some(config) = change.config {
        entry.config = config;
        what.push("configured");
    }
    let comment = Some(format!("addon {name}: {}", what.join(", ")));
    apply(&s, &auth, expected, settings, comment).await
}

// ------------------------------------------------------------------------------------------------
// Users

/// The roles a user may have: those of the settings, and `admin`.
async fn check_roles(s: &AppState, roles: &[String]) -> ApiResult<()> {
    let (_, settings) = s.node.settings().await;
    match roles
        .iter()
        .find(|r| *r != ADMIN_ROLE && !settings.roles.contains_key(*r))
    {
        Some(unknown) => Err(invalid(format!(
            "there is no role {unknown:?}; the settings define {:?}, and `admin` is built in",
            settings.roles.keys().collect::<Vec<_>>()
        ))),
        None => Ok(()),
    }
}

fn is_admin(user: &User) -> bool {
    !user.disabled && user.roles.iter().any(|r| r == ADMIN_ROLE)
}

/// Refuses a change after which no enabled administrator would remain: `changed` replaces the user
/// of its name (`None` deletes `name`).
async fn keep_an_admin(s: &AppState, name: &str, changed: Option<&User>) -> ApiResult<()> {
    let users = s.archive.index.list_users().await?;
    let remains = users
        .iter()
        .filter(|u| u.name != name)
        .chain(changed)
        .any(is_admin);
    if remains {
        Ok(())
    } else {
        Err(Error::conflict("this would leave no enabled user with the admin role").into())
    }
}

async fn existing_user(s: &AppState, name: &str) -> ApiResult<User> {
    s.archive
        .index
        .get_user(name)
        .await?
        .ok_or_else(|| Error::not_found(format!("no user {name:?}")).into())
}

/// `GET /api/v1/users`.
async fn list_users(State(s): State<AppState>, auth: Auth) -> ApiResult<Json<Vec<UserView>>> {
    auth.require(&s, Permission::AdminUsers)?;
    let users = s.archive.index.list_users().await?;
    Ok(Json(users.iter().map(UserView::from).collect()))
}

/// `GET /api/v1/users/{name}`.
async fn get_user(
    State(s): State<AppState>,
    auth: Auth,
    Path(name): Path<String>,
) -> ApiResult<Json<UserView>> {
    auth.require(&s, Permission::AdminUsers)?;
    Ok(Json(UserView::from(&existing_user(&s, &name).await?)))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NewUser {
    name: String,
    /// Without a password the user can only use API tokens.
    password: Option<String>,
    display_name: Option<String>,
    #[serde(default)]
    roles: Vec<String>,
}

/// `POST /api/v1/users`.
async fn create_user(
    State(s): State<AppState>,
    auth: Auth,
    Json(new): Json<NewUser>,
) -> ApiResult<Response> {
    auth.require(&s, Permission::AdminUsers)?;
    check_roles(&s, &new.roles).await?;
    let user = s
        .accounts
        .create_user(
            &new.name,
            new.password.as_deref(),
            new.roles,
            new.display_name,
        )
        .await?;
    tracing::info!(user = %user.name, by = %auth.user.name, "user created");
    Ok((StatusCode::CREATED, Json(UserView::from(&user))).into_response())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UserChange {
    /// An empty string removes the display name.
    display_name: Option<String>,
    roles: Option<Vec<String>>,
    disabled: Option<bool>,
    password: Option<String>,
}

/// `PATCH /api/v1/users/{name}`: changes what is given; disabling ends the user's sessions.
async fn update_user(
    State(s): State<AppState>,
    auth: Auth,
    Path(name): Path<String>,
    Json(change): Json<UserChange>,
) -> ApiResult<Json<UserView>> {
    auth.require(&s, Permission::AdminUsers)?;
    let mut user = existing_user(&s, &name).await?;
    if let Some(display_name) = change.display_name {
        user.display_name = Some(display_name).filter(|d| !d.is_empty());
    }
    if let Some(roles) = change.roles {
        check_roles(&s, &roles).await?;
        user.roles = roles;
    }
    if let Some(disabled) = change.disabled {
        user.disabled = disabled;
    }
    keep_an_admin(&s, &name, Some(&user)).await?;
    let user = s
        .accounts
        .update_user(user, change.password.as_deref())
        .await?;
    tracing::info!(user = %user.name, by = %auth.user.name, "user changed");
    Ok(Json(UserView::from(&user)))
}

/// `DELETE /api/v1/users/{name}`: with their sessions and tokens.
async fn delete_user(
    State(s): State<AppState>,
    auth: Auth,
    Path(name): Path<String>,
) -> ApiResult<StatusCode> {
    auth.require(&s, Permission::AdminUsers)?;
    existing_user(&s, &name).await?;
    keep_an_admin(&s, &name, None).await?;
    s.archive.index.delete_user(&name).await?;
    tracing::info!(user = %name, by = %auth.user.name, "user deleted");
    Ok(StatusCode::NO_CONTENT)
}

// ------------------------------------------------------------------------------------------------
// The caller's own account

/// An API token as its owner sees it: never the secret, which is shown once at creation.
#[derive(Debug, Serialize)]
struct TokenView {
    id: uuid::Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    created_at: time::OffsetDateTime,
    #[serde(
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    expires_at: Option<time::OffsetDateTime>,
    #[serde(
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    last_used_at: Option<time::OffsetDateTime>,
}

impl From<&Credential> for TokenView {
    fn from(c: &Credential) -> Self {
        Self {
            id: c.id,
            name: c.name.clone(),
            created_at: c.created_at,
            expires_at: c.expires_at,
            last_used_at: c.last_used_at,
        }
    }
}

/// `GET /api/v1/auth/tokens`: the caller's API tokens.
async fn list_tokens(State(s): State<AppState>, auth: Auth) -> ApiResult<Json<Vec<TokenView>>> {
    let tokens = s.accounts.tokens(&auth.user.name).await?;
    Ok(Json(tokens.iter().map(TokenView::from).collect()))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NewToken {
    name: Option<String>,
    /// Days until it expires; without, it never does.
    days: Option<u32>,
}

/// `POST /api/v1/auth/tokens`: creates an API token for the caller; the answer holds the secret,
/// which is shown only now.
async fn create_token(
    State(s): State<AppState>,
    auth: Auth,
    Json(new): Json<NewToken>,
) -> ApiResult<Response> {
    let ttl = new.days.map(|d| time::Duration::days(i64::from(d)));
    let issued = s
        .accounts
        .create_token(&auth.user.name, new.name, ttl)
        .await?;
    let mut body = serde_json::to_value(TokenView::from(&issued.credential))
        .map_err(|e| Error::internal(e.to_string()))?;
    body["token"] = json!(issued.secret);
    Ok((StatusCode::CREATED, Json(body)).into_response())
}

/// `DELETE /api/v1/auth/tokens/{id}`: revokes one of the caller's tokens.
async fn delete_token(
    State(s): State<AppState>,
    auth: Auth,
    Path(id): Path<uuid::Uuid>,
) -> ApiResult<StatusCode> {
    let own = s.accounts.tokens(&auth.user.name).await?;
    if !own.iter().any(|c| c.id == id) {
        return Err(Error::not_found(format!("you have no token {id}")).into());
    }
    s.archive.index.delete_credential(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PasswordChange {
    current: String,
    new: String,
}

/// `PUT /api/v1/auth/password`: changes the caller's password.
async fn change_password(
    State(s): State<AppState>,
    auth: Auth,
    Json(change): Json<PasswordChange>,
) -> ApiResult<StatusCode> {
    if !s.accounts.verify(&auth.user.name, &change.current).await? {
        return Err(
            Error::new(ErrorKind::PermissionDenied, "the current password is wrong").into(),
        );
    }
    let user = existing_user(&s, &auth.user.name).await?;
    s.accounts.update_user(user, Some(&change.new)).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ------------------------------------------------------------------------------------------------
// System

/// `POST /api/v1/system/restart`: stops the server gracefully and starts it again.
async fn restart(State(s): State<AppState>, auth: Auth) -> ApiResult<Response> {
    auth.require(&s, Permission::AdminSystem)?;
    tracing::info!(by = %auth.user.name, "restart requested through the admin API");
    s.node.request_restart();
    Ok((StatusCode::ACCEPTED, Json(json!({ "restarting": true }))).into_response())
}
