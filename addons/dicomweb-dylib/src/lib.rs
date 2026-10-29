//! `dicomweb` packaged as a loadable library (`libmedicus_addon_dicomweb.so` / `.dylib` / `.dll`).

medicus_addon_sdk::export_addon!(manifest = dicomweb::manifest(), init = dicomweb::init);
