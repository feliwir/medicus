//! The Storage SCP against a dicom-ul client: C-ECHO, fragmented C-STORE, refusals.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use dicom_dictionary_std::{tags, uids};
use dicom_ul::Pdu;
use dicom_ul::association::{AsyncClientAssociation, ClientAssociationOptions};
use dicom_ul::pdu::{PDataValue, PDataValueType};
use futures::TryStreamExt as _;
use medicus_core::capability::ByteStream;
use medicus_dimse::{AssociationInfo, Command, Handler, Scp, ScpOptions, Status, StoreRequest};
use medicus_testkit::dicom::InstanceBuilder;
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

/// Collects stored files; answers with `status`.
struct Collect {
    files: Mutex<Vec<(StoreRequest, Vec<u8>)>>,
    status: u16,
}

#[async_trait::async_trait]
impl Handler for Collect {
    async fn store(
        &self,
        association: Arc<AssociationInfo>,
        request: StoreRequest,
        data: ByteStream,
    ) -> Status {
        assert_eq!(association.calling_ae, "TESTSCU");
        let chunks: Vec<_> = data.try_collect().await.unwrap();
        self.files.lock().unwrap().push((request, chunks.concat()));
        Status::new(self.status)
    }
}

async fn start(options: ScpOptions, status: u16) -> (SocketAddr, Arc<Collect>, CancellationToken) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handler = Arc::new(Collect {
        files: Mutex::new(Vec::new()),
        status,
    });
    let shutdown = CancellationToken::new();
    let scp = Scp::new(options, Shared(handler.clone()));
    let token = shutdown.clone();
    tokio::spawn(async move { scp.serve(listener, token).await });
    (addr, handler, shutdown)
}

/// Lets the test keep a handle on the handler it gives to the SCP.
struct Shared(Arc<Collect>);

#[async_trait::async_trait]
impl Handler for Shared {
    async fn store(
        &self,
        association: Arc<AssociationInfo>,
        request: StoreRequest,
        data: ByteStream,
    ) -> Status {
        self.0.store(association, request, data).await
    }
}

async fn connect(addr: SocketAddr, called: &str) -> AsyncClientAssociation<TcpStream> {
    ClientAssociationOptions::new()
        .calling_ae_title("TESTSCU")
        .called_ae_title(called)
        .with_presentation_context(uids::VERIFICATION, vec![uids::IMPLICIT_VR_LITTLE_ENDIAN])
        .with_presentation_context(
            uids::CT_IMAGE_STORAGE,
            vec![uids::EXPLICIT_VR_LITTLE_ENDIAN],
        )
        .max_pdu_length(16_384)
        .establish_async(addr)
        .await
        .unwrap()
}

fn context_id(association: &AsyncClientAssociation<TcpStream>, abstract_syntax: &str) -> u8 {
    association
        .presentation_contexts()
        .iter()
        .find(|pc| pc.abstract_syntax == abstract_syntax)
        .unwrap()
        .id
}

async fn send_command(
    association: &mut AsyncClientAssociation<TcpStream>,
    context: u8,
    command: &Command,
) {
    let data = command.encode().unwrap();
    association
        .send(&Pdu::PData {
            data: vec![PDataValue {
                presentation_context_id: context,
                value_type: PDataValueType::Command,
                is_last: true,
                data,
            }],
        })
        .await
        .unwrap();
}

async fn receive_response(association: &mut AsyncClientAssociation<TcpStream>) -> Command {
    match association.receive().await.unwrap() {
        Pdu::PData { data } => {
            assert_eq!(data.len(), 1);
            assert_eq!(data[0].value_type, PDataValueType::Command);
            Command::decode(&data[0].data).unwrap()
        }
        other => panic!("unexpected {other:?}"),
    }
}

fn store_rq(message_id: u16, sop_instance: &str) -> Command {
    Command::store_rq(message_id, uids::CT_IMAGE_STORAGE, sop_instance)
}

/// Sends `data_set` in fragments of `fragment` bytes, one P-DATA PDU each.
async fn send_data_set(
    association: &mut AsyncClientAssociation<TcpStream>,
    context: u8,
    data_set: &[u8],
    fragment: usize,
) {
    let chunks: Vec<&[u8]> = data_set.chunks(fragment).collect();
    for (i, chunk) in chunks.iter().enumerate() {
        association
            .send(&Pdu::PData {
                data: vec![PDataValue {
                    presentation_context_id: context,
                    value_type: PDataValueType::Data,
                    is_last: i + 1 == chunks.len(),
                    data: chunk.to_vec(),
                }],
            })
            .await
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn echo_and_fragmented_store() {
    let (addr, handler, shutdown) = start(ScpOptions::new(["MEDICUS"]), Status::SUCCESS).await;
    let mut association = connect(addr, "MEDICUS").await;

    let verification = context_id(&association, uids::VERIFICATION);
    send_command(&mut association, verification, &Command::echo_rq(1)).await;
    let rsp = receive_response(&mut association).await;
    assert_eq!(rsp.status(), Some(0));
    assert_eq!(rsp.message_id_being_responded_to(), Some(1));

    let ct = context_id(&association, uids::CT_IMAGE_STORAGE);
    let builder = InstanceBuilder::new("1.2.3", "1.2.3.1", "1.2.3.1.1").pixel_data(100_000);
    let data_set = builder.build_data_set();
    send_command(&mut association, ct, &store_rq(2, "1.2.3.1.1")).await;
    send_data_set(&mut association, ct, &data_set, 7_000).await;
    let rsp = receive_response(&mut association).await;
    assert_eq!(rsp.status(), Some(0));
    assert_eq!(rsp.message_id_being_responded_to(), Some(2));
    assert_eq!(rsp.affected_sop_instance().as_deref(), Some("1.2.3.1.1"));
    association.release().await.unwrap();

    let files = handler.files.lock().unwrap();
    let (request, file) = &files[0];
    assert_eq!(request.sop_instance, "1.2.3.1.1");
    assert_eq!(request.transfer_syntax, uids::EXPLICIT_VR_LITTLE_ENDIAN);
    // The data set is stored exactly as received, behind a valid Part 10 header.
    assert!(file.ends_with(&data_set));
    let obj = dicom_object::from_reader(&file[128..]).unwrap();
    assert_eq!(obj.meta().media_storage_sop_instance_uid(), "1.2.3.1.1");
    assert_eq!(
        obj.meta().source_application_entity_title(),
        Some("TESTSCU")
    );
    assert_eq!(
        obj.element(tags::SOP_INSTANCE_UID)
            .unwrap()
            .to_str()
            .unwrap(),
        "1.2.3.1.1"
    );
    shutdown.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn handler_status_is_returned() {
    let (addr, _handler, shutdown) =
        start(ScpOptions::new(["MEDICUS"]), Status::OUT_OF_RESOURCES).await;
    let mut association = connect(addr, "MEDICUS").await;
    let ct = context_id(&association, uids::CT_IMAGE_STORAGE);
    let data_set = InstanceBuilder::new("1.2.3", "1.2.3.1", "1.2.3.1.1").build_data_set();
    send_command(&mut association, ct, &store_rq(5, "1.2.3.1.1")).await;
    send_data_set(&mut association, ct, &data_set, 16_000).await;
    assert_eq!(
        receive_response(&mut association).await.status(),
        Some(0xA700)
    );
    association.release().await.unwrap();
    shutdown.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn wrong_called_ae_is_rejected() {
    let options = ScpOptions::new(["MEDICUS", "OTHER-NODE"]);
    let (addr, _handler, shutdown) = start(options, Status::SUCCESS).await;
    let result = ClientAssociationOptions::new()
        .calling_ae_title("TESTSCU")
        .called_ae_title("OTHER")
        .with_abstract_syntax(uids::VERIFICATION)
        .establish_async(addr)
        .await;
    assert!(result.is_err());
    shutdown.cancel();
}
