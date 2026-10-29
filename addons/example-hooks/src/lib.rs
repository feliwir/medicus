//! Reference addon: the smallest useful addon. It logs every event it receives.
//!
//! ```toml
//! [addons.example-hooks]
//! config = { kinds = ["instance.stored", "study.stable"] }   # optional; default: all events
//! ```

use medicus_addon_sdk::{AddonManifest, Capability, Registrar, async_trait, export_addon};
use medicus_core::Result;
use medicus_core::capability::{EventFilter, EventHook};
use medicus_core::event::{Event, EventKind};
use serde::Deserialize;

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
struct Config {
    #[schemars(with = "Vec<String>")]
    kinds: Vec<EventKind>,
}

#[derive(Debug)]
struct LogEvents {
    filter: EventFilter,
}

#[async_trait]
impl EventHook for LogEvents {
    fn filter(&self) -> EventFilter {
        self.filter.clone()
    }

    async fn on_event(&self, event: &Event) -> Result<()> {
        let resource = event.resource.as_ref();
        tracing::info!(
            seq = event.seq,
            kind = %event.kind,
            study = ?resource.and_then(|r| r.study.as_ref()).map(ToString::to_string),
            instance = ?resource.and_then(|r| r.instance.as_ref()).map(ToString::to_string),
            "event"
        );
        Ok(())
    }
}

fn init(reg: &mut Registrar) -> Result<()> {
    let cfg: Option<Config> = reg.config()?;
    let kinds = cfg.unwrap_or_default().kinds;
    reg.register_event_hook(
        "log-events",
        LogEvents {
            filter: EventFilter { kinds },
        },
    )
}

export_addon!(
    manifest = AddonManifest::new("example-hooks", env!("CARGO_PKG_VERSION"))
        .provides([Capability::EventHook])
        .config_schema(medicus_addon_sdk::config_schema::<Config>()),
    init = init
);
