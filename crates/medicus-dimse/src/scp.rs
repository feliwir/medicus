//! The DIMSE service provider: accepts associations and serves C-ECHO and C-STORE.
//!
//! A C-STORE data set is never buffered as a whole: its P-DATA fragments are forwarded through a
//! bounded channel to the [`StoreHandler`] as they arrive, behind a generated Part 10 preamble
//! and File Meta Information. When the handler falls behind, the channel fills up and the
//! association stops reading from the socket, so TCP flow control slows the sender down.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use dicom_dictionary_std::uids;
use dicom_object::FileMetaTableBuilder;
use dicom_transfer_syntax_registry::TransferSyntaxRegistry;
use dicom_ul::Pdu;
use dicom_ul::association::server::{AccessControl, DefaultNegotiation};
use dicom_ul::association::{Association as _, AsyncServerAssociation, ServerAssociationOptions};
use dicom_ul::pdu::{
    AssociationRJServiceUserReason, PDataValue, PDataValueType, PresentationContextResultReason,
    UserIdentity,
};
use futures::StreamExt as _;
use medicus_core::capability::ByteStream;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, mpsc};
use tokio::task::{JoinHandle, JoinSet};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::command::{Command, CommandField, Status};
use crate::{Error, IMPLEMENTATION_CLASS_UID, IMPLEMENTATION_VERSION_NAME};

/// P-DATA fragments buffered between the association and the store handler.
const STORE_CHANNEL_CAPACITY: usize = 16;
/// How long running associations may continue after shutdown was requested.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

/// Configuration of an [`Scp`].
#[derive(Debug, Clone)]
pub struct ScpOptions {
    /// This node's AE title.
    pub ae_title: String,
    /// Reject associations whose Called AE Title is not [`ae_title`](Self::ae_title).
    pub require_called_ae: bool,
    /// Calling AE titles allowed to associate; `None` allows any.
    pub allowed_calling: Option<HashSet<String>>,
    /// Abstract syntaxes accepted for C-STORE (Verification is always accepted).
    pub storage_sop_classes: Vec<String>,
    /// Accepted transfer syntaxes. Empty (the default) accepts any transfer syntax known to
    /// dicom-rs whose data sets it can read, compressed pixel data included; unknown syntaxes
    /// cannot be accepted, as the archive must parse the header.
    pub transfer_syntaxes: Vec<String>,
    /// Maximum PDU length this node receives.
    pub max_pdu_length: u32,
    /// Abort an association that has been silent for this long.
    pub idle_timeout: Option<Duration>,
    /// Concurrent associations; further connections wait for a free slot.
    pub max_associations: usize,
}

impl ScpOptions {
    pub fn new(ae_title: impl Into<String>) -> Self {
        Self {
            ae_title: ae_title.into(),
            require_called_ae: false,
            allowed_calling: None,
            storage_sop_classes: crate::storage::all().map(str::to_owned).collect(),
            transfer_syntaxes: Vec::new(),
            max_pdu_length: 64 * 1024,
            idle_timeout: Some(Duration::from_mins(5)),
            max_associations: 64,
        }
    }
}

impl ScpOptions {
    /// The transfer syntaxes offered in negotiation: the configured ones or, by default, every
    /// syntax known to dicom-rs whose data sets it can read (the archive must parse the header).
    pub fn accepted_transfer_syntaxes(&self) -> Vec<String> {
        if self.transfer_syntaxes.is_empty() {
            TransferSyntaxRegistry
                .iter()
                .filter(|ts| !ts.is_unsupported())
                .map(|ts| ts.uid().to_owned())
                .collect()
        } else {
            self.transfer_syntaxes.clone()
        }
    }
}

/// The parties of an established association.
#[derive(Debug, Clone)]
pub struct AssociationInfo {
    pub id: Uuid,
    pub calling_ae: String,
    pub called_ae: String,
    pub peer: SocketAddr,
}

/// A C-STORE request, as announced by its command set and presentation context.
#[derive(Debug, Clone)]
pub struct StoreRequest {
    pub message_id: u16,
    pub sop_class: String,
    pub sop_instance: String,
    pub transfer_syntax: String,
    /// The C-MOVE that caused this C-STORE: originator AE title and message ID.
    pub move_originator: Option<(String, u16)>,
}

/// Receives the data sets of C-STORE requests.
#[async_trait::async_trait]
pub trait StoreHandler: Send + Sync + 'static {
    /// Stores one instance. `data` is a complete DICOM Part 10 file: a generated preamble and
    /// File Meta Information followed by the data set exactly as received. The stream yields an
    /// error if the association breaks off before the data set is complete.
    async fn store(
        &self,
        association: Arc<AssociationInfo>,
        request: StoreRequest,
        data: ByteStream,
    ) -> Status;
}

/// A DIMSE service provider.
#[derive(Debug)]
pub struct Scp<H> {
    options: Arc<ServerAssociationOptions<'static, Access, DefaultNegotiation>>,
    config: Arc<ScpOptions>,
    handler: Arc<H>,
}

impl<H: StoreHandler> Scp<H> {
    pub fn new(config: ScpOptions, handler: H) -> Self {
        let access = Access {
            ae_title: config.ae_title.clone(),
            require_called_ae: config.require_called_ae,
            allowed_calling: config.allowed_calling.clone(),
        };
        let mut options = ServerAssociationOptions::new()
            .ae_access_control(access)
            .ae_title(config.ae_title.clone())
            .max_pdu_length(config.max_pdu_length)
            .with_abstract_syntax(uids::VERIFICATION);
        if let Some(timeout) = config.idle_timeout {
            options = options.read_timeout(timeout).write_timeout(timeout);
        }
        for class in &config.storage_sop_classes {
            options = options.with_abstract_syntax(class.clone());
        }
        for ts in config.accepted_transfer_syntaxes() {
            options = options.with_transfer_syntax(ts);
        }
        Self {
            options: Arc::new(options),
            config: Arc::new(config),
            handler: Arc::new(handler),
        }
    }

    /// Accepts associations on `listener` until `shutdown` is cancelled, then gives running
    /// associations a short grace period before aborting them.
    pub async fn serve(&self, listener: TcpListener, shutdown: CancellationToken) {
        let slots = Arc::new(Semaphore::new(self.config.max_associations.max(1)));
        let mut associations = JoinSet::new();
        loop {
            let permit = tokio::select! {
                () = shutdown.cancelled() => break,
                permit = slots.clone().acquire_owned() => match permit {
                    Ok(permit) => permit,
                    Err(_) => break, // the semaphore is never closed
                },
            };
            let (socket, peer) = tokio::select! {
                () = shutdown.cancelled() => break,
                accepted = listener.accept() => match accepted {
                    Ok(accepted) => accepted,
                    Err(e) => {
                        tracing::warn!(error = %e, "cannot accept DIMSE connection");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                },
            };
            // Reap finished associations so the set does not grow without bound.
            while associations.try_join_next().is_some() {}
            let options = self.options.clone();
            let handler = self.handler.clone();
            let shutdown = shutdown.clone();
            associations.spawn(async move {
                let _permit = permit;
                if let Err(e) = serve_connection(&options, handler, socket, peer, shutdown).await {
                    tracing::info!(peer = %peer, error = %e, "association ended with an error");
                }
            });
        }
        drop(listener);
        let drain = async { while associations.join_next().await.is_some() {} };
        if tokio::time::timeout(SHUTDOWN_GRACE, drain).await.is_err() {
            tracing::warn!("aborting DIMSE associations still running at shutdown");
            associations.shutdown().await;
        }
    }
}

/// AE title based access control.
#[derive(Debug, Clone)]
struct Access {
    ae_title: String,
    require_called_ae: bool,
    allowed_calling: Option<HashSet<String>>,
}

impl AccessControl for Access {
    fn check_access(
        &self,
        _this_ae_title: &str,
        calling_ae_title: &str,
        called_ae_title: &str,
        _user_identity: Option<&UserIdentity>,
    ) -> Result<(), AssociationRJServiceUserReason> {
        if self.require_called_ae && called_ae_title.trim() != self.ae_title {
            return Err(AssociationRJServiceUserReason::CalledAETitleNotRecognized);
        }
        if let Some(allowed) = &self.allowed_calling
            && !allowed.contains(calling_ae_title.trim())
        {
            return Err(AssociationRJServiceUserReason::CallingAETitleNotRecognized);
        }
        Ok(())
    }
}

async fn serve_connection<H: StoreHandler>(
    options: &ServerAssociationOptions<'static, Access, DefaultNegotiation>,
    handler: Arc<H>,
    socket: TcpStream,
    peer: SocketAddr,
    shutdown: CancellationToken,
) -> Result<(), Error> {
    socket.set_nodelay(true)?;
    let association = options.establish_async(socket).await?;
    let info = Arc::new(AssociationInfo {
        id: Uuid::now_v7(),
        calling_ae: association.peer_ae_title().trim().to_owned(),
        called_ae: association.called_ae_title().trim().to_owned(),
        peer,
    });
    tracing::info!(
        association = %info.id,
        calling_ae = %info.calling_ae,
        called_ae = %info.called_ae,
        peer = %peer,
        contexts = association
            .presentation_contexts()
            .iter()
            .filter(|pc| pc.reason == PresentationContextResultReason::Acceptance)
            .count(),
        "association established"
    );
    let mut session = Session {
        association,
        info,
        handler,
        command: Vec::new(),
        pending: None,
    };
    let result = tokio::select! {
        result = session.run() => result,
        () = shutdown.cancelled() => Err(Error::Protocol("server shutting down".into())),
    };
    session.cancel_pending();
    match result {
        Ok(()) => Ok(()),
        Err(e) => {
            // Best effort: the peer may already be gone.
            let _ = session.association.abort().await;
            Err(e)
        }
    }
}

/// Where the data set of the current message goes.
enum Sink {
    /// Into a running store handler.
    Store {
        tx: Option<mpsc::Sender<medicus_core::Result<Bytes>>>,
        task: JoinHandle<Status>,
    },
    /// Nowhere: the request is answered with this status once its data set is complete.
    Discard(Status),
}

/// A request whose data set is still arriving.
struct Pending {
    command: Command,
    context_id: u8,
    sink: Sink,
}

struct Session<H> {
    association: AsyncServerAssociation<TcpStream>,
    info: Arc<AssociationInfo>,
    handler: Arc<H>,
    /// Command fragments received so far.
    command: Vec<u8>,
    pending: Option<Pending>,
}

impl<H: StoreHandler> Session<H> {
    async fn run(&mut self) -> Result<(), Error> {
        loop {
            match self.association.receive().await? {
                Pdu::PData { data } => {
                    for pdv in data {
                        self.on_pdv(pdv).await?;
                    }
                }
                Pdu::ReleaseRQ => {
                    if self.pending.is_some() {
                        return Err(Error::Protocol("release during a data set".into()));
                    }
                    self.association.send(&Pdu::ReleaseRP).await?;
                    tracing::info!(association = %self.info.id, "association released");
                    return Ok(());
                }
                Pdu::AbortRQ { source } => {
                    tracing::info!(association = %self.info.id, ?source, "association aborted by peer");
                    self.cancel_pending();
                    return Ok(());
                }
                other => {
                    return Err(Error::Protocol(format!(
                        "unexpected PDU: {}",
                        other.short_description()
                    )));
                }
            }
        }
    }

    async fn on_pdv(&mut self, pdv: PDataValue) -> Result<(), Error> {
        match pdv.value_type {
            PDataValueType::Command => {
                if self.pending.is_some() {
                    return Err(Error::Protocol("command fragment during a data set".into()));
                }
                if self.command.is_empty() {
                    self.command = pdv.data;
                } else {
                    self.command.extend_from_slice(&pdv.data);
                }
                if pdv.is_last {
                    let command = Command::decode(&std::mem::take(&mut self.command))?;
                    self.on_command(command, pdv.presentation_context_id)
                        .await?;
                }
            }
            PDataValueType::Data => {
                let Some(pending) = &mut self.pending else {
                    return Err(Error::Protocol("data fragment without a command".into()));
                };
                if pending.context_id != pdv.presentation_context_id {
                    return Err(Error::Protocol(
                        "data set on a different presentation context than its command".into(),
                    ));
                }
                if let Sink::Store { tx, .. } = &mut pending.sink
                    && let Some(sender) = tx
                    && sender.send(Ok(Bytes::from(pdv.data))).await.is_err()
                {
                    // The handler stopped reading (it failed); drain the rest.
                    *tx = None;
                }
                if pdv.is_last {
                    let Some(pending) = self.pending.take() else {
                        unreachable!("checked above");
                    };
                    let status = match pending.sink {
                        Sink::Store { tx, task } => {
                            drop(tx);
                            task.await.unwrap_or_else(|e| {
                                tracing::error!(association = %self.info.id, error = %e, "store handler failed");
                                Status::new(Status::CANNOT_UNDERSTAND)
                            })
                        }
                        Sink::Discard(status) => status,
                    };
                    self.respond(&pending.command, pending.context_id, &status)
                        .await?;
                }
            }
        }
        Ok(())
    }

    async fn on_command(&mut self, command: Command, context_id: u8) -> Result<(), Error> {
        let Some(field) = command.field() else {
            return Err(Error::Protocol(
                "command set without a Command Field".into(),
            ));
        };
        let context = self
            .association
            .presentation_contexts()
            .iter()
            .find(|pc| {
                pc.id == context_id && pc.reason == PresentationContextResultReason::Acceptance
            })
            .ok_or_else(|| {
                Error::Protocol(format!("presentation context {context_id} is not accepted"))
            })?;
        let abstract_syntax = context.abstract_syntax.clone();
        let transfer_syntax = context.transfer_syntax.clone();

        if field == CommandField::C_STORE_RQ {
            let sink = self.store_sink(&command, &abstract_syntax, transfer_syntax);
            self.expect_data_set(command, context_id, sink).await
        } else if field == CommandField::C_ECHO_RQ {
            self.respond(&command, context_id, &Status::success()).await
        } else if field == CommandField::C_CANCEL_RQ || !field.is_request() {
            // Nothing of ours to cancel; responses are not expected from an SCU.
            Ok(())
        } else {
            tracing::debug!(association = %self.info.id, field = format_args!("{:#06x}", field.0), "unsupported DIMSE operation");
            let status = Status::new(Status::UNRECOGNIZED_OPERATION);
            self.expect_data_set(command, context_id, Sink::Discard(status))
                .await
        }
    }

    /// Answers `command` right away if it carries no data set, else waits for the data set.
    async fn expect_data_set(
        &mut self,
        command: Command,
        context_id: u8,
        sink: Sink,
    ) -> Result<(), Error> {
        if command.has_data_set() {
            self.pending = Some(Pending {
                command,
                context_id,
                sink,
            });
            return Ok(());
        }
        let status = match sink {
            Sink::Store { tx, task } => {
                drop(tx);
                task.abort();
                Status::new(Status::CANNOT_UNDERSTAND).with_comment("C-STORE without a data set")
            }
            Sink::Discard(status) => status,
        };
        self.respond(&command, context_id, &status).await
    }

    /// Validates a C-STORE request and starts its store handler.
    fn store_sink(
        &self,
        command: &Command,
        abstract_syntax: &str,
        transfer_syntax: String,
    ) -> Sink {
        let (Some(message_id), Some(sop_class), Some(sop_instance)) = (
            command.message_id(),
            command.affected_sop_class(),
            command.affected_sop_instance(),
        ) else {
            return Sink::Discard(
                Status::new(Status::CANNOT_UNDERSTAND)
                    .with_comment("C-STORE-RQ lacks required command elements"),
            );
        };
        if sop_class != abstract_syntax {
            return Sink::Discard(
                Status::new(Status::SOP_CLASS_NOT_SUPPORTED)
                    .with_comment("SOP class differs from the presentation context"),
            );
        }
        let request = StoreRequest {
            message_id,
            sop_class,
            sop_instance,
            transfer_syntax,
            move_originator: command.move_originator(),
        };
        let prefix = match part10_prefix(&request, &self.info.calling_ae) {
            Ok(prefix) => prefix,
            Err(e) => {
                return Sink::Discard(
                    Status::new(Status::CANNOT_UNDERSTAND).with_comment(e.to_string()),
                );
            }
        };
        let (tx, rx) = mpsc::channel(STORE_CHANNEL_CAPACITY);
        let data: ByteStream = Box::pin(
            futures::stream::once(async move { Ok(prefix) }).chain(ReceiverStream::new(rx)),
        );
        let handler = self.handler.clone();
        let info = self.info.clone();
        let task = tokio::spawn(async move { handler.store(info, request, data).await });
        Sink::Store { tx: Some(tx), task }
    }

    async fn respond(
        &mut self,
        request: &Command,
        context_id: u8,
        status: &Status,
    ) -> Result<(), Error> {
        let data = Command::response_to(request, status).encode()?;
        self.association
            .send(&Pdu::PData {
                data: vec![PDataValue {
                    presentation_context_id: context_id,
                    value_type: PDataValueType::Command,
                    is_last: true,
                    data,
                }],
            })
            .await?;
        Ok(())
    }

    /// Fails the data stream of an interrupted C-STORE, so the handler discards it.
    fn cancel_pending(&mut self) {
        if let Some(Pending {
            sink: Sink::Store { tx: Some(tx), .. },
            ..
        }) = self.pending.take()
        {
            let _ = tx.try_send(Err(medicus_core::Error::new(
                medicus_core::ErrorKind::Cancelled,
                "the association ended before the data set was complete",
            )));
        }
    }
}

/// The Part 10 preamble, magic code and File Meta Information for a received data set.
fn part10_prefix(request: &StoreRequest, calling_ae: &str) -> Result<Bytes, Error> {
    let mut meta = FileMetaTableBuilder::new()
        .media_storage_sop_class_uid(request.sop_class.as_str())
        .media_storage_sop_instance_uid(request.sop_instance.as_str())
        .transfer_syntax(request.transfer_syntax.as_str())
        .implementation_class_uid(IMPLEMENTATION_CLASS_UID)
        .implementation_version_name(IMPLEMENTATION_VERSION_NAME);
    if !calling_ae.is_empty() {
        meta = meta.source_application_entity_title(calling_ae);
    }
    let meta = meta
        .build()
        .map_err(|e| Error::Protocol(format!("cannot build file meta information: {e}")))?;
    let mut out = vec![0u8; 128];
    out.extend_from_slice(b"DICM");
    meta.write(&mut out)
        .map_err(|e| Error::Protocol(format!("cannot write file meta information: {e}")))?;
    Ok(Bytes::from(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(deprecated)] // Explicit VR Big Endian is retired but still received
    fn accepts_any_readable_transfer_syntax_by_default() {
        let mut options = ScpOptions::new("MEDICUS");
        let accepted = options.accepted_transfer_syntaxes();
        for ts in [
            uids::IMPLICIT_VR_LITTLE_ENDIAN,
            uids::EXPLICIT_VR_BIG_ENDIAN,
            uids::DEFLATED_EXPLICIT_VR_LITTLE_ENDIAN,
            uids::JPEG_BASELINE8_BIT,
            uids::JPEG_LOSSLESS_SV1,
            uids::JPEG2000_LOSSLESS,
            uids::RLE_LOSSLESS,
            uids::HTJ2K,
        ] {
            assert!(accepted.iter().any(|a| a == ts), "{ts} not accepted");
        }
        options.transfer_syntaxes = vec![uids::EXPLICIT_VR_LITTLE_ENDIAN.to_owned()];
        assert_eq!(
            options.accepted_transfer_syntaxes(),
            [uids::EXPLICIT_VR_LITTLE_ENDIAN]
        );
    }
}
