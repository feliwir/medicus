//! `events-webhook`: delivers events to HTTP endpoints.
//!
//! ```toml
//! [addons.events-webhook]
//! config = { hooks = [
//!   { name = "ris", url = "https://ris.example.org/medicus", kinds = ["instance.stored"],
//!     partitions = ["tenant-a"], secret = "...", headers = { Authorization = "Bearer ..." } },
//! ] }
//! ```
//!
//! Every hook is a durable subscriber of its own: the core delivers its events in order and at
//! least once, retrying with backoff while the endpoint fails. Each event is sent (POST) as its JSON
//! envelope with an `Idempotency-Key` (the event id), so receivers can drop redeliveries. With a
//! `secret`, `X-Medicus-Signature: sha256=<hex HMAC-SHA256 of the body>` lets them verify the
//! sender.
//!
//! Responses: 2xx is delivered; 408, 429 and 5xx (and network errors or timeouts) are retried;
//! any other status means the endpoint will never accept the event, so it is logged and skipped
//! rather than blocking all later events.

use std::collections::BTreeMap;
use std::time::Duration;

use hmac::{Hmac, KeyInit as _, Mac as _};
use medicus_addon_sdk::{AddonManifest, Capability, Registrar, async_trait, static_addon};
use medicus_core::capability::{EventFilter, EventHook};
use medicus_core::event::{Event, EventKind};
use medicus_core::{Error, ErrorKind, Partition, Result};
use serde::Deserialize;
use sha2::Sha256;

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub hooks: Vec<HookConfig>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HookConfig {
    /// Subscriber name; delivery progress is kept per name.
    pub name: String,
    pub url: String,
    /// Event kinds to deliver; empty: all.
    #[serde(default)]
    #[schemars(with = "Vec<String>")]
    pub kinds: Vec<EventKind>,
    /// Partitions whose events to deliver; empty: all (including node-wide events).
    #[serde(default)]
    #[schemars(with = "Vec<String>")]
    pub partitions: Vec<Partition>,
    /// Shared secret for the `X-Medicus-Signature` header.
    #[serde(default)]
    pub secret: Option<String>,
    /// Extra request headers (e.g. `Authorization`).
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

fn default_timeout() -> u64 {
    10
}

#[derive(Debug)]
struct Webhook {
    cfg: HookConfig,
    client: reqwest::Client,
}

/// `sha256=<hex>` of the HMAC-SHA256 of `body` under `secret`.
fn signature(secret: &str, body: &[u8]) -> String {
    #[allow(clippy::expect_used)] // HMAC accepts keys of any length
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("any key length");
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

#[async_trait]
impl EventHook for Webhook {
    fn filter(&self) -> EventFilter {
        EventFilter {
            kinds: self.cfg.kinds.clone(),
        }
    }

    async fn on_event(&self, event: &Event) -> Result<()> {
        if !self.cfg.partitions.is_empty()
            && !event
                .partition
                .as_ref()
                .is_some_and(|p| self.cfg.partitions.contains(p))
        {
            return Ok(());
        }
        let body = serde_json::to_vec(event).map_err(|e| Error::internal(e.to_string()))?;
        let mut request = self
            .client
            .post(&self.cfg.url)
            .header("content-type", "application/json")
            .header("idempotency-key", event.id.to_string())
            .header("x-medicus-event", event.kind.as_str());
        for (name, value) in &self.cfg.headers {
            request = request.header(name, value);
        }
        if let Some(secret) = &self.cfg.secret {
            request = request.header("x-medicus-signature", signature(secret, &body));
        }
        let response = request.body(body).send().await.map_err(|e| {
            Error::new(
                ErrorKind::Unavailable,
                format!("webhook {}: {e}", self.cfg.name),
            )
        })?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        if status.is_server_error() || status.as_u16() == 408 || status.as_u16() == 429 {
            return Err(Error::new(
                ErrorKind::Unavailable,
                format!("webhook {} answered {status}", self.cfg.name),
            ));
        }
        tracing::error!(hook = %self.cfg.name, seq = event.seq, kind = %event.kind, %status, "webhook refused the event; skipping it");
        Ok(())
    }
}

pub fn init(reg: &mut Registrar) -> Result<()> {
    let cfg: Config = reg.config()?;
    for hook in cfg.hooks {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(hook.timeout_secs.max(1)))
            .build()
            .map_err(|e| Error::internal(format!("webhook {}: {e}", hook.name)))?;
        tracing::info!(hook = %hook.name, url = %hook.url, "webhook configured");
        let name = hook.name.clone();
        reg.register_event_hook(&name, Webhook { cfg: hook, client })?;
    }
    Ok(())
}

/// What this addon provides and needs (ADR-0004 §2).
pub fn manifest() -> AddonManifest {
    AddonManifest::new("events-webhook", env!("CARGO_PKG_VERSION"))
        .provides([Capability::EventHook])
        .requires([Capability::IndexStore])
        .config_schema(medicus_addon_sdk::config_schema::<Config>())
}

/// Entry points for linking this addon into the server binary.
pub static ADDON: medicus_addon_sdk::abi::StaticAddon =
    static_addon!(manifest = manifest(), init = init);

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use medicus_core::event::Origin;
    use std::sync::{Arc, Mutex};

    type Seen = Arc<Mutex<Vec<(String, Option<String>, String)>>>;

    /// An endpoint answering with `status`, recording what it received.
    async fn endpoint(status: u16) -> (String, Seen) {
        use axum::http::HeaderMap;
        let seen: Seen = Arc::default();
        let record = seen.clone();
        let app = axum::Router::new().route(
            "/hook",
            axum::routing::post(move |headers: HeaderMap, body: String| {
                let record = record.clone();
                async move {
                    let header = |n: &str| headers.get(n).map(|v| v.to_str().unwrap().to_owned());
                    record.lock().unwrap().push((
                        header("idempotency-key").unwrap(),
                        header("x-medicus-signature"),
                        body,
                    ));
                    axum::http::StatusCode::from_u16(status).unwrap()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/hook", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (url, seen)
    }

    fn hook(url: String, partitions: Vec<Partition>) -> Webhook {
        Webhook {
            cfg: HookConfig {
                name: "test".into(),
                url,
                kinds: Vec::new(),
                partitions,
                secret: Some("s3cret".into()),
                headers: BTreeMap::new(),
                timeout_secs: 5,
            },
            client: reqwest::Client::new(),
        }
    }

    fn event() -> Event {
        Event::new(
            EventKind::InstanceStored,
            Origin::Internal {
                component: "test".into(),
            },
        )
        .with_partition(Partition::parse("tenant-a").unwrap())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn delivers_signed_events() {
        let (url, seen) = endpoint(204).await;
        let e = event();
        hook(url, Vec::new()).on_event(&e).await.unwrap();
        let seen = seen.lock().unwrap();
        let (key, signature_header, body) = &seen[0];
        assert_eq!(key, &e.id.to_string());
        assert_eq!(
            signature_header.as_deref(),
            Some(signature("s3cret", body.as_bytes()).as_str())
        );
        let back: Event = serde_json::from_str(body).unwrap();
        assert_eq!(back.id, e.id);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn retries_server_errors_and_skips_refusals() {
        let (url, _) = endpoint(503).await;
        let err = hook(url, Vec::new()).on_event(&event()).await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::Unavailable);
        let (url, seen) = endpoint(400).await;
        hook(url, Vec::new()).on_event(&event()).await.unwrap();
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn filters_by_partition() {
        let (url, seen) = endpoint(200).await;
        let other = vec![Partition::parse("tenant-b").unwrap()];
        hook(url, other).on_event(&event()).await.unwrap();
        assert!(seen.lock().unwrap().is_empty());
    }
}
