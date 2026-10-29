//! Mounting the addons' HTTP routes (`HttpRoutes` v1, ADR-0004 §2) into the core's HTTP server.
//!
//! The core's own routes come first; every other request goes to the addon routes with the
//! longest prefix that matches whole path segments. Paths below the core's reserved prefixes never
//! reach an addon, even if no core route matches them.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::bail;
use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use medicus_core::capability::http::MAX_REQUEST_BODY;
use medicus_core::capability::{HttpRequest, HttpRoutes};
use serde_json::json;

use crate::api::AppState;

/// Path prefixes that belong to the core.
const RESERVED: [&str; 4] = ["/api", "/health", "/rs", "/nodes"];

/// The registered routes, by prefix.
#[derive(Clone, Default)]
pub(crate) struct Routes(pub(crate) BTreeMap<String, Arc<dyn HttpRoutes>>);

impl std::fmt::Debug for Routes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.0.keys()).finish()
    }
}

/// Whether `path` is `prefix` or below it, segment by segment.
fn under(path: &str, prefix: &str) -> bool {
    prefix == "/"
        || path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Checks a prefix an addon registers routes under.
pub(crate) fn check_prefix(prefix: &str) -> anyhow::Result<()> {
    let valid = prefix == "/"
        || (prefix.starts_with('/')
            && !prefix.ends_with('/')
            && prefix[1..].split('/').all(|segment| {
                !segment.is_empty()
                    && segment
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            }));
    if !valid {
        bail!("invalid HTTP route prefix {prefix:?}: use `/` or `/segment[/segment...]`");
    }
    if let Some(reserved) = RESERVED
        .iter()
        .find(|r| under(prefix, r) || (prefix != "/" && under(r, prefix)))
    {
        bail!("HTTP route prefix {prefix:?} overlaps {reserved}, which belongs to the core");
    }
    Ok(())
}

/// The routes with the longest prefix that `path` is under.
fn find<'r>(routes: &'r Routes, path: &str) -> Option<&'r Arc<dyn HttpRoutes>> {
    if RESERVED.iter().any(|r| under(path, r)) {
        return None;
    }
    routes
        .0
        .iter()
        .filter(|(prefix, _)| under(path, prefix))
        .max_by_key(|(prefix, _)| prefix.len())
        .map(|(_, routes)| routes)
}

fn error(status: StatusCode, message: &str) -> Response {
    (status, axum::Json(json!({ "message": message }))).into_response()
}

/// The fallback of the core's router: hands the request to the matching addon routes.
pub(crate) async fn handle(State(s): State<AppState>, request: Request) -> Response {
    let path = request.uri().path().to_owned();
    let Some(routes) = find(&s.routes, &path).cloned() else {
        return error(StatusCode::NOT_FOUND, "not found");
    };
    let (parts, body) = request.into_parts();
    let principal = match crate::api::credential(&parts.headers) {
        Some((secret, _)) => match s.accounts.authenticate(&secret).await {
            Ok(user) => user.map(|u| u.name),
            Err(e) => return crate::api::ApiError::from(e).into_response(),
        },
        None => None,
    };
    let Ok(body) = axum::body::to_bytes(body, MAX_REQUEST_BODY).await else {
        return error(
            StatusCode::PAYLOAD_TOO_LARGE,
            &format!("request bodies are limited to {MAX_REQUEST_BODY} bytes here"),
        );
    };
    let request = HttpRequest {
        method: parts.method.to_string(),
        path,
        query: parts.uri.query().map(str::to_owned),
        headers: parts
            .headers
            .iter()
            .filter_map(|(name, value)| {
                Some((name.as_str().to_owned(), value.to_str().ok()?.to_owned()))
            })
            .collect(),
        remote_addr: parts
            .extensions
            .get::<ConnectInfo<std::net::SocketAddr>>()
            .map(|c| c.0.to_string())
            .unwrap_or_default(),
        principal,
    };
    let response = match routes.handle(request, body).await {
        Ok(response) => response,
        Err(e) => return crate::api::ApiError::from(e).into_response(),
    };
    let Ok(status) = StatusCode::from_u16(response.head.status) else {
        return error(
            StatusCode::BAD_GATEWAY,
            &format!(
                "addon answered with invalid status {}",
                response.head.status
            ),
        );
    };
    let mut out = Response::new(Body::from_stream(response.body));
    *out.status_mut() = status;
    for (name, value) in response.head.headers {
        if let (Ok(name), Ok(value)) = (
            HeaderName::try_from(name.as_str()),
            HeaderValue::try_from(value.as_str()),
        ) {
            out.headers_mut().append(name, value);
        } else {
            tracing::warn!(header = %name, "addon response header is invalid; dropped");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes() {
        for ok in ["/", "/viewer", "/ui/v2", "/a.b-c_d"] {
            assert!(check_prefix(ok).is_ok(), "{ok}");
        }
        for bad in [
            "", "viewer", "/viewer/", "//", "/a//b", "/a b", "/api", "/api/x", "/health", "/rs",
            "/nodes/x",
        ] {
            assert!(check_prefix(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn longest_prefix_by_segments() {
        struct Named(&'static str);
        #[async_trait::async_trait]
        impl HttpRoutes for Named {
            async fn handle(
                &self,
                _: HttpRequest,
                _: bytes::Bytes,
            ) -> medicus_core::Result<medicus_core::capability::HttpResponse> {
                Ok(medicus_core::capability::HttpResponse::new(200, self.0))
            }
        }
        let routes = Routes(
            [("/", "root"), ("/viewer", "viewer")]
                .into_iter()
                .map(|(p, n)| (p.to_owned(), Arc::new(Named(n)) as Arc<dyn HttpRoutes>))
                .collect(),
        );
        let name = |path: &str| {
            find(&routes, path).map(|r| {
                let any: &dyn HttpRoutes = r.as_ref();
                std::ptr::from_ref(any).cast::<()>()
            })
        };
        let root = name("/");
        assert!(root.is_some());
        assert_eq!(name("/index.html"), root);
        assert_eq!(name("/viewerx"), root, "whole segments only");
        assert_ne!(name("/viewer"), root);
        assert_eq!(name("/viewer/app.js"), name("/viewer"));
        assert_eq!(name("/api/v1/nothing"), None);
        assert_eq!(name("/nodes/a/rs/studies"), None);
    }
}
