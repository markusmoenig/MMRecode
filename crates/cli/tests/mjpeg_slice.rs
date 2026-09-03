//! End-to-end command-line coverage for the Motion JPEG vertical slice.

use std::{fs, path::Path, process::Command};

#[test]
fn y4m_encode_verify_decode_compare_round_trip() {
    let binary = env!("CARGO_BIN_EXE_mmrecode");
    let repository = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let source = repository.join("testdata/y4m/valid/two-frame-420.y4m");
    let stem = format!("mmrecode-slice-{}", std::process::id());
    let encoded = std::env::temp_dir().join(format!("{stem}.mjpg"));
    let decoded = std::env::temp_dir().join(format!("{stem}.y4m"));

    run(binary, ["encode", path(&source), path(&encoded), "85"]);
    run(binary, ["inspect", path(&encoded)]);
    run(binary, ["verify", path(&encoded), path(&source)]);
    run(binary, ["decode", path(&encoded), path(&decoded)]);
    run(binary, ["compare", path(&source), path(&decoded)]);

    let _ = fs::remove_file(encoded);
    let _ = fs::remove_file(decoded);
}

fn run<const N: usize>(binary: &str, arguments: [&str; N]) {
    let output = Command::new(binary)
        .args(arguments)
        .output()
        .expect("run mmrecode CLI");
    assert!(
        output.status.success(),
        "CLI failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn path(path: &Path) -> &str {
    path.to_str().expect("test paths must be valid UTF-8")
}
