//! Build script: generate the C header via cbindgen.
//!
//! Best-effort: a cbindgen config/parse error MUST NOT fail the cargo build
//! (it would break every downstream consumer). The header is regenerated on
//! demand; if generation fails, a warning is emitted and the build continues
//! using the previously-generated header in `bindings/sdr_fox.h`.

use std::env;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;

fn main() {
    let crate_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let workspace_root = PathBuf::from(&crate_dir)
        .parent()
        .and_then(|p| p.parent())
        .expect("could not resolve workspace root")
        .to_path_buf();
    let out_path = workspace_root.join("bindings").join("sdr_fox.h");

    // cbindgen panics on config errors rather than returning Err; wrap it.
    let result = catch_unwind(AssertUnwindSafe(|| cbindgen::generate(&crate_dir)));
    match result {
        Ok(Ok(bindings)) => {
            if let Some(parent) = out_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            // write_to_file returns false when the file is unchanged (cbindgen
            // skips the rewrite to preserve mtime). That is success, not error.
            let _ = bindings.write_to_file(&out_path);
        }
        Ok(Err(e)) => {
            println!("cargo:warning=cbindgen failed (header not regenerated): {e}");
        }
        Err(_) => {
            println!(
                "cargo:warning=cbindgen panicked (header not regenerated; check cbindgen.toml)"
            );
        }
    }

    println!("cargo:rerun-if-changed=src/lib.rs");
    println!("cargo:rerun-if-changed=cbindgen.toml");
}
