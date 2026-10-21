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
