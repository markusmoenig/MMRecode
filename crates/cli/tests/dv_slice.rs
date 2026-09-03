//! End-to-end command-line coverage for the raw DV25 vertical slice.

use std::{fs, path::Path, process::Command};

#[test]
fn dv_inspect_decode_encode_and_decode_round_trip() {
    let binary = env!("CARGO_BIN_EXE_mmrecode");
    let repository = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let source = repository.join("testdata/dv/valid/dv25-525-60-one-frame.dv");
    let stem = format!("mmrecode-dv-slice-{}", std::process::id());
    let y4m = std::env::temp_dir().join(format!("{stem}-source.y4m"));
    let encoded = std::env::temp_dir().join(format!("{stem}.dv"));
    let reconstructed = std::env::temp_dir().join(format!("{stem}-reconstructed.y4m"));
    let audio = std::env::temp_dir().join(format!("{stem}.s16le"));

    run(binary, ["inspect", path(&source)]);
    run(binary, ["extract-dv-audio", path(&source), path(&audio)]);
    run(binary, ["decode", path(&source), path(&y4m)]);
    run(binary, ["encode-dv", path(&y4m), path(&encoded)]);
    run(binary, ["inspect", path(&encoded)]);
    run(binary, ["decode", path(&encoded), path(&reconstructed)]);
    run(binary, ["compare", path(&y4m), path(&reconstructed)]);

    assert_eq!(fs::metadata(&encoded).unwrap().len(), 120_000);
    assert_eq!(fs::metadata(&audio).unwrap().len(), 1_600 * 2 * 2);
    for path in [y4m, encoded, reconstructed, audio] {
        let _ = fs::remove_file(path);
    }
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
