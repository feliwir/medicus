//! `index-postgres` packaged as a loadable library (`libmedicus_index_postgres.so` / `.dylib` / `.dll`).

medicus_addon_sdk::export_addon!(
    manifest = index_postgres::manifest(),
    init = index_postgres::init
);
