//! A Storage SCU: sends stored instances to a remote AE (C-MOVE sub-operations, send jobs).
//!
//! Instances are sent as stored: one presentation context is proposed per SOP class and transfer
//! syntax, and no transcoding takes place. An instance whose context the peer rejected fails
//! without contacting the peer.

use std::time::Duration;

use dicom_ul::association::{AsyncClientAssociation, ClientAssociationOptions};
use dicom_ul::pdu::PresentationContextResultReason;
use medicus_core::capability::ByteStream;
use tokio::net::TcpStream;

use crate::command::{Command, CommandField, Status};
use crate::{Error, message};

/// The DICOM limit on presentation contexts per association (odd IDs 1..=255).
pub const MAX_CONTEXTS: usize = 128;

/// Where and as whom to connect.
#[derive(Debug, Clone)]
pub struct ScuOptions {
    pub calling_ae: String,
    pub called_ae: String,
    /// `host:port` of the peer.
    pub address: String,
    pub max_pdu_length: u32,
    /// Timeout for connecting and for each read or write.
    pub timeout: Option<Duration>,
}

impl ScuOptions {
    pub fn new(
        calling_ae: impl Into<String>,
        called_ae: impl Into<String>,
        address: impl Into<String>,
    ) -> Self {
        Self {
            calling_ae: calling_ae.into(),
            called_ae: called_ae.into(),
            address: address.into(),
            max_pdu_length: 64 * 1024,
            timeout: Some(Duration::from_mins(1)),
        }
    }
}

/// Verifies that the peer answers DIMSE (C-ECHO); returns its status.
pub async fn echo(options: &ScuOptions) -> Result<Status, Error> {
    let mut builder = ClientAssociationOptions::new()
        .calling_ae_title(options.calling_ae.clone())
        .called_ae_title(options.called_ae.clone())
        .max_pdu_length(options.max_pdu_length)
        .with_abstract_syntax(dicom_dictionary_std::uids::VERIFICATION);
    if let Some(timeout) = options.timeout {
        builder = builder
            .connection_timeout(timeout)
            .read_timeout(timeout)
            .write_timeout(timeout);
    }
    let mut association = builder.establish_async(options.address.as_str()).await?;
    let context_id = association
        .presentation_contexts()
        .iter()
        .find(|pc| pc.reason == PresentationContextResultReason::Acceptance)
        .map(|pc| pc.id)
        .ok_or_else(|| Error::Protocol("the peer does not accept Verification".into()))?;
    message::send_command(&mut association, context_id, &Command::echo_rq(1)).await?;
    let response = message::receive_message(&mut association).await?;
    let status = Status::new(
        response
            .status()
            .ok_or_else(|| Error::Protocol("C-ECHO-RSP without a status".into()))?,
    );
    let _ = association.release().await;
    Ok(status)
}

/// An instance to send: its identity and how it is encoded.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Instance {
    pub sop_class: String,
    pub sop_instance: String,
    pub transfer_syntax: String,
}

/// The C-MOVE on whose behalf a C-STORE is sent.
#[derive(Debug, Clone)]
pub struct MoveOriginator {
    pub ae_title: String,
    pub message_id: u16,
}

/// An association for C-STORE requests.
#[derive(Debug)]
pub struct StoreScu {
    association: AsyncClientAssociation<TcpStream>,
    next_message_id: u16,
}

impl StoreScu {
    /// Connects, proposing one presentation context per distinct (SOP class, transfer syntax)
    /// of `instances` (at most [`MAX_CONTEXTS`]).
    pub async fn connect<'a>(
        options: &ScuOptions,
        instances: impl IntoIterator<Item = &'a Instance>,
    ) -> Result<Self, Error> {
        let mut contexts: Vec<(&str, &str)> = Vec::new();
        for i in instances {
            let pair = (i.sop_class.as_str(), i.transfer_syntax.as_str());
            if !contexts.contains(&pair) {
                contexts.push(pair);
            }
        }
        if contexts.len() > MAX_CONTEXTS {
            return Err(Error::Protocol(format!(
                "{} presentation contexts needed, at most {MAX_CONTEXTS} possible",
                contexts.len()
            )));
        }
        let mut builder = ClientAssociationOptions::new()
            .calling_ae_title(options.calling_ae.clone())
            .called_ae_title(options.called_ae.clone())
            .max_pdu_length(options.max_pdu_length);
        if let Some(timeout) = options.timeout {
            builder = builder
                .connection_timeout(timeout)
                .read_timeout(timeout)
                .write_timeout(timeout);
        }
        for (sop_class, ts) in contexts {
            builder = builder.with_presentation_context(sop_class.to_owned(), vec![ts.to_owned()]);
        }
        let association = builder.establish_async(options.address.as_str()).await?;
        Ok(Self {
            association,
            next_message_id: 1,
        })
    }

    /// The accepted presentation context for an instance, if any.
    fn context(&self, instance: &Instance) -> Option<u8> {
        self.association
            .presentation_contexts()
            .iter()
            .find(|pc| {
                pc.reason == PresentationContextResultReason::Acceptance
                    && pc.abstract_syntax == instance.sop_class
                    && pc.transfer_syntax == instance.transfer_syntax
            })
            .map(|pc| pc.id)
    }

    /// Sends one instance; `file` is its stored Part 10 file. Returns the peer's status, or a
    /// local failure status if no presentation context was accepted for the instance. `Err`
    /// means the association is no longer usable.
    pub async fn store(
        &mut self,
        instance: &Instance,
        file: ByteStream,
        originator: Option<&MoveOriginator>,
    ) -> Result<Status, Error> {
        let Some(context_id) = self.context(instance) else {
            return Ok(Status::new(Status::SOP_CLASS_NOT_SUPPORTED)
                .with_comment("the peer accepts no presentation context for this instance"));
        };
        let (_, data) = message::split_part10(file).await?;
        let message_id = self.next_message_id;
        self.next_message_id = self.next_message_id.wrapping_add(1).max(1);
        let mut command =
            Command::store_rq(message_id, &instance.sop_class, &instance.sop_instance);
        if let Some(originator) = originator {
            command = command.with_move_originator(&originator.ae_title, originator.message_id);
        }
        message::send_command(&mut self.association, context_id, &command).await?;
        let max_pdu = self.association.acceptor_max_pdu_length();
        message::send_data_stream(&mut self.association, context_id, max_pdu, data).await?;
        let response = message::receive_message(&mut self.association).await?;
        if response.field() != Some(CommandField::C_STORE_RSP)
            || response.message_id_being_responded_to() != Some(message_id)
        {
            return Err(Error::Protocol("unexpected response to C-STORE-RQ".into()));
        }
        let code = response
            .status()
            .ok_or_else(|| Error::Protocol("C-STORE-RSP without a status".into()))?;
        let mut status = Status::new(code);
        if let Some(comment) = response.error_comment() {
            status = status.with_comment(comment);
        }
        Ok(status)
    }

    pub async fn release(self) -> Result<(), Error> {
        Ok(self.association.release().await?)
    }

    pub async fn abort(self) -> Result<(), Error> {
        Ok(self.association.abort().await?)
    }
}
