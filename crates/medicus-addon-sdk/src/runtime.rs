use std::sync::OnceLock;

use tokio::runtime::{Builder, Handle, Runtime};

static RUNTIME: OnceLock<Runtime> = OnceLock::new();

/// The addon's own async runtime. Every capability call runs on it; addons may also spawn their
/// own background tasks here.
///
/// # Panics
/// If the operating system refuses to create the runtime's threads (on first use).
///
/// Each dynamically loaded addon has its own copy of this static and therefore its own runtime,
/// independent of the host's runtime and tokio version.
pub fn runtime() -> &'static Handle {
    RUNTIME
        .get_or_init(|| {
            #[allow(clippy::expect_used)] // without a runtime the addon cannot do anything
            Builder::new_multi_thread()
                .enable_all()
                .thread_name("medicus-addon")
                .build()
                .expect("failed to start the addon runtime")
        })
        .handle()
}

/// Runs `fut` to completion on the addon runtime from synchronous code, typically an addon's
/// `init` (e.g. to open a connection pool).
///
/// Safe to call from a thread that is itself driving an async runtime (as when the addon is
/// linked statically and loaded from async host code): the future is driven by a scoped helper
/// thread, so no runtime is ever entered recursively.
///
/// # Panics
/// Re-raises a panic of `fut`.
pub fn block_on<F>(fut: F) -> F::Output
where
    F: std::future::Future + Send,
    F::Output: Send,
{
    let handle = runtime();
    std::thread::scope(|s| {
        s.spawn(|| handle.block_on(fut))
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    })
}
