//! Server configuration: a TOML file layered with `MEDICUS_*` environment variables.
//!
//! Nested keys use `__` in environment variables, e.g. `MEDICUS_HTTP__LISTEN=0.0.0.0:8042`.
//! Unknown keys are rejected so that typos do not silently fall back to defaults.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use figment::Figment;
use figment::providers::{Env, Format, Serialized, Toml};
use medicus_core::capability::Endpoint;
use medicus_core::capability::index::{DuplicatePolicy, ExtraTag};
use medicus_core::{AeTitle, Partition, Permission, QueryLevel};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    pub http: HttpConfig,
    pub log: LogConfig,
    pub archive: ArchiveConfig,
    pub addons: AddonsConfig,
    /// Remote nodes by name, e.g. `[peers.pacs] protocol = "dimse", ae_title = "PACS", ...`.
    pub peers: BTreeMap<String, Endpoint>,
    pub jobs: JobsConfig,
    /// Partitions (ADR-0003); `default` exists implicitly.
    pub partitions: BTreeMap<Partition, PartitionConfig>,
    /// Nodes by name. Without any, the node `MEDICUS` (AE title `MEDICUS`, `DICOMweb` path
    /// `medicus`) serves the `default` partition.
    pub nodes: BTreeMap<String, NodeConfig>,
    /// Roles: named sets of permissions on nodes.
    pub roles: BTreeMap<String, RoleConfig>,
    /// Users and entities, with how they are recognized and their roles.
    pub principals: BTreeMap<String, PrincipalConfig>,
    pub access: AccessConfig,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PartitionConfig {
    /// Blob store for the partition's new instances (default: `archive.storage`).
    pub storage: Option<String>,
}

/// A node: backed by a local partition or a remote peer, served over DIMSE and/or `DICOMweb`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeConfig {
    /// The local partition behind the node.
    #[serde(default)]
    pub partition: Option<Partition>,
    /// The peer behind a remote node (instead of a partition).
    #[serde(default)]
    pub peer: Option<String>,
    #[serde(default)]
    pub dimse: Option<NodeDimseConfig>,
    #[serde(default)]
    pub dicomweb: Option<NodeDicomwebConfig>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeDimseConfig {
    pub ae_title: AeTitle,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeDicomwebConfig {
    /// Path segment: the node is served at `/nodes/{path}/rs`.
    pub path: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RoleConfig {
    pub grants: Vec<GrantConfig>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantConfig {
    /// Node names; `*` means every node.
    pub nodes: Vec<String>,
    pub permissions: Vec<Permission>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PrincipalConfig {
    /// Recognizes DIMSE peers as this principal.
    pub dimse: Option<DimsePrincipalConfig>,
    pub roles: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DimsePrincipalConfig {
    pub calling_ae: AeTitle,
    /// Networks the peer must connect from (e.g. `10.0.0.0/24`); empty allows any.
    #[serde(default)]
    pub networks: Vec<ipnet::IpNet>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AccessConfig {
    /// Roles of callers that match no principal. If no roles and principals are configured at
    /// all, access is open (everyone may do everything), with a warning at start-up.
    pub anonymous_roles: Vec<String>,
}

/// The job engine (PLAN §3.5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct JobsConfig {
    /// Jobs running at the same time.
    pub concurrency: usize,
    /// Attempts per job before it fails.
    pub max_attempts: u32,
    /// Delay before the first retry; it doubles with every further attempt (up to an hour).
    pub retry_base_secs: u64,
}

impl Default for JobsConfig {
    fn default() -> Self {
        Self {
            concurrency: 4,
            max_attempts: 5,
            retry_base_secs: 30,
        }
    }
}

/// How instances are stored and indexed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ArchiveConfig {
    /// Registered name of the index to use.
    pub index: String,
    /// Registered name of the blob store new instances are written to.
    pub storage: String,
    pub duplicate_policy: DuplicatePolicy,
    /// Largest accepted header (everything before Pixel Data), in bytes.
    pub max_header_bytes: usize,
    /// Attributes indexed in addition to the standard query keys.
    pub extra_tags: Vec<ExtraTagConfig>,
    /// When ingest is refused because storage is (nearly) full.
    pub capacity: CapacityConfig,
}

/// The critical-capacity guard (PLAN §3.10). It works without any eviction policy: when the
/// default store reaches `critical_percent`, new instances are refused until usage drops below
/// `resume_percent`. Retrieval is never affected.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CapacityConfig {
    pub critical_percent: f64,
    pub resume_percent: f64,
    /// Optional quota for the default store; usage is then also measured against it, using the
    /// bytes referenced by the index. Required for stores without a physical limit (S3).
    pub quota_bytes: Option<u64>,
    /// Seconds between two capacity checks.
    pub check_interval_secs: u64,
}

impl Default for CapacityConfig {
    fn default() -> Self {
        Self {
            critical_percent: 95.0,
            resume_percent: 85.0,
            quota_bytes: None,
            check_interval_secs: 10,
        }
    }
}

impl Default for ArchiveConfig {
    fn default() -> Self {
        Self {
            index: "sqlite".into(),
            storage: "local".into(),
            duplicate_policy: DuplicatePolicy::Overwrite,
            max_header_bytes: 64 * 1024 * 1024,
            extra_tags: Vec::new(),
            capacity: CapacityConfig::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtraTagConfig {
    /// Dictionary keyword (`SliceThickness`) or tag (`00180050`, `(0018,0050)`).
    pub tag: String,
    /// The level the attribute belongs to.
    #[serde(default = "default_extra_level")]
    pub level: QueryLevel,
}

fn default_extra_level() -> QueryLevel {
    QueryLevel::Instance
}

impl ExtraTagConfig {
    /// Resolves keyword or tag and the VR through the standard dictionary.
    pub fn resolve(&self) -> Result<ExtraTag, String> {
        use dicom_core::dictionary::{DataDictionary as _, DataDictionaryEntry as _, TagRange};
        use dicom_dictionary_std::StandardDataDictionary;
        let entry = match medicus_core::tag::parse_tag(&self.tag) {
            Ok(tag) => StandardDataDictionary.by_tag(tag),
            Err(_) => StandardDataDictionary.by_name(&self.tag),
        }
        .ok_or_else(|| format!("extra tag {:?} is not in the DICOM dictionary", self.tag))?;
        let TagRange::Single(tag) = entry.tag_range() else {
            return Err(format!(
                "extra tag {:?} is a tag range, not a single attribute",
                self.tag
            ));
        };
        Ok(ExtraTag {
            tag,
            vr: entry.vr().relaxed(),
            level: self.level,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    /// Human-readable name of this node, reported by `/api/v1/system`.
    pub name: String,
    /// Directory for local state (spill files, `SQLite` index by default, ...).
    pub data_dir: PathBuf,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            name: "medicus".into(),
            data_dir: PathBuf::from("data"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HttpConfig {
    pub listen: SocketAddr,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([127, 0, 0, 1], 8042)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    #[default]
    Pretty,
    Json,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LogConfig {
    /// A `tracing` filter directive, e.g. `info,medicus=debug`. `RUST_LOG` takes precedence.
    pub filter: String,
    pub format: LogFormat,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            filter: "info".into(),
            format: LogFormat::Pretty,
        }
    }
}

// No `deny_unknown_fields` here: serde does not support it together with `flatten`. Every other key
// becomes an addon entry, and `AddonEntry` itself rejects unknown fields.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AddonsConfig {
    /// Directories searched for addon manifests (`<name>.addon.toml`).
    pub search_paths: Vec<PathBuf>,
    /// Per-addon settings, keyed by addon name.
    #[serde(flatten)]
    pub entries: BTreeMap<String, AddonEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddonEntry {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Shared library to load. Without it, the addon must be built into the server.
    #[serde(default)]
    pub library: Option<PathBuf>,
    /// Optional SHA-256 (hex) the addon library must match before it is loaded.
    #[serde(default)]
    pub sha256: Option<String>,
    /// Addon-specific configuration, passed to the addon as JSON.
    #[serde(default)]
    pub config: serde_json::Value,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("configuration file {0} does not exist")]
    Missing(PathBuf),
    #[error(transparent)]
    Invalid(#[from] Box<figment::Error>),
}

impl Config {
    /// Loads defaults, then `file` (if given), then `MEDICUS_*` environment variables.
    pub fn load(file: Option<&Path>) -> Result<Self, ConfigError> {
        Self::figment(file)?
            .extract()
            .map_err(|e| ConfigError::Invalid(Box::new(e)))
    }

    fn figment(file: Option<&Path>) -> Result<Figment, ConfigError> {
        let mut fig = Figment::from(Serialized::defaults(Self::default()));
        if let Some(path) = file {
            if !path.exists() {
                return Err(ConfigError::Missing(path.to_owned()));
            }
            fig = fig.merge(Toml::file_exact(path));
        }
        Ok(fig.merge(Env::prefixed("MEDICUS_").split("__")))
    }
}

#[cfg(test)]
#[allow(clippy::result_large_err)] // figment::Jail's closure signature
mod tests {
    use super::*;
    use std::io::Write;

    fn write(contents: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::Builder::new().suffix(".toml").tempfile().unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        f
    }

    #[test]
    fn defaults_without_file() {
        figment::Jail::expect_with(|_| {
            assert_eq!(Config::load(None).unwrap(), Config::default());
            Ok(())
        });
    }

    #[test]
    fn file_and_env_layering() {
        let f = write(
            r#"
            [server]
            name = "node-a"
            [http]
            listen = "0.0.0.0:9000"
            [addons]
            search_paths = ["/opt/medicus/addons"]
            [addons.storage-fs]
            config = { root = "/var/lib/medicus" }
            [addons.dicomweb]
            enabled = false
            "#,
        );
        figment::Jail::expect_with(|jail| {
            jail.set_env("MEDICUS_HTTP__LISTEN", "127.0.0.1:9100");
            let c = Config::load(Some(f.path())).unwrap();
            assert_eq!(c.server.name, "node-a");
            assert_eq!(c.http.listen.port(), 9100);
            let fs = &c.addons.entries["storage-fs"];
            assert!(fs.enabled);
            assert_eq!(fs.config["root"], "/var/lib/medicus");
            assert!(!c.addons.entries["dicomweb"].enabled);
            Ok(())
        });
    }

    #[test]
    fn rejects_unknown_keys_and_missing_files() {
        let f = write("[http]\nlisten = \"127.0.0.1:1\"\nlistne = 1\n");
        assert!(matches!(
            Config::load(Some(f.path())),
            Err(ConfigError::Invalid(_))
        ));
        assert!(matches!(
            Config::load(Some(Path::new("/nonexistent/medicus.toml"))),
            Err(ConfigError::Missing(_))
        ));
    }
}
