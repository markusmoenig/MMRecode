#![allow(missing_docs)]

use std::{path::PathBuf, process::Command};

#[test]
fn transport_inspect_mux_demux_and_decode_round_trip() {
    let binary = env!("CARGO_BIN_EXE_mmrecode");
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let elementary = root.join("testdata/mpeg2/valid/main-ml-progressive-ibp.m2v");
    let independent = root.join("testdata/mpegts/valid/single-program-mpeg2.ts");
    let stem = format!("mmrecode-cli-mpegts-slice-{}", std::process::id());
    let transport = std::env::temp_dir().join(format!("{stem}.ts"));
    let extracted = std::env::temp_dir().join(format!("{stem}.m2v"));
    let decoded = std::env::temp_dir().join(format!("{stem}.y4m"));

    let inspect = Command::new(binary)
        .args(["inspect", independent.to_str().unwrap()])
        .output()
        .expect("inspect independent MPEG-TS");
    assert!(inspect.status.success());
    let report = String::from_utf8(inspect.stdout).unwrap();
    assert!(report.contains("MPEG-2 Transport Stream:"));
    assert!(report.contains("PMT PID 0x1000, PCR PID 0x0100"));
    assert!(report.contains("MPEG-2 Video: 96x64"));

    assert!(
        Command::new(binary)
            .args([
                "mux-mpegts",
                elementary.to_str().unwrap(),
                transport.to_str().unwrap(),
            ])
            .status()
            .expect("mux MPEG-TS")
            .success()
    );
    assert!(
        Command::new(binary)
            .args([
                "demux-mpegts",
                transport.to_str().unwrap(),
                extracted.to_str().unwrap(),
            ])
            .status()
            .expect("demux MPEG-TS")
            .success()
    );
    assert_eq!(
        std::fs::read(&extracted).unwrap(),
        std::fs::read(&elementary).unwrap()
    );
    assert!(
        Command::new(binary)
            .args([
                "decode",
                transport.to_str().unwrap(),
                decoded.to_str().unwrap(),
            ])
            .status()
            .expect("decode MPEG-TS")
            .success()
    );
    assert!(std::fs::metadata(&decoded).unwrap().len() > 1_000);

    let _ = std::fs::remove_file(transport);
    let _ = std::fs::remove_file(extracted);
    let _ = std::fs::remove_file(decoded);
}
