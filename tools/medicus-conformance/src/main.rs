//! Rewrites the generated blocks of the DICOM Conformance Statement.
//!
//! ```text
//! cargo run -p medicus-conformance            # update docs/conformance/conformance-statement.md
//! cargo run -p medicus-conformance -- --check # fail if it is out of date
//! ```

use std::path::Path;

use anyhow::{Context as _, bail};

fn main() -> anyhow::Result<()> {
    let check = std::env::args().skip(1).any(|a| a == "--check");
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(medicus_conformance::STATEMENT);
    let document =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let updated = medicus_conformance::update(&document)?;
    if updated == document {
        return Ok(());
    }
    if check {
        bail!(
            "{} is out of date: run `cargo run -p medicus-conformance`",
            medicus_conformance::STATEMENT
        );
    }
    std::fs::write(&path, updated).with_context(|| format!("writing {}", path.display()))?;
    eprintln!("updated {}", medicus_conformance::STATEMENT);
    Ok(())
}
