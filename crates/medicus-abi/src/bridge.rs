//! Safe Rust helpers on top of [`crate::ffi`], shared by the host and the addon SDK.
//!
//! Naming: the *caller* of an ABI function creates completions with [`completion`] /
//! [`stream_completion`] and awaits them; the *callee* wraps what it received in a
//! [`CompletionGuard`] / [`StreamCompletionGuard`], which guarantees exactly-once completion
//! (also on early return and on panic). Byte streams cross the boundary with [`export_stream`]
//! (producer) and [`import_stream`] (consumer).

use std::ffi::c_void;
use std::fmt;
use std::future::Future;
use std::mem::ManuallyDrop;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures::StreamExt;
use medicus_core::capability::ByteStream;
use medicus_core::{Error, ErrorKind};
use tokio::runtime::Handle;
use tokio::sync::oneshot;

use crate::ffi::{
    MD_ALREADY_EXISTS, MD_CANCELLED, MD_INTERNAL, MD_INVALID_ARGUMENT, MD_NOT_FOUND, MD_OK,
    MD_PANICKED, MD_REJECTED, MD_RESOURCE_EXHAUSTED, MD_UNAVAILABLE, MD_UNSUPPORTED, MdBuffer,
    MdCancel, MdCompletion, MdRange, MdReadStream, MdReadStreamVtable, MdStatus, MdStr,
    MdStreamCompletion,
};

// ------------------------------------------------------------------------------------------------
// Status codes

pub fn status_of(kind: ErrorKind) -> MdStatus {
    match kind {
        ErrorKind::InvalidArgument => MD_INVALID_ARGUMENT,
        ErrorKind::NotFound => MD_NOT_FOUND,
        ErrorKind::AlreadyExists => MD_ALREADY_EXISTS,
        ErrorKind::Rejected => MD_REJECTED,
        ErrorKind::ResourceExhausted => MD_RESOURCE_EXHAUSTED,
        ErrorKind::Unavailable => MD_UNAVAILABLE,
        ErrorKind::Cancelled => MD_CANCELLED,
        ErrorKind::Unsupported => MD_UNSUPPORTED,
        ErrorKind::Panicked => MD_PANICKED,
        _ => MD_INTERNAL,
    }
}

/// Maps a non-OK status to an error kind; unknown codes (from newer peers) become `Internal`.
pub fn kind_of(status: MdStatus) -> ErrorKind {
    match status {
        MD_INVALID_ARGUMENT => ErrorKind::InvalidArgument,
        MD_NOT_FOUND => ErrorKind::NotFound,
        MD_ALREADY_EXISTS => ErrorKind::AlreadyExists,
        MD_REJECTED => ErrorKind::Rejected,
        MD_RESOURCE_EXHAUSTED => ErrorKind::ResourceExhausted,
        MD_UNAVAILABLE => ErrorKind::Unavailable,
        MD_CANCELLED => ErrorKind::Cancelled,
        MD_UNSUPPORTED => ErrorKind::Unsupported,
        MD_PANICKED => ErrorKind::Panicked,
        _ => ErrorKind::Internal,
    }
}

fn error_from(status: MdStatus, message: &OwnedBuffer) -> Error {
    let kind = kind_of(status);
    let msg = String::from_utf8_lossy(message.as_slice());
    Error::new(
        kind,
        if msg.is_empty() {
            kind.as_str().to_owned()
        } else {
            msg.into_owned()
        },
    )
}

// ------------------------------------------------------------------------------------------------
// Versions and targets

/// Whether an addon built against ABI `addon` can be used by a host implementing ABI `host`
/// (both encoded as `(major << 16) | minor`): the majors must match, and the addon must not need
/// a newer minor than the host offers. While the major is 0, minors must match exactly.
pub fn abi_compatible(addon: u32, host: u32) -> bool {
    let (am, an) = (addon >> 16, addon & 0xFFFF);
    let (hm, hn) = (host >> 16, host & 0xFFFF);
    am == hm && if am == 0 { an == hn } else { an <= hn }
}

/// Formats an encoded ABI version as `major.minor`.
pub fn abi_version_string(v: u32) -> String {
    format!("{}.{}", v >> 16, v & 0xFFFF)
}

/// `<arch>-<os>` of the running binary; host and addon must agree.
pub fn target() -> String {
    format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
}

// ------------------------------------------------------------------------------------------------
// Strings and buffers

impl MdStr {
    pub fn new(s: &str) -> Self {
        Self::from_bytes(s.as_bytes())
    }

    pub fn from_bytes(b: &[u8]) -> Self {
        Self {
            ptr: b.as_ptr(),
            len: b.len(),
        }
    }

    pub const fn empty() -> Self {
        Self {
            ptr: std::ptr::null(),
            len: 0,
        }
    }

    /// # Safety
    /// `ptr`/`len` must describe initialized memory that stays valid and unmodified for `'a`
    /// (for ABI arguments: the duration of the call).
    pub unsafe fn as_bytes<'a>(self) -> &'a [u8] {
        if self.len == 0 || self.ptr.is_null() {
            &[]
        } else {
            // SAFETY: guaranteed by the caller.
            unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
        }
    }

    /// # Safety
    /// See [`MdStr::as_bytes`].
    pub unsafe fn to_str<'a>(self) -> Result<&'a str, Error> {
        // SAFETY: forwarded to the caller.
        std::str::from_utf8(unsafe { self.as_bytes() })
            .map_err(|_| Error::invalid_argument("string is not valid UTF-8"))
    }
}

impl MdBuffer {
    pub const fn empty() -> Self {
        Self {
            ptr: std::ptr::null_mut(),
            len: 0,
            ctx: std::ptr::null_mut(),
            release: None,
        }
    }

    pub fn from_vec(v: Vec<u8>) -> Self {
        if v.is_empty() {
            return Self::empty();
        }
        let mut v = ManuallyDrop::new(v);
        Self {
            ptr: v.as_mut_ptr(),
            len: v.len(),
            // The capacity travels in `ctx`, so `release_vec` can rebuild the Vec.
            ctx: v.capacity() as *mut c_void,
            release: Some(release_vec),
        }
    }

    pub fn from_string(s: String) -> Self {
        Self::from_vec(s.into_bytes())
    }

    /// Zero-copy: the `Bytes` handle is kept alive until the receiver releases the buffer.
    pub fn from_bytes(b: Bytes) -> Self {
        if b.is_empty() {
            return Self::empty();
        }
        let ptr = b.as_ptr().cast_mut();
        let len = b.len();
        Self {
            ptr,
            len,
            ctx: Box::into_raw(Box::new(b)).cast(),
            release: Some(release_bytes),
        }
    }
}

unsafe extern "C" fn release_vec(ctx: *mut c_void, ptr: *mut u8, len: usize) {
    // SAFETY: produced by `MdBuffer::from_vec` in this module: ptr/len/capacity come from a
    // leaked Vec<u8> of this allocator, and release is called exactly once.
    drop(unsafe { Vec::from_raw_parts(ptr, len, ctx as usize) });
}

unsafe extern "C" fn release_bytes(ctx: *mut c_void, _ptr: *mut u8, _len: usize) {
    // SAFETY: produced by `MdBuffer::from_bytes`: ctx is a leaked Box<Bytes>, released once.
    drop(unsafe { Box::from_raw(ctx.cast::<Bytes>()) });
}

/// An [`MdBuffer`] received from the other side, released on drop.
pub struct OwnedBuffer(MdBuffer);

// SAFETY: the buffer is exclusively owned; the ABI requires `release` to be callable from any
// thread.
unsafe impl Send for OwnedBuffer {}
// SAFETY: only shared read access to immutable bytes is handed out.
unsafe impl Sync for OwnedBuffer {}

impl OwnedBuffer {
    /// # Safety
    /// `raw` must be a valid buffer per the ABI rules whose ownership is transferred to us.
    pub unsafe fn from_raw(raw: MdBuffer) -> Self {
        Self(raw)
    }

    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: an owned ABI buffer stays valid until released, which only `Drop` does.
        unsafe {
            MdStr {
                ptr: self.0.ptr,
                len: self.0.len,
            }
            .as_bytes()
        }
    }

    pub fn is_empty(&self) -> bool {
        self.0.len == 0
    }

    /// Zero-copy conversion; the buffer is released when the last `Bytes` clone is dropped.
    pub fn into_bytes(self) -> Bytes {
        if self.is_empty() {
            Bytes::new()
        } else {
            Bytes::from_owner(self)
        }
    }
}

impl AsRef<[u8]> for OwnedBuffer {
    fn as_ref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl fmt::Debug for OwnedBuffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OwnedBuffer")
            .field("len", &self.0.len)
            .finish()
    }
}

impl Drop for OwnedBuffer {
    fn drop(&mut self) {
        if let Some(release) = self.0.release {
            // SAFETY: we own the buffer and release it exactly once.
            unsafe { release(self.0.ctx, self.0.ptr, self.0.len) };
        }
    }
}

impl MdCancel {
    /// A cancel signal that is never set.
    pub const fn none() -> Self {
        Self {
            size: size_of::<Self>(),
            ctx: std::ptr::null_mut(),
            is_cancelled: None,
        }
    }
}

impl MdRange {
    pub fn from_range(range: Option<&std::ops::Range<u64>>) -> Self {
        match range {
            Some(r) => Self {
                has_range: 1,
                start: r.start,
                end: r.end,
            },
            None => Self {
                has_range: 0,
                start: 0,
                end: 0,
            },
        }
    }

    pub fn to_range(self) -> Option<std::ops::Range<u64>> {
        (self.has_range != 0).then_some(self.start..self.end)
    }
}

// ------------------------------------------------------------------------------------------------
// Completions — caller side

type Outcome = (MdStatus, OwnedBuffer);

/// Creates a completion to pass to an ABI call, and the future resolving when it fires.
pub fn completion() -> (MdCompletion, Completion) {
    let (tx, rx) = oneshot::channel::<Outcome>();
    let ctx = Box::into_raw(Box::new(tx)).cast();
    (
        MdCompletion {
            ctx,
            complete: complete_trampoline,
        },
        Completion(rx),
    )
}

unsafe extern "C" fn complete_trampoline(ctx: *mut c_void, status: MdStatus, result: MdBuffer) {
    // SAFETY: ownership of `result` is transferred to us by the ABI.
    let result = unsafe { OwnedBuffer::from_raw(result) };
    // SAFETY: ctx is the Box<Sender> leaked in `completion`; the ABI guarantees exactly-once.
    let tx = unsafe { Box::from_raw(ctx.cast::<oneshot::Sender<Outcome>>()) };
    // If the caller stopped waiting, the result is simply dropped (and released).
    let _ = tx.send((status, result));
}

/// Future of an [`MdCompletion`]. Resolves to the result buffer or the reported error.
#[derive(Debug)]
pub struct Completion(oneshot::Receiver<Outcome>);

impl Future for Completion {
    type Output = Result<OwnedBuffer, Error>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.0).poll(cx).map(|r| match r {
            Ok((MD_OK, buf)) => Ok(buf),
            Ok((status, msg)) => Err(error_from(status, &msg)),
            Err(_) => Err(Error::internal(
                "completion was dropped without being called",
            )),
        })
    }
}

type StreamOutcome = (MdStatus, Option<Imported>, OwnedBuffer);

/// Like [`completion`], for calls that deliver a stream.
pub fn stream_completion() -> (MdStreamCompletion, StreamCompletion) {
    let (tx, rx) = oneshot::channel::<StreamOutcome>();
    let ctx = Box::into_raw(Box::new(tx)).cast();
    (
        MdStreamCompletion {
            ctx,
            complete: stream_complete_trampoline,
        },
        StreamCompletion(rx),
    )
}

unsafe extern "C" fn stream_complete_trampoline(
    ctx: *mut c_void,
    status: MdStatus,
    stream: MdReadStream,
    error: MdBuffer,
) {
    // SAFETY: ownership of `error` and (on success) `stream` is transferred to us.
    let error = unsafe { OwnedBuffer::from_raw(error) };
    let stream = (status == MD_OK && !stream.instance.is_null()).then(|| Imported(stream));
    // SAFETY: ctx is the Box<Sender> leaked in `stream_completion`, called exactly once.
    let tx = unsafe { Box::from_raw(ctx.cast::<oneshot::Sender<StreamOutcome>>()) };
    let _ = tx.send((status, stream, error));
}

#[derive(Debug)]
pub struct StreamCompletion(oneshot::Receiver<StreamOutcome>);

impl Future for StreamCompletion {
    type Output = Result<ByteStream, Error>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.0).poll(cx).map(|r| match r {
            Ok((MD_OK, Some(stream), _)) => Ok(stream.into_byte_stream()),
            Ok((MD_OK, None, _)) => Ok(Box::pin(futures::stream::empty()) as ByteStream),
            Ok((status, _, msg)) => Err(error_from(status, &msg)),
            Err(_) => Err(Error::internal(
                "completion was dropped without being called",
            )),
        })
    }
}

// ------------------------------------------------------------------------------------------------
// Completions — callee side

fn dropped_status() -> (MdStatus, &'static str) {
    if std::thread::panicking() {
        (MD_PANICKED, "addon panicked while handling the call")
    } else {
        (MD_INTERNAL, "operation was dropped without completing")
    }
}

/// Owns a received [`MdCompletion`] and completes it exactly once: explicitly, or on drop
/// (with `MD_PANICKED` while unwinding, `MD_INTERNAL` otherwise).
pub struct CompletionGuard(Option<MdCompletion>);

// SAFETY: the ABI requires completions to be callable from any thread.
unsafe impl Send for CompletionGuard {}

impl CompletionGuard {
    pub fn new(done: MdCompletion) -> Self {
        Self(Some(done))
    }

    pub fn complete(mut self, status: MdStatus, result: MdBuffer) {
        if let Some(c) = self.0.take() {
            // SAFETY: completing a received completion exactly once (it was taken out).
            unsafe { (c.complete)(c.ctx, status, result) };
        }
    }

    pub fn ok(self, result: MdBuffer) {
        self.complete(MD_OK, result);
    }

    pub fn err(self, e: &Error) {
        self.complete(status_of(e.kind), MdBuffer::from_string(e.message.clone()));
    }

    pub fn finish(self, r: Result<MdBuffer, Error>) {
        match r {
            Ok(b) => self.ok(b),
            Err(e) => self.err(&e),
        }
    }
}

impl Drop for CompletionGuard {
    fn drop(&mut self) {
        if let Some(c) = self.0.take() {
            let (status, msg) = dropped_status();
            // SAFETY: see `complete`.
            unsafe { (c.complete)(c.ctx, status, MdBuffer::from_string(msg.to_owned())) };
        }
    }
}

impl fmt::Debug for CompletionGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CompletionGuard")
            .field("pending", &self.0.is_some())
            .finish()
    }
}

/// [`CompletionGuard`] for stream-delivering calls.
pub struct StreamCompletionGuard(Option<MdStreamCompletion>);

// SAFETY: as for `CompletionGuard`.
unsafe impl Send for StreamCompletionGuard {}

impl StreamCompletionGuard {
    pub fn new(done: MdStreamCompletion) -> Self {
        Self(Some(done))
    }

    fn complete(mut self, status: MdStatus, stream: MdReadStream, error: MdBuffer) {
        if let Some(c) = self.0.take() {
            // SAFETY: completing a received completion exactly once.
            unsafe { (c.complete)(c.ctx, status, stream, error) };
        }
    }

    pub fn ok(self, stream: MdReadStream) {
        self.complete(MD_OK, stream, MdBuffer::empty());
    }

    pub fn err(self, e: &Error) {
        self.complete(
            status_of(e.kind),
            null_stream(),
            MdBuffer::from_string(e.message.clone()),
        );
    }
}

impl Drop for StreamCompletionGuard {
    fn drop(&mut self) {
        if let Some(c) = self.0.take() {
            let (status, msg) = dropped_status();
            // SAFETY: see `complete`.
            unsafe {
                (c.complete)(
                    c.ctx,
                    status,
                    null_stream(),
                    MdBuffer::from_string(msg.to_owned()),
                );
            }
        }
    }
}

impl fmt::Debug for StreamCompletionGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamCompletionGuard")
            .field("pending", &self.0.is_some())
            .finish()
    }
}

fn null_stream() -> MdReadStream {
    MdReadStream {
        instance: std::ptr::null_mut(),
        vtable: std::ptr::null(),
    }
}

// ------------------------------------------------------------------------------------------------
// Streams

struct Exported {
    stream: tokio::sync::Mutex<ByteStream>,
    rt: Handle,
}

static EXPORTED_VTABLE: MdReadStreamVtable = MdReadStreamVtable {
    size: size_of::<MdReadStreamVtable>(),
    read: exported_read,
    release: exported_release,
};

/// Exposes a Rust byte stream as an [`MdReadStream`]. Reads are served by tasks on `rt`.
pub fn export_stream(stream: ByteStream, rt: &Handle) -> MdReadStream {
    let exported = Arc::new(Exported {
        stream: tokio::sync::Mutex::new(stream),
        rt: rt.clone(),
    });
    MdReadStream {
        instance: Arc::into_raw(exported).cast_mut().cast(),
        vtable: &raw const EXPORTED_VTABLE,
    }
}

unsafe extern "C" fn exported_read(instance: *mut c_void, done: MdCompletion) {
    let guard = CompletionGuard::new(done);
    let ptr = instance.cast_const().cast::<Exported>();
    // SAFETY: `instance` comes from `export_stream` and has not been released yet (ABI rule), so
    // the Arc is alive; we take an additional reference for the task.
    let this = unsafe {
        Arc::increment_strong_count(ptr);
        Arc::from_raw(ptr)
    };
    let rt = this.rt.clone();
    // If the runtime is shutting down, the task is dropped and the guard completes the read.
    rt.spawn(async move {
        let mut stream = this.stream.lock().await;
        loop {
            match stream.next().await {
                None => return guard.ok(MdBuffer::empty()),
                // An empty chunk would signal end-of-stream; skip it.
                Some(Ok(chunk)) if chunk.is_empty() => {}
                Some(Ok(chunk)) => return guard.ok(MdBuffer::from_bytes(chunk)),
                Some(Err(e)) => return guard.err(&e),
            }
        }
    });
}

unsafe extern "C" fn exported_release(instance: *mut c_void) {
    // SAFETY: releases the reference created by `export_stream`, exactly once (ABI rule).
    // Read tasks still in flight hold their own references.
    drop(unsafe { Arc::from_raw(instance.cast_const().cast::<Exported>()) });
}

/// A received [`MdReadStream`], released on drop.
pub struct Imported(MdReadStream);

// SAFETY: the ABI requires stream functions to be callable from any thread.
unsafe impl Send for Imported {}

impl Imported {
    /// # Safety
    /// `raw` must be a valid stream per the ABI rules whose ownership is transferred to us.
    pub unsafe fn from_raw(raw: MdReadStream) -> Self {
        Self(raw)
    }

    fn read(&self) -> Completion {
        let (c, fut) = completion();
        // SAFETY: the stream is alive (not yet released), and we never have two reads in
        // flight: `into_byte_stream` awaits each read before issuing the next.
        unsafe { ((*self.0.vtable).read)(self.0.instance, c) };
        fut
    }

    pub fn into_byte_stream(self) -> ByteStream {
        Box::pin(futures::stream::unfold(Some(self), |state| async move {
            let stream = state?;
            match stream.read().await {
                Ok(buf) if buf.is_empty() => None,
                Ok(buf) => Some((Ok(buf.into_bytes()), Some(stream))),
                Err(e) => Some((Err(e), None)),
            }
        }))
    }
}

impl Drop for Imported {
    fn drop(&mut self) {
        if !self.0.instance.is_null() {
            // SAFETY: we own the stream and release it exactly once. A read may still be in
            // flight if the consumer stopped early; the ABI requires producers to tolerate that.
            unsafe { ((*self.0.vtable).release)(self.0.instance) };
        }
    }
}

impl fmt::Debug for Imported {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Imported").finish_non_exhaustive()
    }
}

/// Takes ownership of a received stream and adapts it to a [`ByteStream`].
///
/// # Safety
/// `raw` must be a valid stream per the ABI rules whose ownership is transferred to us.
pub unsafe fn import_stream(raw: MdReadStream) -> ByteStream {
    if raw.instance.is_null() {
        return Box::pin(futures::stream::empty());
    }
    // SAFETY: forwarded to the caller.
    unsafe { Imported::from_raw(raw) }.into_byte_stream()
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::TryStreamExt;

    fn chunks(parts: &[&'static [u8]]) -> ByteStream {
        let items: Vec<Result<Bytes, Error>> =
            parts.iter().map(|p| Ok(Bytes::from_static(p))).collect();
        Box::pin(futures::stream::iter(items))
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stream_roundtrip_skips_empty_chunks() {
        let raw = export_stream(chunks(&[b"ab", b"", b"cde"]), &Handle::current());
        // SAFETY: freshly exported stream, ownership moves to the importer.
        let s = unsafe { import_stream(raw) };
        let got: Vec<Bytes> = s.try_collect().await.unwrap();
        assert_eq!(got.concat(), b"abcde");
        assert_eq!(got.len(), 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stream_errors_propagate() {
        let items: Vec<Result<Bytes, Error>> =
            vec![Ok(Bytes::from_static(b"x")), Err(Error::not_found("gone"))];
        let raw = export_stream(Box::pin(futures::stream::iter(items)), &Handle::current());
        // SAFETY: as above.
        let mut s = unsafe { import_stream(raw) };
        assert_eq!(s.next().await.unwrap().unwrap(), Bytes::from_static(b"x"));
        let e = s.next().await.unwrap().unwrap_err();
        assert_eq!(e.kind, ErrorKind::NotFound);
        assert_eq!(e.message, "gone");
        assert!(s.next().await.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dropping_consumer_early_is_safe() {
        let raw = export_stream(chunks(&[b"a", b"b", b"c"]), &Handle::current());
        // SAFETY: as above.
        let mut s = unsafe { import_stream(raw) };
        assert!(s.next().await.is_some());
        drop(s);
    }

    #[tokio::test]
    async fn guards_complete_exactly_once() {
        let (c, fut) = completion();
        CompletionGuard::new(c).ok(MdBuffer::from_vec(b"hi".to_vec()));
        assert_eq!(fut.await.unwrap().as_slice(), b"hi");

        let (c, fut) = completion();
        CompletionGuard::new(c).err(&Error::new(ErrorKind::Rejected, "no"));
        let e = fut.await.unwrap_err();
        assert_eq!((e.kind, e.message.as_str()), (ErrorKind::Rejected, "no"));

        let (c, fut) = completion();
        drop(CompletionGuard::new(c));
        assert_eq!(fut.await.unwrap_err().kind, ErrorKind::Internal);
    }

    #[test]
    fn guard_reports_panics() {
        let (c, fut) = completion();
        let guard = CompletionGuard::new(c);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _g = guard;
            panic!("boom");
        }));
        assert!(r.is_err());
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        assert_eq!(rt.block_on(fut).unwrap_err().kind, ErrorKind::Panicked);
    }

    #[test]
    fn buffers_are_zero_copy_and_released() {
        let b = Bytes::from(vec![1u8, 2, 3]);
        let ptr = b.as_ptr();
        let raw = MdBuffer::from_bytes(b);
        assert_eq!(raw.ptr.cast_const(), ptr);
        // SAFETY: freshly created buffer, ownership moves to `owned`.
        let owned = unsafe { OwnedBuffer::from_raw(raw) };
        let back = owned.into_bytes();
        assert_eq!(back.as_ptr(), ptr);
        assert_eq!(&back[..], &[1, 2, 3]);
    }

    #[test]
    fn abi_compatibility_rules() {
        let v = |major: u32, minor: u32| (major << 16) | minor;
        assert!(abi_compatible(v(0, 1), v(0, 1)));
        assert!(!abi_compatible(v(0, 1), v(0, 2)), "0.x minors are breaking");
        assert!(
            abi_compatible(v(1, 2), v(1, 5)),
            "older addon on newer host"
        );
        assert!(!abi_compatible(v(1, 5), v(1, 2)), "addon needs newer host");
        assert!(!abi_compatible(v(2, 0), v(1, 9)));
        assert_eq!(abi_version_string(v(1, 7)), "1.7");
    }

    #[test]
    fn status_mapping_roundtrips() {
        for kind in [
            ErrorKind::InvalidArgument,
            ErrorKind::NotFound,
            ErrorKind::AlreadyExists,
            ErrorKind::Rejected,
            ErrorKind::ResourceExhausted,
            ErrorKind::Unavailable,
            ErrorKind::Cancelled,
            ErrorKind::Unsupported,
            ErrorKind::Internal,
            ErrorKind::Panicked,
        ] {
            assert_eq!(kind_of(status_of(kind)), kind);
        }
        assert_eq!(kind_of(12345), ErrorKind::Internal);
    }
}
