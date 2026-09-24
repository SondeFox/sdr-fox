// SPDX-License-Identifier: MIT OR Apache-2.0
//! Android-only page layout for the distributed JNI shared library.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // A build script runs on the host. Read Cargo's target metadata instead
    // of cfg!(target_os), which would test the host while cross-compiling.
    let target_os = std::env::var("CARGO_CFG_TARGET_OS")
        .expect("Cargo must provide the target operating system");
    if target_os == "android" {
        // max-page-size aligns LOAD segments; common-page-size also aligns
        // the GNU_RELRO protected range on the pinned Android NDK r27 linker.
        // Scope the ELF flags to this package's cdylib, never host/Mac links.
        println!("cargo:rustc-link-arg-cdylib=-Wl,-z,max-page-size=16384");
        println!("cargo:rustc-link-arg-cdylib=-Wl,-z,common-page-size=16384");
    }
}
