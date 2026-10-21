//! Compiles `example_c.c` into a shared library with the platform's C compiler (gcc, clang or
//! MSVC, as found by the `cc` crate) and exposes its path as `MEDICUS_EXAMPLE_C_ADDON`.

// Build scripts report failure by panicking.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let source = manifest.join("example_c.c");
    let include = manifest.join("../../include");
    let os = std::env::var("CARGO_CFG_TARGET_OS").expect("CARGO_CFG_TARGET_OS");
    let file = match os.as_str() {
        "windows" => "example_c.dll",
        "macos" => "libexample_c.dylib",
        _ => "libexample_c.so",
    };
    let library = out_dir.join(file);

    let compiler = cc::Build::new().cargo_metadata(false).get_compiler();
    let mut cmd: Command = compiler.to_command();
    if compiler.is_like_msvc() {
        cmd.args(["/nologo", "/LD", "/W4", "/WX"])
            .arg("/I")
            .arg(&include)
            .arg(&source)
            .arg(format!("/Fe{}", library.display()))
            .arg(format!("/Fo{}\\", out_dir.display()));
    } else {
        cmd.args([
            "-std=c11",
            "-shared",
            "-fPIC",
            "-fvisibility=hidden",
            "-Wall",
            "-Wextra",
        ])
        .args(["-Wpedantic", "-Werror", "-O1"])
        .arg("-I")
        .arg(&include)
        .arg(&source)
        .arg("-o")
        .arg(&library);
        if os != "windows" {
            cmd.arg("-pthread");
        }
        // Opt-in sanitizer build, e.g. `MEDICUS_C_SANITIZE=address`; run the tests with the
        // sanitizer runtime preloaded (LD_PRELOAD=$(cc -print-file-name=libasan.so)).
        if let Ok(sanitizer) = std::env::var("MEDICUS_C_SANITIZE") {
            cmd.arg(format!("-fsanitize={sanitizer}"))
                .arg("-fno-omit-frame-pointer")
                .arg("-g");
        }
    }
    let status = cmd.status().expect("failed to run the C compiler");
    assert!(status.success(), "compiling example_c.c failed: {cmd:?}");

    println!(
        "cargo:rustc-env=MEDICUS_EXAMPLE_C_ADDON={}",
        library.display()
    );
    println!("cargo:rerun-if-changed={}", source.display());
    println!("cargo:rerun-if-env-changed=MEDICUS_C_SANITIZE");
    println!(
        "cargo:rerun-if-changed={}",
        include.join("medicus_addon.h").display()
    );
}
