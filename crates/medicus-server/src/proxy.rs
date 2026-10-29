//! The `DICOMweb` proxy (ADR-0004 §6): the `dicomweb` addon's service roots (`node_root` and
//! `default_root`, by default `/nodes/{path}/rs/**` and `/rs/**`) on the core's HTTP server are
//! forwarded to the addon's listener, so the admin UI reaches the data through the same origin and
//! needs no CORS. The core serves the same paths as the addon, so the URLs in responses work
//! through either.
//!
//! Requests and responses are streamed. The caller's credentials (cookie, `Authorization`) pass
//! through, and the addon authenticates them through the host as for any other client; the header
//! a trusted proxy would set (`user_header`) is removed, since this proxy does not authenticate.
//! The original `Host` is kept, so the URLs the addon builds point to the core.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderName, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures::TryStreamExt as _;
use serde_json::json;

use crate::api::AppState;
use crate::config::Config;

/// Headers that belong to one connection and are not forwarded (RFC 9110 §7.6.1).
const HOP_BY_HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// The `dicomweb` addon's service roots.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct Roots {
    /// A node's, with `{node}` for its path.
    pub(crate) node_root: String,
    /// The default node's.
    pub(crate) default_root: String,
}

impl Default for Roots {
    fn default() -> Self {
        Self {
            node_root: "/nodes/{node}/rs".into(),
            default_root: "/rs".into(),
        }
    }
}

impl Roots {
    /// The service root of the node at `path`.
    pub(crate) fn of_node(&self, path: &str) -> String {
        self.node_root.replace("{node}", path)
    }

    /// The paths the proxy forwards everything below: the fixed part of each root (before
    /// `{node}`), without those inside another. The addon answers what it does not serve.
    pub(crate) fn prefixes(&self) -> Vec<String> {
        let fixed = |root: &str| match root.find("/{node}") {
            Some(i) => root[..i].to_owned(),
            None => root.to_owned(),
        };
        let mut prefixes = vec![fixed(&self.node_root), fixed(&self.default_root)];
        prefixes.sort();
        prefixes.dedup();
        let all = prefixes.clone();
        prefixes.retain(|p| {
            !all.iter()
                .any(|other| other != p && p.starts_with(&format!("{other}/")))
        });
        prefixes
    }
}

/// Where the `dicomweb` addon listens, and what the proxy has to strip.
#[derive(Debug)]
pub(crate) struct Upstream {
    addr: SocketAddr,
    user_header: Option<HeaderName>,
    /// Checked by the addon when it was loaded.
    pub(crate) roots: Roots,
    client: reqwest::Client,
}

impl Upstream {
    /// The `dicomweb` addon's listener, if the addon is loaded. An unspecified address (all
    /// interfaces) is reached through loopback.
    pub(crate) fn of(cfg: &Config, loaded: bool) -> Option<Self> {
        if !loaded {
            return None;
        }
        let config = cfg
            .addons
            .entries
            .get("dicomweb")
            .map(|e| e.config.clone())
            .unwrap_or_default();
        let mut addr: SocketAddr = config
            .get("listen")
            .and_then(|v| v.as_str())
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], 8080)));
        if addr.ip().is_unspecified() {
            addr.set_ip(match addr.ip() {
                IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
                IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::LOCALHOST),
            });
        }
        let user_header = config
            .get("user_header")
            .and_then(|v| v.as_str())
            .and_then(|v| HeaderName::try_from(v).ok());
        let defaults = Roots::default();
        let root = |key: &str, default: String| {
            config
                .get(key)
                .and_then(|v| v.as_str())
                .map_or(default, str::to_owned)
        };
        let roots = Roots {
            node_root: root("node_root", defaults.node_root),
            default_root: root("default_root", defaults.default_root),
        };
        Some(Self {
            addr,
            user_header,
            roots,
            client: reqwest::Client::new(),
        })
    }
}

fn error(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        axum::Json(json!({ "error": code, "message": message })),
    )
        .into_response()
}

/// Forwards a `DICOMweb` request to the addon's listener.
pub(crate) async fn forward(State(s): State<AppState>, request: Request) -> Response {
    let Some(upstream) = &s.node.dicomweb else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "dicomweb_disabled",
            "the dicomweb addon is not enabled",
        );
    };
    let (parts, body) = request.into_parts();
    let path = parts
        .uri
        .path_and_query()
        .map_or_else(|| parts.uri.path(), |p| p.as_str());
    let url = format!("http://{}{path}", upstream.addr);
    let mut forwarded = upstream.client.request(parts.method.clone(), url);
    for (name, value) in &parts.headers {
        let strip = HOP_BY_HOP.contains(&name.as_str())
            || upstream.user_header.as_ref() == Some(name)
            || *name == header::HOST;
        if !strip {
            forwarded = forwarded.header(name, value);
        }
    }
    if let Some(host) = parts.headers.get(header::HOST) {
        forwarded = forwarded.header(header::HOST, host);
    }
    let body = reqwest::Body::wrap_stream(body.into_data_stream());
    let answer = match forwarded.body(body).send().await {
        Ok(answer) => answer,
        Err(e) => {
            tracing::warn!(upstream = %upstream.addr, error = %e, "DICOMweb proxy request failed");
            return error(
                StatusCode::BAD_GATEWAY,
                "dicomweb_unreachable",
                "the dicomweb addon did not answer",
            );
        }
    };
    let mut response = Response::builder().status(answer.status());
    for (name, value) in answer.headers() {
        if !HOP_BY_HOP.contains(&name.as_str()) {
            response = response.header(name, value);
        }
    }
    let stream = answer
        .bytes_stream()
        .map_err(|e| std::io::Error::other(e.to_string()));
    response
        .body(Body::from_stream(stream))
        .unwrap_or_else(|e| error(StatusCode::BAD_GATEWAY, "dicomweb_invalid", &e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes_cover_the_roots_without_overlap() {
        let roots = |node: &str, default: &str| Roots {
            node_root: node.into(),
            default_root: default.into(),
        };
        assert_eq!(Roots::default().prefixes(), ["/nodes", "/rs"]);
        assert_eq!(
            roots("/dicom-web/{node}", "/dicom-web").prefixes(),
            ["/dicom-web"]
        );
        assert_eq!(
            roots("/pacs/{node}/rs", "/pacs/default/rs").prefixes(),
            ["/pacs"]
        );
        assert_eq!(roots("/a/b/{node}", "/a").prefixes(), ["/a"]);
    }
}
