#![allow(missing_docs)]

use std::{io::Write as _, process::Command};

#[test]
fn renders_typed_mmfx_text_with_module_relative_font() {
    let scene = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/mmfx/lower-third.mmfx");
    let output =
        std::env::temp_dir().join(format!("mmrecode-mmfx-text-{}.png", std::process::id()));
    let result = Command::new(env!("CARGO_BIN_EXE_mmrecode"))
        .arg("render-mmfx")
        .arg(&scene)
        .arg(&output)
        .output()
        .expect("render MMFX example");
    assert!(
        result.status.success(),
        "MMFX render failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(String::from_utf8_lossy(&result.stdout).contains("Rendered MMFX scene 'lower-third'"));
    let image = image::open(&output).expect("decode rendered PNG");
    assert_eq!((image.width(), image.height()), (1280, 720));
    let _ = std::fs::remove_file(output);
}

#[test]
fn scripted_scene_load_fits_fixed_scroll_duration() {
    let token = std::process::id();
    let script = std::env::temp_dir().join(format!("mmrecode-mmfx-fit-{token}.mmrs"));
    let project = std::env::temp_dir().join(format!("mmrecode-mmfx-fit-{token}.mmrecode"));
    let scene = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/mmfx/eldiron-patrons.mmfx");
    let mut file = std::fs::File::create(&script).unwrap();
    writeln!(
        file,
        "add scene Credits 1:00\ncd Credits\nscene load \"{}\"\nsave as \"{}\"",
        scene.display(),
        project.display()
    )
    .unwrap();

    let result = Command::new(env!("CARGO_BIN_EXE_mmrecode"))
        .arg("edit")
        .arg(&script)
        .output()
        .expect("load fixed-duration scene from script");
    assert!(
        result.status.success(),
        "scripted scene load failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(
        String::from_utf8_lossy(&result.stdout).contains("fitted scene placement to 300 frames")
    );
    let saved = mmrecode_edit::load_project_file(&project).unwrap();
    let placement = saved.placement_links().next().unwrap();
    assert_eq!(placement.source_range.end.value, 300);
    assert_eq!(placement.timeline_range.end.value, 300);

    let _ = std::fs::remove_file(script);
    let _ = std::fs::remove_file(project);
}
