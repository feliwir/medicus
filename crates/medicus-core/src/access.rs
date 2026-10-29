//! Nodes and permissions (ADR-0003).
//!
//! A *node* is a named endpoint (a DIMSE AE title and/or a `DICOMweb` path) backed by one local
//! partition or one remote peer. Protocol addons say through which node and on whose behalf they
//! call the host ([`CallContext`]); the core resolves the caller, checks the permission and
//! scopes the call to the node's partition.

use serde::{Deserialize, Serialize};

use crate::AeTitle;
use crate::event::Origin;

/// The cookie that carries a browser session of the server (ADR-0004 §5). Protocol addons that
/// serve browsers accept it, like API tokens, through the host's `authenticate`.
pub const SESSION_COOKIE: &str = "medicus_session";

/// What a caller may do on a node, or (the `admin.*` permissions) on the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    /// Searching (C-FIND, QIDO-RS).
    Query,
    /// Reading instances (C-GET, C-MOVE source, WADO-RS).
    Retrieve,
    /// Putting instances into the node (C-STORE, STOW-RS).
    Store,
    /// Starting transfers from the node to elsewhere (C-MOVE destination, send jobs).
    Send,
    /// Server status, addons and settings; restarting the server.
    #[serde(rename = "admin.system")]
    AdminSystem,
    /// Managing user accounts.
    #[serde(rename = "admin.users")]
    AdminUsers,
    /// Reading and cancelling jobs.
    #[serde(rename = "admin.jobs")]
    AdminJobs,
    /// Listing peers and checking their connectivity.
    #[serde(rename = "admin.peers")]
    AdminPeers,
    /// Reading the event log and stream.
    #[serde(rename = "admin.events")]
    AdminEvents,
}

impl Permission {
    /// The permissions on a node's data.
    pub const DATA: [Self; 4] = [Self::Query, Self::Retrieve, Self::Store, Self::Send];
    /// The permissions on the server. They are granted on all nodes (`*`) only.
    pub const ADMIN: [Self; 5] = [
        Self::AdminSystem,
        Self::AdminUsers,
        Self::AdminJobs,
        Self::AdminPeers,
        Self::AdminEvents,
    ];

    pub fn is_admin(self) -> bool {
        Self::ADMIN.contains(&self)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Query => "query",
            Self::Retrieve => "retrieve",
            Self::Store => "store",
            Self::Send => "send",
            Self::AdminSystem => "admin.system",
            Self::AdminUsers => "admin.users",
            Self::AdminJobs => "admin.jobs",
            Self::AdminPeers => "admin.peers",
            Self::AdminEvents => "admin.events",
        }
    }
}

/// On whose behalf, and through which node, a protocol addon calls the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallContext {
    /// The node's configured name.
    pub node: String,
    /// The caller, as the protocol identified it (calling AE and address, HTTP principal, ...).
    pub origin: Origin,
}

impl CallContext {
    pub fn new(node: impl Into<String>, origin: Origin) -> Self {
        Self {
            node: node.into(),
            origin,
        }
    }
}

/// What a protocol addon needs to know about a node to serve it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeInfo {
    pub name: String,
    /// The AE title under which DIMSE serves the node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ae_title: Option<AeTitle>,
    /// The path segment under which `DICOMweb` serves the node (`/nodes/{path}/rs`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dicomweb_path: Option<String>,
    /// `true` for a node backed by a remote peer (a proxy) rather than a local partition.
    #[serde(default)]
    pub remote: bool,
}
