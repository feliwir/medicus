//! Conformance suites and fixtures for medicus backends.
//!
//! Every implementation of a capability (native Rust, C, statically linked or loaded from a
//! library) runs the same suite, so all backends behave identically for the core. Suites panic on
//! the first violation, like tests do.

// Suites are test code: failing loudly is the point.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::missing_panics_doc,
    clippy::too_many_lines,
    clippy::cast_possible_wrap
)]

pub mod blob;
pub mod dicom;
pub mod index;

use bytes::Bytes;
use medicus_core::capability::ByteStream;

/// Deterministic test payload of `len` bytes.
pub fn payload(len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| u8::try_from(i * 7 % 251).unwrap())
        .collect()
}

/// A stream of `bytes` in chunks of `chunk` bytes. Like streams that cross the addon ABI, it
/// panics if polled again after it ended.
pub fn stream(bytes: &[u8], chunk: usize) -> ByteStream {
    let chunks: Vec<medicus_core::Result<Bytes>> = bytes
        .chunks(chunk.max(1))
        .map(|c| Ok(Bytes::copy_from_slice(c)))
        .collect();
    let mut chunks = chunks.into_iter();
    let mut ended = false;
    Box::pin(futures::stream::poll_fn(move |_| {
        assert!(!ended, "stream polled after it ended");
        let next = chunks.next();
        ended = next.is_none();
        std::task::Poll::Ready(next)
    }))
}

/// Collects a stream into one buffer.
pub async fn read_all(stream: ByteStream) -> Vec<u8> {
    use futures::TryStreamExt as _;
    let parts: Vec<Bytes> = stream.try_collect().await.unwrap();
    parts.concat()
}
