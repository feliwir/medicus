//! `dimse` packaged as a loadable library (`libmedicus_addon_dimse.so` / `.dylib` / `.dll`).

medicus_addon_sdk::export_addon!(
    name = "dimse",
    version = env!("CARGO_PKG_VERSION"),
    init = dimse::init
);
