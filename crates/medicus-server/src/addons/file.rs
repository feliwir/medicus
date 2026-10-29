//! Addon manifest files: `<name>.addon.toml` next to an addon library, in the configured
//! `addons.search_paths` (ADR-0004 §2).
//!
//! A manifest file describes an addon without loading it: loading a native library runs its
//! static initializers, so discovering what addons exist — for the admin UI or an extension
//! browser — must not require that. The file repeats the library's embedded manifest and names
//! the library (and optionally its SHA-256). The embedded manifest stays authoritative: when the
//! addon is loaded, both must agree.
//!
//! ```toml
//! name = "dimse"
//! version = "0.1.0"
//! target = "x86_64-linux"
//! library = "libmedicus_addon_dimse.so"
//! sha256 = "…"
//! provides = ["service", "remote_node"]
//!
//! [[requires]]
//! capability = "index_store"
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use medicus_core::addon::{AddonManifest, Capability, Requirement};
use serde::{Deserialize, Serialize};

/// File name suffix of manifest files.
pub(crate) const SUFFIX: &str = ".addon.toml";

/// The contents of a manifest file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ManifestFile {
    pub(crate) name: String,
    pub(crate) version: String,
    pub(crate) target: String,
    /// The library, relative to the file's directory or absolute.
    pub(crate) library: PathBuf,
    /// SHA-256 (hex) the library must match before it is loaded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) sha256: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) required: bool,
    #[serde(default)]
    pub(crate) provides: Vec<Capability>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) requires: Vec<Requirement>,
    /// The configuration's JSON Schema, as JSON text (TOML has no `null`, which schemas use).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) config_schema: Option<String>,
}

impl ManifestFile {
    pub(crate) fn new(manifest: AddonManifest, library: PathBuf, sha256: Option<String>) -> Self {
        Self {
            name: manifest.name,
            version: manifest.version,
            target: manifest.target,
            library,
            sha256,
            required: manifest.required,
            provides: manifest.provides,
            requires: manifest.requires,
            config_schema: manifest
                .config_schema
                .map(|schema| serde_json::to_string_pretty(&schema).unwrap_or_default()),
        }
    }

    pub(crate) fn manifest(&self) -> anyhow::Result<AddonManifest> {
        Ok(AddonManifest {
            name: self.name.clone(),
            version: self.version.clone(),
            target: self.target.clone(),
            provides: self.provides.clone(),
            requires: self.requires.clone(),
            required: self.required,
            config_schema: self
                .config_schema
                .as_deref()
                .map(serde_json::from_str)
                .transpose()
                .context("`config_schema` is not valid JSON")?,
        })
    }

    pub(crate) fn to_toml(&self) -> anyhow::Result<String> {
        toml::to_string_pretty(self).context("serializing the addon manifest")
    }
}

/// An addon found through its manifest file.
#[derive(Debug, Clone)]
pub(crate) struct Discovered {
    pub(crate) manifest: AddonManifest,
    /// The library, resolved against the file's directory.
    pub(crate) library: PathBuf,
    pub(crate) sha256: Option<String>,
    /// The manifest file, for messages.
    pub(crate) file: PathBuf,
}

/// Reads the manifest file at `path`.
pub(crate) fn read(path: &Path) -> anyhow::Result<Discovered> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let file: ManifestFile = toml::from_str(&text)
        .with_context(|| format!("invalid addon manifest {}", path.display()))?;
    let stem = path
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_suffix(SUFFIX))
        .unwrap_or_default();
    if stem != file.name {
        bail!(
            "addon manifest {} describes {:?}; the file must be named {:?}",
            path.display(),
            file.name,
            format!("{}{SUFFIX}", file.name)
        );
    }
    let library = path
        .parent()
        .map_or_else(|| file.library.clone(), |dir| dir.join(&file.library));
    Ok(Discovered {
        manifest: file
            .manifest()
            .with_context(|| format!("invalid addon manifest {}", path.display()))?,
        library,
        sha256: file.sha256,
        file: path.to_owned(),
    })
}

/// The addons described by manifest files in `search_paths` that are built for `target`; files
/// for other targets are skipped.
pub(crate) fn discover(
    search_paths: &[PathBuf],
    target: &str,
) -> anyhow::Result<BTreeMap<String, Discovered>> {
    let mut found: BTreeMap<String, Discovered> = BTreeMap::new();
    for dir in search_paths {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) => {
                tracing::warn!(dir = %dir.display(), error = %e, "cannot read addon search path");
                continue;
            }
        };
        let mut files: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.ends_with(SUFFIX))
            })
            .collect();
        files.sort();
        for path in files {
            let addon = read(&path)?;
            if addon.manifest.target != target {
                tracing::debug!(file = %path.display(), target = %addon.manifest.target, "addon built for another target; skipped");
                continue;
            }
            if let Some(first) = found.get(&addon.manifest.name) {
                bail!(
                    "addon {:?} is described twice: {} and {}",
                    addon.manifest.name,
                    first.file.display(),
                    path.display()
                );
            }
            found.insert(addon.manifest.name.clone(), addon);
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dimse() -> AddonManifest {
        AddonManifest::new("dimse", "0.1.0")
            .provides([Capability::Service, Capability::RemoteNode])
            .requires([Capability::IndexStore, Capability::BlobStore])
    }

    #[test]
    fn roundtrip_and_discovery() {
        let dir = tempfile::tempdir().unwrap();
        let file = ManifestFile::new(dimse(), "libdimse.so".into(), Some("ab".into()));
        let text = file.to_toml().unwrap();
        assert!(text.contains("library = \"libdimse.so\""), "{text}");
        assert!(text.contains("[[requires]]"), "{text}");
        std::fs::write(dir.path().join("dimse.addon.toml"), &text).unwrap();
        std::fs::write(dir.path().join("unrelated.toml"), "x = 1").unwrap();

        let found = discover(&[dir.path().to_owned()], &dimse().target).unwrap();
        let d = &found["dimse"];
        assert_eq!(d.manifest, dimse());
        assert_eq!(d.library, dir.path().join("libdimse.so"));
        assert_eq!(d.sha256.as_deref(), Some("ab"));
        // Other targets are skipped, missing directories are tolerated.
        let none = discover(
            &[dir.path().to_owned(), dir.path().join("missing")],
            "sparc-plan9",
        )
        .unwrap();
        assert!(none.is_empty());
    }

    #[test]
    fn invalid_files_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        let text = ManifestFile::new(dimse(), "lib.so".into(), None)
            .to_toml()
            .unwrap();
        // The file name must match the addon name.
        std::fs::write(dir.path().join("other.addon.toml"), &text).unwrap();
        let err = discover(&[dir.path().to_owned()], &dimse().target).unwrap_err();
        assert!(err.to_string().contains("must be named"), "{err}");

        // Unknown keys are typos, not extensions.
        std::fs::remove_file(dir.path().join("other.addon.toml")).unwrap();
        std::fs::write(
            dir.path().join("dimse.addon.toml"),
            format!("provide = []\n{text}"),
        )
        .unwrap();
        assert!(discover(&[dir.path().to_owned()], &dimse().target).is_err());
    }
}
