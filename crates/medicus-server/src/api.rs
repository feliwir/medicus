//! The admin HTTP API (`/api/v1`, PLAN §3.6) and health endpoints.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use futures::{Stream, TryStreamExt as _};
use medicus_core::capability::index::ResourceUid;
use medicus_core::event::{Event, EventKind, Origin};
use medicus_core::uid::{SeriesInstanceUid, SopInstanceUid, StudyInstanceUid};
use medicus_core::{Error, ErrorKind};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::archive::Archive;

/// Shared state of all handlers.
#[derive(Debug, Clone)]
pub(crate) struct AppState {
    pub(crate) archive: Arc<Archive>,
    pub(crate) name: String,
    pub(crate) addons: Arc<[medicus_host::AddonInfo]>,
}

pub(crate) fn router(state: AppState) -> Router {
    let v1 = Router::new()
        .route("/system", get(system))
        .route(
            "/instances",
            post(upload).layer(DefaultBodyLimit::disable()),
        )
        .route("/instances/{uid}", get(download).delete(delete_instance))
        .route("/series/{uid}", delete(delete_series))
        .route("/studies/{uid}", delete(delete_study))
        .route("/events", get(list_events))
        .route("/events/stream", get(stream_events));
    Router::new()
        .route("/health/live", get(|| async { "ok" }))
        .route("/health/ready", get(ready))
        .nest("/api/v1", v1)
        .with_state(state)
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

fn origin(addr: SocketAddr) -> Origin {
    Origin::AdminApi {
        remote_addr: addr.to_string(),
        principal: None,
    }
}

fn bad_uid(e: impl std::fmt::Display) -> ApiError {
    ApiError(Error::invalid_argument(e.to_string()))
}

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

/// `POST /api/v1/instances` with a DICOM Part 10 file as body (streamed).
async fn upload(
    State(s): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    body: Body,
) -> ApiResult<(StatusCode, Json<crate::ingest::Stored>)> {
    let data = body.into_data_stream().map_err(|e| {
        Error::new(
            ErrorKind::InvalidArgument,
            format!("reading request body: {e}"),
        )
    });
    let stored = s.archive.ingest(Box::pin(data), origin(addr)).await?;
    Ok((StatusCode::CREATED, Json(stored)))
}

/// `GET /api/v1/instances/{uid}`: the stored Part 10 file.
async fn download(State(s): State<AppState>, Path(uid): Path<String>) -> ApiResult<Response> {
    let uid = SopInstanceUid::parse(&uid).map_err(bad_uid)?;
    let (location, data) = s.archive.retrieve(&uid).await?;
    let body = Body::from_stream(data.map_err(|e| std::io::Error::other(e.to_string())));
    Ok((
        [
            (header::CONTENT_TYPE, "application/dicom".to_owned()),
            (header::CONTENT_LENGTH, location.size.to_string()),
        ],
        body,
    )
        .into_response())
}

#[derive(Debug, Serialize)]
struct Deleted {
    seq: u64,
    instances: usize,
}

async fn delete_resource(
    s: &AppState,
    addr: SocketAddr,
    resource: ResourceUid,
) -> ApiResult<Json<Deleted>> {
    let out = s.archive.delete(&resource, origin(addr)).await?;
    Ok(Json(Deleted {
        seq: out.seq,
        instances: out.removed.len(),
    }))
}

async fn delete_instance(
    State(s): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Path(uid): Path<String>,
) -> ApiResult<Json<Deleted>> {
    let uid = SopInstanceUid::parse(&uid).map_err(bad_uid)?;
    delete_resource(&s, addr, ResourceUid::Instance(uid)).await
}

async fn delete_series(
    State(s): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Path(uid): Path<String>,
) -> ApiResult<Json<Deleted>> {
    let uid = SeriesInstanceUid::parse(&uid).map_err(bad_uid)?;
    delete_resource(&s, addr, ResourceUid::Series(uid)).await
}

async fn delete_study(
    State(s): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Path(uid): Path<String>,
) -> ApiResult<Json<Deleted>> {
    let uid = StudyInstanceUid::parse(&uid).map_err(bad_uid)?;
    delete_resource(&s, addr, ResourceUid::Study(uid)).await
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
                k.parse()
                    .map_err(|e: medicus_core::event::EventKindError| bad_uid(e))
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
    use axum::http::Request;
    use futures::StreamExt as _;
    use http_body_util::BodyExt as _;
    use medicus_testkit::dicom::InstanceBuilder;
    use tower::ServiceExt as _;

    async fn app(dir: &std::path::Path) -> (Router, Arc<Archive>) {
        let mut cfg = Config::default();
        cfg.server.data_dir = dir.to_owned();
        let caps = crate::addons::load(&cfg, &tokio::runtime::Handle::current()).unwrap();
        let archive = Arc::new(Archive::new(&caps, &cfg.archive).await.unwrap());
        let state = AppState {
            archive: archive.clone(),
            name: "test".into(),
            addons: caps.addons.clone().into(),
        };
        (router(state), archive)
    }

    fn request(method: &str, uri: &str, body: Body) -> Request<Body> {
        let mut r = Request::builder()
            .method(method)
            .uri(uri)
            .body(body)
            .unwrap();
        r.extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 4242))));
        r
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
    async fn upload_download_events_delete() {
        let dir = tempfile::tempdir().unwrap();
        let (app, _archive) = app(dir.path()).await;
        assert_eq!(
            send(&app, request("GET", "/health/ready", Body::empty()))
                .await
                .0,
            StatusCode::OK
        );

        // A 3 MiB upload: well above axum's default body limit.
        let file = InstanceBuilder::new("1.2.3", "1.2.3.1", "1.2.3.1.1")
            .pixel_data(3 << 20)
            .build();
        let (status, body) = send(
            &app,
            request("POST", "/api/v1/instances", Body::from(file.clone())),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::CREATED,
            "{}",
            String::from_utf8_lossy(&body)
        );
        let stored = json_of(&body);
        assert_eq!(stored["instance"], "1.2.3.1.1");
        assert_eq!(stored["duplicate"], "none");

        let (status, body) = send(
            &app,
            request("GET", "/api/v1/instances/1.2.3.1.1", Body::empty()),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, file);

        let (status, body) = send(
            &app,
            request("GET", "/api/v1/events?since=0", Body::empty()),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let page = json_of(&body);
        assert_eq!(page["events"][0]["kind"], "instance.stored");
        assert_eq!(page["events"][0]["origin"]["type"], "admin_api");
        assert_eq!(page["events"][0]["origin"]["remote_addr"], "127.0.0.1:4242");
        assert_eq!(page["next"], 1);

        let (status, body) = send(
            &app,
            request("DELETE", "/api/v1/studies/1.2.3", Body::empty()),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json_of(&body)["instances"], 1);
        let (_, body) = send(
            &app,
            request(
                "GET",
                "/api/v1/events?since=1&kind=study.deleted",
                Body::empty(),
            ),
        )
        .await;
        assert_eq!(json_of(&body)["events"][0]["kind"], "study.deleted");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn errors_map_to_status_codes() {
        let dir = tempfile::tempdir().unwrap();
        let (app, _) = app(dir.path()).await;
        let (status, body) = send(
            &app,
            request("POST", "/api/v1/instances", Body::from("not dicom")),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(json_of(&body)["error"], "invalid argument");
        assert_eq!(
            send(
                &app,
                request("GET", "/api/v1/instances/9.9.9", Body::empty())
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            send(
                &app,
                request("GET", "/api/v1/instances/not-a-uid", Body::empty())
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            send(&app, request("DELETE", "/api/v1/series/9.9", Body::empty()))
                .await
                .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            send(
                &app,
                request("GET", "/api/v1/events?kind=Bad", Body::empty())
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn event_stream_replays_then_goes_live() {
        let dir = tempfile::tempdir().unwrap();
        let (app, archive) = app(dir.path()).await;
        let file = InstanceBuilder::new("1.2.3", "1.2.3.1", "1.2.3.1.1").build();
        send(&app, request("POST", "/api/v1/instances", Body::from(file))).await;

        let mut req = request("GET", "/api/v1/events/stream", Body::empty());
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
        let second_file = InstanceBuilder::new("1.2.3", "1.2.3.1", "1.2.3.1.2").build();
        archive
            .ingest(
                medicus_testkit::stream(&second_file, 4096),
                Origin::Internal {
                    component: "t".into(),
                },
            )
            .await
            .unwrap();
        let next = tokio::time::timeout(std::time::Duration::from_secs(5), body.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let next = String::from_utf8(next.to_vec()).unwrap();
        assert!(next.contains("id: 2"), "{next}");
    }
}
