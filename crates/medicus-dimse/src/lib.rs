//! DICOM networking for medicus: DIMSE command sets and a streaming service provider, built on
//! the dicom-rs upper layer (`dicom-ul`) and object model (`dicom-object`).

pub mod command;
pub mod scp;
pub mod storage;

pub use command::{Command, CommandField, Status};
pub use scp::{AssociationInfo, FindRequest, Handler, Matches, Scp, ScpOptions, StoreRequest};

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
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

impl From<dicom_ul::association::Error> for Error {
    fn from(e: dicom_ul::association::Error) -> Self {
        Self::Association(Box::new(e))
    }
}
