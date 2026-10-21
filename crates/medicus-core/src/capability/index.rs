//! The index: hierarchy, query keys, blob locations, the event outbox and subscriber cursors.
//!
//! The index is the single source of truth for what is stored. Everything that changes data also
//! appends the corresponding event in the same transaction (PLAN §3.7), so subscribers never miss
//! a change and never see a change that was rolled back.

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::BlobKey;
use crate::error::Result;
use crate::event::Event;
use crate::query::Query;
use crate::uid::{
    SeriesInstanceUid, SopClassUid, SopInstanceUid, StudyInstanceUid, TransferSyntaxUid,
};

/// A DICOM JSON object (PS3.18 F): `{"GGGGEEEE": {"vr": .., "Value": [..]}, ..}`.
pub type DicomJsonObject = serde_json::Map<String, serde_json::Value>;

/// Where a blob lives: the configured name of a `BlobStore` and the key inside it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BlobRef {
    pub store: String,
    pub key: BlobKey,
}

/// What to do when an instance with the same SOP Instance UID already exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DuplicatePolicy {
    /// Refuse the new instance (`AlreadyExists`).
    Reject,
    /// Keep the stored instance and discard the new one (no event).
    KeepExisting,
    /// Replace the stored instance.
    #[default]
    Overwrite,
}

/// Everything the index records about a newly stored instance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstanceRecord {
    pub study: StudyInstanceUid,
    pub series: SeriesInstanceUid,
    pub instance: SopInstanceUid,
    pub sop_class: SopClassUid,
    pub transfer_syntax: TransferSyntaxUid,
    /// The standard attributes of all levels (`tags::STANDARD`) as DICOM JSON; the index splits
    /// them by level.
    pub attributes: DicomJsonObject,
    /// Size of the stored DICOM file in bytes.
    pub size: u64,
    pub blob: BlobRef,
    /// The instance's full metadata (DICOM JSON, bulk data stripped), if stored separately.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<BlobRef>,
    /// Size of the metadata blob in bytes (0 if none).
    #[serde(default)]
    pub metadata_size: u64,
}

/// Where a stored instance and its metadata are.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceLocation {
    pub study: StudyInstanceUid,
    pub series: SeriesInstanceUid,
    pub instance: SopInstanceUid,
    pub sop_class: SopClassUid,
    pub transfer_syntax: TransferSyntaxUid,
    pub size: u64,
    pub blob: BlobRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<BlobRef>,
    #[serde(default)]
    pub metadata_size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "duplicate", rename_all = "snake_case")]
pub enum Duplicate {
    /// The instance was new.
    None,
    /// An existing instance was replaced; its blobs are no longer referenced.
    Replaced { previous: Box<InstanceLocation> },
    /// The existing instance was kept; the new blobs are not referenced.
    KeptExisting,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreOutcome {
    /// Sequence number of the appended event; `None` if nothing changed (`KeptExisting`).
    pub seq: Option<u64>,
    pub duplicate: Duplicate,
}

/// A resource addressed by its unique key.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "level", content = "uid", rename_all = "snake_case")]
pub enum ResourceUid {
    Study(StudyInstanceUid),
    Series(SeriesInstanceUid),
    Instance(SopInstanceUid),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteOutcome {
    pub seq: u64,
    /// The instances removed from the index; their blobs are no longer referenced.
    pub removed: Vec<InstanceLocation>,
}

/// One page of query results: DICOM JSON objects with the requested attributes of the query
/// level, the unique keys of all higher levels, and computed attributes (e.g. counts).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct QueryResult {
    pub rows: Vec<DicomJsonObject>,
    pub has_more: bool,
    /// Constrained keys the index could not evaluate (e.g. attributes it does not index, or
    /// sequence matching). The caller must post-filter on full metadata; paging is then
    /// approximate.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unfiltered: Vec<crate::Tag>,
}

#[async_trait]
pub trait IndexStore: Send + Sync + 'static {
    /// Records an instance and, unless the outcome is `KeptExisting`, appends `event` (whose
    /// `seq` the index assigns) in the same transaction.
    async fn store_instance(
        &self,
        record: &InstanceRecord,
        policy: DuplicatePolicy,
        event: &Event,
    ) -> Result<StoreOutcome>;

    async fn query(&self, query: &Query) -> Result<QueryResult>;

    /// All instances of a resource. Empty if the resource does not exist.
    async fn locate(&self, resource: &ResourceUid) -> Result<Vec<InstanceLocation>>;

    /// Removes a resource (and parents that become empty) and appends `event` in the same
    /// transaction. `NotFound` if it does not exist.
    async fn delete(&self, resource: &ResourceUid, event: &Event) -> Result<DeleteOutcome>;

    /// Appends an event that is not tied to a data change (e.g. `storage.full`).
    async fn append_event(&self, event: &Event) -> Result<u64>;

    /// Up to `limit` events with `seq > after`, in order.
    async fn events_after(&self, after: u64, limit: u32) -> Result<Vec<Event>>;

    /// The highest assigned sequence number (0 if none).
    async fn last_event_seq(&self) -> Result<u64>;

    /// The last sequence number acknowledged by a durable subscriber.
    async fn cursor(&self, subscriber: &str) -> Result<Option<u64>>;

    async fn set_cursor(&self, subscriber: &str, seq: u64) -> Result<()>;

    /// Deletes events with `seq <= up_to`; returns how many were deleted.
    async fn prune_events(&self, up_to: u64) -> Result<u64>;

    /// Bytes referenced per blob store (instances and metadata), for quota accounting.
    async fn stored_bytes(&self) -> Result<BTreeMap<String, u64>>;
}

/// Wire format of `IndexStore` calls over the ABI (one JSON request per call).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum IndexRequest {
    StoreInstance {
        record: Box<InstanceRecord>,
        policy: DuplicatePolicy,
        event: Box<Event>,
    },
    Query {
        query: Query,
    },
    Locate {
        resource: ResourceUid,
    },
    Delete {
        resource: ResourceUid,
        event: Event,
    },
    AppendEvent {
        event: Event,
    },
    EventsAfter {
        after: u64,
        limit: u32,
    },
    LastEventSeq,
    Cursor {
        subscriber: String,
    },
    SetCursor {
        subscriber: String,
        seq: u64,
    },
    PruneEvents {
        up_to: u64,
    },
    StoredBytes,
}

impl IndexRequest {
    /// Method names understood by this version, so that a callee can answer `Unsupported` for
    /// methods of newer versions instead of `InvalidArgument`.
    pub const METHODS: &'static [&'static str] = &[
        "store_instance",
        "query",
        "locate",
        "delete",
        "append_event",
        "events_after",
        "last_event_seq",
        "cursor",
        "set_cursor",
        "prune_events",
        "stored_bytes",
    ];
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_wire_format() {
        let r = IndexRequest::SetCursor {
            subscriber: "addon/hook".into(),
            seq: 42,
        };
        assert_eq!(
            serde_json::to_value(&r).unwrap(),
            json!({ "method": "set_cursor", "subscriber": "addon/hook", "seq": 42 })
        );
        let r: IndexRequest =
            serde_json::from_value(json!({ "method": "last_event_seq" })).unwrap();
        assert_eq!(r, IndexRequest::LastEventSeq);
        let resource = ResourceUid::Study(StudyInstanceUid::parse("1.2.3").unwrap());
        assert_eq!(
            serde_json::to_value(&resource).unwrap(),
            json!({ "level": "study", "uid": "1.2.3" })
        );
    }

    #[test]
    fn method_list_matches_variants() {
        for m in IndexRequest::METHODS {
            let v = serde_json::from_value::<IndexRequest>(json!({ "method": m }));
            // Variants with fields fail on missing fields, never on an unknown method.
            if let Err(e) = v {
                assert!(!e.to_string().contains("unknown variant"), "{m}: {e}");
            }
        }
    }
}
