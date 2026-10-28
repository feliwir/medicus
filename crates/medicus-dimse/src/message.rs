//! Sending and receiving DIMSE messages (PS3.7 §9.3, PS3.8 §9.3.5) on either side of an
//! association.

use bytes::{Bytes, BytesMut};
use dicom_object::FileMetaTable;
use dicom_ul::Pdu;
use dicom_ul::association::AsyncAssociation;
use dicom_ul::pdu::{PDataValue, PDataValueType};
use futures::StreamExt as _;
use medicus_core::capability::ByteStream;
use tokio::net::TcpStream;

use crate::Error;
use crate::command::Command;

/// Largest File Meta Information accepted when sending a stored file.
const MAX_META: usize = 64 * 1024;

/// Bytes of a data fragment that fit into one P-DATA PDU of `max_pdu` (the peer's maximum PDU
/// length; 0 means unlimited). A PDV item adds 6 bytes: length, context ID and control header.
pub(crate) fn fragment_size(max_pdu: u32) -> usize {
    if max_pdu == 0 {
        1 << 20
    } else {
        (max_pdu as usize).saturating_sub(6).max(1)
    }
}

fn pdv(context_id: u8, value_type: PDataValueType, is_last: bool, data: Vec<u8>) -> Pdu {
    Pdu::PData {
        data: vec![PDataValue {
            presentation_context_id: context_id,
            value_type,
            is_last,
            data,
        }],
    }
}

pub(crate) async fn send_command<A>(
    association: &mut A,
    context_id: u8,
    command: &Command,
) -> Result<(), Error>
where
    A: AsyncAssociation<TcpStream> + Send,
{
    let data = command.encode()?;
    AsyncAssociation::send(
        association,
        &pdv(context_id, PDataValueType::Command, true, data),
    )
    .await?;
    Ok(())
}

/// Sends an encoded data set, fragmented to the peer's maximum PDU length.
pub(crate) async fn send_data<A>(
    association: &mut A,
    context_id: u8,
    max_pdu: u32,
    data: &[u8],
) -> Result<(), Error>
where
    A: AsyncAssociation<TcpStream> + Send,
{
    let chunks: Vec<&[u8]> = if data.is_empty() {
        vec![data]
    } else {
        data.chunks(fragment_size(max_pdu)).collect()
    };
    let last = chunks.len() - 1;
    for (i, chunk) in chunks.into_iter().enumerate() {
        AsyncAssociation::send(
            association,
            &pdv(context_id, PDataValueType::Data, i == last, chunk.to_vec()),
        )
        .await?;
    }
    Ok(())
}

/// Sends a data set as it is read from `data`, fragmented to the peer's maximum PDU length.
/// The last fragment is only known once the stream ends, so one fragment is held back.
pub(crate) async fn send_data_stream<A>(
    association: &mut A,
    context_id: u8,
    max_pdu: u32,
    mut data: ByteStream,
) -> Result<(), Error>
where
    A: AsyncAssociation<TcpStream> + Send,
{
    let size = fragment_size(max_pdu);
    let mut buffer = BytesMut::new();
    while let Some(chunk) = data.next().await {
        buffer.extend_from_slice(&chunk.map_err(Error::Source)?);
        // Keep at least one byte back so the final fragment is never empty.
        while buffer.len() > size {
            let fragment = buffer.split_to(size);
            AsyncAssociation::send(
                association,
                &pdv(context_id, PDataValueType::Data, false, fragment.to_vec()),
            )
            .await?;
        }
    }
    AsyncAssociation::send(
        association,
        &pdv(context_id, PDataValueType::Data, true, buffer.to_vec()),
    )
    .await?;
    Ok(())
}

/// Receives one complete message and returns its command; a data set that follows is read and
/// discarded (meant for responses and cancel requests, which the caller acts on by command).
pub(crate) async fn receive_message<A>(association: &mut A) -> Result<Command, Error>
where
    A: AsyncAssociation<TcpStream> + Send,
{
    let mut command_bytes = Vec::new();
    let mut command: Option<Command> = None;
    loop {
        let pdvs = match AsyncAssociation::receive(association).await? {
            Pdu::PData { data } => data,
            Pdu::AbortRQ { .. } => return Err(Error::Aborted),
            other => {
                return Err(Error::Protocol(format!(
                    "unexpected PDU: {}",
                    other.short_description()
                )));
            }
        };
        for pdv in pdvs {
            match (pdv.value_type, command.is_some()) {
                (PDataValueType::Command, false) => {
                    command_bytes.extend_from_slice(&pdv.data);
                    if pdv.is_last {
                        let decoded = Command::decode(&command_bytes)?;
                        if !decoded.has_data_set() {
                            return Ok(decoded);
                        }
                        command = Some(decoded);
                    }
                }
                (PDataValueType::Data, true) => {
                    if pdv.is_last {
                        return command.ok_or_else(|| unreachable!("matched above"));
                    }
                }
                _ => return Err(Error::Protocol("misordered message fragments".into())),
            }
        }
    }
}

/// Where the data set of a Part 10 file with meta group `meta` starts. dicom-rs does not report
/// this offset (its reader is buffered), but it reads the meta group by its group length:
/// preamble and magic code (132 bytes), the File Meta Information Group Length element (12 bytes)
/// and the `information_group_length` bytes of the group.
fn data_set_offset(meta: &FileMetaTable) -> usize {
    132 + 12 + meta.information_group_length as usize
}

/// Splits a Part 10 file into its File Meta Information and a stream of its data set, using
/// dicom-rs to read the preamble and meta group.
pub(crate) async fn split_part10(
    mut file: ByteStream,
) -> Result<(FileMetaTable, ByteStream), Error> {
    let mut buffer = BytesMut::new();
    loop {
        // Read the preamble and meta group, and stop at the first data set element.
        let opened = dicom_object::OpenFileOptions::new()
            .read_preamble(dicom_object::file::ReadPreamble::Always)
            // (0000,0000) is the lowest tag: stop before the first data set element.
            .read_until(dicom_dictionary_std::tags::COMMAND_GROUP_LENGTH)
            .from_reader(&buffer[..]);
        if let Ok(file_object) = opened {
            let meta = file_object.meta().clone();
            let start = data_set_offset(&meta);
            if buffer.len() >= start {
                let rest = Bytes::from(buffer.split_off(start));
                let data: ByteStream =
                    Box::pin(futures::stream::once(async move { Ok(rest) }).chain(file));
                return Ok((meta, data));
            }
        }
        if buffer.len() > MAX_META {
            return Err(Error::Protocol(
                "cannot read the file meta information".into(),
            ));
        }
        match file.next().await {
            Some(chunk) => buffer.extend_from_slice(&chunk.map_err(Error::Source)?),
            None => return Err(Error::Protocol("truncated DICOM file".into())),
        }
    }
}
