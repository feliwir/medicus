//! Server configuration: a TOML file layered with `MEDICUS_*` environment variables.
//!
//! Nested keys use `__` in environment variables, e.g. `MEDICUS_HTTP__LISTEN=0.0.0.0:8042`.
//! Unknown keys are rejected so that typos do not silently fall back to defaults.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use figment::Figment;
use figment::providers::{Env, Format, Serialized, Toml};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Config {
    pub(crate) server: ServerConfig,
    pub(crate) http: HttpConfig,
    pub(crate) log: LogConfig,
    pub(crate) addons: AddonsConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ServerConfig {
    /// Human-readable name of this node, reported by `/api/v1/system`.
    pub(crate) name: String,
    /// Directory for local state (spill files, `SQLite` index by default, ...).
    pub(crate) data_dir: PathBuf,
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
pub(crate) struct HttpConfig {
    pub(crate) listen: SocketAddr,
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
pub(crate) enum LogFormat {
    #[default]
    Pretty,
    Json,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct LogConfig {
    /// A `tracing` filter directive, e.g. `info,medicus=debug`. `RUST_LOG` takes precedence.
    pub(crate) filter: String,
    pub(crate) format: LogFormat,
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
pub(crate) struct AddonsConfig {
    /// Directories searched for addon manifests (`<name>.addon.toml`).
    pub(crate) search_paths: Vec<PathBuf>,
    /// Per-addon settings, keyed by addon name.
    #[serde(flatten)]
    pub(crate) entries: BTreeMap<String, AddonEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AddonEntry {
    #[serde(default = "default_true")]
    pub(crate) enabled: bool,
    /// Optional SHA-256 (hex) the addon library must match before it is loaded.
    #[serde(default)]
    pub sha256: Option<String>,
    /// Addon-specific configuration, passed to the addon as JSON.
    #[serde(default)]
    pub(crate) config: serde_json::Value,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ConfigError {
    #[error("configuration file {0} does not exist")]
    Missing(PathBuf),
    #[error(transparent)]
    Invalid(#[from] Box<figment::Error>),
}

impl Config {
    /// Loads defaults, then `file` (if given), then `MEDICUS_*` environment variables.
    pub(crate) fn load(file: Option<&Path>) -> Result<Self, ConfigError> {
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
