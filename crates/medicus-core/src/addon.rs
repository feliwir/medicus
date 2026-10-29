//! What an addon declares about itself before it is initialized (ADR-0004 §2): the manifest.
//!
//! The host reads the manifest of every candidate addon first, resolves which addons to load (an
//! enabled addon pulls in providers of the capabilities it requires), and only then initializes
//! them, providers before the addons that need them.

use serde::{Deserialize, Serialize};

/// A kind of capability an addon registers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    BlobStore,
    EventHook,
    IndexStore,
    Service,
    RemoteNode,
    HttpRoutes,
}

impl Capability {
    pub const ALL: [Self; 6] = [
        Self::BlobStore,
        Self::EventHook,
        Self::IndexStore,
        Self::Service,
        Self::RemoteNode,
        Self::HttpRoutes,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::BlobStore => "blob_store",
            Self::EventHook => "event_hook",
            Self::IndexStore => "index_store",
            Self::Service => "service",
            Self::RemoteNode => "remote_node",
            Self::HttpRoutes => "http_routes",
        }
    }
}

impl std::fmt::Display for Capability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A capability an addon needs, optionally from a specific addon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Requirement {
    pub capability: Capability,
    /// The addon that must provide it; `None` accepts any provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub addon: Option<String>,
}

impl From<Capability> for Requirement {
    fn from(capability: Capability) -> Self {
        Self {
            capability,
            addon: None,
        }
    }
}

/// An addon's manifest, exported as JSON by `medicus_addon_manifest`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddonManifest {
    pub name: String,
    pub version: String,
    /// `<arch>-<os>`, compared against the host's.
    pub target: String,
    /// The capability kinds the addon registers; registering any other kind is refused.
    #[serde(default)]
    pub provides: Vec<Capability>,
    #[serde(default)]
    pub requires: Vec<Requirement>,
    /// A required addon is always loaded and cannot be disabled.
    #[serde(default)]
    pub required: bool,
    /// JSON Schema of the addon's configuration (`[addons.<name>] config`), for configuration
    /// forms and checks; `None` if the addon does not describe it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<serde_json::Value>,
}

impl AddonManifest {
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
            target: format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS),
            provides: Vec::new(),
            requires: Vec::new(),
            required: false,
            config_schema: None,
        }
    }

    #[must_use]
    pub fn provides(mut self, capabilities: impl IntoIterator<Item = Capability>) -> Self {
        self.provides.extend(capabilities);
        self
    }

    #[must_use]
    pub fn requires(
        mut self,
        requirements: impl IntoIterator<Item = impl Into<Requirement>>,
    ) -> Self {
        self.requires
            .extend(requirements.into_iter().map(Into::into));
        self
    }

    #[must_use]
    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }

    /// Describes the configuration with a JSON Schema (the SDK's `config_schema` derives one).
    #[must_use]
    pub fn config_schema(mut self, schema: serde_json::Value) -> Self {
        self.config_schema = Some(schema);
        self
    }

    pub fn provides_capability(&self, capability: Capability) -> bool {
        self.provides.contains(&capability)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_representation() {
        let m = AddonManifest::new("dimse", "0.1.0")
            .provides([Capability::Service, Capability::RemoteNode])
            .requires([Capability::IndexStore]);
        let json = serde_json::to_value(&m).unwrap();
        assert_eq!(
            json["provides"],
            serde_json::json!(["service", "remote_node"])
        );
        assert_eq!(
            json["requires"],
            serde_json::json!([{"capability": "index_store"}])
        );
        assert_eq!(json["required"], false);
        // Minimal manifests (e.g. hand-written in C) need only name, version and target.
        let minimal: AddonManifest =
            serde_json::from_str(r#"{"name":"x","version":"1","target":"t"}"#).unwrap();
        assert!(minimal.provides.is_empty() && !minimal.required);
    }
}
