//! Real-world files (opt-in):
//!
//! ```sh
//! MEDICUS_REAL_DATA=/path/to/dicom cargo test -p medicus-server real_data -- --ignored --nocapture
//! ```
//!
//! For every Part 10 file, parsing growing prefixes of it (as the ingest pipeline does with a
//! stream) must give exactly the result of parsing the complete file, and the recorded header
//! prefix alone must reproduce it. Output is limited to counts and masked error categories, as
//! real data may contain patient information.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::parse::{ParsedHeader, parse_prefix};

fn files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut dirs = vec![root.to_owned()];
    while let Some(d) = dirs.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                dirs.push(p);
            } else {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// Error text with digits masked (no UIDs, offsets or lengths).
fn masked(e: &impl std::fmt::Display) -> String {
    let s: String = e
        .to_string()
        .chars()
        .map(|c| if c.is_ascii_digit() { '#' } else { c })
        .collect();
    s.chars().take(100).collect()
}

fn incremental(bytes: &[u8]) -> medicus_core::Result<ParsedHeader> {
    let mut len = 16 * 1024;
    loop {
        if len >= bytes.len() {
            return parse_prefix(bytes, true, &[])?
                .ok_or_else(|| medicus_core::Error::internal("no result at end"));
        }
        if let Some(p) = parse_prefix(&bytes[..len], false, &[])? {
            return Ok(p);
        }
        len *= 2;
    }
}

fn same(a: &ParsedHeader, b: &ParsedHeader) -> bool {
    a.instance == b.instance
        && a.study == b.study
        && a.series == b.series
        && a.attributes == b.attributes
}

#[test]
#[ignore = "needs MEDICUS_REAL_DATA"]
fn real_data() {
    let Some(root) = std::env::var_os("MEDICUS_REAL_DATA") else {
        return;
    };
    let mut outcome: BTreeMap<String, u32> = BTreeMap::new();
    for path in files(Path::new(&root)) {
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        if bytes.len() < 132 || &bytes[128..132] != b"DICM" {
            continue;
        }
        let key = match (incremental(&bytes), parse_prefix(&bytes, true, &[])) {
            (Ok(inc), Ok(Some(full))) if same(&inc, &full) => {
                let prefix = &bytes[..usize::try_from(inc.header_length).unwrap()];
                match parse_prefix(prefix, true, &[]) {
                    Ok(Some(again)) if same(&again, &full) => "agree".to_owned(),
                    _ => "MISMATCH: header prefix does not reproduce the header".to_owned(),
                }
            }
            (Ok(_), Ok(_)) => "MISMATCH: incremental and complete parse differ".to_owned(),
            (Err(a), Err(b)) => format!(
                "rejected: {}",
                if masked(&a) == masked(&b) {
                    masked(&a)
                } else {
                    format!("{} | {}", masked(&a), masked(&b))
                }
            ),
            (Err(e), Ok(_)) => format!("MISMATCH: only incremental parse fails: {}", masked(&e)),
            (Ok(_), Err(e)) => format!("MISMATCH: only complete parse fails: {}", masked(&e)),
        };
        *outcome.entry(key).or_default() += 1;
    }
    for (k, v) in &outcome {
        println!("{v:5}  {k}");
    }
    let bad: u32 = outcome
        .iter()
        .filter(|(k, _)| k.starts_with("MISMATCH"))
        .map(|(_, v)| v)
        .sum();
    assert_eq!(bad, 0);
}
