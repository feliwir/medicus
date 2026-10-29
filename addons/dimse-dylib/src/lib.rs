//! `dimse` packaged as a loadable library (`libmedicus_addon_dimse.so` / `.dylib` / `.dll`).

medicus_addon_sdk::export_addon!(manifest = dimse::manifest(), init = dimse::init);
