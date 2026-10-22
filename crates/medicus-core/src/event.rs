//! The versioned event envelope (PLAN §3.7).
//!
//! The same JSON representation is used over the addon ABI, the admin API (`/api/v1/events`) and
//! webhooks. Consumers must ignore event kinds they do not know and fields they do not use.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::AeTitle;
use crate::uid::{
    SeriesInstanceUid, SopClassUid, SopInstanceUid, StudyInstanceUid, TransferSyntaxUid,
};

/// Current envelope schema version.
pub const ENVELOPE_VERSION: u16 = 1;

/// The kind of an event, e.g. `instance.stored`.
///
/// Core kinds are listed as variants. Addons publish namespaced kinds (`<addon>.<name>`), which
/// are carried as [`EventKind::Other`], as are kinds introduced by newer core versions.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum EventKind {
    InstanceStored,
    InstanceRejected,
    InstanceDeleted,
    SeriesStable,
    StudyStable,
    SeriesDeleted,
    StudyDeleted,
    StudyEvicting,
    StudyEvicted,
    StudyMoved,
    StoragePressure,
    StorageFull,
    StorageRecovered,
    JobStarted,
    JobProgress,
    JobFinished,
    JobFailed,
    PeerUnreachable,
    AddonUnhealthy,
    Other(String),
}

const CORE_KINDS: &[(EventKind, &str)] = &[
    (EventKind::InstanceStored, "instance.stored"),
    (EventKind::InstanceRejected, "instance.rejected"),
    (EventKind::InstanceDeleted, "instance.deleted"),
    (EventKind::SeriesStable, "series.stable"),
    (EventKind::StudyStable, "study.stable"),
    (EventKind::SeriesDeleted, "series.deleted"),
    (EventKind::StudyDeleted, "study.deleted"),
    (EventKind::StudyEvicting, "study.evicting"),
    (EventKind::StudyEvicted, "study.evicted"),
    (EventKind::StudyMoved, "study.moved"),
    (EventKind::StoragePressure, "storage.pressure"),
    (EventKind::StorageFull, "storage.full"),
    (EventKind::StorageRecovered, "storage.recovered"),
    (EventKind::JobStarted, "job.started"),
    (EventKind::JobProgress, "job.progress"),
    (EventKind::JobFinished, "job.finished"),
    (EventKind::JobFailed, "job.failed"),
    (EventKind::PeerUnreachable, "peer.unreachable"),
    (EventKind::AddonUnhealthy, "addon.unhealthy"),
];

/// Namespaces reserved for kinds emitted by the core; addons cannot publish into them.
const RESERVED_NAMESPACES: &[&str] = &[
    "instance", "series", "study", "patient", "storage", "job", "peer", "addon",
];

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EventKindError {
    #[error("event kind {0:?} must be `<namespace>.<name>` using [a-z0-9_.]")]
    Malformed(String),
    #[error("event namespace of {0:?} is reserved for the core")]
    Reserved(String),
}

impl EventKind {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Other(s) => s,
            known => CORE_KINDS
                .iter()
                .find(|(k, _)| k == known)
                .map_or("", |(_, s)| s),
        }
    }

    /// Validates a kind an addon wants to publish: well-formed and outside the core namespaces.
    pub fn addon_kind(raw: &str) -> Result<Self, EventKindError> {
        let kind: Self = raw.parse()?;
        let ns = raw.split('.').next().unwrap_or_default();
        if !matches!(kind, Self::Other(_)) || RESERVED_NAMESPACES.contains(&ns) {
            return Err(EventKindError::Reserved(raw.to_owned()));
        }
        Ok(kind)
    }

    /// Data-changing kinds that are written transactionally to the outbox.
    pub fn is_durable(&self) -> bool {
        !matches!(self, Self::Other(_) | Self::JobProgress)
    }
}

impl fmt::Display for EventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for EventKind {
    type Err = EventKindError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some((k, _)) = CORE_KINDS.iter().find(|(_, name)| *name == s) {
            return Ok(k.clone());
        }
        let well_formed = s.contains('.')
            && !s.starts_with('.')
            && !s.ends_with('.')
            && s.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.');
        if well_formed {
            Ok(Self::Other(s.to_owned()))
        } else {
            Err(EventKindError::Malformed(s.to_owned()))
        }
    }
}

impl TryFrom<String> for EventKind {
    type Error = EventKindError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<EventKind> for String {
    fn from(k: EventKind) -> Self {
        k.as_str().to_owned()
    }
}

/// Where a change came from. Only this differs between equivalent events received over
/// different protocols.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Origin {
    Dimse {
        calling_ae: AeTitle,
        called_ae: AeTitle,
        peer: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        association: Option<Uuid>,
    },
    Dicomweb {
        remote_addr: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        principal: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
    },
    AdminApi {
        remote_addr: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        principal: Option<String>,
    },
    Import {
        path: String,
    },
    Job {
        job_id: Uuid,
    },
    Internal {
        component: String,
    },
}

/// The resource an event is about. Fields not applicable to the event's level are absent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceRef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patient_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub study: Option<StudyInstanceUid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub series: Option<SeriesInstanceUid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<SopInstanceUid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sop_class: Option<SopClassUid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transfer_syntax: Option<TransferSyntaxUid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob_store: Option<String>,
}

/// An event as delivered to subscribers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// Envelope schema version.
    pub v: u16,
    /// Position in the outbox; strictly increasing for durable events, 0 for ephemeral ones.
    pub seq: u64,
    /// Unique id (`UUIDv7`), used by consumers for de-duplication.
    pub id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    pub time: OffsetDateTime,
    pub kind: EventKind,
    /// The partition the event concerns; `None` for node-wide events (e.g. `storage.full`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partition: Option<crate::Partition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<ResourceRef>,
    pub origin: Origin,
    /// Kind-specific details.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub detail: serde_json::Value,
}

impl Event {
    /// A new event with a fresh id and the current time; `seq` is assigned by the outbox.
    pub fn new(kind: EventKind, origin: Origin) -> Self {
        Self {
            v: ENVELOPE_VERSION,
            seq: 0,
            id: Uuid::now_v7(),
            time: OffsetDateTime::now_utc(),
            kind,
            partition: None,
            resource: None,
            origin,
            detail: serde_json::Value::Null,
        }
    }

    #[must_use]
    pub fn with_partition(mut self, partition: crate::Partition) -> Self {
        self.partition = Some(partition);
        self
    }

    #[must_use]
    pub fn with_resource(mut self, resource: ResourceRef) -> Self {
        self.resource = Some(resource);
        self
    }

    #[must_use]
    pub fn with_detail(mut self, detail: serde_json::Value) -> Self {
        self.detail = detail;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn kinds_roundtrip() {
        for (k, s) in CORE_KINDS {
            assert_eq!(k.as_str(), *s);
            assert_eq!(s.parse::<EventKind>().unwrap(), *k);
        }
        assert_eq!(
            "dimse.association.opened".parse::<EventKind>().unwrap(),
            EventKind::Other("dimse.association.opened".into())
        );
        assert!("Instance.Stored".parse::<EventKind>().is_err());
        assert!("nodot".parse::<EventKind>().is_err());
    }

    #[test]
    fn addons_cannot_use_core_namespaces() {
        assert!(EventKind::addon_kind("dimse.association.opened").is_ok());
        assert!(EventKind::addon_kind("instance.stored").is_err());
        assert!(EventKind::addon_kind("study.forged").is_err());
    }

    #[test]
    fn envelope_json_shape() {
        let ev = Event::new(
            EventKind::InstanceStored,
            Origin::Dimse {
                calling_ae: AeTitle::parse("MODALITY1").unwrap(),
                called_ae: AeTitle::parse("MEDICUS").unwrap(),
                peer: "10.0.0.7:51234".into(),
                association: None,
            },
        )
        .with_resource(ResourceRef {
            study: Some(StudyInstanceUid::parse("1.2.3").unwrap()),
            size: Some(512),
            ..ResourceRef::default()
        })
        .with_detail(json!({ "duplicate": "none" }));
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["v"], 1);
        assert_eq!(v["kind"], "instance.stored");
        assert_eq!(v["origin"]["type"], "dimse");
        assert_eq!(v["origin"]["calling_ae"], "MODALITY1");
        assert_eq!(v["resource"]["study"], "1.2.3");
        assert!(v["resource"].get("series").is_none());
        let back: Event = serde_json::from_value(v).unwrap();
        assert_eq!(back, ev);
    }

    #[test]
    fn unknown_kinds_from_newer_versions_deserialize() {
        let v = json!({
            "v": 1, "seq": 7, "id": "0192f0c4-7a45-7cc2-9f4e-0b7d6a1c2e3f",
            "time": "2026-10-21T20:11:05Z", "kind": "patient.merged",
            "origin": { "type": "internal", "component": "x" }, "extra": true
        });
        let ev: Event = serde_json::from_value(v).unwrap();
        assert_eq!(ev.kind, EventKind::Other("patient.merged".into()));
    }
}
