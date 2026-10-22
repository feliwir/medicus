//! `multipart/related` (RFC 2387) as `DICOMweb` uses it: parsing STOW-RS requests (with `multer`)
//! and writing WADO-RS responses.

use bytes::Bytes;
use futures::{Stream, StreamExt as _};
use medicus_core::capability::ByteStream;

/// The boundary of a `multipart/related` Content-Type.
pub(crate) fn boundary(content_type: &str) -> Option<String> {
    let mime: mime::Mime = content_type.parse().ok()?;
    (mime.type_() == mime::MULTIPART && mime.subtype().as_str() == "related")
        .then(|| {
            mime.get_param(mime::BOUNDARY)
                .map(|b| b.as_str().to_owned())
        })
        .flatten()
}

/// One part of a response: its Content-Type and body.
pub(crate) struct Part {
    pub(crate) content_type: String,
    pub(crate) body: ByteStream,
}

/// A new random boundary.
pub(crate) fn new_boundary() -> String {
    format!("medicus-{}", uuid::Uuid::now_v7().simple())
}

/// The body of a `multipart/related` response made of `parts`, written as they are produced.
pub(crate) fn body(
    boundary: String,
    parts: impl Stream<Item = medicus_core::Result<Part>> + Send + 'static,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static {
    async_stream::try_stream! {
        let mut parts = Box::pin(parts);
        while let Some(part) = parts.next().await {
            let part = part.map_err(|e| std::io::Error::other(e.to_string()))?;
            yield Bytes::from(format!(
                "--{boundary}\r\nContent-Type: {}\r\n\r\n",
                part.content_type
            ));
            let mut body = part.body;
            while let Some(chunk) = body.next().await {
                yield chunk.map_err(|e| std::io::Error::other(e.to_string()))?;
            }
            yield Bytes::from_static(b"\r\n");
        }
        yield Bytes::from(format!("--{boundary}--\r\n"));
    }
}
