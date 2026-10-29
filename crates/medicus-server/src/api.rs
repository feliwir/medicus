//! The admin HTTP API (`/api/v1`, PLAN §3.6) and health endpoints.
//!
//! Every admin endpoint needs a user: a browser session (cookie) or an API token (bearer), and the
//! endpoint's admin permission (ADR-0004 §5). While no user exists the server is in setup mode:
//! only `/api/v1/setup` works, which creates the first administrator with the setup token.

use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use futures::Stream;
use medicus_core::capability::{Job, JobState, Peer, User};
use medicus_core::event::{Event, EventKind};
use medicus_core::{Error, ErrorKind, NodeInfo, Permission};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::access::Caller;
use crate::archive::Archive;
use crate::auth::{Accounts, Issued, SESSION_COOKIE, SESSION_TTL};

/// Shared state of all handlers.
#[derive(Debug, Clone)]
pub(crate) struct AppState {
    pub(crate) node: Arc<crate::Node>,
    pub(crate) archive: Arc<Archive>,
    pub(crate) jobs: Arc<crate::jobs::Jobs>,
    pub(crate) live: Arc<crate::live::Live>,
    pub(crate) accounts: Arc<Accounts>,
    /// The addons' HTTP routes, served for paths the core does not handle.
    pub(crate) routes: Arc<crate::routes::Routes>,
    pub(crate) name: String,
    pub(crate) addons: Arc<[crate::addons::AddonStatus]>,
}

impl AppState {
    pub(crate) fn new(node: &Arc<crate::Node>) -> Self {
        Self {
            node: node.clone(),
            archive: node.archive.clone(),
            jobs: node.jobs.clone(),
            live: node.live.clone(),
            accounts: node.accounts.clone(),
            routes: Arc::new(node.caps.http_routes.clone()),
            name: node.cfg.server.name.clone(),
            addons: node.caps.addons.clone().into(),
        }
    }
}

pub(crate) fn router(state: AppState) -> Router {
    let v1 = Router::new()
        .route("/setup", get(setup_status).post(setup))
        .route("/auth/login", post(login))
        .route("/auth/logout", post(logout))
        .route("/auth/me", get(me))
        .route("/system", get(system))
        .route("/events", get(list_events))
        .route("/events/stream", get(stream_events))
        .route("/nodes", get(list_nodes))
        .route("/peers", get(list_peers))
        .route("/peers/{name}/echo", post(echo_peer))
        .route("/jobs", get(list_jobs))
        .route("/jobs/{id}", get(get_job))
        .route("/jobs/{id}/cancel", post(cancel_job))
        .merge(crate::admin::routes());
    Router::new()
        .route("/health/live", get(|| async { "ok" }))
        .route("/health/ready", get(ready))
        .nest("/api/v1", v1)
        .route("/rs", any(crate::proxy::forward))
        .route("/rs/{*rest}", any(crate::proxy::forward))
        .route("/nodes/{node}/rs", any(crate::proxy::forward))
        .route("/nodes/{node}/rs/{*rest}", any(crate::proxy::forward))
        .fallback(crate::routes::handle)
        .with_state(state)
}

// ------------------------------------------------------------------------------------------------
// Peers and jobs

async fn list_nodes(State(s): State<AppState>, _auth: Auth) -> Json<Vec<NodeInfo>> {
    Json(
        s.live
            .load()
            .directory
            .nodes()
            .map(crate::access::Node::info)
            .collect(),
    )
}

async fn list_peers(State(s): State<AppState>, auth: Auth) -> ApiResult<Json<Vec<Peer>>> {
    auth.require(&s, Permission::AdminPeers)?;
    Ok(Json(s.jobs.peers()))
}

async fn echo_peer(
    State(s): State<AppState>,
    auth: Auth,
    Path(name): Path<String>,
) -> ApiResult<StatusCode> {
    auth.require(&s, Permission::AdminPeers)?;
    s.jobs.echo(&name).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// A job as returned by the API, with its id also as a DICOM UID.
#[derive(Debug, Serialize)]
struct JobView {
    #[serde(flatten)]
    job: Job,
    transaction_uid: String,
}

impl From<Job> for JobView {
    fn from(job: Job) -> Self {
        Self {
            transaction_uid: job.transaction_uid(),
            job,
        }
    }
}

#[derive(Debug, Deserialize)]
struct JobsQuery {
    state: Option<JobState>,
    #[serde(default = "default_limit")]
    limit: u32,
    #[serde(default)]
    offset: u64,
}

async fn list_jobs(
    State(s): State<AppState>,
    auth: Auth,
    Query(q): Query<JobsQuery>,
) -> ApiResult<Json<Vec<JobView>>> {
    auth.require(&s, Permission::AdminJobs)?;
    let jobs = s.jobs.list(q.state, q.limit.min(1000), q.offset).await?;
    Ok(Json(jobs.into_iter().map(JobView::from).collect()))
}

fn job_id(raw: &str) -> Result<uuid::Uuid, ApiError> {
    raw.parse()
        .ok()
        .or_else(|| Job::id_from_transaction_uid(raw))
        .ok_or_else(|| Error::invalid_argument(format!("invalid job id {raw:?}")).into())
}

async fn get_job(
    State(s): State<AppState>,
    auth: Auth,
    Path(id): Path<String>,
) -> ApiResult<Json<JobView>> {
    auth.require(&s, Permission::AdminJobs)?;
    Ok(Json(s.jobs.get(job_id(&id)?).await?.into()))
}

async fn cancel_job(
    State(s): State<AppState>,
    auth: Auth,
    Path(id): Path<String>,
) -> ApiResult<Json<JobView>> {
    auth.require(&s, Permission::AdminJobs)?;
    Ok(Json(s.jobs.cancel(job_id(&id)?).await?.into()))
}

// ------------------------------------------------------------------------------------------------
// Authentication

/// The server's own credential on a request: a bearer token, else the session cookie (then the
/// flag is set).
pub(crate) fn credential(headers: &HeaderMap) -> Option<(String, bool)> {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    if let Some(token) = bearer {
        return Some((token.trim().to_owned(), false));
    }
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|c| {
            c.trim()
                .strip_prefix(SESSION_COOKIE)?
                .strip_prefix('=')
                .map(|s| (s.to_owned(), true))
        })
}

/// The authenticated user of an admin API request.
pub(crate) struct Auth {
    pub(crate) user: User,
    remote_addr: String,
}

impl Auth {
    /// `PermissionDenied` unless the user has the admin `permission`.
    pub(crate) fn require(&self, s: &AppState, permission: Permission) -> ApiResult<()> {
        let caller = Caller::User {
            name: self.user.name.clone(),
            roles: self.user.roles.clone(),
        };
        s.live.load().directory.require_admin(&caller, permission)?;
        Ok(())
    }

    /// The origin of events the user causes.
    #[allow(dead_code)] // used by the write endpoints (phase 7)
    pub(crate) fn origin(&self) -> medicus_core::event::Origin {
        medicus_core::event::Origin::AdminApi {
            remote_addr: self.remote_addr.clone(),
            principal: Some(self.user.name.clone()),
        }
    }
}

impl FromRequestParts<AppState> for Auth {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, s: &AppState) -> ApiResult<Self> {
        if s.accounts.in_setup() {
            return Err(ApiError::status(
                StatusCode::SERVICE_UNAVAILABLE,
                "setup_required",
                "no user exists yet: finish the setup first (POST /api/v1/setup)",
            ));
        }
        let Some((secret, from_cookie)) = credential(&parts.headers) else {
            return Err(unauthenticated("log in or send an API token"));
        };
        // A browser sends the cookie along with requests other sites make it send.
        if from_cookie
            && !matches!(parts.method, Method::GET | Method::HEAD | Method::OPTIONS)
            && !parts.headers.contains_key("x-requested-with")
        {
            return Err(ApiError::status(
                StatusCode::FORBIDDEN,
                "csrf",
                "a request authenticated by the session cookie that changes something needs an \
                 X-Requested-With header",
            ));
        }
        let user = s
            .accounts
            .authenticate(&secret)
            .await?
            .ok_or_else(|| unauthenticated("the session or API token is invalid or expired"))?;
        let remote_addr = parts
            .extensions
            .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
            .map(|c| c.0.to_string())
            .unwrap_or_default();
        Ok(Self { user, remote_addr })
    }
}

fn unauthenticated(message: &str) -> ApiError {
    ApiError::status(StatusCode::UNAUTHORIZED, "unauthenticated", message)
}

/// A user as the API shows it: without the password hash.
#[derive(Debug, Serialize)]
pub(crate) struct UserView {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    display_name: Option<String>,
    roles: Vec<String>,
    disabled: bool,
    /// Whether the user can log in with a password (else only with API tokens).
    password: bool,
    #[serde(with = "time::serde::rfc3339")]
    created_at: time::OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    updated_at: time::OffsetDateTime,
}

impl From<&User> for UserView {
    fn from(u: &User) -> Self {
        Self {
            name: u.name.clone(),
            display_name: u.display_name.clone(),
            roles: u.roles.clone(),
            disabled: u.disabled,
            password: u.password_hash.is_some(),
            created_at: u.created_at,
            updated_at: u.updated_at,
        }
    }
}

/// A response that logs the user in: the session cookie and the user.
fn session_response(status: StatusCode, issued: &Issued) -> Response {
    let cookie = format!(
        "{SESSION_COOKIE}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}",
        issued.secret,
        SESSION_TTL.whole_seconds()
    );
    let mut response = (status, Json(UserView::from(&issued.user))).into_response();
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    response
}

/// `GET /api/v1/setup`: whether the server waits for its first user.
async fn setup_status(State(s): State<AppState>) -> Json<serde_json::Value> {
    Json(json!({ "required": s.accounts.in_setup() }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SetupRequest {
    /// The setup token from the server's console.
    token: String,
    name: String,
    password: String,
    #[serde(default)]
    display_name: Option<String>,
}

/// `POST /api/v1/setup`: creates the first user, an administrator, and logs them in.
async fn setup(State(s): State<AppState>, Json(r): Json<SetupRequest>) -> ApiResult<Response> {
    let issued = s
        .accounts
        .setup(&r.token, &r.name, &r.password, r.display_name)
        .await?;
    Ok(session_response(StatusCode::CREATED, &issued))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LoginRequest {
    name: String,
    password: String,
}

/// `POST /api/v1/auth/login`: starts a session (cookie).
async fn login(State(s): State<AppState>, Json(r): Json<LoginRequest>) -> ApiResult<Response> {
    if s.accounts.in_setup() {
        return Err(ApiError::status(
            StatusCode::SERVICE_UNAVAILABLE,
            "setup_required",
            "no user exists yet: finish the setup first (POST /api/v1/setup)",
        ));
    }
    let issued = s
        .accounts
        .login(&r.name, &r.password)
        .await?
        .ok_or_else(|| unauthenticated("invalid user name or password"))?;
    Ok(session_response(StatusCode::OK, &issued))
}

/// `POST /api/v1/auth/logout`: ends the session (or revokes the token) of the request.
async fn logout(State(s): State<AppState>, _auth: Auth, headers: HeaderMap) -> ApiResult<Response> {
    if let Some((secret, _)) = credential(&headers) {
        s.accounts.logout(&secret).await?;
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    let clear = format!("{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0");
    if let Ok(value) = HeaderValue::from_str(&clear) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    Ok(response)
}

/// `GET /api/v1/auth/me`: the user and their admin permissions.
async fn me(State(s): State<AppState>, auth: Auth) -> Json<serde_json::Value> {
    let caller = Caller::User {
        name: auth.user.name.clone(),
        roles: auth.user.roles.clone(),
    };
    let permissions: Vec<_> = s
        .live
        .load()
        .directory
        .admin_permissions(&caller)
        .into_iter()
        .map(Permission::as_str)
        .collect();
    Json(json!({ "user": UserView::from(&auth.user), "permissions": permissions }))
}

/// An error as an HTTP response with a JSON body `{"error": <code>, "message": ...}`.
pub(crate) struct ApiError {
    status: StatusCode,
    code: &'static str,
    error: Error,
}

impl ApiError {
    /// An error with its own status and code, outside the mapping of [`ErrorKind`]s.
    pub(crate) fn status(status: StatusCode, code: &'static str, message: &str) -> Self {
        Self {
            status,
            code,
            error: Error::new(ErrorKind::Rejected, message),
        }
    }
}

impl From<Error> for ApiError {
    fn from(error: Error) -> Self {
        let status = match error.kind {
            ErrorKind::InvalidArgument => StatusCode::BAD_REQUEST,
            ErrorKind::NotFound => StatusCode::NOT_FOUND,
            ErrorKind::AlreadyExists | ErrorKind::Conflict => StatusCode::CONFLICT,
            ErrorKind::Rejected | ErrorKind::PermissionDenied => StatusCode::FORBIDDEN,
            ErrorKind::ResourceExhausted => StatusCode::INSUFFICIENT_STORAGE,
            ErrorKind::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            ErrorKind::Unsupported => StatusCode::NOT_IMPLEMENTED,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self {
            status,
            code: error.kind.as_str(),
            error,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if self.status.is_server_error() {
            tracing::error!(error = %self.error, "request failed");
        }
        let body = json!({ "error": self.code, "message": self.error.message });
        (self.status, Json(body)).into_response()
    }
}

pub(crate) type ApiResult<T> = Result<T, ApiError>;

async fn ready(State(s): State<AppState>) -> StatusCode {
    match s.archive.index.last_event_seq().await {
        Ok(_) => StatusCode::OK,
        Err(e) => {
            tracing::warn!(error = %e, "index not ready");
            StatusCode::SERVICE_UNAVAILABLE
        }
    }
}

async fn system(State(s): State<AppState>, auth: Auth) -> ApiResult<Json<serde_json::Value>> {
    auth.require(&s, Permission::AdminSystem)?;
    let (revision, _, restart_required) = s.node.settings_status().await;
    Ok(Json(json!({
        "settings_revision": revision,
        "restart_required": restart_required,
        "name": s.name,
        "version": env!("CARGO_PKG_VERSION"),
        "abi_version": medicus_abi_version(),
        "storage": s.archive.default_store,
        "addons": s.addons,
    })))
}

fn medicus_abi_version() -> String {
    // The ABI version this server speaks, e.g. "0.2".
    let v = medicus_host::abi_version();
    format!("{}.{}", v >> 16, v & 0xFFFF)
}

#[derive(Debug, Deserialize)]
struct EventsQuery {
    /// Return events with `seq > since`.
    #[serde(default)]
    since: u64,
    #[serde(default = "default_limit")]
    limit: u32,
    /// Comma-separated event kinds; empty = all.
    #[serde(default)]
    kind: String,
    /// Start this many events before the latest instead of after `since`.
    #[serde(default)]
    tail: Option<u64>,
}

fn default_limit() -> u32 {
    100
}

impl EventsQuery {
    fn kinds(&self) -> ApiResult<Vec<EventKind>> {
        self.kind
            .split(',')
            .filter(|k| !k.is_empty())
            .map(|k| {
                k.parse().map_err(|e: medicus_core::event::EventKindError| {
                    Error::invalid_argument(e.to_string()).into()
                })
            })
            .collect()
    }
}

#[derive(Debug, Serialize)]
struct EventPage {
    events: Vec<Event>,
    /// Pass as `since` to continue.
    next: u64,
}

/// `GET /api/v1/events?since=&limit=&kind=&tail=`: a page of events from the outbox, after `since`
/// or the last `tail` events.
async fn list_events(
    State(s): State<AppState>,
    auth: Auth,
    Query(q): Query<EventsQuery>,
) -> ApiResult<Json<EventPage>> {
    auth.require(&s, Permission::AdminEvents)?;
    let kinds = q.kinds()?;
    let since = match q.tail {
        Some(tail) => s.archive.index.last_event_seq().await?.saturating_sub(tail),
        None => q.since,
    };
    let raw = s
        .archive
        .index
        .events_after(since, q.limit.clamp(1, 1000))
        .await?;
    let next = raw.last().map_or(since, |e| e.seq);
    let events = raw
        .into_iter()
        .filter(|e| kinds.is_empty() || kinds.contains(&e.kind))
        .collect();
    Ok(Json(EventPage { events, next }))
}

/// `GET /api/v1/events/stream?since=&kind=&tail=`: Server-Sent Events, starting after the
/// `Last-Event-ID` header (the event's `seq`), else `tail` events before the latest, else after
/// `since`; then live.
async fn stream_events(
    State(s): State<AppState>,
    auth: Auth,
    Query(q): Query<EventsQuery>,
    headers: HeaderMap,
) -> ApiResult<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>> {
    auth.require(&s, Permission::AdminEvents)?;
    let kinds = q.kinds()?;
    let resume = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let since = match (resume, q.tail) {
        (Some(seq), _) => seq,
        (None, Some(tail)) => s.archive.index.last_event_seq().await?.saturating_sub(tail),
        (None, None) => q.since,
    };
    let archive = s.archive.clone();
    let stream = async_stream::stream! {
        let mut cursor = since;
        let mut committed = archive.watch_events();
        loop {
            committed.borrow_and_update();
            let events = match archive.index.events_after(cursor, 100).await {
                Ok(e) => e,
                Err(e) => {
                    yield Ok(SseEvent::default().event("error").data(e.to_string()));
                    return;
                }
            };
            if events.is_empty() {
                if committed.changed().await.is_err() {
                    return;
                }
                continue;
            }
            for event in events {
                cursor = event.seq;
                if !kinds.is_empty() && !kinds.contains(&event.kind) {
                    continue;
                }
                let data = serde_json::to_string(&event).unwrap_or_default();
                yield Ok(SseEvent::default().id(event.seq.to_string()).event(event.kind.as_str()).data(data));
            }
        }
    };
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use axum::body::Body;
    use axum::http::{HeaderName, Request};
    use futures::StreamExt as _;
    use http_body_util::BodyExt as _;
    use medicus_core::capability::JobKind;
    use medicus_core::event::Origin;
    use medicus_testkit::dicom::InstanceBuilder;
    use tower::ServiceExt as _;

    /// The API of a node, and an administrator's API token that [`send`] adds to requests
    /// without credentials.
    #[derive(Clone)]
    struct TestApp {
        router: Router,
        accounts: Arc<Accounts>,
        token: Option<String>,
        #[allow(dead_code)] // used by the admin tests
        node: Arc<crate::Node>,
    }

    /// The API of a node with a default configuration and an administrator.
    async fn app(dir: &std::path::Path) -> (TestApp, Arc<Archive>) {
        let mut cfg = Config::default();
        cfg.server.data_dir = dir.to_owned();
        let (mut app, archive) = app_in_setup(cfg, None).await;
        app.accounts
            .create_user("root", None, vec![crate::access::ADMIN_ROLE.into()], None)
            .await
            .unwrap();
        app.token = Some(
            app.accounts
                .create_token("root", None, None)
                .await
                .unwrap()
                .secret,
        );
        (app, archive)
    }

    /// The API of a node configured by `cfg`, without users; `routes` replaces the addons'.
    async fn app_in_setup(
        cfg: Config,
        routes: Option<crate::routes::Routes>,
    ) -> (TestApp, Arc<Archive>) {
        let node = Arc::new(crate::Node::open(cfg).await.unwrap());
        let mut state = AppState::new(&node);
        if let Some(routes) = routes {
            state.routes = Arc::new(routes);
        }
        let app = TestApp {
            router: router(state),
            accounts: node.accounts.clone(),
            token: None,
            node: node.clone(),
        };
        (app, node.archive.clone())
    }

    /// Stores an instance the way a protocol front-end would.
    async fn ingest(archive: &Archive, sop: &str) {
        let file = InstanceBuilder::new("1.2.3", "1.2.3.1", sop).build();
        let origin = Origin::Internal {
            component: "test".into(),
        };
        archive
            .ingest(
                &medicus_core::Partition::default_partition(),
                medicus_testkit::stream(&file, 4096),
                origin,
            )
            .await
            .unwrap();
    }

    fn request(method: &str, uri: &str) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .body(Body::empty())
            .unwrap()
    }

    /// Sends `req`, with the app's token unless it carries credentials of its own.
    async fn send(app: &TestApp, req: Request<Body>) -> (StatusCode, Vec<u8>) {
        let resp = respond(app, req).await;
        let status = resp.status();
        let body = resp
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec();
        (status, body)
    }

    async fn respond(app: &TestApp, mut req: Request<Body>) -> Response {
        let has_credentials = req.headers().contains_key(header::AUTHORIZATION)
            || req.headers().contains_key(header::COOKIE);
        if let (false, Some(token)) = (has_credentials, &app.token) {
            req.headers_mut().insert(
                header::AUTHORIZATION,
                format!("Bearer {token}").parse().unwrap(),
            );
        }
        app.router.clone().oneshot(req).await.unwrap()
    }

    fn json_of(body: &[u8]) -> serde_json::Value {
        serde_json::from_slice(body).unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn health_system_and_events() {
        let dir = tempfile::tempdir().unwrap();
        let (app, archive) = app(dir.path()).await;
        assert_eq!(
            send(&app, request("GET", "/health/ready")).await.0,
            StatusCode::OK
        );
        let (status, body) = send(&app, request("GET", "/api/v1/system")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json_of(&body)["storage"], "local");
        // Without the dicomweb addon, the proxy says so.
        let (status, body) = send(&app, request("GET", "/rs/studies")).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(json_of(&body)["error"], "dicomweb_disabled");

        ingest(&archive, "1.2.3.1.1").await;
        let (status, body) = send(&app, request("GET", "/api/v1/events?since=0")).await;
        assert_eq!(status, StatusCode::OK);
        let page = json_of(&body);
        assert_eq!(page["events"][0]["kind"], "instance.stored");
        assert_eq!(page["next"], 1);
        let (_, body) = send(
            &app,
            request("GET", "/api/v1/events?since=0&kind=study.deleted"),
        )
        .await;
        assert_eq!(json_of(&body)["events"], json!([]));
        let (_, body) = send(&app, request("GET", "/api/v1/events?tail=1")).await;
        assert_eq!(json_of(&body)["events"][0]["seq"], 1);
        assert_eq!(
            send(&app, request("GET", "/api/v1/events?kind=Bad"))
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn no_data_endpoints() {
        // Data goes in and out through DIMSE and DICOMweb only.
        let dir = tempfile::tempdir().unwrap();
        let (app, archive) = app(dir.path()).await;
        ingest(&archive, "1.2.3.1.1").await;
        for (method, uri) in [
            ("POST", "/api/v1/instances"),
            ("GET", "/api/v1/instances/1.2.3.1.1"),
            ("DELETE", "/api/v1/studies/1.2.3"),
            ("POST", "/api/v1/peers/p/send"),
        ] {
            let status = send(&app, request(method, uri)).await.0;
            assert!(
                status == StatusCode::NOT_FOUND || status == StatusCode::METHOD_NOT_ALLOWED,
                "{method} {uri}: {status}"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn event_stream_replays_then_goes_live() {
        let dir = tempfile::tempdir().unwrap();
        let (app, archive) = app(dir.path()).await;
        ingest(&archive, "1.2.3.1.1").await;

        let mut req = request("GET", "/api/v1/events/stream");
        req.headers_mut()
            .insert("last-event-id", "0".parse().unwrap());
        let resp = respond(&app, req).await;
        assert_eq!(resp.headers()[header::CONTENT_TYPE], "text/event-stream");
        let mut body = resp.into_body().into_data_stream();
        let first = String::from_utf8(body.next().await.unwrap().unwrap().to_vec()).unwrap();
        assert!(
            first.contains("id: 1") && first.contains("event: instance.stored"),
            "{first}"
        );

        // A change committed after the stream started is delivered live.
        ingest(&archive, "1.2.3.1.2").await;
        let next = tokio::time::timeout(std::time::Duration::from_secs(5), body.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let next = String::from_utf8(next.to_vec()).unwrap();
        assert!(next.contains("id: 2"), "{next}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn peers_and_jobs_endpoints() {
        let dir = tempfile::tempdir().unwrap();
        let (app, _) = app(dir.path()).await;
        let (status, body) = send(&app, request("GET", "/api/v1/peers")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json_of(&body), json!([]));
        assert_eq!(
            send(&app, request("POST", "/api/v1/peers/nowhere/echo"))
                .await
                .0,
            StatusCode::NOT_FOUND
        );
        let (status, body) = send(&app, request("GET", "/api/v1/jobs")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json_of(&body), json!([]));
        let kind = JobKind::Send {
            peer: "p".into(),
            resources: vec![],
        };
        let missing = format!(
            "/api/v1/jobs/{}",
            Job::new(
                medicus_core::Partition::default_partition(),
                "MEDICUS".into(),
                kind,
                1,
            )
            .transaction_uid()
        );
        assert_eq!(
            send(&app, request("GET", &missing)).await.0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            send(&app, request("GET", "/api/v1/jobs/nonsense")).await.0,
            StatusCode::BAD_REQUEST
        );
    }

    fn with_header(mut req: Request<Body>, name: header::HeaderName, value: &str) -> Request<Body> {
        req.headers_mut().insert(name, value.parse().unwrap());
        req
    }

    fn post_json(uri: &str, body: &serde_json::Value) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    /// `name=value` of a response's `Set-Cookie`, to send back as `Cookie`.
    fn cookie_of(resp: &Response) -> String {
        let set = resp.headers()[header::SET_COOKIE].to_str().unwrap();
        assert!(
            set.contains("HttpOnly") && set.contains("SameSite=Strict"),
            "{set}"
        );
        set.split(';').next().unwrap().to_owned()
    }

    #[tokio::test(flavor = "multi_thread")]
    #[allow(clippy::too_many_lines)] // one scenario, step by step
    async fn setup_sessions_and_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.server.data_dir = dir.path().to_owned();
        cfg.roles.insert(
            "ops".into(),
            crate::config::RoleConfig {
                grants: vec![crate::config::GrantConfig {
                    nodes: vec!["*".into()],
                    permissions: vec![Permission::AdminJobs],
                }],
            },
        );
        let (app, _) = app_in_setup(cfg, None).await;

        // Setup mode: only the setup works.
        let (status, body) = send(&app, request("GET", "/api/v1/system")).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(json_of(&body)["error"], "setup_required");
        let (_, body) = send(&app, request("GET", "/api/v1/setup")).await;
        assert_eq!(json_of(&body)["required"], true);
        let login = json!({ "name": "admin", "password": "long enough" });
        assert_eq!(
            send(&app, post_json("/api/v1/auth/login", &login)).await.0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        let mut setup = json!({ "token": "wrong", "name": "admin", "password": "long enough" });
        assert_eq!(
            send(&app, post_json("/api/v1/setup", &setup)).await.0,
            StatusCode::FORBIDDEN
        );
        setup["token"] = json!(app.accounts.setup_token().unwrap());
        let resp = respond(&app, post_json("/api/v1/setup", &setup)).await;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let admin = cookie_of(&resp);
        assert_eq!(
            send(&app, post_json("/api/v1/setup", &setup)).await.0,
            StatusCode::FORBIDDEN,
            "only once"
        );
        let (_, body) = send(&app, request("GET", "/api/v1/setup")).await;
        assert_eq!(json_of(&body)["required"], false);

        // The session cookie authenticates; nothing else does.
        let as_admin = |req| with_header(req, header::COOKIE, &admin);
        let (status, body) = send(&app, as_admin(request("GET", "/api/v1/auth/me"))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json_of(&body)["user"]["name"], "admin");
        assert!(
            json_of(&body)["permissions"]
                .as_array()
                .unwrap()
                .contains(&json!("admin.system"))
        );
        assert_eq!(json_of(&body)["user"].get("password_hash"), None);
        assert_eq!(
            send(&app, request("GET", "/api/v1/system")).await.0,
            StatusCode::UNAUTHORIZED
        );
        let forged = with_header(
            request("GET", "/api/v1/system"),
            header::AUTHORIZATION,
            "Bearer mdt_00",
        );
        assert_eq!(send(&app, forged).await.0, StatusCode::UNAUTHORIZED);
        assert_eq!(
            send(&app, as_admin(request("GET", "/api/v1/system")))
                .await
                .0,
            StatusCode::OK
        );

        // Changes authenticated by the cookie alone need X-Requested-With.
        let cancel = || as_admin(request("POST", "/api/v1/jobs/nonsense/cancel"));
        let (status, body) = send(&app, cancel()).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(json_of(&body)["error"], "csrf");
        let with_xrw = with_header(
            cancel(),
            HeaderName::from_static("x-requested-with"),
            "fetch",
        );
        assert_eq!(send(&app, with_xrw).await.0, StatusCode::BAD_REQUEST);

        // Users get what their roles grant.
        app.accounts
            .create_user("op", Some("operator pw"), vec!["ops".into()], None)
            .await
            .unwrap();
        let wrong = json!({ "name": "op", "password": "not it" });
        assert_eq!(
            send(&app, post_json("/api/v1/auth/login", &wrong)).await.0,
            StatusCode::UNAUTHORIZED
        );
        let login = json!({ "name": "op", "password": "operator pw" });
        let resp = respond(&app, post_json("/api/v1/auth/login", &login)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let op = cookie_of(&resp);
        let as_op = |req| with_header(req, header::COOKIE, &op);
        assert_eq!(
            send(&app, as_op(request("GET", "/api/v1/jobs"))).await.0,
            StatusCode::OK
        );
        assert_eq!(
            send(&app, as_op(request("GET", "/api/v1/nodes"))).await.0,
            StatusCode::OK
        );
        for uri in ["/api/v1/system", "/api/v1/peers", "/api/v1/events"] {
            assert_eq!(
                send(&app, as_op(request("GET", uri))).await.0,
                StatusCode::FORBIDDEN,
                "{uri}"
            );
        }

        // Logging out ends the session.
        let logout = with_header(
            as_op(request("POST", "/api/v1/auth/logout")),
            HeaderName::from_static("x-requested-with"),
            "fetch",
        );
        let resp = respond(&app, logout).await;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        assert!(
            resp.headers()[header::SET_COOKIE]
                .to_str()
                .unwrap()
                .contains("Max-Age=0")
        );
        assert_eq!(
            send(&app, as_op(request("GET", "/api/v1/jobs"))).await.0,
            StatusCode::UNAUTHORIZED
        );
    }

    /// Addon routes are served for paths the core does not handle, with the caller's user.
    #[tokio::test(flavor = "multi_thread")]
    async fn addon_routes() {
        struct Echo;
        #[async_trait::async_trait]
        impl medicus_core::capability::HttpRoutes for Echo {
            async fn handle(
                &self,
                request: medicus_core::capability::HttpRequest,
                body: bytes::Bytes,
            ) -> medicus_core::Result<medicus_core::capability::HttpResponse> {
                let text = format!(
                    "{} {} {:?} {}",
                    request.method,
                    request.path,
                    request.principal,
                    body.len()
                );
                Ok(
                    medicus_core::capability::HttpResponse::new(200, text)
                        .header("x-from", "addon"),
                )
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.server.data_dir = dir.path().to_owned();
        let routes = crate::routes::Routes(
            [(
                "/".to_owned(),
                Arc::new(Echo) as Arc<dyn medicus_core::capability::HttpRoutes>,
            )]
            .into(),
        );
        let (mut app, _) = app_in_setup(cfg, Some(routes)).await;
        app.accounts
            .create_user("root", None, vec![crate::access::ADMIN_ROLE.into()], None)
            .await
            .unwrap();
        let token = app
            .accounts
            .create_token("root", None, None)
            .await
            .unwrap()
            .secret;

        // Public without credentials; with valid ones, the user is passed on.
        let (status, body) = send(&app, request("GET", "/index.html")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(String::from_utf8(body).unwrap(), "GET /index.html None 0");
        app.token = Some(token);
        let resp = respond(&app, post_json("/settings/x", &json!({ "a": 1 }))).await;
        assert_eq!(resp.headers()["x-from"], "addon");
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"POST /settings/x Some(\"root\") 7");

        // The core's paths never reach the addon; bodies are limited.
        let (status, body) = send(&app, request("GET", "/api/v1/nothing")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(!String::from_utf8_lossy(&body).contains("GET"));
        let big = Request::builder()
            .method("POST")
            .uri("/upload")
            .body(Body::from(vec![
                0u8;
                medicus_core::capability::http::MAX_REQUEST_BODY
                    + 1
            ]))
            .unwrap();
        assert_eq!(send(&app, big).await.0, StatusCode::PAYLOAD_TOO_LARGE);
    }

    fn json_request(method: &str, uri: &str, body: &serde_json::Value) -> Request<Body> {
        let mut req = post_json(uri, body);
        *req.method_mut() = method.parse().unwrap();
        req
    }

    /// Sends `req` and returns the status and the JSON body (`null` if empty).
    async fn call(app: &TestApp, req: Request<Body>) -> (StatusCode, serde_json::Value) {
        let (status, body) = send(app, req).await;
        let value = if body.is_empty() {
            serde_json::Value::Null
        } else {
            json_of(&body)
        };
        (status, value)
    }

    #[tokio::test(flavor = "multi_thread")]
    #[allow(clippy::too_many_lines)] // one scenario, step by step
    async fn settings_endpoints() {
        let dir = tempfile::tempdir().unwrap();
        let (app, _) = app(dir.path()).await;
        let resp = respond(&app, request("GET", "/api/v1/settings")).await;
        assert_eq!(resp.headers()[header::ETAG], "\"1\"");
        let (status, body) = call(&app, request("GET", "/api/v1/settings")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["revision"], 1);
        let mut settings = body["settings"].clone();

        // Replacing the document needs the revision it is based on.
        settings["server"]["name"] = json!("renamed");
        let put = |rev: Option<&str>| {
            let mut req = json_request("PUT", "/api/v1/settings?comment=rename", &settings);
            if let Some(rev) = rev {
                req.headers_mut()
                    .insert(header::IF_MATCH, rev.parse().unwrap());
            }
            req
        };
        assert_eq!(
            call(&app, put(None)).await.0,
            StatusCode::PRECONDITION_REQUIRED
        );
        let (status, body) = call(&app, put(Some("\"1\""))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({ "revision": 2, "restart_required": ["server"] })
        );
        assert_eq!(
            call(&app, put(Some("1"))).await.0,
            StatusCode::PRECONDITION_FAILED
        );

        // Items one by one; nodes take effect at once.
        let node_b = json!({ "partition": "default", "dicomweb": { "path": "b" } });
        let (status, body) = call(
            &app,
            json_request("PUT", "/api/v1/settings/nodes/B", &node_b),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["revision"], 3);
        let (_, nodes) = call(&app, request("GET", "/api/v1/nodes")).await;
        assert!(
            nodes.as_array().unwrap().iter().any(|n| n["name"] == "B"),
            "{nodes}"
        );
        let (_, section) = call(&app, request("GET", "/api/v1/settings/nodes")).await;
        assert_eq!(section["B"]["dicomweb"]["path"], "b");
        let bad = json!({ "partition": "nowhere" });
        assert_eq!(
            call(&app, json_request("PUT", "/api/v1/settings/nodes/C", &bad))
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            call(&app, json_request("PUT", "/api/v1/settings/bogus/x", &bad))
                .await
                .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            call(&app, request("DELETE", "/api/v1/settings/nodes/B"))
                .await
                .0,
            StatusCode::OK
        );
        assert_eq!(
            call(&app, request("DELETE", "/api/v1/settings/nodes/B"))
                .await
                .0,
            StatusCode::NOT_FOUND
        );

        // History, single revisions and rollback.
        let (_, history) = call(&app, request("GET", "/api/v1/settings/history")).await;
        let revisions: Vec<_> = history
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["revision"].clone())
            .collect();
        assert_eq!(revisions, [json!(4), json!(3), json!(2), json!(1)]);
        assert_eq!(history[0]["author"], "root");
        assert_eq!(history[0]["comment"], "nodes: removed B");
        assert_eq!(history[2]["comment"], "rename");
        let (_, two) = call(&app, request("GET", "/api/v1/settings/revisions/2")).await;
        assert_eq!(two["settings"]["server"]["name"], "renamed");
        let (status, body) = call(
            &app,
            request("POST", "/api/v1/settings/revisions/1/rollback"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({ "revision": 5, "restart_required": [] }));
        let (_, system) = call(&app, request("GET", "/api/v1/system")).await;
        assert_eq!(system["settings_revision"], 5);
        assert_eq!(system["restart_required"], json!([]));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn addon_endpoints() {
        let dir = tempfile::tempdir().unwrap();
        let (app, _) = app(dir.path()).await;
        let (status, addons) = call(&app, request("GET", "/api/v1/addons")).await;
        assert_eq!(status, StatusCode::OK);
        let addon = |list: &serde_json::Value, name: &str| {
            list.as_array()
                .unwrap()
                .iter()
                .find(|a| a["name"] == name)
                .unwrap()
                .clone()
        };
        let index = addon(&addons, "index-sqlite");
        assert_eq!(index["bootstrap"], true);
        assert_eq!(index["loaded_by"]["by"], "archive");
        assert_eq!(
            addon(&addons, "storage-fs")["required_by"],
            json!(["archive"])
        );
        let dimse = addon(&addons, "dimse");
        assert_eq!(dimse["selected"], false);
        assert_eq!(
            dimse["config_schema"]["properties"]["listen"]["type"],
            "string"
        );

        let put = |name: &str, body: serde_json::Value| {
            json_request("PUT", &format!("/api/v1/addons/{name}"), &body)
        };
        // The archive needs a blob store; the index is configured in the file; schemas apply.
        for (name, body) in [
            ("storage-fs", json!({ "enabled": false })),
            ("index-sqlite", json!({ "enabled": true })),
            (
                "dimse",
                json!({ "enabled": true, "config": { "listen": 5 } }),
            ),
        ] {
            let (status, err) = call(&app, put(name, body)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{name}: {err}");
        }
        assert_eq!(
            call(&app, put("nope", json!({}))).await.0,
            StatusCode::NOT_FOUND
        );

        let enable = json!({ "enabled": true, "config": { "listen": "127.0.0.1:0" } });
        let (status, body) = call(&app, put("dimse", enable)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["restart_required"], json!(["addons"]));
        let (_, addons) = call(&app, request("GET", "/api/v1/addons")).await;
        let dimse = addon(&addons, "dimse");
        assert_eq!(
            (dimse["enabled"].clone(), dimse["selected"].clone()),
            (json!(true), json!(true))
        );
        assert_eq!(dimse["config"]["listen"], "127.0.0.1:0");
    }

    #[tokio::test(flavor = "multi_thread")]
    #[allow(clippy::too_many_lines)] // one scenario, step by step
    async fn user_endpoints() {
        let dir = tempfile::tempdir().unwrap();
        let (app, _) = app(dir.path()).await;
        let alice = json!({ "name": "alice", "password": "alice's pw", "roles": ["nope"] });
        assert_eq!(
            call(&app, post_json("/api/v1/users", &alice)).await.0,
            StatusCode::BAD_REQUEST
        );
        let alice = json!({ "name": "alice", "password": "alice's pw" });
        let (status, created) = call(&app, post_json("/api/v1/users", &alice)).await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(created["password"], true);
        assert_eq!(created.get("password_hash"), None);
        assert_eq!(
            call(&app, post_json("/api/v1/users", &alice)).await.0,
            StatusCode::CONFLICT
        );
        let (_, users) = call(&app, request("GET", "/api/v1/users")).await;
        assert_eq!(users.as_array().unwrap().len(), 2);

        // Alice has no admin permission.
        let login = json!({ "name": "alice", "password": "alice's pw" });
        let resp = respond(&app, post_json("/api/v1/auth/login", &login)).await;
        let cookie = cookie_of(&resp);
        let as_alice = |req: Request<Body>| {
            with_header(
                with_header(req, header::COOKIE, &cookie),
                HeaderName::from_static("x-requested-with"),
                "test",
            )
        };
        assert_eq!(
            call(&app, as_alice(request("GET", "/api/v1/users")))
                .await
                .0,
            StatusCode::FORBIDDEN
        );

        // Her own password and tokens she manages herself.
        let wrong = json!({ "current": "not it", "new": "a new password" });
        assert_eq!(
            call(
                &app,
                as_alice(json_request("PUT", "/api/v1/auth/password", &wrong))
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        let change = json!({ "current": "alice's pw", "new": "a new password" });
        assert_eq!(
            call(
                &app,
                as_alice(json_request("PUT", "/api/v1/auth/password", &change))
            )
            .await
            .0,
            StatusCode::NO_CONTENT
        );
        let (status, token) = call(
            &app,
            as_alice(post_json("/api/v1/auth/tokens", &json!({ "name": "ci" }))),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let secret = token["token"].as_str().unwrap().to_owned();
        let (_, tokens) = call(&app, as_alice(request("GET", "/api/v1/auth/tokens"))).await;
        assert_eq!(tokens[0]["name"], "ci");
        assert_eq!(tokens[0].get("token"), None);
        let bearer = |req| with_header(req, header::AUTHORIZATION, &format!("Bearer {secret}"));
        let (_, me) = call(&app, bearer(request("GET", "/api/v1/auth/me"))).await;
        assert_eq!(me["user"]["name"], "alice");
        // Others cannot revoke it; she can.
        let revoke = format!("/api/v1/auth/tokens/{}", token["id"].as_str().unwrap());
        assert_eq!(
            call(&app, request("DELETE", &revoke)).await.0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            call(&app, as_alice(request("DELETE", &revoke))).await.0,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            call(&app, bearer(request("GET", "/api/v1/auth/me")))
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );

        // There is always an enabled administrator.
        let disable = json!({ "disabled": true });
        assert_eq!(
            call(&app, json_request("PATCH", "/api/v1/users/root", &disable))
                .await
                .0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            call(&app, request("DELETE", "/api/v1/users/root")).await.0,
            StatusCode::CONFLICT
        );
        let promote = json!({ "roles": ["admin"], "display_name": "Alice" });
        let (status, alice) =
            call(&app, json_request("PATCH", "/api/v1/users/alice", &promote)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(alice["display_name"], "Alice");
        assert_eq!(
            call(&app, request("DELETE", "/api/v1/users/root")).await.0,
            StatusCode::NO_CONTENT
        );
        // Now she is the only administrator and cannot drop her own role.
        let (status, _) = call(
            &app,
            as_alice(json_request(
                "PATCH",
                "/api/v1/users/alice",
                &json!({ "roles": [] }),
            )),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "she is the last administrator now"
        );
    }
}
