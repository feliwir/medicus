//! `admin-ui`: the administration and viewing interface (ADR-0004 §1).
//!
//! The single-page application in this addon's `ui/` directory is built with Vite and embedded at compile time (see
//! `build.rs`). The addon serves it under `/` through `HttpRoutes`: files by path, and
//! `index.html` for every other path that does not name a file type, so the application's own
//! routes work on reload (including those that end in a DICOM UID, which contains dots). The application talks to the core's admin API and `DICOMweb` proxy on the same
//! origin. Without an embedded build, a page explains how to build it.
//!
//! The addon is required: it cannot be disabled, and it is the one functional addon of a fresh
//! install.

use bytes::Bytes;
use medicus_addon_sdk::{AddonManifest, Capability, Registrar, async_trait, static_addon};
use medicus_core::Result;
use medicus_core::capability::{HttpRequest, HttpResponse, HttpRoutes};

mod assets {
    include!(concat!(env!("OUT_DIR"), "/assets.rs"));
}

/// Shown when the server was built without the UI.
const PLACEHOLDER: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>medicus</title>
<meta name="viewport" content="width=device-width, initial-scale=1">
<style>body{font:15px/1.5 system-ui,sans-serif;max-width:40rem;margin:4rem auto;padding:0 1rem;color:#1f2937}
code{background:#f3f4f6;padding:.1rem .3rem;border-radius:.25rem}</style></head>
<body><h1>medicus</h1>
<p>This server was built without the admin UI. Build it and the server again:</p>
<p><code>cd addons/admin-ui/ui &amp;&amp; npm ci &amp;&amp; npm run build</code>, then <code>cargo build</code>.</p>
<p>The admin API is available at <code>/api/v1</code>.</p></body></html>
"#;

/// The policy for the application's pages: everything from this origin; images also as data and
/// blob URLs (thumbnails, rendered frames); inline styles for the component library.
const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; img-src 'self' data: blob:; \
     style-src 'self' 'unsafe-inline'; worker-src 'self' blob:; frame-ancestors 'none'; \
     base-uri 'self'; form-action 'self'";

/// The content type of a file served by its extension; `None` for other paths.
fn file_type(path: &str) -> Option<&'static str> {
    Some(match path.rsplit_once('.').map(|(_, ext)| ext) {
        Some("html") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json" | "map") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        Some("wasm") => "application/wasm",
        Some("txt") => "text/plain; charset=utf-8",
        _ => return None,
    })
}

fn asset(path: &str) -> Option<&'static [u8]> {
    assets::ASSETS
        .binary_search_by(|(name, _)| (*name).cmp(path))
        .ok()
        .map(|i| assets::ASSETS[i].1)
}

/// The response for a page: the application (or the placeholder) with its security headers.
fn page(body: Bytes) -> HttpResponse {
    HttpResponse::new(200, body)
        .header("content-type", "text/html; charset=utf-8")
        .header("cache-control", "no-cache")
        .header("content-security-policy", CONTENT_SECURITY_POLICY)
        .header("x-content-type-options", "nosniff")
        .header("referrer-policy", "same-origin")
}

struct Ui;

impl Ui {
    fn serve(path: &str) -> HttpResponse {
        let file = path.trim_start_matches('/');
        if assets::ASSETS.is_empty() {
            return page(Bytes::from_static(PLACEHOLDER.as_bytes()));
        }
        if let Some(bytes) = asset(file).filter(|_| !file.is_empty()) {
            // Vite fingerprints everything below `assets/`: those never change.
            let cache = if file.starts_with("assets/") {
                "public, max-age=31536000, immutable"
            } else {
                "no-cache"
            };
            return HttpResponse::new(200, Bytes::from_static(bytes))
                .header(
                    "content-type",
                    file_type(file).unwrap_or("application/octet-stream"),
                )
                .header("cache-control", cache)
                .header("x-content-type-options", "nosniff");
        }
        // A route of the application, or the start page; a missing file is not one.
        if file_type(file).is_none()
            && let Some(index) = asset("index.html")
        {
            return page(Bytes::from_static(index));
        }
        HttpResponse::new(404, "not found").header("content-type", "text/plain; charset=utf-8")
    }
}

#[async_trait]
impl HttpRoutes for Ui {
    async fn handle(&self, request: HttpRequest, _body: Bytes) -> Result<HttpResponse> {
        if !matches!(request.method.as_str(), "GET" | "HEAD") {
            return Ok(HttpResponse::new(405, "method not allowed").header("allow", "GET, HEAD"));
        }
        Ok(Self::serve(&request.path))
    }
}

pub fn init(reg: &mut Registrar) -> Result<()> {
    reg.register_http_routes("/", Ui)?;
    if assets::ASSETS.is_empty() {
        medicus_addon_sdk::tracing::warn!("built without the admin UI: serving a placeholder page");
    } else {
        medicus_addon_sdk::tracing::info!(files = assets::ASSETS.len(), "admin UI ready");
    }
    Ok(())
}

/// What this addon provides and needs (ADR-0004 §2): it stores its users and settings through the
/// core, which needs an index.
pub fn manifest() -> AddonManifest {
    AddonManifest::new("admin-ui", env!("CARGO_PKG_VERSION"))
        .provides([Capability::HttpRoutes])
        .requires([Capability::IndexStore])
        .required()
}

/// Entry points for linking this addon into the server binary.
pub static ADDON: medicus_addon_sdk::abi::StaticAddon =
    static_addon!(manifest = manifest(), init = init);

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt as _;

    async fn get(path: &str) -> (u16, Vec<(String, String)>, Vec<u8>) {
        let request = HttpRequest {
            method: "GET".into(),
            path: path.into(),
            query: None,
            headers: Vec::new(),
            remote_addr: String::new(),
            principal: None,
        };
        let response = Ui.handle(request, Bytes::new()).await.unwrap();
        let mut body = Vec::new();
        let mut stream = response.body;
        while let Some(chunk) = stream.next().await {
            body.extend_from_slice(&chunk.unwrap());
        }
        (response.head.status, response.head.headers, body)
    }

    #[tokio::test]
    async fn pages_and_files() {
        let (status, headers, body) = get("/").await;
        assert_eq!(status, 200);
        assert!(headers.iter().any(|(n, _)| n == "content-security-policy"));
        assert!(String::from_utf8_lossy(&body).contains("<html"));
        // Application routes get the page as well; files that do not exist do not.
        assert_eq!(get("/config/nodes").await.0, 200);
        assert_eq!(get("/browse/archive/1.2.840.113619.2.55").await.0, 200);
        if !assets::ASSETS.is_empty() {
            assert_eq!(get("/missing.js").await.0, 404);
            let (name, bytes) = assets::ASSETS[0];
            let (status, _, body) = get(&format!("/{name}")).await;
            assert_eq!((status, body.as_slice()), (200, bytes));
        }
    }

    #[test]
    fn manifest_is_required() {
        let m = manifest();
        assert!(m.required);
        assert!(m.provides_capability(Capability::HttpRoutes));
    }
}
