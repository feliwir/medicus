//! Degraded mode (ADR-0005 §3): the archive index could not be opened, but the state store could.
//! The server then runs with [`UnavailableIndex`] as the archive's index, so the admin UI and API
//! keep working and every archive operation fails with `Unavailable`, until an administrator
//! corrects the settings and restarts the server.

use async_trait::async_trait;
use medicus_core::capability::index::{
    DeleteOutcome, DicomJsonObject, DuplicatePolicy, ExtraTag, ExtraTagStatus, InstanceLocation,
    InstanceRecord, QueryResult, ResourceUid, StoreOutcome, StoredBytes,
};
use medicus_core::capability::{Credential, IndexStore, Job, JobState, SettingsRevision, User};
use medicus_core::event::Event;
use medicus_core::query::Query;
use medicus_core::uid::SopInstanceUid;
use medicus_core::{Error, ErrorKind, Partition, Result};

/// Why the server runs degraded, as `GET /api/v1/system` reports it.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct Degraded {
    /// The archive index that could not be opened.
    pub(crate) index: String,
    pub(crate) error: String,
}

/// An index store whose every call fails with `Unavailable`.
pub(crate) struct UnavailableIndex(String);

impl UnavailableIndex {
    pub(crate) fn new(degraded: &Degraded) -> Self {
        Self(format!(
            "the archive index {:?} is unavailable: {}",
            degraded.index, degraded.error
        ))
    }

    fn fail<T>(&self) -> Result<T> {
        Err(Error::new(ErrorKind::Unavailable, self.0.clone()))
    }
}

#[async_trait]
impl IndexStore for UnavailableIndex {
    async fn store_instance(
        &self,
        _: &InstanceRecord,
        _: DuplicatePolicy,
        _: &Event,
    ) -> Result<StoreOutcome> {
        self.fail()
    }
    async fn query(&self, _: &Partition, _: &Query) -> Result<QueryResult> {
        self.fail()
    }
    async fn locate(&self, _: &Partition, _: &ResourceUid) -> Result<Vec<InstanceLocation>> {
        self.fail()
    }
    async fn delete(&self, _: &Partition, _: &ResourceUid, _: &Event) -> Result<DeleteOutcome> {
        self.fail()
    }
    async fn append_event(&self, _: &Event) -> Result<u64> {
        self.fail()
    }
    async fn events_after(&self, _: u64, _: u32) -> Result<Vec<Event>> {
        self.fail()
    }
    async fn last_event_seq(&self) -> Result<u64> {
        self.fail()
    }
    async fn cursor(&self, _: &str) -> Result<Option<u64>> {
        self.fail()
    }
    async fn set_cursor(&self, _: &str, _: u64) -> Result<()> {
        self.fail()
    }
    async fn prune_events(&self, _: u64) -> Result<u64> {
        self.fail()
    }
    async fn stored_bytes(&self) -> Result<Vec<StoredBytes>> {
        self.fail()
    }
    async fn configure_extra_tags(&self, _: &[ExtraTag]) -> Result<Vec<ExtraTagStatus>> {
        self.fail()
    }
    async fn update_extra(
        &self,
        _: &Partition,
        _: &SopInstanceUid,
        _: &DicomJsonObject,
    ) -> Result<()> {
        self.fail()
    }
    async fn mark_extra_ready(&self, _: medicus_core::Tag) -> Result<()> {
        self.fail()
    }
    async fn put_job(&self, _: &Job) -> Result<()> {
        self.fail()
    }
    async fn get_job(&self, _: uuid::Uuid) -> Result<Option<Job>> {
        self.fail()
    }
    async fn list_jobs(&self, _: Option<JobState>, _: u32, _: u64) -> Result<Vec<Job>> {
        self.fail()
    }
    async fn claim_job(&self, _: time::OffsetDateTime) -> Result<Option<Job>> {
        self.fail()
    }
    async fn settings_current(&self) -> Result<Option<SettingsRevision>> {
        self.fail()
    }
    async fn settings_revision(&self, _: u64) -> Result<Option<SettingsRevision>> {
        self.fail()
    }
    async fn settings_history(&self, _: u32, _: u64) -> Result<Vec<SettingsRevision>> {
        self.fail()
    }
    async fn put_settings(&self, _: &SettingsRevision) -> Result<()> {
        self.fail()
    }
    async fn create_user(&self, _: &User) -> Result<()> {
        self.fail()
    }
    async fn update_user(&self, _: &User) -> Result<()> {
        self.fail()
    }
    async fn get_user(&self, _: &str) -> Result<Option<User>> {
        self.fail()
    }
    async fn list_users(&self) -> Result<Vec<User>> {
        self.fail()
    }
    async fn delete_user(&self, _: &str) -> Result<()> {
        self.fail()
    }
    async fn put_credential(&self, _: &Credential) -> Result<()> {
        self.fail()
    }
    async fn find_credential(&self, _: &str) -> Result<Option<Credential>> {
        self.fail()
    }
    async fn list_credentials(&self, _: &str) -> Result<Vec<Credential>> {
        self.fail()
    }
    async fn delete_credential(&self, _: uuid::Uuid) -> Result<()> {
        self.fail()
    }
    async fn prune_credentials(&self, _: time::OffsetDateTime) -> Result<u64> {
        self.fail()
    }
}
