//! `index-sqlite` packaged as a loadable library (`libmedicus_index_sqlite.so` / `.dylib` / `.dll`).

medicus_addon_sdk::export_addon!(
    manifest = index_sqlite::manifest(),
    init = index_sqlite::init
);
