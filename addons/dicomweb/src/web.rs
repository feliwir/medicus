//! Routing: every resource is served per node, below the node's service root (`node_root`, by
//! default `/nodes/{path}/rs/...`), and for the default node also below `default_root` (`/rs/...`).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::extract::{ConnectInfo, FromRequestParts, Path};
use axum::http::request::Parts;
use axum::http::{HeaderName, HeaderValue, Method, header};
use axum::routing::{get, post};
use medicus_addon_sdk::Host;
use medicus_core::CallContext;
use medicus_core::capability::HostServices as _;
use medicus_core::event::Origin;
use tower_http::cors::{AllowOrigin, Any, CorsLayer};

use crate::error::{WebError, WebResult};
use crate::{qido, render, stow, wado};

/// What every handler needs.
#[derive(Debug, Clone)]
pub(crate) struct State {
    pub(crate) host: Host,
    /// `DICOMweb` path → node name.
    pub(crate) nodes: Arc<HashMap<String, String>>,
    /// A node's service root, with `{node}` for its path.
    pub(crate) node_root: String,
    /// The default node's service root.
    pub(crate) default_root: String,
    /// The path of the node also served at `default_root`.
    pub(crate) default_path: Option<String>,
    /// Absolute base for Retrieve URLs (e.g. `https://pacs.example.org`); else from `Host`.
    pub(crate) base_url: Option<String>,
    /// Header naming the authenticated user, set by a trusted reverse proxy.
    pub(crate) user_header: Option<HeaderName>,
}

/// A request's node, caller and URL base, plus its other path parameters.
#[derive(Debug)]
pub(crate) struct Request {
    pub(crate) context: CallContext,
    /// The node's service root, e.g. `http://host/nodes/radiology/rs`.
    pub(crate) root: String,
    pub(crate) params: HashMap<String, String>,
}

impl Request {
    pub(crate) fn param(&self, name: &str) -> WebResult<&str> {
        self.params
            .get(name)
            .map(String::as_str)
            .ok_or_else(|| WebError::bad_request(format!("missing path parameter {name}")))
    }
}

impl FromRequestParts<State> for Request {
    type Rejection = WebError;

    async fn from_request_parts(parts: &mut Parts, state: &State) -> WebResult<Self> {
        let Path(mut params): Path<HashMap<String, String>> =
            Path::from_request_parts(parts, state)
                .await
                .map_err(|e| WebError::bad_request(e.to_string()))?;
        let (path, prefix) = match params.remove("node") {
            Some(path) => {
                let prefix = state.node_root.replace("{node}", &path);
                (path, prefix)
            }
            None => (
                state.default_path.clone().ok_or_else(|| {
                    WebError::bad_request(format!("no default node; use {}", state.node_root))
                })?,
                state.default_root.clone(),
            ),
        };
        let node = state
            .nodes
            .get(&path)
            .ok_or_else(|| {
                WebError::new(
                    axum::http::StatusCode::NOT_FOUND,
                    format!("no node at {path:?}"),
                )
            })?
            .clone();
        let remote_addr = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|c| c.0.to_string())
            .unwrap_or_default();
        let principal = match credential(parts) {
            Some((secret, from_cookie)) => {
                // A browser sends the cookie along with requests other sites make it send.
                if from_cookie
                    && !matches!(parts.method, Method::GET | Method::HEAD | Method::OPTIONS)
                    && !parts.headers.contains_key("x-requested-with")
                {
                    return Err(WebError::new(
                        axum::http::StatusCode::FORBIDDEN,
                        "a request authenticated by the session cookie that changes data needs \
                         an X-Requested-With header",
                    ));
                }
                let user = state.host.authenticate(&secret).await?.ok_or_else(|| {
                    WebError::new(
                        axum::http::StatusCode::UNAUTHORIZED,
                        "the session or API token is invalid or expired",
                    )
                })?;
                Some(user)
            }
            None => state.user_header.as_ref().and_then(|name| {
                parts
                    .headers
                    .get(name)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned)
            }),
        };
        let request_id = parts
            .headers
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let base = state.base_url.clone().unwrap_or_else(|| {
            let host = parts
                .headers
                .get(header::HOST)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("localhost");
            let scheme = parts
                .headers
                .get("x-forwarded-proto")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("http");
            format!("{scheme}://{host}")
        });
        Ok(Self {
            context: CallContext::new(
                node,
                Origin::Dicomweb {
                    remote_addr,
                    principal,
                    request_id,
                },
            ),
            root: format!("{}{prefix}", base.trim_end_matches('/')),
            params,
        })
    }
}

/// The server's own credential on a request: a bearer token, else the session cookie (then the
/// flag is set).
fn credential(parts: &Parts) -> Option<(String, bool)> {
    let bearer = parts
        .headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    if let Some(token) = bearer {
        return Some((token.trim().to_owned(), false));
    }
    parts
        .headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|c| {
            c.trim()
                .strip_prefix(medicus_core::access::SESSION_COOKIE)?
                .strip_prefix('=')
                .map(|s| (s.to_owned(), true))
        })
}

/// The routes of one service root.
fn routes() -> Router<State> {
    Router::new()
        // QIDO-RS
        .route("/studies", get(qido::studies).post(stow::store))
        .route("/series", get(qido::series))
        .route("/instances", get(qido::instances))
        .route("/studies/{study}/series", get(qido::series))
        .route("/studies/{study}/instances", get(qido::instances))
        .route(
            "/studies/{study}/series/{series}/instances",
            get(qido::instances),
        )
        // WADO-RS and STOW-RS into a study
        .route("/studies/{study}", get(wado::retrieve).post(stow::store))
        .route("/studies/{study}/metadata", get(wado::metadata))
        .route("/studies/{study}/series/{series}", get(wado::retrieve))
        .route(
            "/studies/{study}/series/{series}/metadata",
            get(wado::metadata),
        )
        .route(
            "/studies/{study}/series/{series}/instances/{instance}",
            get(wado::retrieve),
        )
        .route(
            "/studies/{study}/series/{series}/instances/{instance}/metadata",
            get(wado::metadata),
        )
        .route(
            "/studies/{study}/series/{series}/instances/{instance}/frames/{frames}",
            get(wado::frames),
        )
        // Rendered resources and thumbnails
        .route("/studies/{study}/rendered", get(render::rendered))
        .route("/studies/{study}/thumbnail", get(render::rendered))
        .route(
            "/studies/{study}/series/{series}/rendered",
            get(render::rendered),
        )
        .route(
            "/studies/{study}/series/{series}/thumbnail",
            get(render::rendered),
        )
        .route(
            "/studies/{study}/series/{series}/instances/{instance}/rendered",
            get(render::rendered),
        )
        .route(
            "/studies/{study}/series/{series}/instances/{instance}/thumbnail",
            get(render::rendered),
        )
        .route(
            "/studies/{study}/series/{series}/instances/{instance}/frames/{frames}/rendered",
            get(render::rendered),
        )
        .route(
            "/studies/{study}/series/{series}/instances/{instance}/frames/{frames}/thumbnail",
            get(render::rendered),
        )
}

pub(crate) fn router(state: State, cors_origins: &[String]) -> medicus_core::Result<Router> {
    let router = Router::new()
        .nest(&state.node_root, routes())
        .nest(&state.default_root, routes())
        .route("/health", post(|| async { "ok" }).get(|| async { "ok" }))
        .layer(axum::extract::DefaultBodyLimit::disable())
        .with_state(state);
    if cors_origins.is_empty() {
        return Ok(router);
    }
    let origins = if cors_origins.iter().any(|o| o == "*") {
        AllowOrigin::any()
    } else {
        let list = cors_origins
            .iter()
            .map(|o| {
                HeaderValue::from_str(o).map_err(|e| {
                    medicus_core::Error::invalid_argument(format!("cors_origins: {o:?}: {e}"))
                })
            })
            .collect::<medicus_core::Result<Vec<_>>>()?;
        AllowOrigin::list(list)
    };
    let cors = CorsLayer::new()
        .allow_origin(origins)
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers(Any)
        .expose_headers([header::CONTENT_TYPE, header::WARNING]);
    Ok(router.layer(cors))
}
