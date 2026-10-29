//! Server state kept in the index besides the archive (ADR-0004 §3): revisions of the settings
//! document, user accounts and their credentials (browser sessions and API tokens).
//!
//! Keeping them in the index means every server sharing an index shares them too. The index stores
//! them as given; hashing passwords and secrets, and deciding what the settings mean, is the core's
//! job.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

/// One revision of the settings document. Revisions are numbered from 1 without gaps, and a
/// revision is never changed once written: rolling back writes a new revision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettingsRevision {
    pub revision: u64,
    /// The whole settings document. Its structure is defined by the core, not by the index.
    pub document: serde_json::Value,
    /// The user who made the change; `None` for the server itself (e.g. the import of the
    /// bootstrap file on first start).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    /// Why the change was made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// A local user account. Users are principals (ADR-0003): their roles grant permissions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    /// The login name; unique and never changed.
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// The password as a PHC string (argon2id); `None` if the user cannot log in with a password.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_hash: Option<String>,
    #[serde(default)]
    pub roles: Vec<String>,
    /// A disabled user cannot log in, and their credentials are not accepted.
    #[serde(default)]
    pub disabled: bool,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialKind {
    /// A browser session, created by logging in.
    Session,
    /// An API token, created by the user for scripts and other clients.
    ApiToken,
}

/// A secret that authenticates a user: a session cookie or an API token. Only a hash of the
/// secret is stored, so the index never holds anything that could be presented as a credential.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credential {
    pub id: Uuid,
    /// The name of the user it belongs to. Deleting the user deletes the credential.
    pub user: String,
    pub kind: CredentialKind,
    /// The SHA-256 of the secret, in lowercase hex; unique.
    pub secret_hash: String,
    /// A label chosen by the user (API tokens).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// After this, the credential is no longer accepted; `None` never expires.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub expires_at: Option<OffsetDateTime>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub last_used_at: Option<OffsetDateTime>,
}

impl Credential {
    pub fn is_expired(&self, now: OffsetDateTime) -> bool {
        self.expires_at.is_some_and(|t| t <= now)
    }
}
