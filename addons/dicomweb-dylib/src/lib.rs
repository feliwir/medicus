//! `dicomweb` packaged as a loadable library (`libmedicus_addon_dicomweb.so` / `.dylib` / `.dll`).

medicus_addon_sdk::export_addon!(
    name = "dicomweb",
    version = env!("CARGO_PKG_VERSION"),
    init = dicomweb::init
);
