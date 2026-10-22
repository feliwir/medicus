//! DIMSE command sets (PS3.7 §9.3, §E): decoding, encoding and the values the SCP needs.
//!
//! Command sets are always encoded in Implicit VR Little Endian (PS3.7 §6.3.1).

use dicom_core::{DataElement, PrimitiveValue, VR};
use dicom_dictionary_std::tags;
use dicom_object::InMemDicomObject;
use dicom_transfer_syntax_registry::entries::IMPLICIT_VR_LITTLE_ENDIAN;

use crate::Error;

/// The Command Field (0000,0100) of a DIMSE message (PS3.7 Table E.1-1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CommandField(pub u16);

impl CommandField {
    pub const C_STORE_RQ: Self = Self(0x0001);
    pub const C_STORE_RSP: Self = Self(0x8001);
    pub const C_GET_RQ: Self = Self(0x0010);
    pub const C_GET_RSP: Self = Self(0x8010);
    pub const C_FIND_RQ: Self = Self(0x0020);
    pub const C_FIND_RSP: Self = Self(0x8020);
    pub const C_MOVE_RQ: Self = Self(0x0021);
    pub const C_MOVE_RSP: Self = Self(0x8021);
    pub const C_ECHO_RQ: Self = Self(0x0030);
    pub const C_ECHO_RSP: Self = Self(0x8030);
    pub const C_CANCEL_RQ: Self = Self(0x0FFF);

    /// Whether this is a request (as opposed to a response).
    pub fn is_request(self) -> bool {
        self.0 & 0x8000 == 0
    }

    /// The response to this request.
    #[must_use]
    pub fn response(self) -> Self {
        Self(self.0 | 0x8000)
    }
}

/// A DIMSE status code (PS3.7 Annex C) with an optional Error Comment (0000,0902).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub code: u16,
    pub comment: Option<String>,
}

impl Status {
    pub const SUCCESS: u16 = 0x0000;
    /// Pending: a match (C-FIND) or sub-operations (C-MOVE, C-GET) follow.
    pub const PENDING: u16 = 0xFF00;
    /// Cancelled by a C-CANCEL.
    pub const CANCEL: u16 = 0xFE00;
    /// Refused: SOP Class not supported.
    pub const SOP_CLASS_NOT_SUPPORTED: u16 = 0x0122;
    /// Refused: not authorized.
    pub const NOT_AUTHORIZED: u16 = 0x0124;
    /// Unrecognized operation.
    pub const UNRECOGNIZED_OPERATION: u16 = 0x0211;
    /// C-STORE: Refused: Out of resources (A7xx).
    pub const OUT_OF_RESOURCES: u16 = 0xA700;
    /// C-STORE: Error: Data Set does not match SOP Class (A9xx).
    pub const DATA_SET_MISMATCH: u16 = 0xA900;
    /// C-FIND/C-MOVE/C-GET: Identifier does not match SOP Class (A900).
    pub const IDENTIFIER_MISMATCH: u16 = 0xA900;
    /// C-STORE: Error: Cannot understand (Cxxx); C-FIND etc.: Unable to process.
    pub const CANNOT_UNDERSTAND: u16 = 0xC000;

    /// Maximum length of an Error Comment (LO).
    const MAX_COMMENT: usize = 64;

    pub fn success() -> Self {
        Self::new(Self::SUCCESS)
    }

    pub fn new(code: u16) -> Self {
        Self {
            code,
            comment: None,
        }
    }

    /// Adds an Error Comment, truncated to the 64 characters an LO value may hold.
    #[must_use]
    pub fn with_comment(mut self, comment: impl Into<String>) -> Self {
        let mut comment: String = comment
            .into()
            .chars()
            .filter(|c| !c.is_control() && *c != '\\')
            .take(Self::MAX_COMMENT)
            .collect();
        comment.truncate(comment.trim_end().len());
        self.comment = Some(comment);
        self
    }

    pub fn is_success(&self) -> bool {
        self.code == Self::SUCCESS
    }
}

impl From<&medicus_core::Error> for Status {
    /// The C-STORE status for an ingest failure: `A700` for conditions the sender may retry
    /// later (full storage, temporarily unavailable), `C000` for everything else.
    fn from(e: &medicus_core::Error) -> Self {
        let code = if e.kind.is_transient() {
            Self::OUT_OF_RESOURCES
        } else {
            Self::CANNOT_UNDERSTAND
        };
        Self::new(code).with_comment(e.message.clone())
    }
}

/// Value of Command Data Set Type (0000,0800) meaning "no data set".
const NO_DATA_SET: u16 = 0x0101;

/// A decoded command set.
#[derive(Debug, Clone)]
pub struct Command(InMemDicomObject);

impl Command {
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        InMemDicomObject::read_dataset_with_ts(bytes, &IMPLICIT_VR_LITTLE_ENDIAN.erased())
            .map(Self)
            .map_err(|e| Error::Protocol(format!("cannot decode command set: {e}")))
    }

    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        let mut out = Vec::new();
        self.0
            .write_dataset_with_ts(&mut out, &IMPLICIT_VR_LITTLE_ENDIAN.erased())
            .map_err(|e| Error::Protocol(format!("cannot encode command set: {e}")))?;
        Ok(out)
    }

    fn u16(&self, tag: dicom_core::Tag) -> Option<u16> {
        self.0.get(tag)?.to_int::<u16>().ok()
    }

    fn uid(&self, tag: dicom_core::Tag) -> Option<String> {
        let s = self.0.get(tag)?.to_str().ok()?;
        Some(s.trim_end_matches(['\0', ' ']).to_owned())
    }

    fn text(&self, tag: dicom_core::Tag) -> Option<String> {
        let s = self.0.get(tag)?.to_str().ok()?;
        Some(s.trim().to_owned())
    }

    pub fn field(&self) -> Option<CommandField> {
        self.u16(tags::COMMAND_FIELD).map(CommandField)
    }

    pub fn message_id(&self) -> Option<u16> {
        self.u16(tags::MESSAGE_ID)
    }

    pub fn message_id_being_responded_to(&self) -> Option<u16> {
        self.u16(tags::MESSAGE_ID_BEING_RESPONDED_TO)
    }

    pub fn status(&self) -> Option<u16> {
        self.u16(tags::STATUS)
    }

    pub fn affected_sop_class(&self) -> Option<String> {
        self.uid(tags::AFFECTED_SOP_CLASS_UID)
    }

    pub fn affected_sop_instance(&self) -> Option<String> {
        self.uid(tags::AFFECTED_SOP_INSTANCE_UID)
    }

    /// The Move Originator (AE title and message ID) of a C-STORE issued by a C-MOVE.
    pub fn move_originator(&self) -> Option<(String, u16)> {
        Some((
            self.text(tags::MOVE_ORIGINATOR_APPLICATION_ENTITY_TITLE)?,
            self.u16(tags::MOVE_ORIGINATOR_MESSAGE_ID)?,
        ))
    }

    /// Whether a data set follows the command (Command Data Set Type other than `0101H`).
    pub fn has_data_set(&self) -> bool {
        self.u16(tags::COMMAND_DATA_SET_TYPE)
            .is_some_and(|t| t != NO_DATA_SET)
    }

    /// A C-ECHO-RQ.
    pub fn echo_rq(message_id: u16) -> Self {
        Self(InMemDicomObject::command_from_element_iter([
            uid(
                tags::AFFECTED_SOP_CLASS_UID,
                dicom_dictionary_std::uids::VERIFICATION,
            ),
            us(tags::COMMAND_FIELD, CommandField::C_ECHO_RQ.0),
            us(tags::MESSAGE_ID, message_id),
            us(tags::COMMAND_DATA_SET_TYPE, NO_DATA_SET),
        ]))
    }

    /// A C-STORE-RQ (medium priority) announcing a data set.
    pub fn store_rq(message_id: u16, sop_class: &str, sop_instance: &str) -> Self {
        Self(InMemDicomObject::command_from_element_iter([
            uid(tags::AFFECTED_SOP_CLASS_UID, sop_class),
            us(tags::COMMAND_FIELD, CommandField::C_STORE_RQ.0),
            us(tags::MESSAGE_ID, message_id),
            us(tags::PRIORITY, 0x0000),
            us(tags::COMMAND_DATA_SET_TYPE, 0x0000),
            uid(tags::AFFECTED_SOP_INSTANCE_UID, sop_instance),
        ]))
    }

    /// A response to `request` followed by a data set (e.g. a C-FIND match).
    pub fn pending_response_to(request: &Self, status: &Status) -> Self {
        let mut response = Self::response_to(request, status);
        response.0.put(us(tags::COMMAND_DATA_SET_TYPE, 0x0000));
        // Group length must be recomputed after the change.
        Self(InMemDicomObject::command_from_element_iter(
            response
                .0
                .into_iter()
                .filter(|e| e.header().tag != tags::COMMAND_GROUP_LENGTH),
        ))
    }

    /// The response to `request`, without a data set.
    pub fn response_to(request: &Self, status: &Status) -> Self {
        let field = request
            .field()
            .map_or(CommandField(0x8000), CommandField::response);
        let mut elements = vec![
            us(tags::COMMAND_FIELD, field.0),
            us(
                tags::MESSAGE_ID_BEING_RESPONDED_TO,
                request.message_id().unwrap_or(0),
            ),
            us(tags::COMMAND_DATA_SET_TYPE, NO_DATA_SET),
            us(tags::STATUS, status.code),
        ];
        if let Some(class) = request.affected_sop_class() {
            elements.push(uid(tags::AFFECTED_SOP_CLASS_UID, &class));
        }
        // Affected SOP Instance UID is part of C-STORE-RSP only (PS3.7 Table 9.3-2).
        if field == CommandField::C_STORE_RSP
            && let Some(instance) = request.affected_sop_instance()
        {
            elements.push(uid(tags::AFFECTED_SOP_INSTANCE_UID, &instance));
        }
        if let Some(comment) = &status.comment {
            elements.push(DataElement::new(
                tags::ERROR_COMMENT,
                VR::LO,
                PrimitiveValue::from(comment.as_str()),
            ));
        }
        Self(InMemDicomObject::command_from_element_iter(elements))
    }
}

fn us(tag: dicom_core::Tag, value: u16) -> dicom_object::mem::InMemElement {
    DataElement::new(tag, VR::US, PrimitiveValue::from(value))
}

fn uid(tag: dicom_core::Tag, value: &str) -> dicom_object::mem::InMemElement {
    DataElement::new(tag, VR::UI, PrimitiveValue::from(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dicom_dictionary_std::uids;

    #[test]
    fn store_response_round_trips() {
        let rq = Command::store_rq(7, uids::CT_IMAGE_STORAGE, "1.2.3.4");
        let rq = Command::decode(&rq.encode().unwrap()).unwrap();
        assert_eq!(rq.field(), Some(CommandField::C_STORE_RQ));
        assert!(rq.has_data_set());
        assert_eq!(rq.affected_sop_instance().as_deref(), Some("1.2.3.4"));

        let status = Status::new(Status::OUT_OF_RESOURCES).with_comment("disk\\full\n");
        let rsp = Command::decode(&Command::response_to(&rq, &status).encode().unwrap()).unwrap();
        assert_eq!(rsp.field(), Some(CommandField::C_STORE_RSP));
        assert_eq!(rsp.message_id_being_responded_to(), Some(7));
        assert_eq!(rsp.status(), Some(0xA700));
        assert!(!rsp.has_data_set());
        assert_eq!(
            rsp.affected_sop_class().as_deref(),
            Some(uids::CT_IMAGE_STORAGE)
        );
        assert_eq!(rsp.affected_sop_instance().as_deref(), Some("1.2.3.4"));
        assert_eq!(rsp.text(tags::ERROR_COMMENT).as_deref(), Some("diskfull"));
    }

    #[test]
    fn comments_are_limited_to_64_characters() {
        let s = Status::new(Status::CANNOT_UNDERSTAND).with_comment("x".repeat(100));
        assert_eq!(s.comment.unwrap().len(), 64);
    }
}
