//! `index-sqlite` packaged as a loadable library (`libmedicus_index_sqlite.so` / `.dylib` / `.dll`).

medicus_addon_sdk::export_addon!(
    name = "index-sqlite",
    version = env!("CARGO_PKG_VERSION"),
    init = index_sqlite::init
);
