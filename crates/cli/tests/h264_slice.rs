#![allow(missing_docs)]

use std::process::Command;

#[test]
#[allow(clippy::too_many_lines)]
fn h264_mp4_inspect_import_and_project_match() {
    if Command::new("ffmpeg").arg("-version").output().is_err() {
        eprintln!("skipping H.264 MP4 vertical-slice test: ffmpeg is unavailable");
        return;
    }
    let token = format!("{}", std::process::id());
    let movie = std::env::temp_dir().join(format!("mmrecode-h264-{token}.mp4"));
    let remuxed = std::env::temp_dir().join(format!("mmrecode-h264-remux-{token}.mp4"));
    let script = std::env::temp_dir().join(format!("mmrecode-h264-{token}.mmrs"));
    let status = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=128x72:rate=30000/1001",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000",
            "-frames:v",
            "30",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-g",
            "10",
            "-keyint_min",
            "10",
            "-sc_threshold",
            "0",
            "-c:a",
            "aac",
            "-shortest",
            "-movflags",
            "+faststart",
            "-y",
        ])
        .arg(&movie)
        .status()
        .expect("run ffmpeg test encoder");
    if !status.success() {
        eprintln!("skipping H.264 MP4 vertical-slice test: encoder is unavailable");
        return;
    }

    let inspect = Command::new(env!("CARGO_BIN_EXE_mmrecode"))
        .arg("inspect")
        .arg(&movie)
        .output()
        .expect("inspect H.264 MP4");
    assert!(
        inspect.status.success(),
        "inspect failed: {}",
        String::from_utf8_lossy(&inspect.stderr)
    );
    let report = String::from_utf8(inspect.stdout).unwrap();
    assert!(report.contains("ISO-BMFF/QuickTime:"));
    assert!(report.contains("video/h264 (avc1)"));
    assert!(report.contains("audio/aac (mp4a)"));
    assert!(report.contains("H.264: 128x72"));
    assert!(report.contains("Pictures: 30"));

    std::fs::write(
        &script,
        format!(
            "import \"{}\" as Phone\nproject match\nproject info\ninfo\n",
            movie.display()
        ),
    )
    .unwrap();
    let edit = Command::new(env!("CARGO_BIN_EXE_mmrecode"))
        .arg("edit")
        .arg(&script)
        .output()
        .expect("import H.264 MP4");
    assert!(
        edit.status.success(),
        "editor import failed: {}",
        String::from_utf8_lossy(&edit.stderr)
    );
    let report = String::from_utf8(edit.stdout).unwrap();
    assert!(report.contains("ok: import"));
    assert!(report.contains("project match focused media (video and audio)"));
    assert!(report.contains("canvas: 128x72"));
    assert!(report.contains("rate: 30000/1001 fps"));
    assert!(report.contains("[video/h264]"));

    let plan = Command::new(env!("CARGO_BIN_EXE_mmrecode"))
        .args(["plan-h264"])
        .arg(&movie)
        .args(["10", "20"])
        .output()
        .expect("plan H.264 clean remux");
    assert!(
        plan.status.success(),
        "H.264 plan failed: {}",
        String::from_utf8_lossy(&plan.stderr)
    );
    let report = String::from_utf8(plan.stdout).unwrap();
    assert!(report.contains("presentation frames: 10..20"));
    assert!(report.contains("GOPs copied:         1"));
    assert!(report.contains("encoded frames:      0"));

    let remux = Command::new(env!("CARGO_BIN_EXE_mmrecode"))
        .arg("remux-h264")
        .arg(&movie)
        .arg(&remuxed)
        .args(["10", "20"])
        .output()
        .expect("remux H.264 clean GOP");
    assert!(
        remux.status.success(),
        "H.264 remux failed: {}",
        String::from_utf8_lossy(&remux.stderr)
    );
    let source = mmrecode_isobmff::IsoBmffFile::open(&movie).unwrap();
    let output = mmrecode_isobmff::IsoBmffFile::open(&remuxed).unwrap();
    let source_track = source.h264_track().unwrap();
    let output_track = output.h264_track().unwrap();
    assert_eq!(output_track.samples.len(), 10);
    assert_eq!(output_track.samples[0].dts, 0);
    assert_eq!(
        output_track.samples.iter().map(|sample| sample.pts).min(),
        Some(0)
    );
    for (source_sample, output_sample) in source_track.samples[10..20]
        .iter()
        .zip(&output_track.samples)
    {
        assert_eq!(
            source.sample_data(source_sample).unwrap(),
            output.sample_data(output_sample).unwrap()
        );
    }
    let decode = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(&remuxed)
        .args(["-map", "0:v:0", "-f", "null", "-"])
        .output()
        .expect("decode remuxed H.264 MP4");
    assert!(
        decode.status.success(),
        "remuxed file did not decode: {}",
        String::from_utf8_lossy(&decode.stderr)
    );

    let unsafe_plan = Command::new(env!("CARGO_BIN_EXE_mmrecode"))
        .arg("plan-h264")
        .arg(&movie)
        .args(["1", "10"])
        .output()
        .expect("reject H.264 cut inside GOP");
    assert!(!unsafe_plan.status.success());
    assert!(String::from_utf8_lossy(&unsafe_plan.stderr).contains("require re-encoding"));

    let _ = std::fs::remove_file(movie);
    let _ = std::fs::remove_file(remuxed);
    let _ = std::fs::remove_file(script);
}
