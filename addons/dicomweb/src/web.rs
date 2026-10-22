//! Routing: every resource is served per node, at `/nodes/{path}/rs/...`, and for the default node
//! also at `/rs/...`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::extract::{ConnectInfo, FromRequestParts, Path};
use axum::http::request::Parts;
use axum::http::{HeaderName, header};
use axum::routing::{get, post};
use medicus_addon_sdk::Host;
use medicus_core::CallContext;
use medicus_core::event::Origin;

use crate::error::{WebError, WebResult};
use crate::{qido, stow, wado};

/// What every handler needs.
#[derive(Debug, Clone)]
pub(crate) struct State {
    pub(crate) host: Host,
    /// `DICOMweb` path → node name.
    pub(crate) nodes: Arc<HashMap<String, String>>,
    /// The path of the node also served at `/rs`.
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
                let prefix = format!("/nodes/{path}/rs");
                (path, prefix)
            }
            None => (
                state.default_path.clone().ok_or_else(|| {
                    WebError::bad_request("no default node; use /nodes/{node}/rs")
                })?,
                "/rs".to_owned(),
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
        let principal = state.user_header.as_ref().and_then(|name| {
            parts
                .headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        });
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
}

pub(crate) fn router(state: State) -> Router {
    Router::new()
        .nest("/nodes/{node}/rs", routes())
        .nest("/rs", routes())
        .route("/health", post(|| async { "ok" }).get(|| async { "ok" }))
        .layer(axum::extract::DefaultBodyLimit::disable())
        .with_state(state)
}
