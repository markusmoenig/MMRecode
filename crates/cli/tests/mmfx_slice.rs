#![allow(missing_docs)]

use std::process::Command;

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
