//! `storage-fs` packaged as a loadable library (`libmedicus_storage_fs.so` / `.dylib` / `.dll`).

medicus_addon_sdk::export_addon!(manifest = storage_fs::manifest(), init = storage_fs::init);
