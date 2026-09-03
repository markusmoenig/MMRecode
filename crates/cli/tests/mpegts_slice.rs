#![allow(missing_docs)]

use std::{path::PathBuf, process::Command};

#[test]
#[allow(clippy::too_many_lines)]
fn transport_inspect_mux_demux_and_decode_round_trip() {
    let binary = env!("CARGO_BIN_EXE_mmrecode");
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let elementary = root.join("testdata/mpeg2/valid/main-ml-progressive-ibp.m2v");
    let audio = root.join("testdata/mpegaudio/valid/sine-48k-stereo-192k.mp2");
    let independent = root.join("testdata/mpegts/valid/single-program-mpeg2-mp2.ts");
    let stem = format!("mmrecode-mpegts-slice-{}", std::process::id());
    let transport = std::env::temp_dir().join(format!("{stem}.ts"));
    let extracted = std::env::temp_dir().join(format!("{stem}.m2v"));
    let extracted_audio = std::env::temp_dir().join(format!("{stem}.mp2"));
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
    assert!(report.contains("PID 0x0101: stream type 0x03, audio/mpeg1"));
    assert!(report.contains("MPEG-1 Audio Layer II: 48000 Hz, 2 channel(s)"));

    assert!(
        Command::new(binary)
            .args([
                "mux-mpegts",
                elementary.to_str().unwrap(),
                transport.to_str().unwrap(),
                audio.to_str().unwrap(),
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
                "extract-mpegts-audio",
                transport.to_str().unwrap(),
                extracted_audio.to_str().unwrap(),
            ])
            .status()
            .expect("extract MPEG-TS audio")
            .success()
    );
    assert_eq!(
        std::fs::read(&extracted_audio).unwrap(),
        std::fs::read(&audio).unwrap()
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

    if Command::new("ffprobe").arg("-version").output().is_ok() {
        let probe = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "stream=codec_name,sample_rate,channels,duration",
                "-of",
                "default=noprint_wrappers=1",
                transport.to_str().unwrap(),
            ])
            .output()
            .expect("probe native MPEG-TS A/V");
        assert!(probe.status.success());
        let report = String::from_utf8(probe.stdout).unwrap();
        assert!(report.contains("codec_name=mpeg2video"));
        assert!(report.contains("codec_name=mp2"));
        assert!(report.contains("sample_rate=48000"));
        assert!(report.contains("channels=2"));
        assert!(report.matches("duration=0.480000").count() >= 2);
    }
    if Command::new("ffmpeg").arg("-version").output().is_ok() {
        let decoded_audio = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-i",
                transport.to_str().unwrap(),
                "-map",
                "0:a:0",
                "-f",
                "s16le",
                "-acodec",
                "pcm_s16le",
                "pipe:1",
            ])
            .output()
            .expect("decode native MPEG-TS audio with FFmpeg");
        assert!(decoded_audio.status.success());
        assert_eq!(decoded_audio.stdout.len(), 20 * 1_152 * 2 * 2);
    }

    let _ = std::fs::remove_file(transport);
    let _ = std::fs::remove_file(extracted);
    let _ = std::fs::remove_file(extracted_audio);
    let _ = std::fs::remove_file(decoded);
}
