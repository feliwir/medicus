//! HTTP routes of addons (`HttpRoutes` v1, ADR-0004 §2): an addon registers routes under a path
//! prefix, and the core mounts them into its own HTTP server, next to the admin API.
//!
//! Version 1 is meant for user interfaces and small APIs: the core buffers the request body (up
//! to a limit) and hands it over whole; the response body is streamed. The core resolves the
//! server's own credentials (session cookie, API token) and passes the user as `principal`; it
//! does not refuse requests without them, so the routes decide what is public.

use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::capability::ByteStream;
use crate::error::Result;

/// The largest request body the core hands to `HttpRoutes`; larger requests are refused with 413.
pub const MAX_REQUEST_BODY: usize = 16 * 1024 * 1024;

/// A request, without its body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpRequest {
    pub method: String,
    /// The full path, including the routes' prefix (e.g. `/assets/app.js`).
    pub path: String,
    /// The query string without `?`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// Header names in lowercase, in the order received.
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    pub remote_addr: String,
    /// The user the request's session cookie or API token belongs to, if valid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal: Option<String>,
}

impl HttpRequest {
    /// The first value of a header (name in lowercase).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// A response's status and headers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpResponseHead {
    pub status: u16,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
}

pub struct HttpResponse {
    pub head: HttpResponseHead,
    pub body: ByteStream,
}

impl std::fmt::Debug for HttpResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpResponse")
            .field("head", &self.head)
            .finish_non_exhaustive()
    }
}

impl HttpResponse {
    /// A response with a body that is already in memory.
    pub fn new(status: u16, body: impl Into<Bytes>) -> Self {
        let body: Bytes = body.into();
        Self {
            head: HttpResponseHead {
                status,
                headers: Vec::new(),
            },
            body: Box::pin(once_stream(body)),
        }
    }

    #[must_use]
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.head.headers.push((name.into(), value.into()));
        self
    }
}

/// A stream of one chunk (none if it is empty).
fn once_stream(body: Bytes) -> impl futures_core::Stream<Item = Result<Bytes>> + Send {
    struct Once(Option<Bytes>);
    impl futures_core::Stream for Once {
        type Item = Result<Bytes>;
        fn poll_next(
            mut self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            std::task::Poll::Ready(self.0.take().filter(|b| !b.is_empty()).map(Ok))
        }
    }
    Once(Some(body))
}

/// Routes under a path prefix (the name the capability is registered under, e.g. `/` or
/// `/viewer`). A request goes to the routes with the longest prefix that matches whole path
/// segments; the core's own paths (`/api`, `/health`, `/rs`, `/nodes`) are never routed to addons.
#[async_trait]
pub trait HttpRoutes: Send + Sync + 'static {
    async fn handle(&self, request: HttpRequest, body: Bytes) -> Result<HttpResponse>;
}
