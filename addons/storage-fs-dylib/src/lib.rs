//! `storage-fs` packaged as a loadable library (`libmedicus_storage_fs.so` / `.dylib` / `.dll`).

medicus_addon_sdk::export_addon!(
    name = "storage-fs",
    version = env!("CARGO_PKG_VERSION"),
    init = storage_fs::init
);
