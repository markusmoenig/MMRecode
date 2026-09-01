#![allow(missing_docs)]

use std::{io::Write as _, path::PathBuf, process::Command};

#[test]
#[allow(clippy::too_many_lines)]
fn plans_and_executes_one_frame_mpeg2_replacement() {
    let binary = env!("CARGO_BIN_EXE_mmrecode");
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let video = root.join("testdata/mpeg2/valid/main-ml-progressive-ibp.m2v");
    let audio = root.join("testdata/mpegaudio/valid/sine-48k-stereo-192k.mp2");
    let stem = format!("mmrecode-cli-render-slice-{}", std::process::id());
    let replacement_path = std::env::temp_dir().join(format!("{stem}-replacement.y4m"));
    let output_path = std::env::temp_dir().join(format!("{stem}.ts"));

    let source_bytes = std::fs::read(&video).unwrap();
    let original = mmrecode_mpeg2::decode_stream(&source_bytes).unwrap();
    let mut replacement = original[3].frame.clone();
    replacement.planes[0].data.fill(32);
    replacement.planes[1].data.fill(128);
    replacement.planes[2].data.fill(128);
    let replacement_file = std::fs::File::create(&replacement_path).unwrap();
    let mut replacement_writer =
        mmrecode_y4m::Y4mWriter::new(std::io::BufWriter::new(replacement_file));
    replacement_writer.write_frame(&replacement).unwrap();
    replacement_writer.into_inner().flush().unwrap();

    let plan = Command::new(binary)
        .args([
            "render-plan",
            video.to_str().unwrap(),
            "--replace",
            "3",
            replacement_path.to_str().unwrap(),
            "--audio",
            audio.to_str().unwrap(),
            "--audio-end",
            "exact",
        ])
        .output()
        .expect("plan MPEG-2 replacement render");
    assert!(
        plan.status.success(),
        "render-plan failed: {}",
        String::from_utf8_lossy(&plan.stderr)
    );
    let report = String::from_utf8(plan.stdout).unwrap();
    assert!(report.contains("Changed display frame: 3"));
    assert!(report.contains("Work: 4 decode, 3 encode, 9 copy packet(s)"));
    assert!(report.contains("MPEG-TS dry run"));
    assert!(report.contains("9 copied + 3 regenerated"));
    assert!(report.contains("audio 20/20 frame(s), end delta +0 us (Exact)"));
    assert!(report.contains("MPEG-2 splice metadata:"));
    assert!(report.contains("GOP policy: timecode Recomputed"));

    let render = Command::new(binary)
        .args([
            "render",
            video.to_str().unwrap(),
            output_path.to_str().unwrap(),
            "--replace",
            "3",
            replacement_path.to_str().unwrap(),
            "--audio",
            audio.to_str().unwrap(),
        ])
        .output()
        .expect("execute MPEG-2 replacement render");
    assert!(
        render.status.success(),
        "render failed: {}",
        String::from_utf8_lossy(&render.stderr)
    );
    let report = String::from_utf8(render.stdout).unwrap();
    assert!(report.contains("transport packet(s)"));
    assert!(report.contains("Wrote "));

    let transport_bytes = std::fs::read(&output_path).unwrap();
    let transport = mmrecode_mpegts::demux_transport_stream(&transport_bytes).unwrap();
    assert_eq!(
        transport.mpeg1_audio_bytes().unwrap(),
        std::fs::read(&audio).unwrap()
    );
    let rendered_video = transport.mpeg2_video_bytes().unwrap();
    let rendered = mmrecode_mpeg2::decode_stream(&rendered_video).unwrap();
    assert_eq!(rendered.len(), 12);
    assert_ne!(rendered[3].frame.planes, original[3].frame.planes);
    for index in 4..12 {
        assert_eq!(rendered[index].frame.planes, original[index].frame.planes);
    }

    let _ = std::fs::remove_file(replacement_path);
    let _ = std::fs::remove_file(output_path);
}
