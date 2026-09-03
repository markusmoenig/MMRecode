//! Scripted coverage for the first linked-media editor slice.

use std::{
    io::Write as _,
    process::{Command, Stdio},
};

#[test]
fn no_subcommand_starts_the_editor() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_mmrecode"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"quit\n").unwrap();
    let output = child.wait_with_output().unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("MMRecode linked-media editor"));
    assert!(stdout.contains("Untitled >"));
}

#[test]
fn saving_the_default_project_adds_extension_and_adopts_the_file_name() {
    let name = format!("mmrecode-auto-name-{}", std::process::id());
    let requested = std::env::temp_dir().join(&name);
    let saved = requested.with_file_name(format!("{name}.mmrecode"));
    let mut child = Command::new(env!("CARGO_BIN_EXE_mmrecode"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("save as \"{}\"\nproject info\nquit\n", requested.display()).as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();

    assert!(
        output.status.success(),
        "default project save failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains(&format!("ok: saved {}", saved.display())));
    assert!(stdout.contains(&format!("{name}\ncanvas:")));
    let loaded = mmrecode_edit::load_project_file(&saved).unwrap();
    assert_eq!(loaded.name, name);

    let _ = std::fs::remove_file(saved);
}

#[test]
fn script_and_interactive_engine_share_recursive_media_commands() {
    let script =
        std::env::temp_dir().join(format!("mmrecode-editor-slice-{}.mmrs", std::process::id()));
    std::fs::write(
        &script,
        "add video Clip0 4:00\n\
         cd Clip0\n\
         add text \"Opening Title\" 0:20 at 0:10\n\
         ls\n\
         cd \"Opening Title\"\n\
         in +0:05\n\
         info\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_mmrecode"))
        .arg("edit")
        .arg(&script)
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&script);

    assert!(
        output.status.success(),
        "editor script failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Opening Title    [text"));
    assert!(stdout.contains("ok: in +0:05  [/Clip0/Opening Title]"));
    assert!(stdout.contains("source: 0:05..0:20"));
    assert!(stdout.contains("parent timeline: 0:15..1:00"));
}

#[test]
fn scripted_open_probes_real_mpeg2_and_trims_the_imported_placement() {
    let script =
        std::env::temp_dir().join(format!("mmrecode-editor-open-{}.mmrs", std::process::id()));
    let media = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/mpeg2/valid/main-ml-progressive-ibp.m2v");
    std::fs::write(
        &script,
        format!(
            "import \"{}\" as Clip0\nin +0:02\nout -0:01\ninfo\n",
            media.display()
        ),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_mmrecode"))
        .arg("edit")
        .arg(&script)
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&script);

    assert!(
        output.status.success(),
        "real-media editor script failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("ok: import main-ml-progressive-ibp.m2v as Clip0"));
    assert!(stdout.contains("source: 0:02..0:11"));
    assert!(stdout.contains("parent timeline: 0:02..0:13"));
}

#[test]
fn project_can_be_saved_reopened_planned_and_exported() {
    let token = format!("{}", std::process::id());
    let script = std::env::temp_dir().join(format!("mmrecode-lifecycle-{token}.mmrs"));
    let project = std::env::temp_dir().join(format!("mmrecode-lifecycle-{token}.mmrecode"));
    let exported = std::env::temp_dir().join(format!("mmrecode-lifecycle-{token}.ts"));
    let media = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/mpeg2/valid/main-ml-progressive-ibp.m2v");
    std::fs::write(
        &script,
        format!(
            "new Demo using pal-576i25\nproject set size 96x64\nproject set scan progressive\nimport \"{}\" as Clip0\nout -0:01\nsave as \"{}\"\nexport plan using mpeg2-ts\nexport \"{}\" using mpeg2-ts\n",
            media.display(),
            project.display(),
            exported.display(),
        ),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_mmrecode"))
        .arg("edit")
        .arg(&script)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "project lifecycle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("ok: saved"));
    assert!(stdout.contains("Export preset: mpeg2-ts"));
    assert!(stdout.contains("Wrote"));
    assert!(std::fs::metadata(&exported).unwrap().len() > 0);

    let mut child = Command::new(env!("CARGO_BIN_EXE_mmrecode"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("open \"{}\"\nproject info\nquit\n", project.display()).as_bytes())
        .unwrap();
    let reopened = child.wait_with_output().unwrap();
    assert!(reopened.status.success());
    let reopened_stdout = String::from_utf8(reopened.stdout).unwrap();
    assert!(reopened_stdout.contains("ok: opened"));
    assert!(reopened_stdout.contains("canvas: 96x64"));
    assert!(reopened_stdout.contains("state: saved"));

    let _ = std::fs::remove_file(script);
    let _ = std::fs::remove_file(project);
    let _ = std::fs::remove_file(exported);
}

#[test]
fn export_conforms_source_rate_and_scales_to_the_project_canvas() {
    let token = format!("{}", std::process::id());
    let script = std::env::temp_dir().join(format!("mmrecode-conform-{token}.mmrs"));
    let exported = std::env::temp_dir().join(format!("mmrecode-conform-{token}.ts"));
    let media = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/mpeg2/valid/main-ml-progressive-ibp.m2v");
    std::fs::write(
        &script,
        format!(
            "project set size 128x72\nimport \"{}\" as Clip0\nscale fill\nexport plan using mpeg2-ts\nexport \"{}\" using mpeg2-ts\n",
            media.display(),
            exported.display(),
        ),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_mmrecode"))
        .arg("edit")
        .arg(&script)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "full-render export failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Path: full project-timeline render"));
    assert!(stdout.contains("96x64 -> 128x72 (fill)"));
    assert!(stdout.contains("rate 25/1 -> 30/1 fps"));
    let transport =
        mmrecode_mpegts::demux_transport_stream(&std::fs::read(&exported).unwrap()).unwrap();
    let elementary = transport.mpeg2_video_bytes().unwrap();
    let stream = mmrecode_mpeg2::parse_stream(&elementary).unwrap();
    let sequence = &stream.pictures()[0].sequence;
    assert_eq!((sequence.width, sequence.height), (128, 72));
    assert_eq!(
        sequence.frame_rate,
        mmrecode_core::Rational::new(30, 1).unwrap()
    );

    let _ = std::fs::remove_file(script);
    let _ = std::fs::remove_file(exported);
}

#[test]
fn export_renders_all_sequential_root_timeline_placements() {
    let token = format!("{}", std::process::id());
    let script = std::env::temp_dir().join(format!("mmrecode-timeline-{token}.mmrs"));
    let exported = std::env::temp_dir().join(format!("mmrecode-timeline-{token}.ts"));
    let media = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/mpeg2/valid/main-ml-progressive-ibp.m2v");
    std::fs::write(
        &script,
        format!(
            "project set size 96x64\nimport \"{}\" as First\ncd /\nimport \"{}\" as Second\nexport plan using mpeg2-ts\nexport \"{}\" using mpeg2-ts\n",
            media.display(),
            media.display(),
            exported.display(),
        ),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_mmrecode"))
        .arg("edit")
        .arg(&script)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "timeline export failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Path: full project-timeline render"));
    assert!(stdout.contains("Placements: 2 in project composition order"));
    assert!(stdout.contains("First:"));
    assert!(stdout.contains("Second:"));
    let transport =
        mmrecode_mpegts::demux_transport_stream(&std::fs::read(&exported).unwrap()).unwrap();
    let elementary = transport.mpeg2_video_bytes().unwrap();
    let stream = mmrecode_mpeg2::parse_stream(&elementary).unwrap();
    assert_eq!(stream.pictures().len(), 28);

    let _ = std::fs::remove_file(script);
    let _ = std::fs::remove_file(exported);
}

#[test]
fn project_match_adopts_focused_video_and_audio_format() {
    let token = format!("{}", std::process::id());
    let script = std::env::temp_dir().join(format!("mmrecode-match-{token}.mmrs"));
    let project = std::env::temp_dir().join(format!("mmrecode-match-{token}.mmrecode"));
    let media = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/mpegts/valid/single-program-mpeg2-mp2.ts");
    std::fs::write(
        &script,
        format!(
            "project set audio-rate 44100\nproject set audio-channels 1\nimport \"{}\" as Source\nproject match\nsave as \"{}\"\n",
            media.display(),
            project.display(),
        ),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_mmrecode"))
        .arg("edit")
        .arg(&script)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "project match failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("project match focused media (video and audio)"));
    let loaded = mmrecode_edit::load_project_file(&project).unwrap();
    let settings = loaded.settings();
    assert_eq!((settings.width, settings.height), (96, 64));
    assert_eq!(
        settings.frame_rate,
        mmrecode_core::Rational::new(25, 1).unwrap()
    );
    assert_eq!(settings.audio_sample_rate, 48_000);
    assert_eq!(settings.audio_channels, 2);
    assert_eq!(settings.base_preset, None);

    let _ = std::fs::remove_file(script);
    let _ = std::fs::remove_file(project);
}
