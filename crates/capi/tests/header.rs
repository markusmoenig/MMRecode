//! Compilation check for the public C11 header and smoke-test client.

use std::{path::PathBuf, process::Command};

#[test]
fn public_header_compiles_as_c11() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let result = Command::new("cc")
        .arg("-std=c11")
        .arg("-Wall")
        .arg("-Wextra")
        .arg("-Werror")
        .arg("-fsyntax-only")
        .arg("-I")
        .arg(manifest_dir.join("include"))
        .arg(manifest_dir.join("tests/smoke.c"))
        .status();

    let Ok(status) = result else {
        eprintln!("skipping C header check because no C compiler is available");
        return;
    };
    assert!(status.success(), "C compiler rejected mmrecode.h");
}
