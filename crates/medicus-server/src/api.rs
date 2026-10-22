//! The admin HTTP API (`/api/v1`, PLAN §3.6) and health endpoints.

use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::Stream;
use medicus_core::capability::{Job, JobState, Peer};
use medicus_core::event::{Event, EventKind};
use medicus_core::{Error, ErrorKind};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::archive::Archive;

/// Shared state of all handlers.
#[derive(Debug, Clone)]
pub(crate) struct AppState {
    pub(crate) archive: Arc<Archive>,
    pub(crate) jobs: Arc<crate::jobs::Jobs>,
    pub(crate) name: String,
    pub(crate) addons: Arc<[medicus_host::AddonInfo]>,
}

pub(crate) fn router(state: AppState) -> Router {
    let v1 = Router::new()
        .route("/system", get(system))
        .route("/events", get(list_events))
        .route("/events/stream", get(stream_events))
        .route("/peers", get(list_peers))
        .route("/peers/{name}/echo", post(echo_peer))
        .route("/jobs", get(list_jobs))
        .route("/jobs/{id}", get(get_job))
        .route("/jobs/{id}/cancel", post(cancel_job));
    Router::new()
        .route("/health/live", get(|| async { "ok" }))
        .route("/health/ready", get(ready))
        .nest("/api/v1", v1)
        .with_state(state)
}

// ------------------------------------------------------------------------------------------------
// Peers and jobs

async fn list_peers(State(s): State<AppState>) -> Json<Vec<Peer>> {
    Json(s.jobs.peers().cloned().collect())
}

async fn echo_peer(State(s): State<AppState>, Path(name): Path<String>) -> ApiResult<StatusCode> {
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
    Query(q): Query<JobsQuery>,
) -> ApiResult<Json<Vec<JobView>>> {
    let jobs = s.jobs.list(q.state, q.limit.min(1000), q.offset).await?;
    Ok(Json(jobs.into_iter().map(JobView::from).collect()))
}

fn job_id(raw: &str) -> Result<uuid::Uuid, ApiError> {
    raw.parse()
        .ok()
        .or_else(|| Job::id_from_transaction_uid(raw))
        .ok_or_else(|| ApiError(Error::invalid_argument(format!("invalid job id {raw:?}"))))
}

async fn get_job(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<JobView>> {
    Ok(Json(s.jobs.get(job_id(&id)?).await?.into()))
}

async fn cancel_job(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<JobView>> {
    Ok(Json(s.jobs.cancel(job_id(&id)?).await?.into()))
}

/// An [`Error`] as an HTTP response with a JSON body.
pub(crate) struct ApiError(Error);

impl From<Error> for ApiError {
    fn from(e: Error) -> Self {
        Self(e)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match self.0.kind {
            ErrorKind::InvalidArgument => StatusCode::BAD_REQUEST,
            ErrorKind::NotFound => StatusCode::NOT_FOUND,
            ErrorKind::AlreadyExists => StatusCode::CONFLICT,
            ErrorKind::Rejected => StatusCode::FORBIDDEN,
            ErrorKind::ResourceExhausted => StatusCode::INSUFFICIENT_STORAGE,
            ErrorKind::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            ErrorKind::Unsupported => StatusCode::NOT_IMPLEMENTED,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        if status.is_server_error() {
            tracing::error!(error = %self.0, "request failed");
        }
        let body = json!({ "error": self.0.kind.as_str(), "message": self.0.message });
        (status, Json(body)).into_response()
    }
}

type ApiResult<T> = Result<T, ApiError>;

async fn ready(State(s): State<AppState>) -> StatusCode {
    match s.archive.index.last_event_seq().await {
        Ok(_) => StatusCode::OK,
        Err(e) => {
            tracing::warn!(error = %e, "index not ready");
            StatusCode::SERVICE_UNAVAILABLE
        }
    }
}

async fn system(State(s): State<AppState>) -> Json<serde_json::Value> {
    Json(json!({
        "name": s.name,
        "version": env!("CARGO_PKG_VERSION"),
        "abi_version": medicus_abi_version(),
        "storage": s.archive.default_store,
        "addons": s.addons,
    }))
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
                    ApiError(Error::invalid_argument(e.to_string()))
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

/// `GET /api/v1/events?since=&limit=&kind=`: a page of events from the outbox.
async fn list_events(
    State(s): State<AppState>,
    Query(q): Query<EventsQuery>,
) -> ApiResult<Json<EventPage>> {
    let kinds = q.kinds()?;
    let raw = s
        .archive
        .index
        .events_after(q.since, q.limit.clamp(1, 1000))
        .await?;
    let next = raw.last().map_or(q.since, |e| e.seq);
    let events = raw
        .into_iter()
        .filter(|e| kinds.is_empty() || kinds.contains(&e.kind))
        .collect();
    Ok(Json(EventPage { events, next }))
}

/// `GET /api/v1/events/stream?since=&kind=`: Server-Sent Events, starting after `since` or the
/// `Last-Event-ID` header (the event's `seq`), then live.
async fn stream_events(
    State(s): State<AppState>,
    Query(q): Query<EventsQuery>,
    headers: HeaderMap,
) -> ApiResult<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>> {
    let kinds = q.kinds()?;
    let since = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(q.since);
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
    use axum::http::{Request, header};
    use futures::StreamExt as _;
    use http_body_util::BodyExt as _;
    use medicus_core::capability::JobKind;
    use medicus_core::event::Origin;
    use medicus_testkit::dicom::InstanceBuilder;
    use tower::ServiceExt as _;

    async fn app(dir: &std::path::Path) -> (Router, Arc<Archive>) {
        let mut cfg = Config::default();
        cfg.server.data_dir = dir.to_owned();
        let caps = crate::addons::load(&cfg, &tokio::runtime::Handle::current()).unwrap();
        let archive = Arc::new(Archive::new(&caps, &cfg.archive).await.unwrap());
        let jobs = Arc::new(crate::jobs::Jobs::new(
            archive.clone(),
            [],
            caps.remote_nodes.clone(),
            cfg.jobs.clone(),
        ));
        let state = AppState {
            archive: archive.clone(),
            jobs,
            name: "test".into(),
            addons: caps.addons.clone().into(),
        };
        (router(state), archive)
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

    async fn send(app: &Router, req: Request<Body>) -> (StatusCode, Vec<u8>) {
        let resp = app.clone().oneshot(req).await.unwrap();
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
        let resp = app.clone().oneshot(req).await.unwrap();
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
            Job::new(medicus_core::Partition::default_partition(), kind, 1).transaction_uid()
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
}
