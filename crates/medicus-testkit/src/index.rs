//! `IndexStore` conformance suite. The index must be empty when the suite starts.

use std::collections::BTreeSet;

use medicus_core::capability::IndexStore;
use medicus_core::capability::index::{
    BlobRef, DicomJsonObject, Duplicate, DuplicatePolicy, ExtraTag, ExtraTagState, InstanceRecord,
    ResourceUid,
};
use medicus_core::event::{Event, EventKind, Origin};
use medicus_core::query::{DicomJson, Key, MatchOptions, OrderBy, OrderKey, Query};
use medicus_core::uid::{
    SeriesInstanceUid, SopClassUid, SopInstanceUid, StudyInstanceUid, TransferSyntaxUid,
};
use medicus_core::{BlobKey, ErrorKind, QueryLevel, Tag, Vr, tags};
use serde_json::{Value, json};

const CT: &str = "1.2.840.10008.5.1.4.1.1.2";
const MR: &str = "1.2.840.10008.5.1.4.1.1.4";
const EXPLICIT_LE: &str = "1.2.840.10008.1.2.1";

/// Builds DICOM JSON attributes from `(tag, value)` pairs, using the standard VRs.
pub fn attributes(values: &[(Tag, &str)]) -> DicomJsonObject {
    let mut obj = DicomJsonObject::new();
    for (tag, v) in values {
        let vr = tags::standard_vr(*tag).unwrap_or(Vr::LO);
        let value = match vr {
            Vr::PN => json!([{ "Alphabetic": v }]),
            Vr::IS => json!([v.parse::<i64>().unwrap()]),
            _ => json!([v]),
        };
        obj.insert(
            String::from(*tag),
            json!({ "vr": vr.as_str(), "Value": value }),
        );
    }
    obj
}

/// An instance record with the given UIDs and extra attributes.
pub fn record(
    study: &str,
    series: &str,
    sop: &str,
    sop_class: &str,
    extra: &[(Tag, &str)],
) -> InstanceRecord {
    let mut values = vec![
        (tags::STUDY_INSTANCE_UID, study),
        (tags::SERIES_INSTANCE_UID, series),
        (tags::SOP_INSTANCE_UID, sop),
        (tags::SOP_CLASS_UID, sop_class),
    ];
    values.extend_from_slice(extra);
    InstanceRecord {
        study: StudyInstanceUid::parse(study).unwrap(),
        series: SeriesInstanceUid::parse(series).unwrap(),
        instance: SopInstanceUid::parse(sop).unwrap(),
        sop_class: SopClassUid::parse(sop_class).unwrap(),
        transfer_syntax: TransferSyntaxUid::parse(EXPLICIT_LE).unwrap(),
        attributes: attributes(&values),
        size: 1000,
        blob: BlobRef {
            store: "local".into(),
            key: BlobKey::generate(medicus_core::blob::BlobKind::Instance),
        },
        header_length: 400,
    }
}

fn event(kind: EventKind) -> Event {
    Event::new(
        kind,
        Origin::Internal {
            component: "conformance".into(),
        },
    )
}

fn study_query(keys: &[(Tag, &str)]) -> Query {
    keys.iter()
        .fold(Query::new(QueryLevel::Study), |q, (tag, v)| {
            let vr = tags::standard_vr(*tag).unwrap();
            q.with_key(Key::parse(*tag, vr, v, &MatchOptions::default()).unwrap())
        })
}

fn string_values(row: &DicomJsonObject, tag: Tag) -> Vec<String> {
    row.get(&String::from(tag))
        .and_then(|e| e.get("Value"))
        .and_then(Value::as_array)
        .map(|vs| {
            vs.iter()
                .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn int_value(row: &DicomJsonObject, tag: Tag) -> Option<i64> {
    row.get(&String::from(tag))?.get("Value")?.get(0)?.as_i64()
}

async fn study_uids(index: &dyn IndexStore, q: &Query) -> BTreeSet<String> {
    let r = index.query(q).await.unwrap();
    r.rows
        .iter()
        .flat_map(|row| string_values(row, tags::STUDY_INSTANCE_UID))
        .collect()
}

/// Runs all scenarios in order (they build on each other's data).
pub async fn suite(index: &dyn IndexStore) {
    assert_eq!(
        index.last_event_seq().await.unwrap(),
        0,
        "index must start empty"
    );
    hierarchy_and_counts(index).await;
    duplicates(index).await;
    matching_parity(index).await;
    paging_and_order(index).await;
    unfiltered_and_level_errors(index).await;
    extra_tags(index).await;
    events_and_cursors(index).await;
    counters_match_recount(index).await;
    deletion(index).await;
    counters_match_recount(index).await;
}

/// Maintained counters (`NumberOfStudyRelated*`, `NumberOfSeriesRelatedInstances`) must always
/// equal a recount of what the index actually holds.
async fn counters_match_recount(index: &dyn IndexStore) {
    let studies = index
        .query(&Query::new(QueryLevel::Study))
        .await
        .unwrap()
        .rows;
    assert!(!studies.is_empty());
    for row in &studies {
        let uid = string_values(row, tags::STUDY_INSTANCE_UID).remove(0);
        let located = index
            .locate(&ResourceUid::Study(StudyInstanceUid::parse(&uid).unwrap()))
            .await
            .unwrap();
        let series: BTreeSet<&str> = located.iter().map(|l| l.series.as_str()).collect();
        assert_eq!(
            int_value(row, tags::NUMBER_OF_STUDY_RELATED_INSTANCES),
            Some(located.len() as i64),
            "study {uid}"
        );
        assert_eq!(
            int_value(row, tags::NUMBER_OF_STUDY_RELATED_SERIES),
            Some(series.len() as i64),
            "study {uid}"
        );
    }
    for row in index
        .query(&Query::new(QueryLevel::Series))
        .await
        .unwrap()
        .rows
    {
        let uid = string_values(&row, tags::SERIES_INSTANCE_UID).remove(0);
        let located = index
            .locate(&ResourceUid::Series(
                SeriesInstanceUid::parse(&uid).unwrap(),
            ))
            .await
            .unwrap();
        assert_eq!(
            int_value(&row, tags::NUMBER_OF_SERIES_RELATED_INSTANCES),
            Some(located.len() as i64),
            "series {uid}"
        );
    }
}

async fn hierarchy_and_counts(index: &dyn IndexStore) {
    let p = [
        (tags::PATIENT_ID, "H-1"),
        (tags::PATIENT_NAME, "Hier^Archy"),
    ];
    let ct = [
        &p[..],
        &[(tags::MODALITY, "CT"), (tags::SERIES_NUMBER, "1")],
    ]
    .concat();
    let mr = [
        &p[..],
        &[(tags::MODALITY, "MR"), (tags::SERIES_NUMBER, "2")],
    ]
    .concat();
    for (series, sop, class, attrs) in [
        ("1.1.1", "1.1.1.1", CT, &ct),
        ("1.1.1", "1.1.1.2", CT, &ct),
        ("1.1.2", "1.1.2.1", MR, &mr),
    ] {
        let r = record("1.1", series, sop, class, attrs);
        let out = index
            .store_instance(
                &r,
                DuplicatePolicy::Reject,
                &event(EventKind::InstanceStored),
            )
            .await
            .unwrap();
        assert_eq!(out.duplicate, Duplicate::None);
        assert!(out.seq.is_some());
    }
    let study = |u: &str| ResourceUid::Study(StudyInstanceUid::parse(u).unwrap());
    assert_eq!(index.locate(&study("1.1")).await.unwrap().len(), 3);
    assert_eq!(
        index
            .locate(&ResourceUid::Series(
                SeriesInstanceUid::parse("1.1.1").unwrap()
            ))
            .await
            .unwrap()
            .len(),
        2
    );
    let one = index
        .locate(&ResourceUid::Instance(
            SopInstanceUid::parse("1.1.2.1").unwrap(),
        ))
        .await
        .unwrap();
    assert_eq!(
        (one.len(), one[0].series.as_str(), one[0].size),
        (1, "1.1.2", 1000)
    );
    assert!(index.locate(&study("9.9.9")).await.unwrap().is_empty());

    let rows = index
        .query(&study_query(&[(tags::STUDY_INSTANCE_UID, "1.1")]))
        .await
        .unwrap()
        .rows;
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(
        int_value(row, tags::NUMBER_OF_STUDY_RELATED_INSTANCES),
        Some(3)
    );
    assert_eq!(
        int_value(row, tags::NUMBER_OF_STUDY_RELATED_SERIES),
        Some(2)
    );
    assert_eq!(string_values(row, tags::MODALITIES_IN_STUDY), ["CT", "MR"]);
    assert_eq!(string_values(row, tags::SOP_CLASSES_IN_STUDY).len(), 2);
    assert_eq!(
        row[&String::from(tags::PATIENT_NAME)]["Value"][0]["Alphabetic"],
        "Hier^Archy"
    );

    // Series level returns the parent's unique keys and the series' own count.
    let mut q = Query::new(QueryLevel::Series);
    q.keys.push(
        Key::parse(
            tags::STUDY_INSTANCE_UID,
            Vr::UI,
            "1.1",
            &MatchOptions::default(),
        )
        .unwrap(),
    );
    let series = index.query(&q).await.unwrap().rows;
    assert_eq!(series.len(), 2);
    assert_eq!(
        int_value(&series[0], tags::NUMBER_OF_SERIES_RELATED_INSTANCES),
        Some(2)
    );
    assert_eq!(int_value(&series[0], tags::SERIES_NUMBER), Some(1));
    assert_eq!(string_values(&series[1], tags::STUDY_INSTANCE_UID), ["1.1"]);

    // ModalitiesInStudy matching (index-computed, so not part of the parity test).
    let by_mod = |m: &'static str| study_query(&[(tags::MODALITIES_IN_STUDY, m)]);
    assert!(study_uids(index, &by_mod("MR")).await.contains("1.1"));
    assert!(study_uids(index, &by_mod("US\\CT")).await.contains("1.1"));
    assert!(!study_uids(index, &by_mod("US")).await.contains("1.1"));
    assert!(study_uids(index, &by_mod("M?")).await.contains("1.1"));

    let bytes = index.stored_bytes().await.unwrap();
    assert_eq!(bytes.get("local"), Some(&3000));
}

async fn duplicates(index: &dyn IndexStore) {
    let r = record(
        "1.1",
        "1.1.1",
        "1.1.1.1",
        CT,
        &[(tags::PATIENT_ID, "H-1"), (tags::MODALITY, "CT")],
    );
    let before = index.last_event_seq().await.unwrap();
    let err = index
        .store_instance(
            &r,
            DuplicatePolicy::Reject,
            &event(EventKind::InstanceStored),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::AlreadyExists);
    let kept = index
        .store_instance(
            &r,
            DuplicatePolicy::KeepExisting,
            &event(EventKind::InstanceStored),
        )
        .await
        .unwrap();
    assert_eq!((kept.seq, kept.duplicate), (None, Duplicate::KeptExisting));
    assert_eq!(
        index.last_event_seq().await.unwrap(),
        before,
        "no event without a change"
    );

    let replaced = index
        .store_instance(
            &r,
            DuplicatePolicy::Overwrite,
            &event(EventKind::InstanceStored),
        )
        .await
        .unwrap();
    let Duplicate::Replaced { previous } = replaced.duplicate else {
        panic!("expected Replaced")
    };
    assert_ne!(previous.blob, r.blob);
    let now = index
        .locate(&ResourceUid::Instance(r.instance.clone()))
        .await
        .unwrap();
    assert_eq!(now[0].blob, r.blob);
    let rows = index
        .query(&study_query(&[(tags::STUDY_INSTANCE_UID, "1.1")]))
        .await
        .unwrap()
        .rows;
    assert_eq!(
        int_value(&rows[0], tags::NUMBER_OF_STUDY_RELATED_INSTANCES),
        Some(3),
        "overwrite keeps counts"
    );

    // A series UID reused under another study is refused.
    let clash = record("1.2", "1.1.1", "1.2.1.1", CT, &[]);
    let err = index
        .store_instance(
            &clash,
            DuplicatePolicy::Reject,
            &event(EventKind::InstanceStored),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::InvalidArgument);
    assert!(
        index
            .locate(&ResourceUid::Study(StudyInstanceUid::parse("1.2").unwrap()))
            .await
            .unwrap()
            .is_empty()
    );
}

/// Studies used by the parity test: (study uid, patient/study attributes).
fn parity_studies() -> Vec<(&'static str, Vec<(Tag, &'static str)>)> {
    vec![
        (
            "2.1",
            vec![
                (tags::PATIENT_ID, "P1"),
                (tags::PATIENT_NAME, "Doe^John"),
                (tags::STUDY_DATE, "20240115"),
                (tags::STUDY_TIME, "101500"),
                (tags::ACCESSION_NUMBER, "A1"),
                (tags::STUDY_DESCRIPTION, "CT Thorax"),
            ],
        ),
        (
            "2.2",
            vec![
                (tags::PATIENT_ID, "P1"),
                (tags::PATIENT_NAME, "Doe^John"),
                (tags::STUDY_DATE, "20240220"),
                (tags::STUDY_TIME, "2230"),
                (tags::STUDY_DESCRIPTION, "MR Knee [left]"),
            ],
        ),
        (
            "2.3",
            vec![
                (tags::PATIENT_ID, "P2"),
                (tags::PATIENT_NAME, "DOE^Jane"),
                (tags::STUDY_DATE, "2023.12.31"),
                (tags::STUDY_TIME, "08"),
                (tags::ACCESSION_NUMBER, "A3"),
            ],
        ),
        (
            "2.4",
            vec![
                (tags::PATIENT_ID, "P3"),
                (tags::PATIENT_NAME, "Müller^Jürgen"),
                (tags::STUDY_DATE, "20240301"),
                (tags::REFERRING_PHYSICIAN_NAME, "House^Gregory"),
                (tags::STUDY_DESCRIPTION, "ct thorax"),
            ],
        ),
        (
            "2.5",
            vec![
                (tags::PATIENT_ID, "P4"),
                (tags::ISSUER_OF_PATIENT_ID, "HOSP_B"),
                (tags::PATIENT_NAME, "Smith"),
                (tags::PATIENT_SEX, "F"),
                (tags::PATIENT_BIRTH_DATE, "19800101"),
            ],
        ),
    ]
}

async fn matching_parity(index: &dyn IndexStore) {
    let studies = parity_studies();
    let mut records = Vec::new();
    for (i, (uid, attrs)) in studies.iter().enumerate() {
        let r = record(uid, &format!("{uid}.1"), &format!("{uid}.1.1"), CT, attrs);
        index
            .store_instance(
                &r,
                DuplicatePolicy::Reject,
                &event(EventKind::InstanceStored),
            )
            .await
            .unwrap();
        records.push((i, r));
    }
    let queries: Vec<Vec<(Tag, &str)>> = vec![
        vec![(tags::PATIENT_ID, "P1")],
        vec![(tags::PATIENT_ID, "p1")],
        vec![(tags::PATIENT_NAME, "doe^john")],
        vec![(tags::PATIENT_NAME, "DOE*")],
        vec![(tags::PATIENT_NAME, "*^J?n*")],
        vec![(tags::PATIENT_NAME, "müller*")],
        vec![(tags::STUDY_DATE, "20240101-20240229")],
        vec![(tags::STUDY_DATE, "-20231231")],
        vec![(tags::STUDY_DATE, "20240220-")],
        vec![(tags::STUDY_DATE, "20231231")],
        vec![(tags::STUDY_TIME, "1015")],
        vec![(tags::STUDY_TIME, "10-23")],
        vec![(tags::STUDY_TIME, "-0830")],
        vec![(tags::STUDY_TIME, "223000")],
        vec![(tags::STUDY_DESCRIPTION, "CT*")],
        vec![(tags::STUDY_DESCRIPTION, "*[left]")],
        vec![(tags::STUDY_DESCRIPTION, "*thorax")],
        vec![(tags::ACCESSION_NUMBER, "A1\\A3")],
        vec![(tags::STUDY_INSTANCE_UID, "2.1\\2.4\\9.9")],
        vec![(tags::REFERRING_PHYSICIAN_NAME, "HOUSE*")],
        vec![
            (tags::ISSUER_OF_PATIENT_ID, "HOSP_B"),
            (tags::PATIENT_SEX, "F"),
        ],
        vec![(tags::PATIENT_BIRTH_DATE, "19800101-19901231")],
        vec![
            (tags::PATIENT_NAME, "Doe*"),
            (tags::STUDY_DATE, "20240101-20241231"),
        ],
    ];
    for keys in queries {
        let mut q = study_query(&keys);
        // Restrict to the parity studies.
        let uids = studies
            .iter()
            .map(|(u, _)| *u)
            .collect::<Vec<_>>()
            .join("\\");
        q.keys.push(
            Key::parse(
                tags::STUDY_INSTANCE_UID,
                Vr::UI,
                &uids,
                &MatchOptions::default(),
            )
            .unwrap(),
        );
        let expected: BTreeSet<String> = records
            .iter()
            .filter(|(_, r)| q.matches(&DicomJson::new(&r.attributes)))
            .map(|(_, r)| r.study.to_string())
            .collect();
        let got = study_uids(index, &q).await;
        assert_eq!(
            got, expected,
            "index and in-memory matcher disagree for {keys:?}"
        );
    }
}

async fn paging_and_order(index: &dyn IndexStore) {
    let mut q = study_query(&[(tags::STUDY_INSTANCE_UID, "2.1\\2.2\\2.3\\2.4\\2.5")]);
    q.order_by = vec![OrderBy {
        key: OrderKey::Attribute(tags::STUDY_DATE),
        descending: true,
    }];
    q.limit = Some(2);
    let page1 = index.query(&q).await.unwrap();
    assert!(page1.has_more);
    let dates: Vec<String> = page1
        .rows
        .iter()
        .flat_map(|r| string_values(r, tags::STUDY_DATE))
        .collect();
    assert_eq!(dates, ["20240301", "20240220"]);
    q.offset = 4;
    let last = index.query(&q).await.unwrap();
    assert_eq!((last.rows.len(), last.has_more), (1, false));
}

async fn unfiltered_and_level_errors(index: &dyn IndexStore) {
    let slice_thickness = Tag::new(0x0018, 0x0050);
    let mut q = Query::new(QueryLevel::Instance);
    q.keys
        .push(Key::parse(slice_thickness, Vr::DS, "1.5", &MatchOptions::default()).unwrap());
    q.keys
        .push(Key::universal(Tag::new(0x0028, 0x0010), Vr::US));
    assert_eq!(
        index.query(&q).await.unwrap().unfiltered,
        vec![slice_thickness]
    );

    let below = study_query(&[])
        .with_key(Key::parse(tags::MODALITY, Vr::CS, "CT", &MatchOptions::default()).unwrap());
    assert_eq!(
        index.query(&below).await.unwrap_err().kind,
        ErrorKind::InvalidArgument
    );
}

async fn events_and_cursors(index: &dyn IndexStore) {
    let last = index.last_event_seq().await.unwrap();
    let e = event(EventKind::StorageFull);
    let seq = index.append_event(&e).await.unwrap();
    assert!(seq > last);
    let all = index.events_after(0, 1000).await.unwrap();
    assert!(
        all.windows(2).all(|w| w[0].seq < w[1].seq),
        "events are ordered by seq"
    );
    let mine = all.iter().find(|x| x.id == e.id).unwrap();
    assert_eq!((mine.seq, &mine.kind), (seq, &EventKind::StorageFull));
    assert_eq!(index.events_after(seq - 1, 1).await.unwrap()[0].id, e.id);
    assert!(index.events_after(seq, 10).await.unwrap().is_empty());

    assert_eq!(index.cursor("conformance/sub").await.unwrap(), None);
    index.set_cursor("conformance/sub", 5).await.unwrap();
    index.set_cursor("conformance/sub", 7).await.unwrap();
    assert_eq!(index.cursor("conformance/sub").await.unwrap(), Some(7));

    let pruned = index.prune_events(3).await.unwrap();
    assert_eq!(pruned, 3);
    assert_eq!(index.events_after(0, 1).await.unwrap()[0].seq, 4);
    assert_eq!(index.last_event_seq().await.unwrap(), seq);
}

async fn deletion(index: &dyn IndexStore) {
    let series = ResourceUid::Series(SeriesInstanceUid::parse("1.1.2").unwrap());
    let out = index
        .delete(&series, &event(EventKind::SeriesDeleted))
        .await
        .unwrap();
    assert_eq!(out.removed.len(), 1);
    let rows = index
        .query(&study_query(&[(tags::STUDY_INSTANCE_UID, "1.1")]))
        .await
        .unwrap()
        .rows;
    assert_eq!(
        int_value(&rows[0], tags::NUMBER_OF_STUDY_RELATED_SERIES),
        Some(1)
    );
    assert_eq!(
        string_values(&rows[0], tags::MODALITIES_IN_STUDY),
        ["CT"],
        "aggregates refreshed"
    );

    let study = ResourceUid::Study(StudyInstanceUid::parse("1.1").unwrap());
    assert_eq!(
        index
            .delete(&study, &event(EventKind::StudyDeleted))
            .await
            .unwrap()
            .removed
            .len(),
        2
    );
    assert_eq!(
        index
            .delete(&study, &event(EventKind::StudyDeleted))
            .await
            .unwrap_err()
            .kind,
        ErrorKind::NotFound
    );
    let mut patients = Query::new(QueryLevel::Patient);
    patients
        .keys
        .push(Key::parse(tags::PATIENT_ID, Vr::LO, "H-1", &MatchOptions::default()).unwrap());
    assert!(
        index.query(&patients).await.unwrap().rows.is_empty(),
        "empty patient removed"
    );

    // Patient level: P1 has two studies.
    let mut p1 = Query::new(QueryLevel::Patient);
    p1.keys
        .push(Key::parse(tags::PATIENT_ID, Vr::LO, "P1", &MatchOptions::default()).unwrap());
    let rows = index.query(&p1).await.unwrap().rows;
    assert_eq!(
        int_value(&rows[0], tags::NUMBER_OF_PATIENT_RELATED_STUDIES),
        Some(2)
    );
}

/// Adds an attribute with an explicit VR (for non-standard tags) to a record.
pub fn with_attribute(mut r: InstanceRecord, tag: Tag, vr: Vr, values: &[&str]) -> InstanceRecord {
    let vals: Vec<Value> = values
        .iter()
        .map(|v| match vr {
            Vr::PN => json!({ "Alphabetic": v }),
            Vr::IS | Vr::DS => v.parse::<f64>().map_or_else(|_| json!(v), |n| json!(n)),
            _ => json!(v),
        })
        .collect();
    r.attributes.insert(
        String::from(tag),
        json!({ "vr": vr.as_str(), "Value": vals }),
    );
    r
}

const SLICE_THICKNESS: Tag = Tag::new(0x0018, 0x0050);
const IMAGE_TYPE: Tag = Tag::new(0x0008, 0x0008);
const ACQUISITION_DATE: Tag = Tag::new(0x0008, 0x0022);
const PROTOCOL_NAME: Tag = Tag::new(0x0018, 0x1030);
const OPERATORS_NAME: Tag = Tag::new(0x0008, 0x1070);

fn extra_config() -> Vec<ExtraTag> {
    vec![
        ExtraTag {
            tag: SLICE_THICKNESS,
            vr: Vr::DS,
            level: QueryLevel::Instance,
        },
        ExtraTag {
            tag: IMAGE_TYPE,
            vr: Vr::CS,
            level: QueryLevel::Instance,
        },
        ExtraTag {
            tag: ACQUISITION_DATE,
            vr: Vr::DA,
            level: QueryLevel::Instance,
        },
        ExtraTag {
            tag: PROTOCOL_NAME,
            vr: Vr::LO,
            level: QueryLevel::Series,
        },
        ExtraTag {
            tag: OPERATORS_NAME,
            vr: Vr::PN,
            level: QueryLevel::Series,
        },
    ]
}

fn instance_query(keys: &[(Tag, Vr, &str)]) -> Query {
    keys.iter()
        .fold(Query::new(QueryLevel::Instance), |q, (tag, vr, v)| {
            q.with_key(Key::parse(*tag, *vr, v, &MatchOptions::default()).unwrap())
        })
}

/// On an empty index, newly configured extra tags are immediately ready. The index must be empty.
pub async fn extra_tags_on_empty_index(index: &dyn IndexStore) {
    let status = index.configure_extra_tags(&extra_config()).await.unwrap();
    assert!(
        status.iter().all(|s| s.state == ExtraTagState::Ready),
        "{status:?}"
    );
    let r = with_attribute(
        record("8.1", "8.1.1", "8.1.1.1", CT, &[]),
        SLICE_THICKNESS,
        Vr::DS,
        &["2.5"],
    );
    index
        .store_instance(
            &r,
            DuplicatePolicy::Reject,
            &event(EventKind::InstanceStored),
        )
        .await
        .unwrap();
    let q = instance_query(&[(SLICE_THICKNESS, Vr::DS, "2.50")]);
    let res = index.query(&q).await.unwrap();
    assert!(res.unfiltered.is_empty());
    assert_eq!(res.rows.len(), 1);
    // Extra tags are returned in their original DICOM JSON form.
    assert_eq!(
        res.rows[0][&String::from(SLICE_THICKNESS)],
        json!({ "vr": "DS", "Value": [2.5] })
    );
}

async fn extra_tags(index: &dyn IndexStore) {
    // Configuration errors.
    let standard = ExtraTag {
        tag: tags::MODALITY,
        vr: Vr::CS,
        level: QueryLevel::Series,
    };
    assert_eq!(
        index
            .configure_extra_tags(&[standard])
            .await
            .unwrap_err()
            .kind,
        ErrorKind::InvalidArgument
    );

    // The index already holds instances, so new tags need a backfill first.
    let status = index.configure_extra_tags(&extra_config()).await.unwrap();
    assert!(
        status.iter().all(|s| s.state == ExtraTagState::Backfilling),
        "{status:?}"
    );
    let q = instance_query(&[(SLICE_THICKNESS, Vr::DS, "1.5")]);
    assert_eq!(
        index.query(&q).await.unwrap().unfiltered,
        vec![SLICE_THICKNESS]
    );
    for t in extra_config() {
        index.mark_extra_ready(t.tag).await.unwrap();
    }
    assert_eq!(
        index
            .mark_extra_ready(Tag::new(0x0011, 0x0011))
            .await
            .unwrap_err()
            .kind,
        ErrorKind::NotFound
    );
    // Re-configuring keeps the state.
    let status = index.configure_extra_tags(&extra_config()).await.unwrap();
    assert!(status.iter().all(|s| s.state == ExtraTagState::Ready));

    let mk = |sop: &str,
              series: &str,
              thick: &str,
              types: &[&str],
              date: &str,
              protocol: &str,
              op: &str| {
        let r = record("7.1", series, sop, CT, &[(tags::PATIENT_ID, "X7")]);
        let r = with_attribute(r, SLICE_THICKNESS, Vr::DS, &[thick]);
        let r = with_attribute(r, IMAGE_TYPE, Vr::CS, types);
        let r = with_attribute(r, ACQUISITION_DATE, Vr::DA, &[date]);
        let r = with_attribute(r, PROTOCOL_NAME, Vr::LO, &[protocol]);
        with_attribute(r, OPERATORS_NAME, Vr::PN, &[op])
    };
    let records = vec![
        mk(
            "7.1.1.1",
            "7.1.1",
            "1.5",
            &["ORIGINAL", "PRIMARY", "AXIAL"],
            "20240105",
            "Chest Routine",
            "Smith^Anna",
        ),
        mk(
            "7.1.1.2",
            "7.1.1",
            "1.50",
            &["DERIVED", "SECONDARY"],
            "20240106",
            "Chest Routine",
            "Smith^Anna",
        ),
        mk(
            "7.1.2.1",
            "7.1.2",
            "5",
            &["ORIGINAL", "PRIMARY", "LOCALIZER"],
            "20231231",
            "Scout [low dose]",
            "Brown^Bob",
        ),
    ];
    for r in &records {
        index
            .store_instance(
                r,
                DuplicatePolicy::Reject,
                &event(EventKind::InstanceStored),
            )
            .await
            .unwrap();
    }

    let queries: Vec<Vec<(Tag, Vr, &str)>> = vec![
        vec![(SLICE_THICKNESS, Vr::DS, "1.5")],
        vec![(SLICE_THICKNESS, Vr::DS, "5.0")],
        vec![(IMAGE_TYPE, Vr::CS, "PRIMARY")],
        vec![(IMAGE_TYPE, Vr::CS, "LOCALIZER\\AXIAL")],
        vec![(IMAGE_TYPE, Vr::CS, "*ARY")],
        vec![(ACQUISITION_DATE, Vr::DA, "20240101-")],
        vec![(ACQUISITION_DATE, Vr::DA, "-20231231")],
        vec![(PROTOCOL_NAME, Vr::LO, "Chest*")],
        vec![(PROTOCOL_NAME, Vr::LO, "*[low dose]")],
        vec![(OPERATORS_NAME, Vr::PN, "smith^anna")],
        vec![(OPERATORS_NAME, Vr::PN, "B*")],
        vec![
            (SLICE_THICKNESS, Vr::DS, "1.5"),
            (IMAGE_TYPE, Vr::CS, "DERIVED"),
        ],
    ];
    let instances = records
        .iter()
        .map(|r| r.instance.as_str())
        .collect::<Vec<_>>()
        .join("\\");
    let mut counts = Vec::new();
    for keys in queries {
        let mut q = instance_query(&keys);
        q.keys.push(
            Key::parse(
                tags::SOP_INSTANCE_UID,
                Vr::UI,
                &instances,
                &MatchOptions::default(),
            )
            .unwrap(),
        );
        let res = index.query(&q).await.unwrap();
        assert!(
            res.unfiltered.is_empty(),
            "{keys:?} not evaluated by the index"
        );
        let got: BTreeSet<String> = res
            .rows
            .iter()
            .flat_map(|r| string_values(r, tags::SOP_INSTANCE_UID))
            .collect();
        let expected: BTreeSet<String> = records
            .iter()
            .filter(|r| q.matches(&DicomJson::new(&r.attributes)))
            .map(|r| r.instance.to_string())
            .collect();
        assert_eq!(
            got, expected,
            "index and in-memory matcher disagree for {keys:?}"
        );
        counts.push(got.len());
    }
    // Guard against vacuous agreement: known answers for some of the queries above.
    assert_eq!(counts[0], 2, "1.5 and 1.50 are numerically equal");
    assert_eq!(counts[2], 2);
    assert_eq!(counts[3], 2, "any of LOCALIZER, AXIAL");
    assert_eq!(counts[9], 2, "case-insensitive PN");
    assert_eq!(counts[11], 1);

    // Series-level extra tags are usable at series level; instance-level ones are not.
    let mut sq = Query::new(QueryLevel::Series);
    sq.keys
        .push(Key::parse(PROTOCOL_NAME, Vr::LO, "Scout*", &MatchOptions::default()).unwrap());
    let rows = index.query(&sq).await.unwrap().rows;
    assert_eq!(
        rows.iter()
            .flat_map(|r| string_values(r, tags::SERIES_INSTANCE_UID))
            .collect::<Vec<_>>(),
        ["7.1.2"]
    );
    let mut bad = Query::new(QueryLevel::Series);
    bad.keys
        .push(Key::parse(SLICE_THICKNESS, Vr::DS, "1", &MatchOptions::default()).unwrap());
    assert_eq!(
        index.query(&bad).await.unwrap_err().kind,
        ErrorKind::InvalidArgument
    );

    // Backfill an instance stored before the tags were configured.
    let old = SopInstanceUid::parse("2.1.1.1").unwrap();
    let attrs = with_attribute(
        record("2.1", "2.1.1", "2.1.1.1", CT, &[]),
        SLICE_THICKNESS,
        Vr::DS,
        &["9"],
    )
    .attributes;
    index.update_extra(&old, &attrs).await.unwrap();
    let rows = index
        .query(&instance_query(&[(SLICE_THICKNESS, Vr::DS, "9")]))
        .await
        .unwrap()
        .rows;
    assert_eq!(
        rows.iter()
            .flat_map(|r| string_values(r, tags::SOP_INSTANCE_UID))
            .collect::<Vec<_>>(),
        ["2.1.1.1"]
    );
    let missing = SopInstanceUid::parse("9.9.9.9").unwrap();
    assert_eq!(
        index.update_extra(&missing, &attrs).await.unwrap_err().kind,
        ErrorKind::NotFound
    );

    // Results carry extra tags of all selected levels as DICOM JSON (multi-valued, PN as object).
    let mut one = instance_query(&[]);
    one.keys.push(
        Key::parse(
            tags::SOP_INSTANCE_UID,
            Vr::UI,
            "7.1.2.1",
            &MatchOptions::default(),
        )
        .unwrap(),
    );
    let row = &index.query(&one).await.unwrap().rows[0];
    assert_eq!(
        row[&String::from(IMAGE_TYPE)]["Value"],
        json!(["ORIGINAL", "PRIMARY", "LOCALIZER"])
    );
    assert_eq!(
        row[&String::from(OPERATORS_NAME)]["Value"][0]["Alphabetic"],
        "Brown^Bob"
    );
    assert_eq!(row[&String::from(PROTOCOL_NAME)]["vr"], "LO");

    // Removing a tag from the configuration makes queries on it unfiltered again.
    index
        .configure_extra_tags(&extra_config()[1..])
        .await
        .unwrap();
    assert_eq!(
        index.query(&q).await.unwrap().unfiltered,
        vec![SLICE_THICKNESS]
    );
    let row = &index.query(&one).await.unwrap().rows[0];
    assert!(
        !row.contains_key(&String::from(SLICE_THICKNESS)),
        "dropped tags are not returned"
    );
}
