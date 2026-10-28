//! The generated parts of the DICOM Conformance Statement (`docs/conformance`).
//!
//! The statement is hand-written Markdown, except for blocks between
//! `<!-- generated:NAME -->` and `<!-- /generated:NAME -->`: those are rewritten from the lists
//! the code uses, so the SOP classes and transfer syntaxes the document names are exactly those
//! the archive negotiates.

use std::fmt::Write as _;

use anyhow::{Context as _, bail};
use dicom_core::dictionary::UidDictionary as _;
use dicom_dictionary_std::{StandardSopClassDictionary, uids};
use dicom_encoding::TransferSyntaxIndex as _;
use dicom_transfer_syntax_registry::TransferSyntaxRegistry;
use medicus_dimse::{ScpOptions, storage};

/// The statement, relative to the workspace root.
pub const STATEMENT: &str = "docs/conformance/conformance-statement.md";

/// SOP classes accepted but not yet named in dicom-dictionary-std (PS3.6 Annex A).
const NAMES: &[(&str, &str)] = &[
    (
        "1.2.840.10008.5.1.4.1.1.2.3",
        "CT Image Storage - For Processing",
    ),
    (
        "1.2.840.10008.5.1.4.1.1.2.4",
        "Enhanced CT Image Storage - For Processing",
    ),
    (
        "1.2.840.10008.5.1.4.1.1.2.5",
        "Legacy Converted Enhanced CT Image Storage - For Processing",
    ),
];

fn sop_class_name(uid: &str) -> String {
    if let Some((_, name)) = NAMES.iter().find(|(u, _)| *u == uid) {
        return (*name).to_owned();
    }
    StandardSopClassDictionary
        .by_uid(uid)
        .map_or_else(|| uid.to_owned(), |e| e.name.to_owned())
}

/// UIDs in numeric order of their components, so `…4.50` comes before `…4.100`.
fn uid_order(uid: &str) -> Vec<u64> {
    uid.split('.').map(|c| c.parse().unwrap_or(0)).collect()
}

/// A Markdown table; `|` in cells is escaped.
fn table(header: &[&str], rows: impl IntoIterator<Item = Vec<String>>) -> String {
    let line = |cells: &[String]| {
        let escaped: Vec<String> = cells.iter().map(|c| c.replace('|', "\\|")).collect();
        format!("| {} |\n", escaped.join(" | "))
    };
    let header: Vec<String> = header.iter().map(|h| (*h).to_owned()).collect();
    let mut out = line(&header);
    out.push_str(&line(&vec!["---".to_owned(); header.len()]));
    for row in rows {
        out.push_str(&line(&row));
    }
    out
}

fn product() -> String {
    table(
        &["Product", "Version"],
        [vec![
            "medicus".to_owned(),
            env!("CARGO_PKG_VERSION").to_owned(),
        ]],
    )
}

/// The non-storage SOP classes of the DIMSE services, with their roles.
fn network_sop_classes() -> String {
    let mut rows = vec![vec![
        sop_class_name(uids::VERIFICATION),
        format!("`{}`", uids::VERIFICATION),
        "Yes".to_owned(),
        "Yes".to_owned(),
    ]];
    for uid in dimse::FIND_SOP_CLASSES
        .iter()
        .chain(dimse::RETRIEVE_SOP_CLASSES)
    {
        rows.push(vec![
            sop_class_name(uid),
            format!("`{uid}`"),
            "No".to_owned(),
            "Yes".to_owned(),
        ]);
    }
    let storage = storage::all().count();
    rows.push(vec![
        format!("Storage SOP classes ({storage}, see [Annex A](#annex-a-storage-sop-classes))"),
        "–".to_owned(),
        "Yes".to_owned(),
        "Yes".to_owned(),
    ]);
    table(&["SOP Class", "SOP Class UID", "SCU", "SCP"], rows)
}

/// The transfer syntaxes the SCP accepts by default.
fn transfer_syntaxes() -> String {
    let mut accepted = ScpOptions::new(Vec::<String>::new()).accepted_transfer_syntaxes();
    accepted.sort_by_key(|uid| uid_order(uid));
    let rows = accepted.into_iter().map(|uid| {
        let ts = TransferSyntaxRegistry.get(&uid);
        let name = ts.map_or_else(|| uid.clone(), |ts| ts.name().to_owned());
        let encapsulated =
            ts.is_some_and(dicom_encoding::TransferSyntax::is_encapsulated_pixel_data);
        vec![
            name,
            format!("`{uid}`"),
            if encapsulated {
                "Encapsulated"
            } else {
                "Native"
            }
            .to_owned(),
        ]
    });
    table(&["Transfer Syntax", "UID", "Pixel Data"], rows)
}

/// Annex A: every Storage SOP class accepted by default.
fn storage_sop_classes() -> String {
    let rows = storage::STANDARD
        .iter()
        .map(|uid| (uid, false))
        .chain(storage::RETIRED.iter().map(|uid| (uid, true)))
        .map(|(uid, retired)| {
            vec![
                sop_class_name(uid),
                format!("`{uid}`"),
                if retired { "Retired" } else { "" }.to_owned(),
            ]
        });
    table(&["SOP Class", "SOP Class UID", "Status"], rows)
}

/// The generated blocks by name.
pub fn blocks() -> Vec<(&'static str, String)> {
    vec![
        ("product", product()),
        ("network-sop-classes", network_sop_classes()),
        ("transfer-syntaxes", transfer_syntaxes()),
        ("storage-sop-classes", storage_sop_classes()),
    ]
}

/// `document` with every generated block rewritten; each must be present exactly once.
pub fn update(document: &str) -> anyhow::Result<String> {
    let mut out = document.to_owned();
    for (name, content) in blocks() {
        let begin = format!("<!-- generated:{name} -->\n");
        let end = format!("<!-- /generated:{name} -->");
        let start = out
            .find(&begin)
            .with_context(|| format!("missing `{}`", begin.trim_end()))?
            + begin.len();
        if out[start..].contains(&begin) {
            bail!("`{}` appears twice", begin.trim_end());
        }
        let stop = start
            + out[start..]
                .find(&end)
                .with_context(|| format!("missing `{end}`"))?;
        let mut block = String::new();
        let _ = write!(block, "\n{content}\n");
        out.replace_range(start..stop, &block);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statement_is_current() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(STATEMENT);
        let document = std::fs::read_to_string(&path).unwrap();
        assert!(
            update(&document).unwrap() == document,
            "{STATEMENT} is out of date: run `cargo run -p medicus-conformance`"
        );
    }

    #[test]
    fn blocks_are_replaced_in_place() {
        let doc = "a\n<!-- generated:product -->\nold\n<!-- /generated:product -->\nb\n";
        let err = update(doc).unwrap_err().to_string();
        assert!(err.contains("network-sop-classes"), "{err}");
        let mut full = String::new();
        for (n, _) in blocks() {
            let _ = writeln!(full, "<!-- generated:{n} -->\n<!-- /generated:{n} -->");
        }
        let updated = update(&full).unwrap();
        assert!(updated.contains("| medicus |"));
        assert_eq!(update(&updated).unwrap(), updated, "idempotent");
    }

    #[test]
    fn transfer_syntaxes_are_ordered_numerically() {
        let mut uids = ["1.2.840.10008.1.2.4.100", "1.2.840.10008.1.2.4.50"];
        uids.sort_by_key(|u| uid_order(u));
        assert_eq!(uids[0], "1.2.840.10008.1.2.4.50");
    }
}
