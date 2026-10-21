//! Forwards the addon's `tracing` events to the host log.

use std::fmt::{self, Write as _};

use medicus_abi::ffi::{MD_LOG_DEBUG, MD_LOG_ERROR, MD_LOG_INFO, MD_LOG_TRACE, MD_LOG_WARN, MdStr};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt as _};

use crate::registrar::HostRef;

/// Installs the forwarding subscriber as the addon's global default.
///
/// A dynamically loaded addon has its own copy of `tracing`'s globals, which are unset at this
/// point. A statically linked addon shares the host's globals; if the host already installed a
/// subscriber, events reach it directly and nothing is installed here.
pub(crate) fn install(host: HostRef) {
    if tracing::dispatcher::has_been_set() {
        return;
    }
    let subscriber = tracing_subscriber::registry().with(HostLayer { host });
    let _ = tracing::subscriber::set_global_default(subscriber);
}

thread_local! {
    /// Set while an event is being forwarded. With a statically linked addon and no host
    /// subscriber, the host's own log output would otherwise come back here, recursing forever.
    static FORWARDING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

struct HostLayer {
    host: HostRef,
}

#[derive(Default)]
struct Message(String);

impl Visit for Message {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.0, "{value:?}");
        } else {
            let _ = write!(self.0, " {}={value:?}", field.name());
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.0.push_str(value);
        } else {
            let _ = write!(self.0, " {}={value}", field.name());
        }
    }
}

impl<S: Subscriber> Layer<S> for HostLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        let level = match *meta.level() {
            Level::ERROR => MD_LOG_ERROR,
            Level::WARN => MD_LOG_WARN,
            Level::INFO => MD_LOG_INFO,
            Level::DEBUG => MD_LOG_DEBUG,
            Level::TRACE => MD_LOG_TRACE,
        };
        if FORWARDING.with(|f| f.replace(true)) {
            return;
        }
        let mut msg = Message::default();
        event.record(&mut msg);
        self.host.log(level, meta.target(), &msg.0);
        FORWARDING.with(|f| f.set(false));
    }
}

impl HostRef {
    pub(crate) fn log(self, level: u32, target: &str, message: &str) {
        let api = self.api();
        // SAFETY: the host API is valid for the process lifetime; strings are borrowed for the
        // duration of the call only.
        unsafe { (api.log)(api.ctx, level, MdStr::new(target), MdStr::new(message)) };
    }
}
