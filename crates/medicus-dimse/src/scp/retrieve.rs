//! C-MOVE and C-GET (PS3.4 C.4.2, C.4.3): C-STORE sub-operations with progress responses.
//!
//! C-MOVE sends over a new association to the destination AE; C-GET sends on the requesting
//! association, to a peer that took the SCP role for the instance's storage class. Either way a
//! pending response with the sub-operation counts follows each instance, and a C-CANCEL lets the
//! running sub-operation finish and then ends the retrieval with FE00.

use dicom_core::{DataElement, PrimitiveValue, VR};
use dicom_dictionary_std::tags;
use dicom_encoding::TransferSyntaxIndex as _;
use dicom_object::InMemDicomObject;
use dicom_transfer_syntax_registry::TransferSyntaxRegistry;
use dicom_ul::Pdu;
use dicom_ul::association::Association as _;
use dicom_ul::pdu::PresentationContextResultReason;

use super::{Destination, Handler, RetrieveRequest, Session, encode_data_set};
use crate::command::{Command, CommandField, Status};
use crate::scu::{Instance, MoveOriginator, ScuOptions, StoreScu};
use crate::{Error, message};

/// Counts of C-STORE sub-operations (PS3.4 C.4.2.1.4).
#[derive(Debug, Default)]
struct Progress {
    remaining: usize,
    completed: usize,
    failed: usize,
    warning: usize,
    failed_uids: Vec<String>,
}

impl Progress {
    fn new(total: usize) -> Self {
        Self {
            remaining: total,
            ..Self::default()
        }
    }

    fn record(&mut self, instance: &Instance, status: &Status) {
        self.remaining = self.remaining.saturating_sub(1);
        if status.is_success() {
            self.completed += 1;
        } else if status.is_warning() {
            self.warning += 1;
        } else {
            tracing::warn!(
                instance = %instance.sop_instance,
                sop_class = %instance.sop_class,
                transfer_syntax = %instance.transfer_syntax,
                status = format_args!("{:#06x}", status.code),
                comment = status.comment.as_deref().unwrap_or(""),
                "C-STORE sub-operation failed"
            );
            self.failed += 1;
            self.failed_uids.push(instance.sop_instance.clone());
        }
    }

    /// The count elements of a response; Remaining only while pending.
    fn elements(&self, pending: bool) -> Vec<dicom_object::mem::InMemElement> {
        let us = |tag, n: usize| {
            DataElement::new(
                tag,
                VR::US,
                PrimitiveValue::from(u16::try_from(n).unwrap_or(u16::MAX)),
            )
        };
        let mut out = vec![
            us(tags::NUMBER_OF_COMPLETED_SUBOPERATIONS, self.completed),
            us(tags::NUMBER_OF_FAILED_SUBOPERATIONS, self.failed),
            us(tags::NUMBER_OF_WARNING_SUBOPERATIONS, self.warning),
        ];
        if pending {
            out.push(us(tags::NUMBER_OF_REMAINING_SUBOPERATIONS, self.remaining));
        }
        out
    }

    /// The final status once all sub-operations ran.
    fn final_status(&self) -> Status {
        if self.failed == 0 && self.warning == 0 {
            Status::success()
        } else {
            Status::new(Status::SUBOPERATIONS_WARNING)
        }
    }
}

/// How the sub-operations ended.
enum Outcome {
    Done,
    Cancelled,
    /// The peer aborted the association.
    Aborted,
    /// Sub-operations could not run at all.
    Refused(Status),
}

/// What happened first while a C-MOVE sub-operation ran.
enum MoveEvent<T> {
    Stored(T),
    Pdu(Result<Pdu, dicom_ul::association::Error>),
}

impl<H: Handler> Session<H> {
    pub(super) async fn retrieve(
        &mut self,
        command: &Command,
        context_id: u8,
        identifier: &[u8],
        transfer_syntax: &str,
    ) -> Result<(), Error> {
        let Some(ts) = TransferSyntaxRegistry.get(transfer_syntax) else {
            return Err(Error::Protocol(format!(
                "unknown transfer syntax {transfer_syntax}"
            )));
        };
        let identifier = match InMemDicomObject::read_dataset_with_ts(identifier, ts) {
            Ok(identifier) => identifier,
            Err(e) => {
                let status = Status::new(Status::IDENTIFIER_MISMATCH)
                    .with_comment(format!("cannot decode identifier: {e}"));
                return self.respond(command, context_id, &status).await;
            }
        };
        let is_move = command.field() == Some(CommandField::C_MOVE_RQ);
        let (Some(message_id), Some(sop_class)) =
            (command.message_id(), command.affected_sop_class())
        else {
            let status = Status::new(Status::CANNOT_UNDERSTAND)
                .with_comment("request lacks required command elements");
            return self.respond(command, context_id, &status).await;
        };
        let destination = if is_move {
            let ae = command.move_destination().unwrap_or_default();
            let Some(destination) = self.handler.destination(&ae).await else {
                let status = Status::new(Status::MOVE_DESTINATION_UNKNOWN)
                    .with_comment(format!("unknown destination {ae:?}"));
                return self.respond(command, context_id, &status).await;
            };
            Some(destination)
        } else {
            None
        };
        let request = RetrieveRequest {
            message_id,
            sop_class,
            destination: destination.as_ref().map(|d| d.ae_title.clone()),
        };
        let instances = match self
            .handler
            .retrieve(self.info.clone(), request, identifier)
            .await
        {
            Ok(instances) => instances,
            Err(status) => return self.respond(command, context_id, &status).await,
        };
        tracing::info!(
            association = %self.info.id,
            destination = destination.as_ref().map_or("(C-GET)", |d| d.ae_title.as_str()),
            instances = instances.len(),
            "retrieve started"
        );
        let mut progress = Progress::new(instances.len());
        let outcome = match &destination {
            Some(destination) => {
                self.move_to(
                    destination,
                    command,
                    context_id,
                    message_id,
                    &instances,
                    &mut progress,
                )
                .await?
            }
            None => {
                self.get(command, context_id, message_id, &instances, &mut progress)
                    .await?
            }
        };
        let status = match outcome {
            Outcome::Aborted => return Ok(()),
            Outcome::Refused(status) => status,
            Outcome::Cancelled => Status::new(Status::CANCEL),
            Outcome::Done => progress.final_status(),
        };
        tracing::info!(
            association = %self.info.id,
            completed = progress.completed,
            failed = progress.failed,
            warning = progress.warning,
            status = format_args!("{:#06x}", status.code),
            "retrieve finished"
        );
        self.finish(command, context_id, ts, &status, &progress)
            .await
    }

    /// Sends the final response with the counts and, after failures, the identifier listing the
    /// failed instances.
    async fn finish(
        &mut self,
        command: &Command,
        context_id: u8,
        ts: &dicom_encoding::TransferSyntax,
        status: &Status,
        progress: &Progress,
    ) -> Result<(), Error> {
        let response =
            Command::response_to(command, status).with_elements(progress.elements(false));
        if progress.failed_uids.is_empty() {
            return self.send_message(context_id, &response, None).await;
        }
        let failed = InMemDicomObject::from_element_iter([DataElement::new(
            tags::FAILED_SOP_INSTANCE_UID_LIST,
            VR::UI,
            PrimitiveValue::Strs(progress.failed_uids.iter().cloned().collect()),
        )]);
        let data = encode_data_set(&failed, ts)?;
        let response = response.with_elements([DataElement::new(
            tags::COMMAND_DATA_SET_TYPE,
            VR::US,
            PrimitiveValue::from(0x0000_u16),
        )]);
        self.send_message(context_id, &response, Some(&data)).await
    }

    /// Sends a pending response with the current counts.
    async fn progress(
        &mut self,
        command: &Command,
        context_id: u8,
        progress: &Progress,
    ) -> Result<(), Error> {
        let response = Command::response_to(command, &Status::new(Status::PENDING))
            .with_elements(progress.elements(true));
        self.send_message(context_id, &response, None).await
    }

    /// C-MOVE: C-STORE sub-operations over a new association to `destination`.
    async fn move_to(
        &mut self,
        destination: &Destination,
        command: &Command,
        context_id: u8,
        message_id: u16,
        instances: &[Instance],
        progress: &mut Progress,
    ) -> Result<Outcome, Error> {
        if instances.is_empty() {
            return Ok(Outcome::Done);
        }
        let options = ScuOptions::new(
            self.ae_title.clone(),
            destination.ae_title.clone(),
            destination.address.clone(),
        );
        let mut scu = match StoreScu::connect(&options, instances).await {
            Ok(scu) => scu,
            Err(e) if e.is_no_accepted_contexts() => {
                let failure = Status::new(Status::SOP_CLASS_NOT_SUPPORTED)
                    .with_comment("the destination accepts none of the instances as stored");
                for instance in instances {
                    progress.record(instance, &failure);
                }
                return Ok(Outcome::Done);
            }
            Err(e) => {
                tracing::warn!(association = %self.info.id, destination = %destination.ae_title, error = %e, "cannot connect to the C-MOVE destination");
                return Ok(Outcome::Refused(
                    Status::new(Status::UNABLE_TO_PERFORM_SUBOPERATIONS)
                        .with_comment(e.to_string()),
                ));
            }
        };
        let originator = MoveOriginator {
            ae_title: self.info.calling_ae.clone(),
            message_id,
        };
        let handler = self.handler.clone();
        let mut cancelled = false;
        for (i, instance) in instances.iter().enumerate() {
            if cancelled {
                break;
            }
            let sub_operation = async {
                match handler.open(instance).await {
                    Ok(file) => scu.store(instance, file, Some(&originator)).await,
                    Err(status) => Ok(status),
                }
            };
            tokio::pin!(sub_operation);
            let result = loop {
                let event = tokio::select! {
                    result = &mut sub_operation => MoveEvent::Stored(result),
                    pdu = self.association.receive(), if !cancelled => MoveEvent::Pdu(pdu),
                };
                match event {
                    MoveEvent::Stored(result) => break result,
                    MoveEvent::Pdu(pdu) => match pdu? {
                        Pdu::PData { data } => cancelled |= self.is_cancel(data, message_id)?,
                        Pdu::AbortRQ { .. } => {
                            self.closed = true;
                            return Ok(Outcome::Aborted);
                        }
                        other => {
                            return Err(Error::Protocol(format!(
                                "unexpected PDU during C-MOVE: {}",
                                other.short_description()
                            )));
                        }
                    },
                }
            };
            match result {
                Ok(status) => progress.record(instance, &status),
                Err(e) => {
                    // The destination association broke: the rest cannot be sent.
                    tracing::warn!(association = %self.info.id, destination = %destination.ae_title, error = %e, "C-MOVE destination association failed");
                    let failure = Status::new(Status::UNABLE_TO_PERFORM_SUBOPERATIONS);
                    for instance in &instances[i..] {
                        progress.record(instance, &failure);
                    }
                    return Ok(Outcome::Done);
                }
            }
            if progress.remaining > 0 && !cancelled {
                self.progress(command, context_id, progress).await?;
            }
        }
        let _ = scu.release().await;
        Ok(if cancelled {
            Outcome::Cancelled
        } else {
            Outcome::Done
        })
    }

    /// C-GET: C-STORE sub-operations on this association.
    async fn get(
        &mut self,
        command: &Command,
        context_id: u8,
        message_id: u16,
        instances: &[Instance],
        progress: &mut Progress,
    ) -> Result<Outcome, Error> {
        let mut cancelled = false;
        for instance in instances {
            if cancelled {
                break;
            }
            let status = self.get_one(instance, message_id, &mut cancelled).await?;
            if self.closed {
                return Ok(Outcome::Aborted);
            }
            progress.record(instance, &status);
            if progress.remaining > 0 && !cancelled {
                self.progress(command, context_id, progress).await?;
            }
        }
        Ok(if cancelled {
            Outcome::Cancelled
        } else {
            Outcome::Done
        })
    }

    /// Sends one instance to the C-GET requestor and returns its status. A C-CANCEL for
    /// `message_id` received meanwhile sets `cancelled`.
    async fn get_one(
        &mut self,
        instance: &Instance,
        message_id: u16,
        cancelled: &mut bool,
    ) -> Result<Status, Error> {
        let roles = self.association.requestor_roles_for(&instance.sop_class);
        let context = self
            .association
            .presentation_contexts()
            .iter()
            .find(|pc| {
                pc.reason == PresentationContextResultReason::Acceptance
                    && pc.abstract_syntax == instance.sop_class
                    && pc.transfer_syntax == instance.transfer_syntax
            })
            .map(|pc| pc.id);
        let Some(context_id) = context.filter(|_| roles.scp) else {
            return Ok(Status::new(Status::SOP_CLASS_NOT_SUPPORTED).with_comment(
                "no presentation context with the SCP role for this instance's SOP class and transfer syntax",
            ));
        };
        let file = match self.handler.open(instance).await {
            Ok(file) => file,
            Err(status) => return Ok(status),
        };
        let data = match message::split_part10(file).await {
            Ok((_, data)) => data,
            Err(e) => {
                return Ok(Status::new(Status::UNABLE_TO_PERFORM_SUBOPERATIONS)
                    .with_comment(e.to_string()));
            }
        };
        let sub_id = self.next_message_id;
        self.next_message_id = self.next_message_id.wrapping_add(1).max(1);
        let request = Command::store_rq(sub_id, &instance.sop_class, &instance.sop_instance);
        message::send_command(&mut self.association, context_id, &request).await?;
        let max_pdu = self.association.requestor_max_pdu_length();
        message::send_data_stream(&mut self.association, context_id, max_pdu, data).await?;
        loop {
            let response = match message::receive_message(&mut self.association).await {
                Ok(response) => response,
                Err(Error::Aborted) => {
                    self.closed = true;
                    return Ok(Status::new(Status::UNABLE_TO_PERFORM_SUBOPERATIONS));
                }
                Err(e) => return Err(e),
            };
            match response.field() {
                Some(CommandField::C_STORE_RSP)
                    if response.message_id_being_responded_to() == Some(sub_id) =>
                {
                    let mut status =
                        Status::new(response.status().unwrap_or(Status::CANNOT_UNDERSTAND));
                    if let Some(comment) = response.error_comment() {
                        status = status.with_comment(comment);
                    }
                    return Ok(status);
                }
                Some(CommandField::C_CANCEL_RQ)
                    if response.message_id_being_responded_to() == Some(message_id) =>
                {
                    *cancelled = true;
                }
                _ => return Err(Error::Protocol("unexpected message during C-GET".into())),
            }
        }
    }
}
