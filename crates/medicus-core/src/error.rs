//! The error type of capability calls (index, storage, hooks, ...).
//!
//! It is deliberately coarse: an [`ErrorKind`] that callers can act on (retry, map to a DIMSE or
//! HTTP status) and a human-readable message. Kinds map one-to-one onto ABI status codes.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorKind {
    InvalidArgument,
    NotFound,
    AlreadyExists,
    /// The state changed since the caller read it (e.g. a stale revision); re-read and retry.
    Conflict,
    /// Refused by policy (e.g. an ingest filter).
    Rejected,
    /// The caller lacks the permission for the operation on the node (ADR-0003).
    PermissionDenied,
    /// Out of space, quota or another limited resource.
    ResourceExhausted,
    /// Temporarily unavailable; the operation may be retried.
    Unavailable,
    Cancelled,
    Unsupported,
    Internal,
    /// The addon panicked while handling the call.
    Panicked,
}

impl ErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidArgument => "invalid argument",
            Self::NotFound => "not found",
            Self::AlreadyExists => "already exists",
            Self::Conflict => "conflict",
            Self::Rejected => "rejected",
            Self::PermissionDenied => "permission denied",
            Self::ResourceExhausted => "resource exhausted",
            Self::Unavailable => "unavailable",
            Self::Cancelled => "cancelled",
            Self::Unsupported => "unsupported",
            Self::Internal => "internal error",
            Self::Panicked => "addon panicked",
        }
    }

    /// Whether retrying the same operation later may succeed.
    pub fn is_transient(self) -> bool {
        matches!(self, Self::Unavailable | Self::ResourceExhausted)
    }
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{kind}: {message}")]
pub struct Error {
    pub kind: ErrorKind,
    pub message: String,
}

impl Error {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::NotFound, message)
    }

    pub fn invalid_argument(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidArgument, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Internal, message)
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Conflict, message)
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Unsupported, message)
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
