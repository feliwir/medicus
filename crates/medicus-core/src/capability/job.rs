//! Outbound work (PLAN §3.5): persisted jobs, scheduled by the core and executed by the
//! protocol addon of the target peer through [`RemoteNode`].

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::capability::host::Peer;
use crate::capability::index::ResourceUid;
use crate::error::Result;
use crate::uid::SopInstanceUid;
use crate::{NodeInfo, Partition};

/// What a job does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JobKind {
    /// Send studies, series or instances to a peer.
    Send {
        peer: String,
        resources: Vec<ResourceUid>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    /// Waiting to run (again, after a failed attempt, once `not_before` has passed).
    Queued,
    Running,
    Succeeded,
    /// Gave up after the last attempt.
    Failed,
    Cancelled,
}

impl JobState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn is_final(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
}

/// Counts of the instances a send job handled (in its current attempt).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobProgress {
    pub total: u64,
    pub completed: u64,
    pub failed: u64,
    pub warning: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Job {
    pub id: Uuid,
    /// The partition the job's data belongs to (for a send: the source).
    pub partition: Partition,
    /// The node the job was submitted through; its executor acts as that node.
    pub node: String,
    #[serde(flatten)]
    pub kind: JobKind,
    pub state: JobState,
    /// Attempts started so far.
    pub attempts: u32,
    pub max_attempts: u32,
    #[serde(default)]
    pub progress: JobProgress,
    /// Instances still to send after a partly failed attempt; `None` before the first attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<Vec<SopInstanceUid>>,
    /// Why the last attempt failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
    /// Earliest start of the next attempt.
    #[serde(with = "time::serde::rfc3339")]
    pub not_before: OffsetDateTime,
}

impl Job {
    pub fn new(partition: Partition, node: String, kind: JobKind, max_attempts: u32) -> Self {
        let now = OffsetDateTime::now_utc();
        Self {
            id: Uuid::now_v7(),
            partition,
            node,
            kind,
            state: JobState::Queued,
            attempts: 0,
            max_attempts: max_attempts.max(1),
            progress: JobProgress::default(),
            pending: None,
            error: None,
            created_at: now,
            updated_at: now,
            not_before: now,
        }
    }

    /// The job's id as a DICOM UID (`2.25.<UUID as integer>`, PS3.5 B.2), e.g. for a `DICOMweb`
    /// Transaction UID. [`Self::id_from_transaction_uid`] reverses it.
    pub fn transaction_uid(&self) -> String {
        format!("2.25.{}", self.id.as_u128())
    }

    /// The job id of a UID made by [`Self::transaction_uid`].
    pub fn id_from_transaction_uid(uid: &str) -> Option<Uuid> {
        let digits = uid.strip_prefix("2.25.")?;
        // A UID component has no leading zeros (PS3.5 9.1); this keeps the mapping one-to-one.
        if digits.starts_with('0') && digits != "0" {
            return None;
        }
        digits.parse::<u128>().ok().map(Uuid::from_u128)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transaction_uids_round_trip() {
        let job = Job::new(
            Partition::default_partition(),
            "MEDICUS".into(),
            JobKind::Send {
                peer: "p".into(),
                resources: Vec::new(),
            },
            1,
        );
        let uid = job.transaction_uid();
        assert!(crate::uid::Uid::parse(&uid).is_ok(), "{uid}");
        assert!(uid.len() <= 64);
        assert_eq!(Job::id_from_transaction_uid(&uid), Some(job.id));
        assert_eq!(Job::id_from_transaction_uid("1.2.3"), None);
        assert_eq!(Job::id_from_transaction_uid("2.25.01"), None);
    }
}

/// How a send went.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendOutcome {
    pub completed: u64,
    pub warning: u64,
    /// Instances the peer did not store (or that could not be sent).
    pub failed: Vec<SopInstanceUid>,
}

/// Wire format of `RemoteNode` calls over the ABI (one JSON request per call).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum RemoteRequest {
    Echo {
        peer: Peer,
    },
    Send {
        job: Uuid,
        node: NodeInfo,
        peer: Peer,
        instances: Vec<SopInstanceUid>,
    },
}

/// A protocol client for peers, registered under the protocol's name (e.g. `dimse`).
#[async_trait]
pub trait RemoteNode: Send + Sync + 'static {
    /// Checks that the peer is reachable and answers (e.g. C-ECHO).
    async fn echo(&self, peer: &Peer) -> Result<()>;

    /// Sends stored instances of `node` to the peer for job `job`, acting as that node (e.g. its
    /// AE title) and reading through it with a job origin. Progress goes to
    /// [`HostServices::job_progress`](crate::capability::HostServices::job_progress), which also
    /// says when the job was cancelled. An `Err` means nothing more could be sent (e.g. the peer
    /// is unreachable); per-instance failures are part of the outcome.
    async fn send(
        &self,
        job: Uuid,
        node: &NodeInfo,
        peer: &Peer,
        instances: &[SopInstanceUid],
    ) -> Result<SendOutcome>;
}
