//! The index: hierarchy, query keys, blob locations, the event outbox, subscriber cursors, jobs,
//! and the server's settings, users and credentials.
//!
//! The index is the single source of truth for what is stored. Everything that changes data also
//! appends the corresponding event in the same transaction (PLAN §3.7), so subscribers never miss
//! a change and never see a change that was rolled back.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::capability::admin::{Credential, SettingsRevision, User};
use crate::capability::job::{Job, JobState};
use crate::error::Result;
use crate::event::Event;
use crate::query::Query;
use crate::uid::{
    SeriesInstanceUid, SopClassUid, SopInstanceUid, StudyInstanceUid, TransferSyntaxUid,
};
use crate::{BlobKey, Partition};

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
    pub partition: Partition,
    pub study: StudyInstanceUid,
    pub series: SeriesInstanceUid,
    pub instance: SopInstanceUid,
    pub sop_class: SopClassUid,
    pub transfer_syntax: TransferSyntaxUid,
    /// The attributes to index, as DICOM JSON: the standard attributes of all levels
    /// (`tags::STANDARD`) plus any configured extra tags. The index splits them by level.
    pub attributes: DicomJsonObject,
    /// Size of the stored DICOM file in bytes.
    pub size: u64,
    pub blob: BlobRef,
    /// Length in bytes of the file's header: the File Meta Information and all data elements
    /// before the first bulk data element (Pixel Data). A range read of `0..header_length` yields
    /// the instance's complete metadata in its original binary encoding.
    pub header_length: u64,
}

/// Where a stored instance is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceLocation {
    pub partition: Partition,
    pub study: StudyInstanceUid,
    pub series: SeriesInstanceUid,
    pub instance: SopInstanceUid,
    pub sop_class: SopClassUid,
    pub transfer_syntax: TransferSyntaxUid,
    pub size: u64,
    pub blob: BlobRef,
    /// Length in bytes of the file's header: the File Meta Information and all data elements
    /// before the first bulk data element (Pixel Data). A range read of `0..header_length` yields
    /// the instance's complete metadata in its original binary encoding.
    pub header_length: u64,
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

/// An attribute indexed in addition to the standard ones (configured by the operator).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ExtraTag {
    #[serde(with = "crate::tag::serde_tag")]
    pub tag: crate::Tag,
    #[serde(with = "crate::tag::serde_vr")]
    pub vr: crate::VR,
    pub level: crate::QueryLevel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtraTagState {
    /// Every row carries the tag's values; queries on it are evaluated by the index.
    Ready,
    /// Added to a non-empty index: existing rows lack values until a backfill completes. The
    /// index reports queries on the tag as `unfiltered`.
    Backfilling,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtraTagStatus {
    #[serde(flatten)]
    pub tag: ExtraTag,
    pub state: ExtraTagState,
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
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        with = "crate::tag::serde_tags"
    )]
    pub unfiltered: Vec<crate::Tag>,
}

/// Bytes the index references in one blob store for one partition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredBytes {
    pub partition: Partition,
    pub store: String,
    pub bytes: u64,
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

    /// Runs a query within one partition (there are no queries across partitions).
    async fn query(&self, partition: &Partition, query: &Query) -> Result<QueryResult>;

    /// All instances of a resource in `partition`. Empty if the resource does not exist.
    async fn locate(
        &self,
        partition: &Partition,
        resource: &ResourceUid,
    ) -> Result<Vec<InstanceLocation>>;

    /// Removes a resource of `partition` (and parents that become empty) and appends `event` in
    /// the same transaction. `NotFound` if it does not exist.
    async fn delete(
        &self,
        partition: &Partition,
        resource: &ResourceUid,
        event: &Event,
    ) -> Result<DeleteOutcome>;

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

    /// Bytes referenced per partition and blob store, for quota accounting.
    async fn stored_bytes(&self) -> Result<Vec<StoredBytes>>;

    /// Declares the configured extra tags (called at startup). Tags that are new on a non-empty
    /// index start as `Backfilling`; tags no longer configured are dropped. Standard attributes
    /// and sequences are rejected (`InvalidArgument`). Returns the state of every tag.
    async fn configure_extra_tags(&self, tags: &[ExtraTag]) -> Result<Vec<ExtraTagStatus>>;

    /// Replaces the extra-tag values of an existing instance (and its parents) from
    /// `attributes` (DICOM JSON); used by backfill jobs. `NotFound` if the instance is unknown.
    async fn update_extra(
        &self,
        partition: &Partition,
        instance: &SopInstanceUid,
        attributes: &DicomJsonObject,
    ) -> Result<()>;

    /// Marks a backfilled extra tag as `Ready`.
    async fn mark_extra_ready(&self, tag: crate::Tag) -> Result<()>;

    /// Inserts or replaces a job.
    async fn put_job(&self, job: &Job) -> Result<()>;

    async fn get_job(&self, id: uuid::Uuid) -> Result<Option<Job>>;

    /// Jobs, newest first; only those in `state` if given.
    async fn list_jobs(&self, state: Option<JobState>, limit: u32, offset: u64)
    -> Result<Vec<Job>>;

    /// Atomically takes the oldest queued job that is due at `now`: marks it `Running` and
    /// counts the attempt. Concurrent callers never get the same job.
    async fn claim_job(&self, now: time::OffsetDateTime) -> Result<Option<Job>>;

    /// The latest settings revision; `None` before the first one was written.
    async fn settings_current(&self) -> Result<Option<SettingsRevision>>;

    async fn settings_revision(&self, revision: u64) -> Result<Option<SettingsRevision>>;

    /// Settings revisions, newest first.
    async fn settings_history(&self, limit: u32, offset: u64) -> Result<Vec<SettingsRevision>>;

    /// Appends a settings revision. Its number must follow the latest one (1 for the first);
    /// otherwise someone else wrote a revision since the caller read the settings, and the call
    /// fails with `Conflict`.
    async fn put_settings(&self, revision: &SettingsRevision) -> Result<()>;

    /// `AlreadyExists` if a user with the name exists.
    async fn create_user(&self, user: &User) -> Result<()>;

    /// Replaces a user. `NotFound` if there is no user with the name.
    async fn update_user(&self, user: &User) -> Result<()>;

    async fn get_user(&self, name: &str) -> Result<Option<User>>;

    /// All users, by name.
    async fn list_users(&self) -> Result<Vec<User>>;

    /// Deletes a user and their credentials. `NotFound` if there is no user with the name.
    async fn delete_user(&self, name: &str) -> Result<()>;

    /// Inserts or replaces a credential (by id). `NotFound` if its user does not exist;
    /// `AlreadyExists` if another credential has the same secret hash.
    async fn put_credential(&self, credential: &Credential) -> Result<()>;

    /// The credential with the secret hash, expired or not.
    async fn find_credential(&self, secret_hash: &str) -> Result<Option<Credential>>;

    /// A user's credentials, oldest first.
    async fn list_credentials(&self, user: &str) -> Result<Vec<Credential>>;

    /// `NotFound` if there is no credential with the id.
    async fn delete_credential(&self, id: uuid::Uuid) -> Result<()>;

    /// Deletes the credentials that expired at `now`; returns how many were deleted.
    async fn prune_credentials(&self, now: time::OffsetDateTime) -> Result<u64>;
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
        partition: Partition,
        query: Query,
    },
    Locate {
        partition: Partition,
        resource: ResourceUid,
    },
    Delete {
        partition: Partition,
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
    ConfigureExtraTags {
        tags: Vec<ExtraTag>,
    },
    UpdateExtra {
        partition: Partition,
        instance: SopInstanceUid,
        attributes: DicomJsonObject,
    },
    MarkExtraReady {
        #[serde(with = "crate::tag::serde_tag")]
        tag: crate::Tag,
    },
    PutJob {
        job: Box<Job>,
    },
    GetJob {
        id: uuid::Uuid,
    },
    ListJobs {
        #[serde(default)]
        state: Option<JobState>,
        limit: u32,
        #[serde(default)]
        offset: u64,
    },
    ClaimJob {
        #[serde(with = "time::serde::rfc3339")]
        now: time::OffsetDateTime,
    },
    SettingsCurrent,
    SettingsRevision {
        revision: u64,
    },
    SettingsHistory {
        limit: u32,
        #[serde(default)]
        offset: u64,
    },
    PutSettings {
        revision: Box<SettingsRevision>,
    },
    CreateUser {
        user: Box<User>,
    },
    UpdateUser {
        user: Box<User>,
    },
    GetUser {
        name: String,
    },
    ListUsers,
    DeleteUser {
        name: String,
    },
    PutCredential {
        credential: Box<Credential>,
    },
    FindCredential {
        secret_hash: String,
    },
    ListCredentials {
        user: String,
    },
    DeleteCredential {
        id: uuid::Uuid,
    },
    PruneCredentials {
        #[serde(with = "time::serde::rfc3339")]
        now: time::OffsetDateTime,
    },
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
        "configure_extra_tags",
        "update_extra",
        "mark_extra_ready",
        "put_job",
        "get_job",
        "list_jobs",
        "claim_job",
        "settings_current",
        "settings_revision",
        "settings_history",
        "put_settings",
        "create_user",
        "update_user",
        "get_user",
        "list_users",
        "delete_user",
        "put_credential",
        "find_credential",
        "list_credentials",
        "delete_credential",
        "prune_credentials",
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
