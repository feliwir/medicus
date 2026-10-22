//! C-FIND against a dicom-ul client: pending matches, character sets, C-CANCEL.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::SocketAddr;
use std::sync::Arc;

use dicom_core::{DataElement, PrimitiveValue, VR};
use dicom_dictionary_std::{tags, uids};
use dicom_encoding::TransferSyntaxIndex as _;
use dicom_object::InMemDicomObject;
use dicom_transfer_syntax_registry::TransferSyntaxRegistry;
use dicom_ul::Pdu;
use dicom_ul::association::{AsyncClientAssociation, ClientAssociationOptions};
use dicom_ul::pdu::{PDataValue, PDataValueType};
use futures::StreamExt as _;
use medicus_dimse::{
    AssociationInfo, Command, CommandField, FindRequest, Handler, Matches, Scp, ScpOptions, Status,
};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

/// Answers every query with `count` matches named "Müller^<n>"; with `stall`, waits forever
/// after the first one.
struct Patients {
    count: usize,
    stall: bool,
}

#[async_trait::async_trait]
impl Handler for Patients {
    async fn find(
        &self,
        _association: Arc<AssociationInfo>,
        request: FindRequest,
        identifier: InMemDicomObject,
    ) -> Result<Matches, Status> {
        assert_eq!(
            request.sop_class,
            uids::PATIENT_ROOT_QUERY_RETRIEVE_INFORMATION_MODEL_FIND
        );
        assert_eq!(
            identifier
                .get(tags::QUERY_RETRIEVE_LEVEL)
                .unwrap()
                .to_str()
                .unwrap()
                .trim(),
            "PATIENT"
        );
        let found = (0..self.count).map(|n| {
            Ok(InMemDicomObject::from_element_iter([
                DataElement::new(
                    tags::SPECIFIC_CHARACTER_SET,
                    VR::CS,
                    PrimitiveValue::from("ISO_IR 192"),
                ),
                DataElement::new(
                    tags::QUERY_RETRIEVE_LEVEL,
                    VR::CS,
                    PrimitiveValue::from("PATIENT"),
                ),
                DataElement::new(
                    tags::PATIENT_NAME,
                    VR::PN,
                    PrimitiveValue::from(format!("Müller^{n}")),
                ),
            ]))
        });
        let matches = futures::stream::iter(found.collect::<Vec<_>>());
        if self.stall {
            Ok(matches.take(1).chain(futures::stream::pending()).boxed())
        } else {
            Ok(matches.boxed())
        }
    }
}

async fn start(handler: Patients) -> (SocketAddr, CancellationToken) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut options = ScpOptions::new("MEDICUS");
    options.find_sop_classes =
        vec![uids::PATIENT_ROOT_QUERY_RETRIEVE_INFORMATION_MODEL_FIND.to_owned()];
    let shutdown = CancellationToken::new();
    let scp = Scp::new(options, handler);
    let token = shutdown.clone();
    tokio::spawn(async move { scp.serve(listener, token).await });
    (addr, shutdown)
}

async fn connect(addr: SocketAddr) -> (AsyncClientAssociation<TcpStream>, u8) {
    let association = ClientAssociationOptions::new()
        .calling_ae_title("TESTSCU")
        .called_ae_title("MEDICUS")
        .with_presentation_context(
            uids::PATIENT_ROOT_QUERY_RETRIEVE_INFORMATION_MODEL_FIND,
            vec![uids::EXPLICIT_VR_LITTLE_ENDIAN],
        )
        // Small PDUs: identifiers are fragmented.
        .max_pdu_length(4096)
        .establish_async(addr)
        .await
        .unwrap();
    let context = association.presentation_contexts()[0].id;
    (association, context)
}

fn find_rq(message_id: u16) -> Vec<u8> {
    let us = |tag, v: u16| DataElement::new(tag, VR::US, PrimitiveValue::from(v));
    let command = InMemDicomObject::command_from_element_iter([
        DataElement::new(
            tags::AFFECTED_SOP_CLASS_UID,
            VR::UI,
            PrimitiveValue::from(uids::PATIENT_ROOT_QUERY_RETRIEVE_INFORMATION_MODEL_FIND),
        ),
        us(tags::COMMAND_FIELD, CommandField::C_FIND_RQ.0),
        us(tags::MESSAGE_ID, message_id),
        us(tags::PRIORITY, 0),
        us(tags::COMMAND_DATA_SET_TYPE, 0x0000),
    ]);
    implicit(&command)
}

fn cancel_rq(message_id: u16) -> Vec<u8> {
    let us = |tag, v: u16| DataElement::new(tag, VR::US, PrimitiveValue::from(v));
    implicit(&InMemDicomObject::command_from_element_iter([
        us(tags::COMMAND_FIELD, CommandField::C_CANCEL_RQ.0),
        us(tags::MESSAGE_ID_BEING_RESPONDED_TO, message_id),
        us(tags::COMMAND_DATA_SET_TYPE, 0x0101),
    ]))
}

fn implicit(obj: &InMemDicomObject) -> Vec<u8> {
    let mut out = Vec::new();
    obj.write_dataset_with_ts(
        &mut out,
        &dicom_transfer_syntax_registry::entries::IMPLICIT_VR_LITTLE_ENDIAN.erased(),
    )
    .unwrap();
    out
}

fn identifier() -> Vec<u8> {
    let obj = InMemDicomObject::from_element_iter([
        DataElement::new(
            tags::QUERY_RETRIEVE_LEVEL,
            VR::CS,
            PrimitiveValue::from("PATIENT"),
        ),
        DataElement::new(tags::PATIENT_NAME, VR::PN, PrimitiveValue::from("M*")),
    ]);
    let mut out = Vec::new();
    obj.write_dataset_with_ts(
        &mut out,
        TransferSyntaxRegistry
            .get(uids::EXPLICIT_VR_LITTLE_ENDIAN)
            .unwrap(),
    )
    .unwrap();
    out
}

async fn send(association: &mut AsyncClientAssociation<TcpStream>, pdv: PDataValue) {
    association
        .send(&Pdu::PData { data: vec![pdv] })
        .await
        .unwrap();
}

fn pdv(context: u8, value_type: PDataValueType, data: Vec<u8>) -> PDataValue {
    PDataValue {
        presentation_context_id: context,
        value_type,
        is_last: true,
        data,
    }
}

/// Receives one message: the command and, if announced, its (possibly fragmented) data set.
async fn receive(
    association: &mut AsyncClientAssociation<TcpStream>,
) -> (Command, Option<InMemDicomObject>) {
    let mut command = None;
    let mut data = Vec::new();
    loop {
        let Pdu::PData { data: pdvs } = association.receive().await.unwrap() else {
            panic!("expected P-DATA");
        };
        for pdv in pdvs {
            match pdv.value_type {
                PDataValueType::Command => {
                    let c = Command::decode(&pdv.data).unwrap();
                    if !c.has_data_set() {
                        return (c, None);
                    }
                    command = Some(c);
                }
                PDataValueType::Data => {
                    data.extend_from_slice(&pdv.data);
                    if pdv.is_last {
                        let ts = TransferSyntaxRegistry
                            .get(uids::EXPLICIT_VR_LITTLE_ENDIAN)
                            .unwrap();
                        let obj = InMemDicomObject::read_dataset_with_ts(&data[..], ts).unwrap();
                        return (command.unwrap(), Some(obj));
                    }
                }
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn matches_are_pending_responses_in_their_character_set() {
    let (addr, shutdown) = start(Patients {
        count: 3,
        stall: false,
    })
    .await;
    let (mut association, context) = connect(addr).await;
    send(
        &mut association,
        pdv(context, PDataValueType::Command, find_rq(9)),
    )
    .await;
    send(
        &mut association,
        pdv(context, PDataValueType::Data, identifier()),
    )
    .await;
    for n in 0..3 {
        let (rsp, data) = receive(&mut association).await;
        assert_eq!(rsp.field(), Some(CommandField::C_FIND_RSP));
        assert_eq!(rsp.status(), Some(Status::PENDING));
        assert_eq!(rsp.message_id_being_responded_to(), Some(9));
        let data = data.unwrap();
        assert_eq!(
            data.get(tags::PATIENT_NAME).unwrap().to_str().unwrap(),
            format!("Müller^{n}")
        );
    }
    let (rsp, data) = receive(&mut association).await;
    assert_eq!(rsp.status(), Some(Status::SUCCESS));
    assert!(data.is_none());
    association.release().await.unwrap();
    shutdown.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_stops_a_running_query() {
    let (addr, shutdown) = start(Patients {
        count: 5,
        stall: true,
    })
    .await;
    let (mut association, context) = connect(addr).await;
    send(
        &mut association,
        pdv(context, PDataValueType::Command, find_rq(3)),
    )
    .await;
    send(
        &mut association,
        pdv(context, PDataValueType::Data, identifier()),
    )
    .await;
    let (rsp, _) = receive(&mut association).await;
    assert_eq!(rsp.status(), Some(Status::PENDING));
    send(
        &mut association,
        pdv(context, PDataValueType::Command, cancel_rq(3)),
    )
    .await;
    let (rsp, _) = receive(&mut association).await;
    assert_eq!(rsp.status(), Some(Status::CANCEL));
    // The association is still usable.
    send(
        &mut association,
        pdv(context, PDataValueType::Command, find_rq(4)),
    )
    .await;
    send(
        &mut association,
        pdv(context, PDataValueType::Data, identifier()),
    )
    .await;
    let (rsp, _) = receive(&mut association).await;
    assert_eq!(rsp.status(), Some(Status::PENDING));
    association.abort().await.unwrap();
    shutdown.cancel();
}
