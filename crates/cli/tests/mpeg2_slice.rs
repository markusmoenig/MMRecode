#![allow(missing_docs)]

use std::{path::PathBuf, process::Command};

#[test]
fn mpeg2_inspect_decode_encode_and_decode_round_trip() {
    let binary = env!("CARGO_BIN_EXE_mmrecode");
    let input = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/mpeg2/valid/main-ml-progressive-ibp.m2v");
    let stem = format!("mmrecode-cli-mpeg2-slice-{}", std::process::id());
    let decoded = std::env::temp_dir().join(format!("{stem}.y4m"));
    let encoded = std::env::temp_dir().join(format!("{stem}-encoded.m2v"));
    let roundtrip = std::env::temp_dir().join(format!("{stem}-roundtrip.y4m"));

    let inspect = Command::new(binary)
        .args(["inspect", input.to_str().unwrap()])
        .output()
        .expect("run MPEG-2 inspect");
    assert!(inspect.status.success());
    let report = String::from_utf8(inspect.stdout).unwrap();
    assert!(report.contains("MPEG-2 Video:"));
    assert!(report.contains("pictures: 12"));
    assert!(report.contains("Picture 0: I"));
    assert!(report.contains("Picture 1: P"));
    assert!(report.contains("Picture 2: B"));

    let plan = Command::new(binary)
        .args(["plan-mpeg2", input.to_str().unwrap(), "0", "1"])
        .output()
        .expect("plan MPEG-2 smart render");
    assert!(plan.status.success());
    let plan = String::from_utf8(plan.stdout).unwrap();
    assert!(plan.contains("Encode display range(s): 0..4"));
    assert!(plan.contains("Pictures copied: 8; encoded/bridged: 4"));

    assert!(
        Command::new(binary)
            .args(["decode", input.to_str().unwrap(), decoded.to_str().unwrap()])
            .status()
            .expect("decode MPEG-2 vector")
            .success()
    );
    assert!(
        Command::new(binary)
            .args([
                "encode-mpeg2",
                decoded.to_str().unwrap(),
                encoded.to_str().unwrap(),
                "8",
            ])
            .status()
            .expect("encode MPEG-2 vector")
            .success()
    );
    assert!(
        Command::new(binary)
            .args([
                "decode",
                encoded.to_str().unwrap(),
                roundtrip.to_str().unwrap(),
            ])
            .status()
            .expect("decode native MPEG-2 output")
            .success()
    );
    assert!(std::fs::metadata(&encoded).unwrap().len() > 1_000);
    assert!(std::fs::metadata(&roundtrip).unwrap().len() > 1_000);

    let verify = Command::new(binary)
        .args(["verify", input.to_str().unwrap(), decoded.to_str().unwrap()])
        .output()
        .expect("verify MPEG-2 against native Y4M");
    assert!(verify.status.success());
    assert!(
        String::from_utf8(verify.stdout)
            .unwrap()
            .contains("Verification passed for 12 MPEG-2 picture(s)")
    );

    let _ = std::fs::remove_file(decoded);
    let _ = std::fs::remove_file(encoded);
    let _ = std::fs::remove_file(roundtrip);
}
