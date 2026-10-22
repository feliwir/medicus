//! DICOM networking for medicus: DIMSE command sets and a streaming service provider, built on
//! the dicom-rs upper layer (`dicom-ul`) and object model (`dicom-object`).

pub mod command;
mod message;
pub mod scp;
pub mod scu;
pub mod storage;

pub use command::{Command, CommandField, Status};
pub use scp::{
    AssociationInfo, Destination, FindRequest, Handler, Matches, RetrieveRequest, Scp, ScpOptions,
    StoreRequest,
};
pub use scu::{Instance, MAX_CONTEXTS, MoveOriginator, ScuOptions, StoreScu, echo};

/// Implementation Class UID (0002,0012) of files received by medicus.
pub const IMPLEMENTATION_CLASS_UID: &str = "2.25.21299839958676251082708783506226217531";
/// Implementation Version Name (0002,0013) of files received by medicus.
pub const IMPLEMENTATION_VERSION_NAME: &str = concat!("MEDICUS_", env!("CARGO_PKG_VERSION"));

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("DIMSE protocol error: {0}")]
    Protocol(String),
    #[error("association error: {0}")]
    Association(#[from] Box<dicom_ul::association::Error>),
    #[error("association aborted by the peer")]
    Aborted,
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Reading the data to send failed.
    #[error("{0}")]
    Source(medicus_core::Error),
}

impl Error {
    /// The peer was reached but accepted none of the proposed presentation contexts.
    pub fn is_no_accepted_contexts(&self) -> bool {
        matches!(
            self,
            Self::Association(e)
                if matches!(**e, dicom_ul::association::Error::NoAcceptedPresentationContexts { .. })
        )
    }
}

impl From<dicom_ul::association::Error> for Error {
    fn from(e: dicom_ul::association::Error) -> Self {
        Self::Association(Box::new(e))
    }
}
