//! Embeds the built admin UI (`ui/dist`, or `MEDICUS_UI_DIST`) into the addon: generates a table of
//! `(path, bytes)` with one `include_bytes!` per file. Without a build, the table is empty and the
//! addon serves a placeholder page, so building the server never requires Node.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

fn files(dir: &Path, base: &Path, out: &mut Vec<(String, PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files(&path, base, out);
        } else if let Ok(relative) = path.strip_prefix(base) {
            let name = relative
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            out.push((name, path));
        }
    }
}

fn main() {
    println!("cargo::rerun-if-env-changed=MEDICUS_UI_DIST");
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    let dist = std::env::var_os("MEDICUS_UI_DIST")
        .map_or_else(|| manifest_dir.join("ui/dist"), PathBuf::from);
    // A directory makes cargo rebuild when anything below it changes.
    println!("cargo::rerun-if-changed={}", dist.display());
    let mut found = Vec::new();
    if dist.join("index.html").is_file() {
        files(&dist, &dist, &mut found);
    } else {
        println!(
            "cargo::warning=admin UI not built ({} has no index.html): serving a placeholder",
            dist.display()
        );
    }
    found.sort();
    let mut code = String::from("pub(crate) static ASSETS: &[(&str, &[u8])] = &[\n");
    for (name, path) in &found {
        let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
        let _ = writeln!(
            code,
            "    ({name:?}, include_bytes!({:?})),",
            path.display().to_string()
        );
    }
    code.push_str("];\n");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap_or_default()).join("assets.rs");
    if let Err(e) = std::fs::write(&out, code) {
        panic!("writing {}: {e}", out.display());
    }
}
