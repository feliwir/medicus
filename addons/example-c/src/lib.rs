//! Builds the C example addon (`example_c.c`); see `build.rs`. This crate only exists so that
//! Cargo compiles the addon with the platform's C toolchain and tests can locate it.

/// Path of the compiled C addon library.
pub const LIBRARY_PATH: &str = env!("MEDICUS_EXAMPLE_C_ADDON");
